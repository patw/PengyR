#!/bin/bash
# Install Pengy command-line tools from a macOS Pengy.app bundle.
#
# With no APP_PATH, prefer a Pengy.app beside this script. That is the app
# freshly created by `./build_macos.sh`, and it is also the bundle beside the
# installer when launched from the mounted DMG. Fall back to /Applications for
# users who copied only this script elsewhere.
# Override with: APP_PATH=/path/to/Pengy.app ./install_macos_cli.sh
# Override install dir with: INSTALL_DIR=/usr/local/bin ./install_macos_cli.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ -n "${APP_PATH:-}" ]]; then
    APP_PATH="$APP_PATH"
elif [[ -d "$SCRIPT_DIR/Pengy.app" ]]; then
    APP_PATH="$SCRIPT_DIR/Pengy.app"
else
    APP_PATH="/Applications/Pengy.app"
fi
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

if [[ ! -d "$APP_PATH" ]]; then
    echo "ERROR: Could not find $APP_PATH"
    echo ""
    echo "If Pengy.app is somewhere else, run:"
    echo "  APP_PATH=/path/to/Pengy.app $0"
    echo ""
    echo "When installing after a source build, run from the checkout:"
    echo "  ./build_macos.sh && ./install_macos_cli.sh"
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
