# rcargo

A drop-in `cargo` replacement that transparently offloads Rust builds and
tests to a remote host. The remote tree is preserved across runs so
`target/` caches between builds. stdout/stderr stream back live as the
build happens.

```text
local machine                              remote build host
┌─────────────────┐         rsync          ┌──────────────────────────┐
│ rcargo build    │ ─── source only ────►  │ ~/rcargo-builds/<key>/   │
│                 │                        │   ├─ src/                │
│                 │           ssh -tt      │   ├─ Cargo.toml          │
│                 │ ◄── live stdout/err ── │   └─ target/  (cached)   │
│                 │                        │     ↑ persisted between  │
│ exit $?         │ ◄── exit code  ─────── │       runs               │
└─────────────────┘                        └──────────────────────────┘
```

## Quick start

```bash
# install (puts the rcargo binary at ~/.local/bin/rcargo)
cargo install --path crates/rcargo-cli

# point at your remote
export RCARGO_HOST=meta                    # or any ~/.ssh/config alias
export RCARGO_REMOTE_ROOT=~/rcargo-builds  # default

# use it exactly like cargo
cd ~/myproject
rcargo build --release
rcargo test -p mycrate -- --nocapture
rcargo --rcargo-pull-artifacts build --release  # also rsync target/release back
```

The remote host needs:

- `cargo` + `rustc` on `PATH`
- `rsync` installed
- An ssh login (key-based recommended; agent forwarding works)

That's it. No daemon, no agent, no extra config files on the remote.

## Why

- **Your laptop stays cool.** Compile on a 64-core build box, edit on a
  fanless ultrabook.
- **No syncthing dance.** Source goes one way (local → remote), artifacts
  come back on demand. No two-way mirror to fight with.
- **Incremental cache is the remote's `target/`.** Same project always
  maps to the same remote dir (deterministic project key from canonical
  path + git origin), so the cache stays warm.

## Configuration

Precedence: **CLI flag > env var > `./rcargo.toml` > `~/.config/rcargo/config.toml` > defaults.**

Run `rcargo --rcargo-explain-config` to see what the tool actually sees.

| Setting | Env var | CLI flag | Default |
|---|---|---|---|
| host | `RCARGO_HOST` | `--rcargo-host` | `meta` |
| remote root | `RCARGO_REMOTE_ROOT` | `--rcargo-remote-root` | `~/rcargo-builds` |
| ssh args | `RCARGO_SSH_ARGS` | (config file) | none |
| rsync args | `RCARGO_RSYNC_ARGS` | (config file) | none |
| pull artifacts | — | `--rcargo-pull-artifacts` | off |
| local mode | — | `--rcargo-local` | off |
| project key | `RCARGO_PROJECT_KEY` | — | derived from pwd + git origin |

Example `rcargo.toml` in a project:

```toml
host = "buildbox"
remote_root = "/srv/rcargo"
ssh_args = "-o StrictHostKeyChecking=accept-new"
pull_artifacts = true
```

## CLI flag convention

rcargo's own flags are prefixed `--rcargo-*` so they never collide with
cargo's flags. Everything else is forwarded verbatim to remote `cargo`:

```bash
rcargo --rcargo-host buildbox build --release --target wasm32-unknown-unknown
# →
# ssh buildbox -- cd ~/rcargo-builds/<key> && cargo build --release --target wasm32-unknown-unknown
```

## Escape hatch

`rcargo --rcargo-local <args...>` bypasses everything and execs the local
`cargo`. Useful when the remote is unreachable or when you specifically
want a local build for some reason.

## Installation as a shim

If you want `cargo` itself to be rcargo (so existing scripts pick it up):

```bash
./scripts/install.sh
```

This creates `~/.local/bin/cargo` as a symlink to the rcargo binary. Make
sure `~/.local/bin` is earlier in `$PATH` than your real cargo. To get the
real cargo back, run `rcargo --rcargo-local ...` or delete the symlink.

## What gets uploaded (and what doesn't)

`rcargo` uses rsync with these excludes:

- `target/` — never uploaded (that's the whole point — remote has its own)
- `node_modules/`
- `*.swp`, `.DS_Store`
- `.env`, `.env.local`

Everything else, including `.git/`, is sent. This is necessary because
cargo may fetch git dependencies from local refs. If your project is
gigantic and you want to trim more, set `RCARGO_RSYNC_ARGS` and add
`--exclude=...` patterns.

## Architecture

`rcargo` is split into four crates:

- **`rcargo-cli`** — the user-facing binary. Tiny: it parses its own flags
  and forwards everything else.
- **`rcargo-client`** — the library that does the actual work. Has a
  `Transport` enum with one implementation today (ssh+rsync).
- **`rcargod`** — daemon binary, scaffolded for v2 but not required for
  v1. Speaks a line-delimited JSON protocol on stdio or a TCP socket.
- **`rcargo-proto`** — shared serde types for the daemon protocol.

### v2 — WebTransport transport (experimental)

`rcargo-cli` and `rcargod` both grow a `webtransport` feature that
speaks the same JSON-line protocol as the TCP path, but over a
WebTransport (QUIC + h3) bidirectional stream via the vendored
`tlsfetch-wt` stack.

Build the client with WT enabled:

```bash
cargo build --features webtransport -p rcargo-cli
```

Build and run the daemon with WT enabled (and keep the TCP listener for
backwards compat):

```bash
cargo build --release --features webtransport -p rcargod
~/.local/bin/rcargod --listen 127.0.0.1:7474 --wt-listen 0.0.0.0:7475
```

The daemon generates a self-signed certificate at startup and logs its
SHA-256 to stderr (`rcargod WT self-signed leaf cert sha256=...`). The
v2 client trusts any cert (`insecure: true`) — adequate for a private
LAN dev box, NOT for the public internet. SPKI pinning via
`tlsfetch-pin` is a follow-up.

Use from the CLI:

```bash
rcargo --rcargo-transport webtransport \
       --rcargo-wt-host 192.168.10.140 \
       --rcargo-wt-port 7475 \
       check -p mycrate
```

The WT transport currently only handles the build-stream phase. Source
upload and artifact pull-back still go over ssh+rsync via the existing
`Config::host` — that's by design (rsync's delta transfer is more
valuable than fronting it on WT for v2).

**Path-dep caveat** — `rcargo-client` and `rcargod` use path deps to
the local `/home/ubuntu/tlsfetch` checkout under their `webtransport`
features. Cargo validates path deps at manifest-load time even for
optional/feature-gated entries, so any host that runs `cargo` against
the rcargo source tree must have `/home/ubuntu/tlsfetch` present (or
the manifest patched out). This breaks the meta-recursive "rcargo
builds rcargo through rcargo" test on hosts without tlsfetch. Two
options for publishing to crates.io: (a) replace the path deps with a
pinned git rev pointing at a public tlsfetch fork, or (b) hoist the
WT-impl into a separate crate that lives outside this workspace.

## Testing

```bash
cargo test --workspace
```

The integration tests use `--rcargo-local` so they pass without a remote.
To actually test the remote path:

```bash
export RCARGO_HOST=some-reachable-host
cd /some/small/cargo/project
~/rcargo/target/debug/rcargo build
```

## License

MIT — see [LICENSE](./LICENSE).
