//! v2 transport: speak the rcargod protocol over WebTransport via the
//! vendored `tlsfetch-wt` (quinn + h3 + rustls) stack.
//!
//! Wire format is *exactly* the same as the ssh-stdio transport: one
//! line-delimited JSON [`Request`] per outbound chunk, one [`Event`] per
//! inbound chunk, ending with [`Event::Exit`]. The only difference is the
//! framing: instead of ssh's child-stdio we open a single bidirectional
//! WebTransport stream and read/write byte halves of it.
//!
//! Why this design:
//!
//! - The JSON-line protocol already works end-to-end against the ssh
//!   transport — no point reinventing framing for WT.
//! - tlsfetch-wt's [`Connection`](tlsfetch_transport::Connection) trait
//!   gives us `open_bi() -> BiStream`, where `BiStream::into_halves()`
//!   yields a `DynSendStream` + `DynRecvStream`, each of which impls
//!   futures-io `AsyncRead`/`AsyncWrite`. We use futures-io's
//!   `BufReader::read_line` and `AsyncWriteExt::write_all` directly.
//! - Cert handling for v2 is dev-only: the daemon uses a self-signed
//!   cert on each startup and the client trusts any cert (`insecure: true`).
//!   Production needs SPKI pinning via tlsfetch-pin — the daemon already
//!   prints its cert SHA-256 to its log so a follow-up commit can wire
//!   pin verification by reading that hash out of band.
//!
//! Not yet wired:
//!
//! - `sync_up` — still delegates to `ssh+rsync` via [`SshTransport`]. WT
//!   doesn't replace rsync's delta transfer; the right design (and what
//!   gh0stdial's H3 tunnel does) is to keep rsync for the upload phase
//!   and use WT only for the live build-stream phase.
//! - `pull_artifacts` — same reason as above.
//!
//! So `WebTransportTransport` requires the same ssh config the ssh
//! transport uses, and runs sync_up + pull_artifacts through it.

use anyhow::{anyhow, Context, Result};
use rcargo_proto::{Event, Request, PROTO_VERSION};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::io::{AsyncBufReadExt, AsyncWriteExt as _, BufReader as FuturesBufReader};
use tokio::io::AsyncWriteExt as TokioAsyncWriteExt;

use tlsfetch_wt::{connect_dyn, ClientOptions, Resolver};

use crate::config::Config;
use crate::transport::{ssh::SshTransport, TransportOutcome};

/// Minimal resolver: defer to the OS resolver (`tokio::net::lookup_host`).
///
/// rcargo runs on a developer workstation that already has DNS working;
/// gh0stdial-style sandbox bypasses (DoH) are not needed here.
struct OsResolver;

impl Resolver for OsResolver {
    fn resolve(
        &self,
        host: String,
    ) -> Pin<Box<dyn std::future::Future<Output = std::io::Result<Option<SocketAddr>>> + Send>>
    {
        Box::pin(async move {
            // `tokio::net::lookup_host` requires `host:port`; the wtransport
            // contract passes `host:port` already (it parses authority from
            // the URL and appends the port), so we forward verbatim.
            let mut iter = tokio::net::lookup_host(&host).await?;
            Ok(iter.next())
        })
    }
}

/// WebTransport-backed transport for rcargod's v2 protocol.
///
/// Holds the merged [`Config`] (used for sync_up + pull_artifacts via the
/// reused ssh transport) plus the resolved WT URL.
pub struct WebTransportTransport {
    /// Cached ssh transport for sync_up / pull_artifacts. WT only handles
    /// the build-stream phase today.
    ssh: SshTransport,
    /// `https://host:port/rcargo` URL the daemon listens on.
    url: String,
}

impl WebTransportTransport {
    /// `wt_host` and `wt_port` are taken from `cfg` overrides resolved by
    /// the CLI; `path` is hardcoded to `/rcargo` since rcargod accepts any
    /// path today.
    pub fn new(cfg: Config, wt_host: String, wt_port: u16) -> Self {
        let url = format!("https://{wt_host}:{wt_port}/rcargo");
        let ssh = SshTransport::new(cfg);
        Self { ssh, url }
    }

    pub async fn sync_up(&self, local_root: &Path, project_key: &str) -> Result<()> {
        // WT doesn't carry rsync today — see file header. Delegate.
        self.ssh.sync_up(local_root, project_key).await
    }

    pub async fn pull_artifacts(&self, local_root: &Path, project_key: &str) -> Result<()> {
        self.ssh.pull_artifacts(local_root, project_key).await
    }

    /// Open a WT session, send a Build request, stream events to local
    /// stdout/stderr, return the exit code. Mirrors `SshTransport::run_cargo`
    /// in shape.
    pub async fn run_cargo(
        &self,
        project_key: &str,
        args: &[String],
    ) -> Result<TransportOutcome> {
        let opts = ClientOptions {
            url: self.url.clone(),
            resolver: Arc::new(OsResolver),
            // Dev-only. See file header — production needs SPKI pinning.
            insecure: true,
            spki_pins: None,
            idle_timeout: Some(Duration::from_secs(60 * 30)),
            keep_alive: Some(Duration::from_secs(5)),
            headers: vec![],
        };

        tracing::info!("rcargo: opening WebTransport session to {}", self.url);
        let conn = connect_dyn(opts)
            .await
            .map_err(|e| anyhow!("WT connect to {}: {e}", self.url))?;

        let bi = conn
            .open_bi()
            .await
            .map_err(|e| anyhow!("open_bi: {e:?}"))
            .context("opening WT bidirectional stream")?;
        let (mut send, recv, _stream_id) = bi.into_halves();

        // Hello first — server responds with HelloAck. Not strictly
        // required (rcargod's serve_one accepts requests in any order)
        // but keeps the wire shape identical to the ssh-stdio path.
        let hello = Request::Hello {
            client_version: PROTO_VERSION,
        };
        write_line(send.as_mut(), &hello.to_line()).await?;

        let build = Request::Build {
            project_key: project_key.to_string(),
            args: args.to_vec(),
            env: HashMap::new(),
        };
        write_line(send.as_mut(), &build.to_line()).await?;
        // Half-close the send half so the daemon sees EOF on stdin and
        // its lines() loop terminates after producing the final Exit
        // event for this build. The daemon's reading loop is bound to
        // its read-half; closing our send is the WT equivalent of
        // closing stdin on the ssh-stdio path.
        send.close()
            .await
            .map_err(|e| anyhow!("close send half: {e}"))?;

        let mut reader = FuturesBufReader::new(recv);
        let mut line = String::new();
        let mut exit_code: Option<i32> = None;
        let mut stdout = tokio::io::stdout();
        let mut stderr = tokio::io::stderr();

        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| anyhow!("WT recv: {e}"))?;
            if n == 0 {
                // Peer closed the stream cleanly.
                break;
            }
            let trimmed = line.trim_end_matches(|c| c == '\n' || c == '\r');
            if trimmed.is_empty() {
                continue;
            }
            let ev: Event = match serde_json::from_str(trimmed) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("rcargo WT: bad event line: {e} (raw: {trimmed})");
                    continue;
                }
            };
            match ev {
                Event::Stdout { data } => {
                    let _ = stdout.write_all(data.as_bytes()).await;
                    let _ = stdout.write_all(b"\n").await;
                    let _ = stdout.flush().await;
                }
                Event::Stderr { data } => {
                    let _ = stderr.write_all(data.as_bytes()).await;
                    let _ = stderr.write_all(b"\n").await;
                    let _ = stderr.flush().await;
                }
                Event::HelloAck { server_version } => {
                    if server_version != PROTO_VERSION {
                        tracing::warn!(
                            "rcargo WT: protocol mismatch: client v{PROTO_VERSION}, server v{server_version}"
                        );
                    }
                }
                Event::Exit { code } => {
                    exit_code = Some(code);
                    break;
                }
                Event::Error { message } => {
                    return Err(anyhow!("rcargod error: {message}"));
                }
            }
        }

        conn.close(0, b"client done");
        let code = exit_code
            .ok_or_else(|| anyhow!("WT stream closed before daemon sent Exit"))?;
        Ok(TransportOutcome { exit_code: code })
    }
}

/// Write a single line + `\n` to a futures-io `AsyncWrite` half.
async fn write_line(
    send: Pin<&mut (dyn futures::io::AsyncWrite + Send + Unpin)>,
    line: &str,
) -> Result<()> {
    // `Pin<&mut dyn AsyncWrite>` is itself unpinnable for the duration of
    // the call; futures-io's extension methods take `&mut self` and
    // return concrete futures, so we deref to access them.
    let send = Pin::into_inner(send);
    send.write_all(line.as_bytes())
        .await
        .map_err(|e| anyhow!("WT send: {e}"))?;
    send.write_all(b"\n")
        .await
        .map_err(|e| anyhow!("WT send: {e}"))?;
    send.flush()
        .await
        .map_err(|e| anyhow!("WT flush: {e}"))?;
    Ok(())
}
