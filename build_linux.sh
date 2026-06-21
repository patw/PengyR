#!/bin/bash
# Build PengyR for Linux
# Output: PengyR-x86_64.AppImage (portable) or gui/build/pengy (native)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

echo "==> Building Rust core (release)..."
cargo build --release

echo "==> Building Qt6 GUI..."
mkdir -p gui/build
cd gui/build
cmake .. -DCMAKE_BUILD_TYPE=Release
make -j$(nproc)

echo ""
echo "==> Done! Binary: gui/build/pengy"
echo "==> Run with: ./gui/build/pengy"
echo ""
echo "==> To create AppImage: cd appimage && ./build.sh"
