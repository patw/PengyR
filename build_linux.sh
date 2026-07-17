#!/bin/bash
# Build PengyR for Linux
# Output: PengyR-x86_64.AppImage (portable) or gui/build/pengy (native)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

echo "==> Building Rust core + CLI + Web (release)..."
cargo build --release

echo "==> Building Qt6 GUI..."
mkdir -p gui/build
cd gui/build
cmake .. -DCMAKE_BUILD_TYPE=Release
make -j$(nproc)

echo ""
echo "==> Done!"
echo "    GUI: gui/build/pengy"
echo "    CLI: target/release/pengy-cli"
echo "    Web: target/release/pengy-web [port]"
echo ""
echo "==> Run with: ./gui/build/pengy"
echo "==> To create AppImage: cd appimage && ./build.sh"
