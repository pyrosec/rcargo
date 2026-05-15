//! `rcargod` — long-running daemon for v2 of rcargo.
//!
//! STATUS: scaffolded; the v1 client does NOT need this. v1 simply ssh's into
//! the remote and runs cargo. This daemon exists so that when v2 (WebTransport
//! transport, persistent connection) lands there's already a working server.
//!
//! Currently supports two modes:
//!
//! - `rcargod --stdio`  — reads JSON-line [`Request`]s on stdin, writes
//!   [`Event`]s on stdout. This is the mode the ssh-stdio transport (an
//!   intermediate v1.5) would use: `ssh host -- rcargod --stdio`.
//! - `rcargod --listen <addr>` — binds a TCP socket and serves the same
//!   protocol per-connection. This is what the WebTransport transport will
//!   front (probably via a `wtransport` shim) in v2.
//!
//! The protocol is intentionally trivial — line-delimited JSON in both
//! directions — so the same daemon supports both transports without code
//! changes.

use anyhow::{Context, Result};
use clap::Parser;
use rcargo_proto::{Event, Request, PROTO_VERSION};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "rcargod", about = "rcargo remote build daemon (v2)")]
struct Args {
    /// Run in stdio mode — read JSON-line Requests on stdin, write Events on stdout.
    #[arg(long, conflicts_with_all = ["listen", "wt_listen"])]
    stdio: bool,

    /// Listen on a TCP socket and serve one client per connection.
    #[arg(long, value_name = "ADDR")]
    listen: Option<String>,

    /// Listen on a WebTransport endpoint and serve each bidi stream as one
    /// rcargo session. Requires the `webtransport` feature; uses a
    /// self-signed certificate generated at startup. The cert's SHA-256
    /// is logged to stderr on bind for out-of-band pinning. v2 dev-only:
    /// production deployments should front this with a real cert via a
    /// reverse proxy or wire SPKI pinning end-to-end.
    #[cfg(feature = "webtransport")]
    #[arg(long, value_name = "ADDR")]
    wt_listen: Option<String>,

    /// Parent directory under which project trees live. Same convention as
    /// the client's RCARGO_REMOTE_ROOT.
    #[arg(long, default_value = "~/rcargo-builds")]
    remote_root: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let remote_root = expand_tilde(&args.remote_root);

    if args.stdio {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        serve_one(stdin, stdout, remote_root).await?;
        return Ok(());
    }

    // Coalesce all listener modes into a Vec<JoinHandle<_>> so the same
    // binary can serve TCP and WT simultaneously (or either alone).
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    if let Some(addr) = args.listen.clone() {
        let root = remote_root.clone();
        let h = tokio::spawn(async move {
            if let Err(e) = serve_tcp(addr, root).await {
                tracing::error!("tcp listener died: {e:#}");
            }
        });
        handles.push(h);
    }

    #[cfg(feature = "webtransport")]
    if let Some(addr) = args.wt_listen.clone() {
        let root = remote_root.clone();
        let h = tokio::spawn(async move {
            if let Err(e) = serve_wt(addr, root).await {
                tracing::error!("wt listener died: {e:#}");
            }
        });
        handles.push(h);
    }

    if handles.is_empty() {
        eprintln!(
            "rcargod: pass --stdio, --listen <addr>, or --wt-listen <addr>. See --help."
        );
        std::process::exit(2);
    }

    // Wait for any listener to die — if one does, exit so systemd restarts.
    let (res, _, _) = futures_select(handles).await;
    if let Err(e) = res {
        tracing::error!("listener task panicked: {e}");
    }
    Ok(())
}

/// Wait for the first task in `handles` to finish and return its result.
/// (Stand-in for `futures::future::select_all` without bringing in the
/// futures crate just for this.)
async fn futures_select<T>(
    handles: Vec<tokio::task::JoinHandle<T>>,
) -> (
    Result<T, tokio::task::JoinError>,
    usize,
    Vec<tokio::task::JoinHandle<T>>,
)
where
    T: 'static,
{
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct SelectAll<T> {
        inner: Vec<tokio::task::JoinHandle<T>>,
    }
    impl<T: 'static> Future for SelectAll<T> {
        type Output = (
            Result<T, tokio::task::JoinError>,
            usize,
            Vec<tokio::task::JoinHandle<T>>,
        );
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            for (i, h) in self.inner.iter_mut().enumerate() {
                if let Poll::Ready(res) = Pin::new(h).poll(cx) {
                    let mut remaining = std::mem::take(&mut self.inner);
                    remaining.swap_remove(i);
                    return Poll::Ready((res, i, remaining));
                }
            }
            Poll::Pending
        }
    }
    SelectAll { inner: handles }.await
}

/// Plain TCP listener — the original `--listen` path, factored out of `main`.
async fn serve_tcp(addr: String, remote_root: String) -> Result<()> {
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("rcargod TCP listening on {addr}");
    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        tracing::info!("connection from {peer}");
        let root = remote_root.clone();
        tokio::spawn(async move {
            let (r, w) = sock.into_split();
            if let Err(e) = serve_one(r, w, root).await {
                tracing::warn!("session ended with error: {e:#}");
            }
        });
    }
}

/// WebTransport listener — generates a self-signed cert at startup, logs
/// its SHA-256 (so a client can pin it out of band), and spawns one
/// `serve_one` per inbound bidirectional stream.
///
/// Wire format on each bi stream is identical to TCP/stdio (line-delimited
/// JSON Request → Event stream → Exit). `wtransport`'s SendStream/RecvStream
/// impl tokio's AsyncRead/AsyncWrite directly, so `serve_one` plugs right in.
#[cfg(feature = "webtransport")]
async fn serve_wt(addr: String, remote_root: String) -> Result<()> {
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::time::Duration;

    let sock_addr: SocketAddr = SocketAddr::from_str(&addr)
        .with_context(|| format!("--wt-listen address {addr} must be an IP:port"))?;

    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1", "::1"])
        .context("generate self-signed WT identity")?;
    // Log the leaf cert's SHA-256 so a client can pin it out of band.
    // For multi-host certs (the SANs above) we still only have one leaf;
    // there's exactly one cert in the chain.
    for cert in identity.certificate_chain().as_slice() {
        let hash = cert.hash();
        let hash_hex = hex::encode(hash.as_ref());
        tracing::warn!(
            "rcargod WT self-signed leaf cert sha256={} (pin this in clients for production)",
            hash_hex
        );
    }

    let server_config = wtransport::ServerConfig::builder()
        .with_bind_address(sock_addr)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();
    let endpoint = wtransport::Endpoint::server(server_config)
        .with_context(|| format!("bind WT endpoint on {sock_addr}"))?;
    tracing::info!("rcargod WebTransport listening on {sock_addr}");

    loop {
        let incoming = endpoint.accept().await;
        let root = remote_root.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_wt_session(incoming, root).await {
                tracing::warn!("wt session ended with error: {e:#}");
            }
        });
    }
}

/// Walk wtransport's three-stage accept (Incoming → SessionRequest → Connection),
/// then loop on `accept_bi` and dispatch each bi stream to a fresh `serve_one`.
#[cfg(feature = "webtransport")]
async fn handle_wt_session(
    incoming: wtransport::endpoint::IncomingSession,
    remote_root: String,
) -> Result<()> {
    let session_req = incoming.await.context("wt incoming")?;
    tracing::info!(
        "wt session: authority={:?} path={:?}",
        session_req.authority(),
        session_req.path()
    );
    let conn = session_req.accept().await.context("wt session accept")?;
    loop {
        let stream = conn.accept_bi().await;
        let (send, recv) = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::info!("wt session closed by peer: {e}");
                return Ok(());
            }
        };
        let root = remote_root.clone();
        tokio::spawn(async move {
            // wtransport's SendStream/RecvStream impl tokio AsyncRead/Write;
            // serve_one is shape-generic over those, so we can call it
            // directly with no adapter.
            if let Err(e) = serve_one(recv, send, root).await {
                tracing::warn!("wt stream ended with error: {e:#}");
            }
        });
    }
}

/// Serve a single client until they hang up or send `Bye`. Reads `Request`s
/// from `r`, writes `Event`s to `w`.
async fn serve_one<R, W>(r: R, mut w: W, remote_root: String) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                emit(
                    &mut w,
                    &Event::Error {
                        message: format!("parse error: {e}"),
                    },
                )
                .await?;
                continue;
            }
        };
        match req {
            Request::Hello { client_version: _ } => {
                emit(
                    &mut w,
                    &Event::HelloAck {
                        server_version: PROTO_VERSION,
                    },
                )
                .await?;
            }
            Request::Bye => break,
            Request::Build {
                project_key,
                args,
                env,
            } => {
                run_cargo(&mut w, &remote_root, &project_key, &args, &env).await?;
            }
        }
    }
    Ok(())
}

async fn run_cargo<W: AsyncWrite + Unpin>(
    w: &mut W,
    remote_root: &str,
    project_key: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<()> {
    let cwd = format!("{remote_root}/{project_key}");
    if let Err(e) = std::fs::create_dir_all(&cwd) {
        emit(
            w,
            &Event::Error {
                message: format!("mkdir {cwd}: {e}"),
            },
        )
        .await?;
        emit(w, &Event::Exit { code: 1 }).await?;
        return Ok(());
    }

    let mut cmd = Command::new("cargo");
    cmd.args(args).current_dir(&cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            emit(
                w,
                &Event::Error {
                    message: format!("spawn cargo: {e}"),
                },
            )
            .await?;
            emit(w, &Event::Exit { code: 127 }).await?;
            return Ok(());
        }
    };

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Single-writer pattern: tasks send events through this mpsc to avoid
    // interleaving partial JSON lines on the wire.
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let tx_out = tx.clone();
    let tx_err = tx.clone();

    let out_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = tx_out.send(Event::Stdout { data: line });
        }
    });
    let err_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = tx_err.send(Event::Stderr { data: line });
        }
    });

    // Drive the wait + events.
    let wait_task = tokio::spawn(async move {
        let status = child.wait().await;
        // Drop sender clones so the receiver knows when no more events come.
        drop(tx);
        status
    });

    while let Some(ev) = rx.recv().await {
        emit(w, &ev).await?;
    }

    let _ = out_task.await;
    let _ = err_task.await;
    let status = wait_task.await?;
    let code = status.map(|s| s.code().unwrap_or(255)).unwrap_or(255);
    emit(w, &Event::Exit { code }).await?;
    Ok(())
}

async fn emit<W: AsyncWrite + Unpin>(w: &mut W, ev: &Event) -> Result<()> {
    let mut line = ev.to_line();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return format!("{}/{}", home.trim_end_matches('/'), rest);
        }
    }
    p.to_string()
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok()
}
