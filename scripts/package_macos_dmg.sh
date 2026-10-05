#!/bin/bash
# Package an already-built app without mounting a writable image to set a
# cosmetic Finder attribute. Direct UDZO creation works on headless runners.
set -euo pipefail
[ "$#" -eq 4 ] || { echo "Usage: $0 app icon cli-installer output.dmg" >&2; exit 2; }
APP="$1"; ICON="$2"; INSTALLER="$3"; OUTPUT="$4"
[ -d "$APP" ] && [ -f "$ICON" ] && [ -f "$INSTALLER" ] || { echo "Missing packaging input" >&2; exit 1; }
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/pengyr-dmg.XXXXXX")
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT
cp -a "$APP" "$STAGE/"
cp "$INSTALLER" "$STAGE/Install CLI Tools.command"
chmod +x "$STAGE/Install CLI Tools.command"
cp "$ICON" "$STAGE/.VolumeIcon.icns"
ln -s /Applications "$STAGE/Applications"
echo "==> Creating compressed DMG directly: $OUTPUT"
hdiutil create -volname "Pengy" -srcfolder "$STAGE" -ov -format UDZO "$OUTPUT"
hdiutil verify "$OUTPUT"
echo "==> DMG ready: $OUTPUT"
