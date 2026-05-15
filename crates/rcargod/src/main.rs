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
    #[arg(long, conflicts_with = "listen")]
    stdio: bool,

    /// Listen on a TCP socket and serve one client per connection.
    #[arg(long, value_name = "ADDR")]
    listen: Option<String>,

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

    if let Some(addr) = args.listen {
        let listener = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        tracing::info!("rcargod listening on {addr}");
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

    eprintln!("rcargod: pass --stdio or --listen <addr>. See --help.");
    std::process::exit(2);
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
