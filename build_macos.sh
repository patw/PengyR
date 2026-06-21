#!/bin/bash
# Build PengyR for macOS
# Prerequisites:
#   brew install qt@6 cmake rust
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${1:-$(uname -m)}"  # arm64 or x86_64

# Ensure Qt6 from Homebrew is found
export CMAKE_PREFIX_PATH="$(brew --prefix qt@6 2>/dev/null || echo '/opt/homebrew/opt/qt@6')"
export PATH="$CMAKE_PREFIX_PATH/bin:$PATH"

echo "==> Building Rust core for $ARCH-apple-darwin..."
cd "$ROOT"
cargo build --release --target "$ARCH-apple-darwin"

echo "==> Building Qt6 GUI..."
mkdir -p gui/build_macos
cd gui/build_macos

# Override the Rust library path
cmake .. \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_ARCHITECTURES="$ARCH" \
    -DRUST_TARGET_DIR="$ROOT/target/$ARCH-apple-darwin/release"

make -j$(sysctl -n hw.ncpu 2>/dev/null || echo 4)

echo ""
echo "==> Done! Binary: gui/build_macos/pengy"

# Create .app bundle
echo "==> Creating Pengy.app bundle..."
APP_DIR="$ROOT/Pengy.app"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp gui/build_macos/pengy "$APP_DIR/Contents/MacOS/"
# Use macdeployqt to bundle Qt frameworks
macdeployqt "$APP_DIR" -verbose=2

echo "==> App bundle: $APP_DIR"
echo "==> Distribute by zipping: zip -r Pengy-macOS-$ARCH.zip Pengy.app"
