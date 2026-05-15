//! Transport layer. v1 has one impl: [`ssh::SshTransport`]. v2 will add a
//! WebTransport impl behind the `webtransport` feature, talking to a
//! long-running `rcargod`. The CLI picks one at startup and calls
//! [`Transport`] methods directly — we use an enum dispatch rather than a
//! trait object to keep the dep graph small.

use anyhow::Result;
use std::path::Path;

pub mod ssh;

/// Outcome of a remote build.
#[derive(Debug, Clone, Copy)]
pub struct TransportOutcome {
    /// Exit code of the remote `cargo` invocation.
    pub exit_code: i32,
}

/// The choice of transport. Today only `Ssh`; tomorrow `WebTransport`.
pub enum Transport {
    Ssh(ssh::SshTransport),
}

impl Transport {
    pub async fn sync_up(&self, local_root: &Path, project_key: &str) -> Result<()> {
        match self {
            Transport::Ssh(t) => t.sync_up(local_root, project_key).await,
        }
    }

    pub async fn run_cargo(&self, project_key: &str, args: &[String]) -> Result<TransportOutcome> {
        match self {
            Transport::Ssh(t) => t.run_cargo(project_key, args).await,
        }
    }

    pub async fn pull_artifacts(&self, local_root: &Path, project_key: &str) -> Result<()> {
        match self {
            Transport::Ssh(t) => t.pull_artifacts(local_root, project_key).await,
        }
    }
}
