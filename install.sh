#!/bin/bash
# Install pengy-cli and pengy-web to ~/.local/bin/
# Usage:
#   ./install.sh              # build + install
#   ./install.sh --prebuilt   # install from existing target/release/
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

if [[ "${1:-}" != "--prebuilt" ]]; then
    echo "==> Building pengy-cli and pengy-web (release)..."
    cd "$ROOT"
    cargo build --release --workspace
fi

echo "==> Installing to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"

for bin in pengy-cli pengy-web; do
    src="$ROOT/target/release/$bin"
    if [[ ! -f "$src" ]]; then
        echo "ERROR: $src not found. Build first with: cargo build --release --workspace"
        exit 1
    fi
    cp "$src" "$INSTALL_DIR/$bin"
    chmod +x "$INSTALL_DIR/$bin"
    echo "    $INSTALL_DIR/$bin"
done

# Check if INSTALL_DIR is in PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    echo ""
    echo "NOTE: $INSTALL_DIR is not in your PATH."
    echo "Add it with:  export PATH=\"$INSTALL_DIR:\$PATH\""
    echo "Or add that line to your ~/.bashrc or ~/.zshrc"
fi

echo ""
echo "==> Done! Installed:"
echo "    pengy-cli  — interactive REPL or single-shot: pengy-cli \"question\""
echo "    pengy-web  — web UI: pengy-web [port]  (default: 5000)"
