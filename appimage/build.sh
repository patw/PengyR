#!/bin/bash
# Build PengyR AppImage
# Requires: linuxdeploy + linuxdeploy-plugin-qt in appimage/tools/
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
APPIMAGE_DIR="$ROOT"
TOOLS="$APPIMAGE_DIR/tools"
APPDIR="$APPIMAGE_DIR/PengyR.AppDir"
PROJECT_ROOT="$(dirname "$ROOT")"

echo "==> Cleaning AppDir..."
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
         "$APPDIR/usr/share/applications" "$APPDIR/usr/plugins/platforms" \
         "$APPDIR/usr/lib"

# 1. Build Rust core (release)
echo "==> Building Rust core..."
cd "$PROJECT_ROOT"
cargo build --release 2>&1 | tail -3

# 2. Build GUI
echo "==> Building Qt GUI..."
cd "$PROJECT_ROOT/gui"
mkdir -p build && cd build
cmake .. -DCMAKE_BUILD_TYPE=Release 2>&1 | grep "Pengy core"
make -j$(nproc) 2>&1 | tail -3

# 3. Copy binary + assets to AppDir
echo "==> Assembling AppDir..."
cp "$PROJECT_ROOT/gui/build/pengy" "$APPDIR/usr/bin/"
cp "$APPIMAGE_DIR/pengy.desktop" "$APPDIR/usr/share/applications/"
cp "$APPIMAGE_DIR/pengy.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/"
cp "$APPIMAGE_DIR/pengy.png" "$APPDIR/pengy.png"

# 4. Copy Wayland platform plugin + libs (linuxdeploy-plugin-qt only
#    bundles XCB by default; without wayland the AppImage falls back
#    to XWayland and looks blurry on HiDPI)
echo "==> Bundling Wayland plugin..."
QT6_PLUGINS="/usr/lib/x86_64-linux-gnu/qt6/plugins"
if [ -f "$QT6_PLUGINS/platforms/libqwayland.so" ]; then
    cp "$QT6_PLUGINS/platforms/libqwayland.so" "$APPDIR/usr/plugins/platforms/"
    # dependencies that linuxdeploy may miss
    for lib in libQt6WaylandClient.so.6 libwayland-client.so.0 \
               libwayland-cursor.so.0 libxkbcommon.so.0; do
        if [ -f "/usr/lib/x86_64-linux-gnu/$lib" ]; then
            cp "/usr/lib/x86_64-linux-gnu/$lib" "$APPDIR/usr/lib/"
        fi
    done
    # Copy Wayland shell-integration plugins (xdg-shell etc).
    # Without these, Qt prints "No shell integration named 'xdg-shell' found"
    # and falls back to XWayland (blurry on HiDPI).
    if [ -d "$QT6_PLUGINS/wayland-shell-integration" ]; then
        mkdir -p "$APPDIR/usr/plugins/wayland-shell-integration"
        cp -a "$QT6_PLUGINS/wayland-shell-integration/"* "$APPDIR/usr/plugins/wayland-shell-integration/"
    fi
    if [ -d "$QT6_PLUGINS/wayland-graphics-integration-client" ]; then
        mkdir -p "$APPDIR/usr/plugins/wayland-graphics-integration-client"
        cp -a "$QT6_PLUGINS/wayland-graphics-integration-client/"* "$APPDIR/usr/plugins/wayland-graphics-integration-client/"
    fi
    if [ -d "$QT6_PLUGINS/wayland-decoration-client" ]; then
        mkdir -p "$APPDIR/usr/plugins/wayland-decoration-client"
        cp -a "$QT6_PLUGINS/wayland-decoration-client/"* "$APPDIR/usr/plugins/wayland-decoration-client/"
    fi
else
    echo "WARNING: Wayland plugin not found, AppImage will use X11 only"
fi

# 5. Run linuxdeploy with Qt plugin
echo "==> Bundling with linuxdeploy..."
export QML_SOURCES_PATHS="$PROJECT_ROOT/gui"
export LDAI_OUTPUT="$PROJECT_ROOT/PengyR-x86_64.AppImage"

"$TOOLS/linuxdeploy-x86_64.AppImage" \
    --appdir "$APPDIR" \
    --plugin qt \
    --desktop-file "$APPDIR/usr/share/applications/pengy.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/256x256/apps/pengy.png" \
    --output appimage 2>&1

echo ""
echo "==> Done!"
ls -lh "$PROJECT_ROOT/PengyR-x86_64.AppImage" 2>/dev/null || echo "AppImage not found, check output above"
