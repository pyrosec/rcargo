//! End-to-end smoke tests that exercise the `rcargo` binary without needing
//! a remote host. We rely on `--rcargo-local` to forward to system cargo and
//! check that flag parsing + passthrough work.

use std::process::Command;

fn rcargo_bin() -> String {
    env!("CARGO_BIN_EXE_rcargo").to_string()
}

#[test]
fn version_flag_works() {
    let out = Command::new(rcargo_bin())
        .arg("--rcargo-version")
        .output()
        .expect("spawn rcargo");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("rcargo "), "got: {stdout}");
}

#[test]
fn help_flag_works() {
    let out = Command::new(rcargo_bin())
        .arg("--rcargo-help")
        .output()
        .expect("spawn rcargo");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE:"));
    assert!(stdout.contains("--rcargo-local"));
}

#[test]
fn unknown_rcargo_flag_fails() {
    let out = Command::new(rcargo_bin())
        .arg("--rcargo-nonsense")
        .output()
        .expect("spawn rcargo");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown rcargo flag"));
}

#[test]
fn explain_config_emits_defaults_with_isolated_env() {
    // Run from a tempdir so there's no project rcargo.toml and we can scrub
    // the env so a stale user file/env in the test runner doesn't taint us.
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(rcargo_bin())
        .arg("--rcargo-explain-config")
        .current_dir(tmp.path())
        // Isolate from any RCARGO_* the host shell may have set.
        .env_remove("RCARGO_HOST")
        .env_remove("RCARGO_REMOTE_ROOT")
        .env_remove("RCARGO_SSH_ARGS")
        .env_remove("RCARGO_RSYNC_ARGS")
        // Point dirs::config_dir() at the tempdir on Linux so we don't pick
        // up the user's actual ~/.config/rcargo/config.toml during CI.
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("spawn rcargo");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("host          = meta"), "got: {stdout}");
}

#[test]
fn local_flag_forwards_to_cargo() {
    // `cargo --version` is universally present; should always succeed.
    let out = Command::new(rcargo_bin())
        .arg("--rcargo-local")
        .arg("--version")
        .output()
        .expect("spawn rcargo");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cargo"));
}
