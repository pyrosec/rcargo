//! Layered configuration: CLI flag > env var > ./rcargo.toml >
//! ~/.config/rcargo/config.toml > defaults.
//!
//! The CLI fills [`Config`] by calling [`Config::load`] (which handles env +
//! files) and then overwriting any field the user set on the command line.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Effective rcargo configuration after all layers have been merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// SSH destination (`user@host` or alias from `~/.ssh/config`).
    pub host: String,
    /// Parent directory on the remote host that holds per-project trees.
    pub remote_root: String,
    /// Extra args forwarded to `ssh` (split on whitespace).
    pub ssh_args: Vec<String>,
    /// Extra args forwarded to `rsync` (split on whitespace).
    pub rsync_args: Vec<String>,
    /// If true, copy `target/release/*` and `target/*/release/*` artifacts
    /// back to the local `target/` after the build succeeds.
    pub pull_artifacts: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "meta".to_string(),
            remote_root: "~/rcargo-builds".to_string(),
            ssh_args: Vec::new(),
            rsync_args: Vec::new(),
            pull_artifacts: false,
        }
    }
}

/// A partial config, used for each layer before merging. Every field is
/// `Option<_>` so "unset" is distinguishable from "set to the default".
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigFile {
    pub host: Option<String>,
    pub remote_root: Option<String>,
    pub ssh_args: Option<String>,
    pub rsync_args: Option<String>,
    pub pull_artifacts: Option<bool>,
}

/// Where each layer's data came from — useful for `rcargo --explain-config`
/// debugging and for unit tests of precedence.
#[derive(Debug, Default, Clone)]
pub struct ConfigSources {
    pub user_file: Option<PathBuf>,
    pub project_file: Option<PathBuf>,
    pub env_vars_seen: Vec<&'static str>,
}

impl Config {
    /// Load config from disk + environment. Does NOT consult CLI flags — the
    /// CLI is expected to override fields after this returns.
    pub fn load() -> Result<(Self, ConfigSources)> {
        let mut layers: Vec<ConfigFile> = Vec::new();
        let mut sources = ConfigSources::default();

        if let Some(user_path) = user_config_path() {
            if user_path.exists() {
                let raw = std::fs::read_to_string(&user_path)
                    .with_context(|| format!("read {}", user_path.display()))?;
                let parsed: ConfigFile = toml::from_str(&raw)
                    .with_context(|| format!("parse {}", user_path.display()))?;
                layers.push(parsed);
                sources.user_file = Some(user_path);
            }
        }

        let project_path = PathBuf::from("rcargo.toml");
        if project_path.exists() {
            let raw = std::fs::read_to_string(&project_path)
                .with_context(|| format!("read {}", project_path.display()))?;
            let parsed: ConfigFile = toml::from_str(&raw)
                .with_context(|| format!("parse {}", project_path.display()))?;
            layers.push(parsed);
            sources.project_file = Some(project_path);
        }

        let env_layer = env_layer(&mut sources);
        layers.push(env_layer);

        let merged = merge_layers(&layers);
        Ok((apply_defaults(merged), sources))
    }

    /// Apply a CLI override: any `Some(_)` field on `overrides` wins.
    pub fn apply_overrides(&mut self, overrides: ConfigFile) {
        if let Some(host) = overrides.host {
            self.host = host;
        }
        if let Some(root) = overrides.remote_root {
            self.remote_root = root;
        }
        if let Some(s) = overrides.ssh_args {
            self.ssh_args = split_args(&s);
        }
        if let Some(s) = overrides.rsync_args {
            self.rsync_args = split_args(&s);
        }
        if let Some(b) = overrides.pull_artifacts {
            self.pull_artifacts = b;
        }
    }
}

fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("rcargo").join("config.toml"))
}

fn env_layer(sources: &mut ConfigSources) -> ConfigFile {
    let mut layer = ConfigFile::default();
    if let Ok(v) = std::env::var("RCARGO_HOST") {
        layer.host = Some(v);
        sources.env_vars_seen.push("RCARGO_HOST");
    }
    if let Ok(v) = std::env::var("RCARGO_REMOTE_ROOT") {
        layer.remote_root = Some(v);
        sources.env_vars_seen.push("RCARGO_REMOTE_ROOT");
    }
    if let Ok(v) = std::env::var("RCARGO_SSH_ARGS") {
        layer.ssh_args = Some(v);
        sources.env_vars_seen.push("RCARGO_SSH_ARGS");
    }
    if let Ok(v) = std::env::var("RCARGO_RSYNC_ARGS") {
        layer.rsync_args = Some(v);
        sources.env_vars_seen.push("RCARGO_RSYNC_ARGS");
    }
    layer
}

fn merge_layers(layers: &[ConfigFile]) -> ConfigFile {
    let mut out = ConfigFile::default();
    for layer in layers {
        if layer.host.is_some() {
            out.host = layer.host.clone();
        }
        if layer.remote_root.is_some() {
            out.remote_root = layer.remote_root.clone();
        }
        if layer.ssh_args.is_some() {
            out.ssh_args = layer.ssh_args.clone();
        }
        if layer.rsync_args.is_some() {
            out.rsync_args = layer.rsync_args.clone();
        }
        if layer.pull_artifacts.is_some() {
            out.pull_artifacts = layer.pull_artifacts;
        }
    }
    out
}

fn apply_defaults(file: ConfigFile) -> Config {
    let defaults = Config::default();
    Config {
        host: file.host.unwrap_or(defaults.host),
        remote_root: file.remote_root.unwrap_or(defaults.remote_root),
        ssh_args: file
            .ssh_args
            .map(|s| split_args(&s))
            .unwrap_or(defaults.ssh_args),
        rsync_args: file
            .rsync_args
            .map(|s| split_args(&s))
            .unwrap_or(defaults.rsync_args),
        pull_artifacts: file.pull_artifacts.unwrap_or(defaults.pull_artifacts),
    }
}

fn split_args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// Test hook: load only the env layer over a given base. Used by precedence
/// tests so they don't touch real disk paths.
#[doc(hidden)]
pub fn merge_for_test(layers: &[ConfigFile]) -> Config {
    apply_defaults(merge_layers(layers))
}

/// Test hook: returns the path that `load` would consult for the user file.
#[doc(hidden)]
pub fn user_config_path_for_test() -> Option<PathBuf> {
    user_config_path()
}

/// Sanity guard the CLI uses to refuse common foot-guns (e.g. remote_root that
/// resolves to `/`). Not exhaustive — defense in depth, not security.
pub fn sanity_check(cfg: &Config) -> Result<()> {
    if cfg.remote_root.trim().is_empty() || cfg.remote_root == "/" {
        anyhow::bail!("remote_root must not be empty or '/'");
    }
    if cfg.host.trim().is_empty() {
        anyhow::bail!("host must not be empty");
    }
    Ok(())
}

/// Helper for tests/CLI: best-effort absolute form of a path for display.
pub fn abs_display(p: &Path) -> String {
    p.canonicalize()
        .map(|c| c.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.host, "meta");
        assert_eq!(c.remote_root, "~/rcargo-builds");
        assert!(!c.pull_artifacts);
    }

    #[test]
    fn cli_overrides_win() {
        let mut c = Config::default();
        c.apply_overrides(ConfigFile {
            host: Some("buildbox".into()),
            pull_artifacts: Some(true),
            ..Default::default()
        });
        assert_eq!(c.host, "buildbox");
        assert!(c.pull_artifacts);
        // Untouched fields keep defaults.
        assert_eq!(c.remote_root, "~/rcargo-builds");
    }

    #[test]
    fn merge_precedence_later_wins() {
        let base = ConfigFile {
            host: Some("base".into()),
            remote_root: Some("/tmp/a".into()),
            ..Default::default()
        };
        let project = ConfigFile {
            host: Some("project".into()),
            ..Default::default()
        };
        let env = ConfigFile {
            host: Some("env".into()),
            ..Default::default()
        };
        let merged = merge_for_test(&[base, project, env]);
        assert_eq!(merged.host, "env");
        // remote_root falls through to the base layer.
        assert_eq!(merged.remote_root, "/tmp/a");
    }

    #[test]
    fn sanity_rejects_root() {
        let c = Config {
            remote_root: "/".into(),
            ..Config::default()
        };
        assert!(sanity_check(&c).is_err());
    }
}
