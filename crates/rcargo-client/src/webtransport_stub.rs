//! v2 transport sketch: speak the rcargod protocol over WebTransport via the
//! tlsfetch HTTP/3 stack.
//!
//! Status: SCAFFOLDED, NOT WIRED. This file exists so that when we light up
//! v2 we have one obvious place to do the work. It compiles only when the
//! `webtransport` feature is on, and even then it does nothing yet — every
//! method is `unimplemented!()`.
//!
//! What's left to do, in rough order:
//! 1. Add `tlsfetch-wt` + `tlsfetch-transport` as path deps in this crate's
//!    Cargo.toml when the `webtransport` feature is enabled. Likely a
//!    `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` block.
//! 2. Pick a URL scheme. Suggestion: `wt+rcargo://host:7474/<project_key>`.
//!    Reuse `Config::host` as the WT authority.
//! 3. On `sync_up`: WT doesn't replace rsync — we still want delta transfer.
//!    Option A: tunnel rsync over a WT bidi stream (rsync-over-stdio mode).
//!    Option B: keep ssh+rsync for the upload, only use WT for the
//!    build-stream phase. (B is simpler for v2.)
//! 4. On `run_cargo`: open a WT bidi stream, send `Request::Build`, read
//!    `Event` lines until `Event::Exit`, mirror to stdout/stderr like the
//!    ssh path already does.
//! 5. Auth: tlsfetch handles TLS — daemon can require a static bearer in the
//!    first `Hello` frame. Defer formal cert pinning to v2.1.

use anyhow::Result;
use std::path::Path;

use crate::config::Config;
use crate::transport::TransportOutcome;

pub struct WebTransportTransport {
    #[allow(dead_code)]
    cfg: Config,
}

impl WebTransportTransport {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    pub async fn sync_up(&self, _local_root: &Path, _project_key: &str) -> Result<()> {
        unimplemented!("v2 WebTransport transport — see file header for plan")
    }

    pub async fn run_cargo(
        &self,
        _project_key: &str,
        _args: &[String],
    ) -> Result<TransportOutcome> {
        unimplemented!("v2 WebTransport transport — see file header for plan")
    }

    pub async fn pull_artifacts(&self, _local_root: &Path, _project_key: &str) -> Result<()> {
        unimplemented!("v2 WebTransport transport — see file header for plan")
    }
}
