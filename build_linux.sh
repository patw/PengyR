#!/bin/bash
# Build PengyR for Linux
# Output: PengyR-x86_64.AppImage (portable) or gui/build/pengy (native)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

echo "==> Building Rust core + CLI + Web (release)..."
# --workspace: build all members. A bare `cargo build --release` at the
# workspace root rebuilds the root package (pengy_core) but NOT the cli/web
# bins, so they can stay stale on a version bump.
cargo build --release --workspace

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
