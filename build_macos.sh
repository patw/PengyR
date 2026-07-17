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

echo "==> Building Rust workspace for $RUST_ARCH-apple-darwin..."
cd "$ROOT"
cargo build --release --workspace --target "$RUST_ARCH-apple-darwin"

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

# Generate .icns from pengy.png
echo "==> Generating app icon..."
ICONSET="$ROOT/.pengy.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for SIZE in 16 32 128 256 512; do
    sips -z $SIZE $SIZE "$ROOT/pengy.png" --out "$ICONSET/icon_${SIZE}x${SIZE}.png" >/dev/null
    DOUBLE=$((SIZE * 2))
    sips -z $DOUBLE $DOUBLE "$ROOT/pengy.png" --out "$ICONSET/icon_${SIZE}x${SIZE}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$ROOT/pengy.icns"
rm -rf "$ICONSET"

# Create .app bundle
echo "==> Creating Pengy.app bundle..."
APP_DIR="$ROOT/Pengy.app"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$ROOT/gui/build_macos/pengy" "$APP_DIR/Contents/MacOS/"
cp "$ROOT/target/$RUST_ARCH-apple-darwin/release/pengy-cli" "$APP_DIR/Contents/MacOS/"
cp "$ROOT/target/$RUST_ARCH-apple-darwin/release/pengy-web" "$APP_DIR/Contents/MacOS/"
chmod +x "$APP_DIR/Contents/MacOS/pengy" "$APP_DIR/Contents/MacOS/pengy-cli" "$APP_DIR/Contents/MacOS/pengy-web"
cp "$ROOT/gui/Info.plist" "$APP_DIR/Contents/"
cp "$ROOT/pengy.icns" "$APP_DIR/Contents/Resources/"
# Use macdeployqt to bundle Qt frameworks
macdeployqt "$APP_DIR" -verbose=2

# macdeployqt modifies dylib load paths which invalidates existing signatures;
# re-sign everything with an ad-hoc signature so macOS will launch the app
codesign --force --deep --sign - "$APP_DIR"

echo "==> App bundle: $APP_DIR"

# Create DMG for distribution
echo "==> Creating DMG..."
DMG_NAME="Pengy-macOS-$MACOS_ARCH.dmg"
DMG_STAGING="$ROOT/.dmg_staging"
rm -rf "$DMG_STAGING"
mkdir -p "$DMG_STAGING"
cp -r "$APP_DIR" "$DMG_STAGING/"
cp "$ROOT/install_macos_cli.sh" "$DMG_STAGING/Install CLI Tools.command"
chmod +x "$DMG_STAGING/Install CLI Tools.command"
ln -s /Applications "$DMG_STAGING/Applications"

# Add volume icon to staging area
cp "$ROOT/pengy.icns" "$DMG_STAGING/.VolumeIcon.icns"

hdiutil create \
    -volname "Pengy" \
    -srcfolder "$DMG_STAGING" \
    -ov \
    -format UDRW \
    "$ROOT/$DMG_NAME"

# Set the volume's custom icon bit so Finder uses .VolumeIcon.icns
DMG_MOUNT=$(hdiutil attach -readwrite -noverify "$ROOT/$DMG_NAME" | grep -oE '/Volumes/.+$')
SetFile -a C "$DMG_MOUNT"
hdiutil detach "$DMG_MOUNT" -quiet

# Convert to compressed read-only DMG
hdiutil convert "$ROOT/$DMG_NAME" -format UDZO -o "$ROOT/${DMG_NAME%.dmg}-compressed.dmg" -ov
mv "$ROOT/${DMG_NAME%.dmg}-compressed.dmg" "$ROOT/$DMG_NAME"

rm -rf "$DMG_STAGING"
echo "==> DMG ready: $ROOT/$DMG_NAME"
