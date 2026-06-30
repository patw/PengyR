#!/bin/bash
# Build a PengyR .deb package
# Prerequisites (Ubuntu/Debian):
#   sudo apt install build-essential cmake qt6-base-dev libgl-dev dpkg-dev
#   curl https://sh.rustup.rs -sSf | sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
VERSION="1.2.3"
ARCH="$(dpkg --print-architecture)"
PKG_NAME="pengy_${VERSION}_${ARCH}"
STAGING="$ROOT/.deb_staging/$PKG_NAME"

# 1. Build Rust workspace
echo "==> Building Rust workspace..."
cd "$ROOT"
cargo build --release --workspace

# 2. Build Qt GUI
echo "==> Building Qt GUI..."
mkdir -p "$ROOT/gui/build"
cd "$ROOT/gui/build"
cmake .. -DCMAKE_BUILD_TYPE=Release
make -j$(nproc)

# 3. Assemble staging tree
echo "==> Assembling package staging tree..."
rm -rf "$ROOT/.deb_staging"
mkdir -p "$STAGING/usr/bin"
mkdir -p "$STAGING/usr/share/applications"
mkdir -p "$STAGING/usr/share/icons/hicolor/256x256/apps"
mkdir -p "$STAGING/usr/share/doc/pengy"
mkdir -p "$STAGING/DEBIAN"

cp "$ROOT/gui/build/pengy"           "$STAGING/usr/bin/pengy"
cp "$ROOT/target/release/pengy-cli"  "$STAGING/usr/bin/pengy-cli"
cp "$ROOT/target/release/pengy-web"  "$STAGING/usr/bin/pengy-web"
chmod 755 "$STAGING/usr/bin/pengy" "$STAGING/usr/bin/pengy-cli" "$STAGING/usr/bin/pengy-web"

cp "$ROOT/pengy.png" "$STAGING/usr/share/icons/hicolor/256x256/apps/pengy.png"

# Compute installed size in KB (required by control file)
INSTALLED_KB=$(du -sk "$STAGING/usr" | cut -f1)

# 4. Write DEBIAN/control
cat > "$STAGING/DEBIAN/control" <<EOF
Package: pengy
Version: $VERSION
Architecture: $ARCH
Maintainer: Pat Wendorf <dungeons@gmail.com>
Installed-Size: $INSTALLED_KB
Depends: libqt6core6 | libqt6core6t64, libqt6widgets6 | libqt6widgets6t64, libqt6network6 | libqt6network6t64, libgl1
Section: utils
Priority: optional
Homepage: https://github.com/patw/PengyR
Description: LLM chat desktop application
 PengyR is a Qt6 desktop AI agent that connects to any OpenAI-compatible
 LLM API and gives the model tools to operate on your machine.
 .
 Also includes pengy-cli (terminal REPL) and pengy-web (browser UI).
EOF

# 5. Write .desktop file
cat > "$STAGING/usr/share/applications/pengy.desktop" <<EOF
[Desktop Entry]
Name=Pengy
Comment=LLM Chat Desktop Application
Exec=pengy
Icon=pengy
Type=Application
Categories=Utility;Development;
Terminal=false
EOF

# 6. Write copyright
cat > "$STAGING/usr/share/doc/pengy/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: pengy
Source: https://github.com/patw/PengyR

Files: *
Copyright: $(date +%Y) Pat Wendorf
License: MIT
EOF

# 7. Post-install hook to update icon cache and desktop database
cat > "$STAGING/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
fi
EOF
chmod 755 "$STAGING/DEBIAN/postinst"

# 8. Build the .deb
echo "==> Building .deb..."
dpkg-deb --build --root-owner-group "$STAGING" "$ROOT/${PKG_NAME}.deb"

rm -rf "$ROOT/.deb_staging"

echo ""
echo "==> Done!"
ls -lh "$ROOT/${PKG_NAME}.deb"
echo "==> Install with: sudo dpkg -i ${PKG_NAME}.deb"
