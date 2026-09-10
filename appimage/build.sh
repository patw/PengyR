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
#    to XWayland and looks blurry on HiDPI).
#
#    IMPORTANT: a Wayland-only compositor (niri, sway, Hyprland, ...) has no
#    X server, so an AppImage that ships *only* libqxcb.so cannot start at all
#    -- Qt picks the "wayland" plugin (because WAYLAND_DISPLAY is set), can't
#    find it, and aborts. We therefore FAIL the build if the wayland plugin
#    isn't available, rather than silently shipping an unbootable AppImage.
echo "==> Bundling Wayland plugin..."
# The wayland platform plugin file name differs by Qt build: most packagers
# ship a single `libqwayland.so`, but Debian/Ubuntu Qt 6.4 (e.g. noble) split
# it into `libqwayland-egl.so` / `libqwayland-generic.so`. mglob handles both.
QT6_PLUGINS="/usr/lib/x86_64-linux-gnu/qt6/plugins"
mapfile -t WAYLAND_PLUGINS < <(find "$QT6_PLUGINS/platforms" -maxdepth 1 -name 'libqwayland*.so' 2>/dev/null)
if [ "${#WAYLAND_PLUGINS[@]}" -gt 0 ]; then
    cp "${WAYLAND_PLUGINS[@]}" "$APPDIR/usr/plugins/platforms/"
    echo "    bundled: $(basename -a "${WAYLAND_PLUGINS[@]}" | tr '\n' ' ')"
    # runtime libs linuxdeploy may miss; glob-matched to whichever exist
    for pattern in libQt6WaylandClient.so.6* libwayland-client.so.0* \
                   libwayland-cursor.so.0* libxkbcommon.so.0* \
                   libQt6WlShellIntegration.so.6*; do
        while IFS= read -r f; do
            cp "$f" "$APPDIR/usr/lib/"
        done < <(find /usr/lib/x86_64-linux-gnu -maxdepth 1 -name "$pattern" 2>/dev/null)
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
    echo "ERROR: no Qt6 wayland platform plugin found under $QT6_PLUGINS/platforms (libqwayland*.so)." >&2
    echo "       The AppImage would ship xcb-only and fail to start on Wayland-only" >&2
    echo "       compositors (niri/sway/Hyprland). Install the 'qt6-wayland' + " >&2
    echo "       'libqt6waylandclient6' packages (on Debian/Ubuntu), then rebuild." >&2
    exit 1
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

# 6. Verify the final AppImage bundles the Wayland platform plugin. linuxdeploy
#    keeps $APPDIR around after --output appimage, so checking it here both
#    catches a strippage and reminds us the published artifact must ship wayland.
echo "==> Verifying Wayland plugin made it into the AppImage..."
if ! ls "$APPDIR"/usr/plugins/platforms/libqwayland*.so >/dev/null 2>&1; then
    echo "ERROR: no 'libqwayland*.so' in $APPDIR/usr/plugins/platforms." >&2
    echo "       The AppImage would fail to start on Wayland-only compositors." >&2
    exit 1
fi
if ! ls "$APPDIR"/usr/lib/libQt6WaylandClient.so.6* >/dev/null 2>&1; then
    echo "ERROR: 'libQt6WaylandClient.so.6' is missing from $APPDIR/usr/lib." >&2
    echo "       The Wayland plugin would load but fail at runtime." >&2
    exit 1
fi

echo ""
echo "==> Done!"
ls -lh "$PROJECT_ROOT/PengyR-x86_64.AppImage" 2>/dev/null || echo "AppImage not found, check output above"
