//! rcargo client: ships local source to a remote host, runs cargo there, and
//! streams output back. v1 uses `rsync` + `ssh` (must be installed on the
//! local machine). v2 will add a WebTransport transport backed by `tlsfetch`
//! and a long-running `rcargod` on the remote side.

pub mod config;
pub mod project_key;
pub mod transport;

#[cfg(feature = "webtransport")]
pub mod webtransport_stub;

#[cfg(feature = "webtransport")]
pub use webtransport_stub::WebTransportTransport;

pub use config::{Config, ConfigFile, ConfigSources};
pub use project_key::project_key_for;
pub use transport::{ssh::SshTransport, Transport, TransportOutcome};
