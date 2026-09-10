# Building from Source (Rust)

## Prerequisites

- Rust (stable) — `curl --proto '=https' --tls v1.2 -sSf https://sh.rustup.rs | sh`
- CMake ≥ 3.16
- C++17 compiler (GCC ≥ 8, Clang ≥ 7, MSVC 2019+)
- Qt 6.4+

## Linux

```bash
# Dependencies
sudo apt install build-essential cmake qt6-base-dev qt6-wayland libgl-dev

# Build everything (GUI + CLI + Web)
./build_linux.sh

# GUI
./gui/build/pengy

# CLI
./target/release/pengy-cli

# Web
./target/release/pengy-web
```

### AppImage

```bash
# qt6-wayland is REQUIRED: without it the build FAILS (build.sh deliberately
# refuses to ship a Wayland-unbootable AppImage). The wayland platform plugin is
# bundled so the AppImage starts on Wayland-only compositors (niri/sway/Hyprland).
sudo apt install qt6-wayland   # provides libqwayland.so

./build_linux.sh
cd appimage && ./build.sh
# → PengyR-x86_64.AppImage
```

### .deb package

```bash
./build_deb.sh
# → pengy_<version>_amd64.deb
```

## macOS

```bash
brew install qt@6 cmake rust
./build_macos.sh [arm64|x86_64]
# → Pengy.app
# → PengyR-macOS-<arch>.dmg
```

## Windows

From a VS 2022 Developer Command Prompt:

```
REM Prerequisites: Rust, Qt6 (MSVC 64-bit), VS Build Tools 2022, CMake
build_windows.bat
REM → PengyR-Windows\pengy.exe
```

## Running tests

```bash
cargo test
```

## Architecture notes

The Rust core is statically linked into the Qt6 GUI binary — a single ~13 MB executable. The CLI and Web binaries are pure Rust and don't need Qt at all. Qt6 shared libraries are bundled by the platform packager (AppImage, DMG, ZIP).
