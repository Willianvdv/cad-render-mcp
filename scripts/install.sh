#!/usr/bin/env bash
# Install cad-render-mcp for everyday use (outside the repo) and register it with Claude Code.
#
#   scripts/install.sh            # binary -> ~/.local/bin, Cycles (if built) -> ~/.local/share/cad-render-mcp/cycles
#   PREFIX=/opt/cad scripts/install.sh
#   CYCLES_INSTALL=/path/to/cycles/install scripts/install.sh
#   NO_CLAUDE=1 scripts/install.sh   # skip the `claude mcp add`
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX="${PREFIX:-$HOME/.local}"
SHARE="$PREFIX/share/cad-render-mcp"
CYCLES_INSTALL="${CYCLES_INSTALL:-$HOME/src/cad-render-mcp-deps/cycles/install}"

command -v brew >/dev/null && { brew list opencascade >/dev/null 2>&1 || brew install opencascade; }
cargo install --path . --root "$PREFIX" --locked --force
BIN="$PREFIX/bin/cad-render-mcp"
"$BIN" --version

if [ -x "$CYCLES_INSTALL/cycles" ]; then
  mkdir -p "$SHARE"
  rsync -a --delete "$CYCLES_INSTALL/" "$SHARE/cycles/"
  echo "Cycles -> $SHARE/cycles (photo/video photo renderer enabled)"
else
  echo "Cycles not found at $CYCLES_INSTALL: photo/video(photo) disabled until it is built (see README)"
fi

if [ -z "${NO_CLAUDE:-}" ] && command -v claude >/dev/null; then
  claude mcp remove cad-render -s user >/dev/null 2>&1 || true
  claude mcp add --scope user cad-render -- "$BIN"
  claude mcp list | grep cad-render || true
  echo "Restart Claude Code (or /mcp -> reconnect) to load the tools."
fi
