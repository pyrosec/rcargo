//! `rcargo` — drop-in cargo shim that offloads builds to a remote host.
//!
//! The vast majority of arguments are NOT parsed by rcargo — they're forwarded
//! verbatim to remote `cargo`. We only own a tiny set of flags (`--local`,
//! `--pull-artifacts`, `--host`, `--remote-root`) and a small set of internal
//! diagnostics (`--explain-config`). Everything else passes through.
//!
//! Argument convention:
//!
//! - Any arg starting with `--rcargo-*` (or the legal short forms) is an
//!   rcargo flag and is consumed locally.
//! - Everything else, in order, is forwarded to remote `cargo`.
//! - The classic clap-style `--` separator is also honoured: anything after
//!   `--` always goes through.
//!
//! This convention avoids collisions with cargo's own `--release`, `-p`,
//! `--target`, etc. We never need to parse those — cargo on the remote
//! handles them.

use anyhow::{Context, Result};
use rcargo_client::config::{sanity_check, ConfigFile};
use rcargo_client::{project_key_for, Config, SshTransport, Transport};
use std::path::PathBuf;
use std::process::ExitCode;

/// Selected transport for this invocation. `Ssh` is always available; the
/// WebTransport variant only links when the `webtransport` feature is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TransportChoice {
    #[default]
    Ssh,
    WebTransport,
}

const HELP: &str = r#"
rcargo — drop-in cargo replacement that runs cargo on a remote host.

USAGE:
    rcargo [RCARGO FLAGS] [--] <cargo args...>

RCARGO FLAGS:
    --rcargo-host <HOST>          Override RCARGO_HOST / config file host.
    --rcargo-remote-root <PATH>   Override remote root directory.
    --rcargo-pull-artifacts       After build, rsync target/release artifacts back.
    --rcargo-transport <KIND>     `ssh` (default) or `webtransport` (requires
                                  the binary to be built with `--features
                                  webtransport` and rcargod's `--wt-listen`).
    --rcargo-wt-host <HOST>       Host part of the WebTransport URL (default:
                                  same as --rcargo-host).
    --rcargo-wt-port <PORT>       Port of the WebTransport listener (default:
                                  7475 — distinct from the JSON-line 7474).
    --rcargo-local                Bypass rcargo entirely; exec local `cargo`.
    --rcargo-explain-config       Print effective config and exit.
    --rcargo-version              Print version and exit.
    --rcargo-help                 Show this help.

Everything else is forwarded to remote `cargo` verbatim. Examples:

    rcargo build --release
    rcargo test -p mycrate -- --nocapture
    rcargo --rcargo-pull-artifacts build --release
    rcargo --rcargo-local check        # run locally instead

ENV VARS:
    RCARGO_HOST              SSH destination (default: meta).
    RCARGO_REMOTE_ROOT       Parent dir on remote (default: ~/rcargo-builds).
    RCARGO_SSH_ARGS          Extra args for ssh, whitespace-split.
    RCARGO_RSYNC_ARGS        Extra args for rsync, whitespace-split.
    RCARGO_PROJECT_KEY       Override the derived project key.

See ~/.config/rcargo/config.toml or ./rcargo.toml for persistent config.
"#;

#[derive(Debug, Default)]
struct ParsedArgs {
    cli_overrides: ConfigFile,
    pull_artifacts_flag: bool,
    local: bool,
    explain_config: bool,
    print_version: bool,
    print_help: bool,
    cargo_args: Vec<String>,
    /// Selected transport. Defaults to ssh; `--rcargo-transport=webtransport`
    /// flips it. WT is only buildable with `--features webtransport`.
    transport_choice: TransportChoice,
    /// WT-specific overrides. None = use cfg.host / default port.
    wt_host: Option<String>,
    wt_port: Option<u16>,
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<ParsedArgs> {
    let mut out = ParsedArgs::default();
    let mut it = argv.into_iter();
    // Skip argv[0].
    let _bin = it.next();
    let mut passthrough = false;
    while let Some(arg) = it.next() {
        if passthrough {
            out.cargo_args.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => {
                passthrough = true;
                out.cargo_args.push(arg);
            }
            "--rcargo-host" => {
                let v = it.next().context("--rcargo-host requires a value")?;
                out.cli_overrides.host = Some(v);
            }
            "--rcargo-remote-root" => {
                let v = it.next().context("--rcargo-remote-root requires a value")?;
                out.cli_overrides.remote_root = Some(v);
            }
            "--rcargo-pull-artifacts" => {
                out.pull_artifacts_flag = true;
                out.cli_overrides.pull_artifacts = Some(true);
            }
            "--rcargo-transport" => {
                let v = it.next().context("--rcargo-transport requires a value")?;
                out.transport_choice = match v.as_str() {
                    "ssh" => TransportChoice::Ssh,
                    "webtransport" | "wt" => TransportChoice::WebTransport,
                    other => anyhow::bail!(
                        "unknown transport `{other}` (expected `ssh` or `webtransport`)"
                    ),
                };
            }
            "--rcargo-wt-host" => {
                let v = it.next().context("--rcargo-wt-host requires a value")?;
                out.wt_host = Some(v);
            }
            "--rcargo-wt-port" => {
                let v = it.next().context("--rcargo-wt-port requires a value")?;
                let port: u16 = v
                    .parse()
                    .with_context(|| format!("--rcargo-wt-port `{v}` is not a u16"))?;
                out.wt_port = Some(port);
            }
            "--rcargo-local" => {
                out.local = true;
            }
            "--rcargo-explain-config" => {
                out.explain_config = true;
            }
            "--rcargo-version" | "-V" if !passthrough => {
                out.print_version = true;
            }
            "--rcargo-help" => {
                out.print_help = true;
            }
            // Anything starting with --rcargo- but unknown: error so typos
            // don't silently get forwarded as cargo flags.
            other if other.starts_with("--rcargo-") => {
                anyhow::bail!("unknown rcargo flag: {other}");
            }
            _ => {
                out.cargo_args.push(arg);
                // Heuristic: as soon as we see a non-rcargo arg, the rest is
                // for cargo. This stops us from accidentally consuming
                // `--rcargo-host` if it sneaks past a positional arg later
                // (it shouldn't, but be safe).
                passthrough = true;
            }
        }
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> ExitCode {
    // tracing is quiet by default; users opt in via RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run().await {
        Ok(code) => {
            // Map non-zero cleanly — ExitCode::from takes u8.
            let clamped: u8 = code.try_into().unwrap_or(1);
            ExitCode::from(clamped)
        }
        Err(e) => {
            eprintln!("rcargo: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<i32> {
    let parsed = parse_args(std::env::args())?;

    if parsed.print_help {
        println!("{HELP}");
        return Ok(0);
    }
    if parsed.print_version {
        println!("rcargo {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }

    if parsed.local {
        return run_local(&parsed.cargo_args).await;
    }

    let (mut cfg, sources) =
        Config::load().context("loading rcargo config (env + ./rcargo.toml + user file)")?;
    cfg.apply_overrides(parsed.cli_overrides.clone());
    sanity_check(&cfg)?;

    if parsed.explain_config {
        println!("rcargo effective config:");
        println!("  host          = {}", cfg.host);
        println!("  remote_root   = {}", cfg.remote_root);
        println!("  ssh_args      = {:?}", cfg.ssh_args);
        println!("  rsync_args    = {:?}", cfg.rsync_args);
        println!("  pull_artifacts= {}", cfg.pull_artifacts);
        println!("sources:");
        if let Some(p) = &sources.user_file {
            println!("  user file     = {}", p.display());
        }
        if let Some(p) = &sources.project_file {
            println!("  project file  = {}", p.display());
        }
        if !sources.env_vars_seen.is_empty() {
            println!("  env vars      = {:?}", sources.env_vars_seen);
        }
        return Ok(0);
    }

    let pwd: PathBuf = std::env::current_dir().context("getting current dir")?;
    let key = project_key_for(&pwd);

    let transport = build_transport(&parsed, &cfg)?;

    eprintln!(
        "rcargo: host={} project={} (cd to ./{}/ on remote) transport={:?}",
        cfg.host, key, key, parsed.transport_choice
    );
    eprintln!("rcargo: syncing source tree...");
    transport.sync_up(&pwd, &key).await?;

    eprintln!("rcargo: running cargo on remote...");
    let outcome = transport.run_cargo(&key, &parsed.cargo_args).await?;

    if cfg.pull_artifacts && outcome.exit_code == 0 {
        eprintln!("rcargo: pulling build artifacts back...");
        transport.pull_artifacts(&pwd, &key).await?;
    }

    Ok(outcome.exit_code)
}

/// Compose the [`Transport`] enum based on the user's `--rcargo-transport`
/// choice. WebTransport is only available when the binary was built with
/// `--features webtransport`; otherwise we error out at parse-time with a
/// clear message rather than silently falling back to ssh.
fn build_transport(parsed: &ParsedArgs, cfg: &Config) -> Result<Transport> {
    match parsed.transport_choice {
        TransportChoice::Ssh => Ok(Transport::Ssh(SshTransport::new(cfg.clone()))),
        TransportChoice::WebTransport => {
            #[cfg(feature = "webtransport")]
            {
                let wt_host = parsed.wt_host.clone().unwrap_or_else(|| cfg.host.clone());
                let wt_port = parsed.wt_port.unwrap_or(7475);
                Ok(Transport::WebTransport(
                    rcargo_client::WebTransportTransport::new(cfg.clone(), wt_host, wt_port),
                ))
            }
            #[cfg(not(feature = "webtransport"))]
            {
                let _ = parsed;
                let _ = cfg;
                anyhow::bail!(
                    "this rcargo build does not include the webtransport feature; \
                     rebuild with `cargo build --features webtransport -p rcargo-cli`"
                )
            }
        }
    }
}

/// `--local` escape hatch: exec system cargo with the captured args.
async fn run_local(cargo_args: &[String]) -> Result<i32> {
    let cargo = which::which("cargo").context("could not find local cargo on PATH")?;
    let mut cmd = tokio::process::Command::new(cargo);
    // Strip the bare `--` separator if it's the first cargo_arg — it was only
    // there to mark end-of-rcargo-flags, cargo doesn't need to see it.
    let mut args = cargo_args.to_vec();
    if args.first().map(|s| s.as_str()) == Some("--") {
        args.remove(0);
    }
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());
    let status = cmd.status().await.context("spawn local cargo")?;
    Ok(status.code().unwrap_or(255))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> ParsedArgs {
        parse_args(std::iter::once("rcargo".to_string()).chain(args.iter().map(|s| s.to_string())))
            .unwrap()
    }

    #[test]
    fn parses_rcargo_host() {
        let r = p(&["--rcargo-host", "buildbox", "test"]);
        assert_eq!(r.cli_overrides.host.as_deref(), Some("buildbox"));
        assert_eq!(r.cargo_args, vec!["test"]);
    }

    #[test]
    fn pull_artifacts_flag() {
        let r = p(&["--rcargo-pull-artifacts", "build", "--release"]);
        assert!(r.pull_artifacts_flag);
        assert_eq!(r.cargo_args, vec!["build", "--release"]);
    }

    #[test]
    fn double_dash_passthrough() {
        let r = p(&["test", "--", "--rcargo-host"]);
        // `--rcargo-host` after a positional cargo arg or after `--` is just
        // a regular cargo arg (forwarded), NOT consumed.
        assert!(r.cargo_args.contains(&"--rcargo-host".to_string()));
    }

    #[test]
    fn local_flag() {
        let r = p(&["--rcargo-local", "check"]);
        assert!(r.local);
        assert_eq!(r.cargo_args, vec!["check"]);
    }

    #[test]
    fn unknown_rcargo_flag_errors() {
        let r = parse_args(["rcargo", "--rcargo-bogus"].iter().map(|s| s.to_string()));
        assert!(r.is_err());
    }

    #[test]
    fn cargo_flags_are_not_consumed() {
        // --release is cargo's, not ours.
        let r = p(&["build", "--release"]);
        assert!(r.cli_overrides.host.is_none());
        assert_eq!(r.cargo_args, vec!["build", "--release"]);
    }
}
