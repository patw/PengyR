#!/bin/bash
# Build PengyR for macOS
# Prerequisites:
#   brew install qt@6 cmake rust
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
MACOS_ARCH="${1:-$(uname -m)}"  # arm64 or x86_64 (macOS / clang naming)
# Rust uses "aarch64" where macOS uses "arm64"
[[ "$MACOS_ARCH" == "arm64" ]] && RUST_ARCH="aarch64" || RUST_ARCH="$MACOS_ARCH"

# Ensure Qt6 from Homebrew is found
export CMAKE_PREFIX_PATH="$(brew --prefix qt@6 2>/dev/null || echo '/opt/homebrew/opt/qt@6')"
export PATH="$CMAKE_PREFIX_PATH/bin:$PATH"

echo "==> Building Rust core for $RUST_ARCH-apple-darwin..."
cd "$ROOT"
cargo build --release --target "$RUST_ARCH-apple-darwin"

echo "==> Building Qt6 GUI..."
mkdir -p gui/build_macos
cd gui/build_macos

# Override the Rust library path; CMAKE_OSX_ARCHITECTURES uses macOS arch names
cmake .. \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_ARCHITECTURES="$MACOS_ARCH" \
    -DRUST_TARGET_DIR="$ROOT/target/$RUST_ARCH-apple-darwin/release"

make -j$(sysctl -n hw.ncpu 2>/dev/null || echo 4)

echo ""
echo "==> Done! Binary: gui/build_macos/pengy"

# Create .app bundle
echo "==> Creating Pengy.app bundle..."
APP_DIR="$ROOT/Pengy.app"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$ROOT/gui/build_macos/pengy" "$APP_DIR/Contents/MacOS/"
cp "$ROOT/gui/Info.plist" "$APP_DIR/Contents/"
# Use macdeployqt to bundle Qt frameworks
macdeployqt "$APP_DIR" -verbose=2

echo "==> App bundle: $APP_DIR"

# Create DMG for distribution
echo "==> Creating DMG..."
DMG_NAME="Pengy-macOS-$MACOS_ARCH.dmg"
DMG_STAGING="$ROOT/.dmg_staging"
rm -rf "$DMG_STAGING"
mkdir -p "$DMG_STAGING"
cp -r "$APP_DIR" "$DMG_STAGING/"
ln -s /Applications "$DMG_STAGING/Applications"

hdiutil create \
    -volname "Pengy" \
    -srcfolder "$DMG_STAGING" \
    -ov \
    -format UDZO \
    "$ROOT/$DMG_NAME"

rm -rf "$DMG_STAGING"
echo "==> DMG ready: $ROOT/$DMG_NAME"
