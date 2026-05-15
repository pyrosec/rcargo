//! v1 transport: shell out to system `rsync` and `ssh`. No daemon required on
//! the remote — just a working cargo toolchain and rsync.
//!
//! Why shell out instead of using a Rust ssh library:
//! - rsync's delta-transfer/compression is already optimal; reinventing it
//!   in Rust would lose us months and is not the value-add.
//! - Both tools are installed everywhere we'd want to run rcargo. The
//!   bootstrap doc is literally "install Rust" — nothing else.
//! - `ssh` correctly handles `~/.ssh/config` aliases, jump hosts, agent
//!   forwarding, multiplexing. Libraries reinvent half of this badly.
//!
//! Streaming model: we use `Stdio::piped()` + `tokio::io::AsyncBufReadExt`
//! to read child stdout/stderr line by line and forward to our own
//! stdout/stderr unbuffered. Cargo's progress lines (which use `\r`) come
//! through as one line per progress update — acceptable for now.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use super::TransportOutcome;
use crate::config::Config;

/// Lines/paths that should never make it into the rsync payload. Trying to
/// upload `target/` would defeat the entire point of remote caching.
const RSYNC_EXCLUDES: &[&str] = &[
    "target/",
    "node_modules/",
    ".DS_Store",
    "*.swp",
    ".env",
    ".env.local",
];

pub struct SshTransport {
    cfg: Config,
}

impl SshTransport {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    /// Send the local tree to the remote. Source-only — `target/` is excluded.
    /// The remote `target/` is preserved across calls (incremental cache).
    pub async fn sync_up(&self, local_root: &Path, project_key: &str) -> Result<()> {
        // Make sure remote_root/<key>/ exists. `mkdir -p` is idempotent.
        let mkdir_status = Command::new("ssh")
            .args(&self.cfg.ssh_args)
            .arg(&self.cfg.host)
            .arg("--")
            .arg(format!(
                "mkdir -p {}/{}",
                shell_quote_path(&self.cfg.remote_root),
                shell_quote(project_key),
            ))
            .stdin(Stdio::null())
            .status()
            .await
            .context("spawn ssh for remote mkdir")?;
        if !mkdir_status.success() {
            return Err(anyhow!(
                "ssh mkdir failed (status {:?}). Is {} reachable?",
                mkdir_status.code(),
                self.cfg.host
            ));
        }

        // rsync sources only. Trailing slash on src is intentional — copies
        // contents, not the directory itself.
        let mut src = local_root.to_path_buf();
        // canonicalize so a relative `.` becomes absolute and the trailing
        // slash semantics are unambiguous.
        if let Ok(c) = src.canonicalize() {
            src = c;
        }
        let src_str = format!("{}/", src.display());

        let dest = format!(
            "{}:{}/{}/",
            self.cfg.host, self.cfg.remote_root, project_key
        );

        let mut rsync = Command::new("rsync");
        rsync.arg("-az"); // archive + compress
        rsync.arg("--delete");
        // Keep target/ alive on the remote across runs.
        rsync.arg("--filter=protect target/");
        for ex in RSYNC_EXCLUDES {
            rsync.arg("--exclude").arg(ex);
        }
        for extra in &self.cfg.rsync_args {
            rsync.arg(extra);
        }
        // Custom rsh so rsync uses the same ssh args we'd pass for `run_cargo`.
        if !self.cfg.ssh_args.is_empty() {
            let rsh = format!("ssh {}", self.cfg.ssh_args.join(" "));
            rsync.arg("-e").arg(rsh);
        }
        rsync.arg(&src_str).arg(&dest);
        rsync.stdin(Stdio::null());

        let status = rsync.status().await.context("spawn rsync")?;
        if !status.success() {
            return Err(anyhow!(
                "rsync failed (status {:?}) — check that rsync is installed locally and remotely",
                status.code()
            ));
        }
        Ok(())
    }

    /// Run `cargo <args>` on the remote. Streams stdout/stderr through our
    /// own stdio unbuffered. Returns the exit code.
    pub async fn run_cargo(&self, project_key: &str, args: &[String]) -> Result<TransportOutcome> {
        let cargo_cmdline = build_remote_cargo_cmd(&self.cfg.remote_root, project_key, args);

        let mut child = Command::new("ssh")
            .args(&self.cfg.ssh_args)
            // -tt forces a pty so colors / progress work as if user ssh'd in
            // themselves. Without -t, cargo detects a non-tty and disables
            // colors.
            .arg("-tt")
            .arg(&self.cfg.host)
            .arg("--")
            .arg(&cargo_cmdline)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn ssh for remote cargo")?;

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Tee stdout → our stdout and stderr → our stderr concurrently.
        let stdout_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            let mut out = tokio::io::stdout();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = out.write_all(line.as_bytes()).await;
                let _ = out.write_all(b"\n").await;
                let _ = out.flush().await;
            }
        });
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            let mut err = tokio::io::stderr();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = err.write_all(line.as_bytes()).await;
                let _ = err.write_all(b"\n").await;
                let _ = err.flush().await;
            }
        });

        let status = child.wait().await.context("wait on ssh child")?;
        let _ = stdout_task.await;
        let _ = stderr_task.await;

        Ok(TransportOutcome {
            exit_code: status.code().unwrap_or(255),
        })
    }

    /// Best-effort pull of `target/release/*` (plus a few common variants)
    /// back to the local `target/`. Missing files are not errors.
    pub async fn pull_artifacts(&self, local_root: &Path, project_key: &str) -> Result<()> {
        let local_target = local_root.join("target");
        std::fs::create_dir_all(&local_target).ok();

        // Patterns we try to pull. Order: most useful first.
        let patterns: &[&str] = &[
            "target/release/",
            "target/debug/",
            "target/wasm32-unknown-unknown/release/",
            "target/wasm32-wasi/release/",
        ];

        for pat in patterns {
            let src = format!(
                "{}:{}/{}/{}",
                self.cfg.host, self.cfg.remote_root, project_key, pat
            );
            let dest: PathBuf = local_target.join(pat.trim_start_matches("target/"));
            std::fs::create_dir_all(&dest).ok();

            let mut rsync = Command::new("rsync");
            rsync
                .arg("-az")
                .arg("--ignore-missing-args")
                // Only files — don't drag subdirs of build/ or deps/ back, they
                // contain GBs of incremental crate artifacts that the LOCAL
                // toolchain can't use anyway.
                .arg("--include=*/")
                .arg("--include=*.wasm")
                .arg("--include=*.so")
                .arg("--include=*.a")
                // Anything without an extension at depth-1 (binaries).
                .arg("--include=*")
                .arg("--exclude=build/")
                .arg("--exclude=deps/")
                .arg("--exclude=incremental/")
                .arg("--exclude=.fingerprint/");
            if !self.cfg.ssh_args.is_empty() {
                let rsh = format!("ssh {}", self.cfg.ssh_args.join(" "));
                rsync.arg("-e").arg(rsh);
            }
            rsync.arg(&src).arg(&dest);
            rsync.stdin(Stdio::null());
            // Don't propagate errors — pattern may simply not exist.
            let _ = rsync.status().await;
        }
        Ok(())
    }
}

/// Compose the remote shell command line. We `cd` into the project tree and
/// then run cargo with whatever args the user gave us. Args are quoted to
/// survive the trip through ssh's single-string command.
///
/// The remote_root and project_key go through [`shell_quote_path`] so that
/// a leading `~/` is preserved as an unquoted tilde — POSIX shells only
/// expand `~` when it's at the start of an *unquoted* word, so naively
/// quoting `~/rcargo-builds` produces a literal `~` directory at the wrong
/// path. This was a real bug discovered against `meta` / `lowbot` on
/// 2026-05-15 (rsync expanded `~` correctly via its own ssh invocation but
/// the subsequent `cd '~/rcargo-builds/key'` did not).
pub fn build_remote_cargo_cmd(remote_root: &str, project_key: &str, args: &[String]) -> String {
    let mut cmd = format!(
        "cd {}/{} && cargo",
        shell_quote_path(remote_root),
        shell_quote(project_key)
    );
    for a in args {
        cmd.push(' ');
        cmd.push_str(&shell_quote(a));
    }
    cmd
}

/// Quote a single shell argument the safe way — wrap in single quotes and
/// escape any embedded single quotes. Sufficient for POSIX shells (sh/bash/
/// zsh/dash). We do NOT need fish/csh support — ssh always invokes the
/// remote default shell, which on every Linux/macOS box is POSIX-compatible.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '/' | '=' | ':' | '@' | ',' | '+')
    }) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Like [`shell_quote`], but preserves a leading `~/` or bare `~` unquoted
/// so the remote shell will tilde-expand it to `$HOME`. The portion after
/// the tilde is quoted via [`shell_quote`] for safety. For paths without a
/// leading tilde this is identical to [`shell_quote`].
pub fn shell_quote_path(s: &str) -> String {
    if s == "~" {
        return "~".to_string();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        // Avoid producing `~/''` for the degenerate `~/` input.
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", shell_quote(rest));
    }
    shell_quote(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_safe() {
        assert_eq!(shell_quote("foo"), "foo");
        assert_eq!(shell_quote("foo bar"), "'foo bar'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        // Generic shell_quote still quotes tildes literally — only the
        // path-aware variant peels the leading `~/` off so the remote
        // shell expands it. shell_quote remains unchanged so other
        // callers don't suddenly start emitting unquoted tildes.
        assert_eq!(shell_quote("~/builds"), "'~/builds'");
        assert_eq!(shell_quote("a=b"), "a=b");
    }

    #[test]
    fn shell_quote_path_preserves_leading_tilde() {
        // The bug this fixes: POSIX shells (sh/bash/zsh/dash) only
        // tilde-expand at the start of an *unquoted* word. Wrapping
        // `~/rcargo-builds` in single quotes produces a literal `~`
        // directory at the wrong path. Verified against meta + lowbot
        // on 2026-05-15.
        assert_eq!(shell_quote_path("~/rcargo-builds"), "~/rcargo-builds");
        // Spaces still need quoting on the tail side.
        assert_eq!(shell_quote_path("~/dir with space"), "~/'dir with space'");
        // Bare tilde stays bare.
        assert_eq!(shell_quote_path("~"), "~");
        // Trailing slash without tail is fine.
        assert_eq!(shell_quote_path("~/"), "~/");
        // Absolute paths fall through to shell_quote.
        assert_eq!(shell_quote_path("/tmp/r"), "/tmp/r");
        assert_eq!(shell_quote_path("/tmp/r d"), "'/tmp/r d'");
        // A tilde in the middle of a path is not an expansion site —
        // POSIX treats it literally — so it stays quoted.
        assert_eq!(shell_quote_path("/foo/~bar"), "'/foo/~bar'");
    }

    #[test]
    fn remote_cmd_has_cd_and_args() {
        let cmd = build_remote_cargo_cmd(
            "~/rcargo-builds",
            "foo-abc",
            &["test".into(), "-p".into(), "bar".into()],
        );
        assert!(cmd.contains("cd"));
        assert!(cmd.contains("foo-abc"));
        assert!(cmd.contains("cargo"));
        assert!(cmd.ends_with(" test -p bar"));
    }

    #[test]
    fn remote_cmd_tilde_remains_unquoted() {
        // Regression test: previously this emitted `cd '~/rcargo-builds'/...`
        // which created a literal `~` directory on the remote.
        let cmd = build_remote_cargo_cmd(
            "~/rcargo-builds",
            "proj-abc",
            &["build".into()],
        );
        assert!(
            cmd.starts_with("cd ~/rcargo-builds/proj-abc &&"),
            "expected unquoted tilde, got: {cmd}"
        );
    }

    #[test]
    fn remote_cmd_quotes_dangerous_args() {
        let cmd = build_remote_cargo_cmd("/tmp/r", "k", &["build".into(), "a b".into()]);
        assert!(cmd.contains("'a b'"));
    }
}
