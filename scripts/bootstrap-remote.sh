#!/usr/bin/env bash
# One-shot remote bootstrap: copy rcargod to a remote host and (optionally)
# install a systemd user unit so it runs in TCP-listen mode on demand.
#
# v1 of rcargo does NOT need rcargod — the client ssh's in and runs cargo
# directly. This script is for v1.5/v2 setups where you want a long-running
# daemon for lower latency.
#
# Builds with `--features webtransport` by default so the resulting unit
# also accepts WT connections on 0.0.0.0:7475 (alongside the TCP socket
# on 127.0.0.1:7474). Pass `--no-wt` to skip the WT feature and ship a
# TCP-only daemon — useful when the build host doesn't have the local
# tlsfetch checkout (see README "Path-dep caveat").
#
# Also runs `loginctl enable-linger $USER` on the remote so the user
# unit survives logout and starts on boot.
#
# Usage:
#   ./scripts/bootstrap-remote.sh user@host [--systemd-user] [--no-wt]
set -euo pipefail

REMOTE="${1:?usage: bootstrap-remote.sh user@host [--systemd-user] [--no-wt]}"
shift || true
SYSTEMD=""
WANT_WT=1
for arg in "$@"; do
  case "$arg" in
    --systemd-user) SYSTEMD="--systemd-user" ;;
    --no-wt)        WANT_WT=0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$WANT_WT" == "1" ]]; then
  echo "Building rcargod for release (with webtransport)..."
  (cd "$REPO_DIR" && cargo build --release --features webtransport -p rcargod)
  WT_LISTEN_FLAG="--wt-listen 0.0.0.0:7475"
else
  echo "Building rcargod for release (TCP only)..."
  (cd "$REPO_DIR" && cargo build --release -p rcargod)
  WT_LISTEN_FLAG=""
fi

echo "Copying rcargod to $REMOTE:~/.local/bin/rcargod..."
ssh "$REMOTE" "mkdir -p ~/.local/bin ~/rcargo-builds"
# Stop a running unit before scp — busy text files (ETXTBSY) otherwise fail.
ssh "$REMOTE" "systemctl --user stop rcargod.service 2>/dev/null || true"
scp "$REPO_DIR/target/release/rcargod" "$REMOTE":~/.local/bin/rcargod
ssh "$REMOTE" "chmod +x ~/.local/bin/rcargod && ~/.local/bin/rcargod --help | head -3"

if [[ "$SYSTEMD" == "--systemd-user" ]]; then
  echo "Installing systemd --user unit..."
  # Prepend $HOME/.cargo/bin to PATH so `cargo` is reachable when the
  # daemon spawns child processes — systemd-user defaults to a minimal
  # PATH that excludes ~/.cargo/bin even when the user shell would add it.
  ssh "$REMOTE" "mkdir -p ~/.config/systemd/user && cat > ~/.config/systemd/user/rcargod.service <<EOF
[Unit]
Description=rcargo build daemon
After=network.target

[Service]
Environment=\"PATH=%h/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"
ExecStart=%h/.local/bin/rcargod --listen 127.0.0.1:7474 ${WT_LISTEN_FLAG} --remote-root %h/rcargo-builds
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now rcargod.service
systemctl --user status rcargod.service --no-pager | head -10"

  echo "Enabling user lingering (survives logout / reboots)..."
  ssh "$REMOTE" "loginctl enable-linger \$USER && loginctl show-user \$USER --property=Linger"
fi

echo
echo "Done. Remote root is ~/rcargo-builds (created if missing)."
