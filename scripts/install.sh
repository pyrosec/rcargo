#!/usr/bin/env bash
# Install rcargo as a cargo shim. Builds the release binary, copies it to
# ~/.local/bin/rcargo, and optionally symlinks ~/.local/bin/cargo -> rcargo
# so existing scripts pick it up.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${HOME}/.local/bin"
mkdir -p "$BIN_DIR"

echo "Building rcargo (release)..."
(cd "$REPO_DIR" && cargo build --release -p rcargo-cli)

cp "$REPO_DIR/target/release/rcargo" "$BIN_DIR/rcargo"
chmod +x "$BIN_DIR/rcargo"
echo "Installed: $BIN_DIR/rcargo"

if [[ "${1:-}" == "--as-cargo" ]]; then
  if [[ -e "$BIN_DIR/cargo" && ! -L "$BIN_DIR/cargo" ]]; then
    echo "Refusing to overwrite non-symlink at $BIN_DIR/cargo" >&2
    exit 1
  fi
  ln -sf "$BIN_DIR/rcargo" "$BIN_DIR/cargo"
  echo "Symlinked: $BIN_DIR/cargo -> rcargo"
  echo
  echo "Make sure $BIN_DIR is earlier in \$PATH than your real cargo."
fi

echo
echo "Done. Try: rcargo --rcargo-version"
