//! Deterministic project keys.
//!
//! The same on-disk tree must always map to the same remote directory so
//! `target/` survives across runs. The key is:
//!
//! ```text
//! <basename>-<sha256(canonical_pwd + git_remote_origin)[..12]>
//! ```
//!
//! - `basename` is the directory name (lowercased, sanitized) — gives humans
//!   something readable when they `ls ~/rcargo-builds`.
//! - The hash includes the canonical absolute path so two checkouts of the
//!   same repo at different paths get different remote dirs (don't fight over
//!   one `target/`).
//! - We also fold in `git remote get-url origin` if it exists — this lets
//!   `~/work/foo` and `~/work/foo-copy` share a build dir if they're truly
//!   the same upstream and the user wants that (they can opt out by setting
//!   `RCARGO_PROJECT_KEY` explicitly).

use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

/// Build the project key for `pwd`. Honours `RCARGO_PROJECT_KEY` override.
pub fn project_key_for(pwd: &Path) -> String {
    if let Ok(override_key) = std::env::var("RCARGO_PROJECT_KEY") {
        if !override_key.trim().is_empty() {
            return sanitize_basename(&override_key);
        }
    }

    let canonical = pwd.canonicalize().unwrap_or_else(|_| pwd.to_path_buf());

    let basename = canonical
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "rcargo-unknown".to_string());

    let git_origin = git_origin_url(&canonical).unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(git_origin.as_bytes());
    let digest = hasher.finalize();
    let hash_hex: String = hex::encode(&digest[..6]); // 12 hex chars = 48 bits

    let safe_base = sanitize_basename(&basename);
    format!("{safe_base}-{hash_hex}")
}

fn sanitize_basename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    // Trim leading/trailing dashes that could confuse shells.
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "rcargo-project".to_string()
    } else {
        trimmed
    }
}

fn git_origin_url(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn same_path_same_key() {
        let tmp = TempDir::new().unwrap();
        let k1 = project_key_for(tmp.path());
        let k2 = project_key_for(tmp.path());
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_path_different_key() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        assert_ne!(project_key_for(a.path()), project_key_for(b.path()));
    }

    #[test]
    fn env_override_wins() {
        // Use a unique value so parallel tests don't collide.
        // SAFETY: env mutation in tests — single-threaded section.
        std::env::set_var("RCARGO_PROJECT_KEY", "MY-Override.123");
        let key = project_key_for(Path::new("."));
        std::env::remove_var("RCARGO_PROJECT_KEY");
        assert_eq!(key, "my-override.123");
    }

    #[test]
    fn sanitize_strips_unsafe_chars() {
        assert_eq!(sanitize_basename("foo bar/baz"), "foo-bar-baz");
        assert_eq!(sanitize_basename("---"), "rcargo-project");
        assert_eq!(sanitize_basename("Hello_World-1.2"), "hello_world-1.2");
    }
}
