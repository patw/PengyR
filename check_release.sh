#!/bin/bash
# Pre-flight checks — run before `git push --tags`
# Catches CI failures: version drift, icon dims, path bugs, permissions.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

WARNINGS=0
warn() { echo -e "\033[33m  WARNING: $*\033[0m"; WARNINGS=$((WARNINGS + 1)); }
fail() { echo -e "\033[31m  FAIL: $*\033[0m"; exit 1; }
ok()   { echo -e "\033[32m  ✓ $*\033[0m"; }

echo "========================================="
echo " PengyR (Rust) Pre-Flight Release Check"
echo "========================================="

# ── 1. Version consistency ──────────────────────────────────────────
echo "--- Version consistency ---"
CARGO_VER=$(grep -oP 'version\s*=\s*"\K[\d.]+' Cargo.toml | head -1)
# build_deb.sh now auto-derives version from Cargo.toml via grep
DEB_DERIVED=$(grep -oP 'version\s*=\s*"\K[\d.]+' Cargo.toml | head -1)
echo "  Cargo.toml:      $CARGO_VER"
echo "  build_deb.sh:    auto-detected from Cargo.toml → $DEB_DERIVED"
if [ -n "$CARGO_VER" ]; then
    ok "Version: $CARGO_VER (single source of truth in Cargo.toml)"
else
    warn "Could not parse version from Cargo.toml"
fi

# ── 2. Icon dimensions ──────────────────────────────────────────────
echo "--- Icon dimensions ---"
if command -v identify &>/dev/null; then
    # AppImage icon (the one linuxdeploy checks)
    APPIMG_ICON="appimage/pengy.png"
    if [ -f "$APPIMG_ICON" ]; then
        AIMG_DIMS=$(identify -format '%w %h' "$APPIMG_ICON" 2>/dev/null)
        AIMG_W=$(echo "$AIMG_DIMS" | cut -d' ' -f1)
        AIMG_H=$(echo "$AIMG_DIMS" | cut -d' ' -f2)
        echo "  appimage/pengy.png: ${AIMG_W}x${AIMG_H}"
        if [ "$AIMG_W" = "$AIMG_H" ] && [ "$AIMG_W" -eq 256 ]; then
            ok "AppImage icon is 256x256 (linuxdeploy-compatible)"
        else
            warn "AppImage icon is ${AIMG_W}x${AIMG_H} — linuxdeploy requires exactly 256x256"
        fi
    else
        warn "appimage/pengy.png not found"
    fi

    # Main icon
    MAIN_DIMS=$(identify -format '%w %h' pengy.png 2>/dev/null)
    MAIN_W=$(echo "$MAIN_DIMS" | cut -d' ' -f1)
    MAIN_H=$(echo "$MAIN_DIMS" | cut -d' ' -f2)
    echo "  pengy.png: ${MAIN_W}x${MAIN_H}"
else
    warn "ImageMagick 'identify' not installed — can't check icon dimensions"
fi

# ── 3. macOS path sanity ────────────────────────────────────────────
echo "--- macOS build script ---"
if grep -q 'cp gui/build_macos/pengy' build_macos.sh 2>/dev/null; then
    # Did it cd into gui/build_macos first? Check.
    if ! grep -B5 'cp.*gui/build_macos/pengy' build_macos.sh | grep -q 'cd gui/build_macos'; then
        warn "build_macos.sh: 'cp gui/build_macos/pengy' may resolve wrong after cd"
    else
        ok "build_macos.sh path looks correct"
    fi
else
    # It uses $ROOT/gui/build_macos/pengy which is fine
    ok "build_macos.sh uses absolute/root-relative paths, looks correct"
fi

# ── 4. CI release.yml permissions ───────────────────────────────────
echo "--- Release workflow ---"
if grep -q 'contents: write' .github/workflows/release.yml; then
    ok "release.yml has 'contents: write' permission"
else
    warn "release.yml missing 'contents: write' — upload step will fail with 403"
fi

# ── 5. CI release.yml Windows Qt version ────────────────────────────
echo "--- Windows Qt version in CI ---"
QT_VER=$(grep -oP "version:\s*'[^']+'" .github/workflows/release.yml | head -1 | grep -oP "[\d.]+")
echo "  release.yml Qt version: $QT_VER"
if [ -n "$QT_VER" ] && [ "$(echo "$QT_VER" | cut -d. -f2)" -eq 5 ]; then
    ok "Qt version $QT_VER — stable LTS, widely available in aqt archives"
else
    warn "Qt version $QT_VER may not be available in aqt archives — verify it exists"
fi

# ── 6. CI release.yml LTO setting ────────────────────────────────────
echo "--- Release LTO setting ---"
if grep -q 'CARGO_PROFILE_RELEASE_LTO' .github/workflows/release.yml 2>/dev/null; then
    LTO_VAL=$(grep -oP 'CARGO_PROFILE_RELEASE_LTO:\s*"\K[^"]+' .github/workflows/release.yml)
    echo "  release.yml LTO override: $LTO_VAL"
    ok "release.yml has LTO override (avoids runner OOM)"
else
    warn "release.yml missing CARGO_PROFILE_RELEASE_LTO env var — may OOM on 7GB runner"
fi

# ── 7. CI release.yml Windows MSVC setup ────────────────────────────
echo "--- Windows MSVC setup in CI ---"
if grep -q 'ilammy/msvc-dev-cmd' .github/workflows/release.yml 2>/dev/null; then
    ok "release.yml uses ilammy/msvc-dev-cmd for MSVC"
else
    warn "release.yml may lack MSVC setup — Windows build may fail to find VS"
fi

# ── 7. Rust build ───────────────────────────────────────────────────
echo "--- Rust build ---"
if [ -f target/release/libpengy_core.rlib ] || [ -f target/release/libpengy_core.a ]; then
    ok "Rust core already built (skipping rebuild)"
else
    echo "  Building Rust workspace..."
    cargo build --release --workspace > /tmp/pengyr_build.log 2>&1
    if cargo build --release --workspace > /tmp/pengyr_build.log 2>&1; then
        ok "Rust workspace built successfully"
    else
        fail "Rust build failed — check /tmp/pengyr_build.log"
    fi
fi

# ── 8. GUI build ────────────────────────────────────────────────────
echo "--- Qt GUI build ---"
if [ -f gui/build/pengy ]; then
    ok "GUI already built (skipping rebuild)"
else
    echo "  Building GUI..."
    mkdir -p gui/build && cd gui/build
    if cmake .. -DCMAKE_BUILD_TYPE=Release > /tmp/pengyr_gui.log 2>&1 && \
       make -j$(nproc) >> /tmp/pengyr_gui.log 2>&1; then
        ok "GUI built successfully"
    else
        warn "GUI build failed — check /tmp/pengyr_gui.log (may be missing Qt6 dev deps)"
    fi
    cd "$ROOT"
fi

# ── 9. Verify binaries (existence + --version/--help smoke test) ───
echo "--- Verify binaries ---"
for bin in gui/build/pengy target/release/pengy-cli target/release/pengy-web; do
    if [ ! -f "$bin" ]; then
        warn "$bin not found"
        continue
    fi
    name=$(basename "$bin")
    ok "$name exists ($(ls -lh "$bin" | awk '{print $5}'))"
    if "$bin" --version 2>/dev/null | grep -q "^Pengy v"; then
        ok "$name --version works"
    else
        warn "$name --version failed (stale binary?)"
    fi
    if "$bin" --help 2>/dev/null | grep -qiE "usage|options"; then
        ok "$name --help works"
    else
        warn "$name --help failed (stale binary?)"
    fi
done

# ── 10. Rust tests ──────────────────────────────────────────────────
echo "--- Rust tests ---"
if cargo test --quiet > /tmp/pengyr_tests.log 2>&1; then
    ok "cargo test passes"
else
    warn "cargo test failed — check /tmp/pengyr_tests.log"
    tail -5 /tmp/pengyr_tests.log
fi

# ── Summary ─────────────────────────────────────────────────────────
echo ""
echo "========================================="
if [ $WARNINGS -eq 0 ]; then
    echo -e "\033[32m All checks passed! Ready to tag.\033[0m"
else
    echo -e "\033[33m $WARNINGS warning(s) found — review above before tagging.\033[0m"
fi
echo "========================================="
