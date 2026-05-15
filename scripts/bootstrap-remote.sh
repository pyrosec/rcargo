#!/usr/bin/env bash
# One-shot remote bootstrap: copy rcargod to a remote host and (optionally)
# install a systemd user unit so it runs in --stdio mode on demand.
#
# v1 of rcargo does NOT need rcargod — the client ssh's in and runs cargo
# directly. This script is for v1.5/v2 setups where you want a long-running
# daemon for lower latency.
#
# Usage:
#   ./scripts/bootstrap-remote.sh user@host [--systemd-user]
set -euo pipefail

REMOTE="${1:?usage: bootstrap-remote.sh user@host [--systemd-user]}"
SYSTEMD="${2:-}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "Building rcargod for release..."
(cd "$REPO_DIR" && cargo build --release -p rcargod)

echo "Copying rcargod to $REMOTE:~/.local/bin/rcargod..."
ssh "$REMOTE" "mkdir -p ~/.local/bin ~/rcargo-builds"
scp "$REPO_DIR/target/release/rcargod" "$REMOTE":~/.local/bin/rcargod
ssh "$REMOTE" "chmod +x ~/.local/bin/rcargod && ~/.local/bin/rcargod --help | head -3"

if [[ "$SYSTEMD" == "--systemd-user" ]]; then
  echo "Installing systemd --user unit..."
  ssh "$REMOTE" 'mkdir -p ~/.config/systemd/user && cat > ~/.config/systemd/user/rcargod.service <<EOF
[Unit]
Description=rcargo build daemon
After=network.target

[Service]
ExecStart=%h/.local/bin/rcargod --listen 127.0.0.1:7474 --remote-root %h/rcargo-builds
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now rcargod.service
systemctl --user status rcargod.service --no-pager | head -10'
fi

echo
echo "Done. Remote root is ~/rcargo-builds (created if missing)."
