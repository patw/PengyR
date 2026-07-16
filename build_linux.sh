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

# ── Smoke test: verify --version and --help on every binary ─────────
echo "==> Smoke testing binaries..."
PASS=0; FAIL=0
smoke() {
    local bin="$1" name; name=$(basename "$bin")
    if [ ! -f "$bin" ]; then
        echo -e "  \033[31m✗\033[0m $name not found"
        FAIL=$((FAIL+1)); return
    fi
    if "$bin" --version 2>/dev/null | grep -q "^Pengy v" && \
       "$bin" --help    2>/dev/null | grep -qiE "usage|options"; then
        echo -e "  \033[32m✓\033[0m $name --version + --help"
        PASS=$((PASS+1))
    else
        echo -e "  \033[31m✗\033[0m $name --version/--help failed (stale or broken?)"
        FAIL=$((FAIL+1))
    fi
}
smoke "$ROOT/target/release/pengy-cli"
smoke "$ROOT/target/release/pengy-web"
if [ -f "$ROOT/gui/build/pengy" ]; then smoke "$ROOT/gui/build/pengy"; fi
if [ "$FAIL" -gt 0 ]; then
    echo -e "\033[31m==> $FAIL binary(s) failed smoke test!\033[0m"
    exit 1
fi
echo "==> All $PASS binary(s) passed smoke test."

echo ""
echo "==> Done!"
echo "    GUI: gui/build/pengy"
echo "    CLI: target/release/pengy-cli"
echo "    Web: target/release/pengy-web [port]"
echo ""
echo "==> Run with: ./gui/build/pengy"
echo "==> To create AppImage: cd appimage && ./build.sh"
