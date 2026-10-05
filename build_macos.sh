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

# Ensure Homebrew tools, Rust, and Qt6 are found. Do not assume the caller's
# PATH contains Homebrew or rustup (for example, non-interactive shells and CI).
if ! command -v brew >/dev/null 2>&1; then
    if [[ -x /opt/homebrew/bin/brew ]]; then
        export PATH="/opt/homebrew/bin:$PATH"
    elif [[ -x /usr/local/bin/brew ]]; then
        export PATH="/usr/local/bin:$PATH"
    fi
fi

BREW_PREFIX="$(brew --prefix 2>/dev/null || true)"
QT_PREFIX="$(brew --prefix qt@6 2>/dev/null || brew --prefix qt 2>/dev/null || echo '/opt/homebrew/opt/qt@6')"
OPENSSL_PREFIX="$(brew --prefix openssl@3 2>/dev/null || brew --prefix openssl 2>/dev/null || true)"
export CMAKE_PREFIX_PATH="$QT_PREFIX${OPENSSL_PREFIX:+;$OPENSSL_PREFIX}"
export PATH="$QT_PREFIX/bin:$HOME/.cargo/bin:${BREW_PREFIX:+$BREW_PREFIX/bin:}$PATH"

if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo was not found. Install Rust with: brew install rust (or rustup)." >&2
    exit 1
fi

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
    -DRUST_TARGET_DIR="$ROOT/target/$RUST_ARCH-apple-darwin/release" \
    ${OPENSSL_PREFIX:+-DOPENSSL_ROOT_DIR="$OPENSSL_PREFIX"}

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
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$ROOT/gui/build_macos/pengy" "$APP_DIR/Contents/MacOS/"
cp "$ROOT/target/$RUST_ARCH-apple-darwin/release/pengy-cli" "$APP_DIR/Contents/MacOS/"
cp "$ROOT/target/$RUST_ARCH-apple-darwin/release/pengy-web" "$APP_DIR/Contents/MacOS/"
chmod +x "$APP_DIR/Contents/MacOS/pengy" "$APP_DIR/Contents/MacOS/pengy-cli" "$APP_DIR/Contents/MacOS/pengy-web"
cp "$ROOT/gui/Info.plist" "$APP_DIR/Contents/"
cp "$ROOT/pengy.icns" "$APP_DIR/Contents/Resources/"
# Deploy Widgets dependencies, not Qt's unrelated QML/optional plugin sweep.
# Delay signing until every copied library's load commands are final.
QT_PLUGIN_DIR="$(qmake -query QT_INSTALL_PLUGINS)"
DEPLOY_ARGS=(-always-overwrite -no-plugins -no-codesign)
for formula in brotli webp; do
    prefix="$(brew --prefix "$formula" 2>/dev/null || true)"
    [ -z "$prefix" ] || DEPLOY_ARGS+=("-libpath=$prefix/lib")
done
macdeployqt "$APP_DIR" "${DEPLOY_ARGS[@]}" -verbose=2
PLUGIN_ARGS=()
# Cocoa and SVG are required by the Widgets UI; image/TLS plugins are optional
# but copied explicitly so functionality is retained without a QML sweep.
for plugin in platforms/libqcocoa.dylib iconengines/libqsvgicon.dylib \
              imageformats/libqsvg.dylib imageformats/libqjpeg.dylib \
              imageformats/libqgif.dylib imageformats/libqico.dylib \
              imageformats/libqwebp.dylib imageformats/libqtiff.dylib \
              styles/libqmacstyle.dylib tls/libqsecuretransportbackend.dylib; do
    source="$QT_PLUGIN_DIR/$plugin"
    if [ -f "$source" ]; then
        mkdir -p "$APP_DIR/Contents/PlugIns/$(dirname "$plugin")"
        cp "$source" "$APP_DIR/Contents/PlugIns/$plugin"
        PLUGIN_ARGS+=("-executable=$APP_DIR/Contents/PlugIns/$plugin")
    elif [ "$plugin" = platforms/libqcocoa.dylib ]; then
        echo "ERROR: required Cocoa plugin missing: $source" >&2; exit 1
    fi
done
macdeployqt "$APP_DIR" "${DEPLOY_ARGS[@]}" "${PLUGIN_ARGS[@]}" -verbose=2

# Normalize library IDs and resolve residual @rpath edges before signing.
python3 "$ROOT/scripts/fix_macos_dependencies.py" "$APP_DIR"

# Sign leaf Mach-O files after deployment, then seal the complete bundle.
while IFS= read -r -d '' binary; do
    if file -b "$binary" | grep -q 'Mach-O'; then
        codesign --force --sign - "$binary"
    fi
done < <(find "$APP_DIR/Contents" -type f -print0)
codesign --force --deep --sign - "$APP_DIR"
codesign --verify --deep --strict "$APP_DIR"

# Fail packaging if a dependency still points at the developer/runner machine.
while IFS= read -r -d '' binary; do
    if file -b "$binary" | grep -q 'Mach-O'; then
        if otool -L "$binary" | tail -n +2 | grep -E '/opt/homebrew/|/usr/local/(opt|Cellar)/|/Users/runner/'; then
            echo "ERROR: nonportable dependency in $binary" >&2; exit 1
        fi
    fi
done < <(find "$APP_DIR/Contents" -type f -print0)
for binary in pengy pengy-cli pengy-web; do
    version_output=$(env -i PATH=/usr/bin:/bin HOME="$HOME" "$APP_DIR/Contents/MacOS/$binary" --version)
    [[ "$version_output" == Pengy\ v* ]] || { echo "ERROR: isolated $binary launch failed" >&2; exit 1; }
    echo "==> Isolated $binary: $version_output"
done

echo "==> App bundle: $APP_DIR"
bash "$ROOT/scripts/package_macos_dmg.sh" "$APP_DIR" "$ROOT/pengy.icns" \
    "$ROOT/install_macos_cli.sh" "$ROOT/Pengy-macOS-$MACOS_ARCH.dmg"
