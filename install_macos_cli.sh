#!/bin/bash
# Install Pengy command-line tools from a macOS Pengy.app bundle.
#
# Default app path: /Applications/Pengy.app
# Override with: APP_PATH=/path/to/Pengy.app ./install_macos_cli.sh
# Override install dir with: INSTALL_DIR=/usr/local/bin ./install_macos_cli.sh
set -euo pipefail

APP_PATH="${APP_PATH:-/Applications/Pengy.app}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

if [[ ! -d "$APP_PATH" ]]; then
    echo "ERROR: Could not find $APP_PATH"
    echo ""
    echo "If Pengy.app is somewhere else, run:"
    echo "  APP_PATH=/path/to/Pengy.app $0"
    exit 1
fi

for bin in pengy-cli pengy-web; do
    if [[ ! -x "$APP_PATH/Contents/MacOS/$bin" ]]; then
        echo "ERROR: $APP_PATH/Contents/MacOS/$bin not found or not executable."
        echo "This Pengy.app bundle may have been built without command-line tools."
        exit 1
    fi
done

echo "==> Installing Pengy command-line tools from:"
echo "    $APP_PATH"
echo "==> Symlink directory:"
echo "    $INSTALL_DIR"

mkdir -p "$INSTALL_DIR"

for bin in pengy-cli pengy-web; do
    ln -sf "$APP_PATH/Contents/MacOS/$bin" "$INSTALL_DIR/$bin"
    echo "    $INSTALL_DIR/$bin -> $APP_PATH/Contents/MacOS/$bin"
done

if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    echo ""
    echo "NOTE: $INSTALL_DIR is not in your PATH."
    echo "Add it to your shell startup file, usually ~/.zshrc on macOS:"
    echo ""
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

echo ""
echo "Done. Try:"
echo "  pengy-cli \"hello\""
echo "  pengy-web"
