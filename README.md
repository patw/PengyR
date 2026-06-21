# PengyR 🐧

**PengyR** is an experimental Rust + Qt6 rewrite of [Pengy](https://github.com/patw/pengy) — a local-first AI agent desktop application that connects to any OpenAI-compatible LLM API and gives the model tools to operate on your machine.

> ⚠️ **Experimental** — PengyR is a work-in-progress Rust port of the stable Python Pengy. Chat history and settings are fully interoperable between the two (both use `~/.config/pengy/`), but the Rust version may lag behind on features.

```
PengyR/
├── Cargo.toml               # Rust library crate
├── src/
│   ├── lib.rs               # C FFI — 20 functions exported to Qt GUI
│   ├── config.rs            # Config: ~/.config/pengy/settings.json
│   ├── chat_manager.rs      # Chats: ~/.config/pengy/chats.json
│   ├── tools.rs             # 11 OpenAI function-calling tools
│   └── llm_client.rs        # Async LLM chat loop (tokio)
├── gui/
│   ├── CMakeLists.txt       # Cross-platform Qt6 + CMake
│   ├── pengy_ffi.h          # C declarations for Rust core
│   ├── main.cpp             # Entry point
│   ├── mainwindow.cpp/h     # Three-pane main window
│   ├── chathistory.cpp/h    # Sidebar — chat list + quick settings
│   ├── chatview.cpp/h       # Chat display — markdown, tables, collapsible tool blocks
│   ├── chatinput.cpp/h      # Message input + attach button
│   ├── chatworker.cpp/h     # QThread worker → Rust FFI
│   └── settingsdialog.cpp/h # Settings dialog + Fetch Models
├── appimage/
│   ├── build.sh             # Bundles PengyR-x86_64.AppImage
│   ├── pengy.desktop        # Desktop entry
│   └── pengy.png            # App icon
├── build_linux.sh           # Linux native build
├── build_macos.sh           # macOS build (Homebrew Qt6)
├── build_windows.bat        # Windows build (MSVC Qt6)
└── SPEC.md                  # Full architecture specification
```

## Quick Start

### Linux

```bash
# Dependencies (Ubuntu/Debian):
sudo apt install build-essential cmake qt6-base-dev libgl-dev

# Build & run
./build_linux.sh
./gui/build/pengy
```

### Linux AppImage (portable, no system deps)

```bash
./build_linux.sh
cd appimage && ./build.sh
# → PengyR-x86_64.AppImage
```

### macOS

```bash
brew install qt@6 cmake

./build_macos.sh [arm64|x86_64]
# → gui/build_macos/pengy
# → Pengy.app
```

### Windows

```
REM Prerequisites: Rust, Qt6 (MSVC 2022 64-bit), VS Build Tools, CMake
REM Run from a VS Developer Command Prompt:
build_windows.bat
REM → gui\build_windows\Release\pengy.exe
```

## Architecture

| Layer | Language | What |
|-------|----------|------|
| Core logic | Rust | Config, chat CRUD, 11 tools, LLM chat loop (tokio async) |
| C FFI boundary | Rust `extern "C"` | 20 functions exported for C++ consumption |
| GUI | C++17 + Qt6 | QMainWindow, QSplitter, QTextBrowser markdown rendering |
| Worker | C++ QThread | Calls `pengy_llm_chat_run()` on background thread |
| Packaging | AppImage (Linux), .app (macOS), windeployqt (Windows) | |

The Rust core is **statically linked** into the Qt6 binary — a single ~13 MB executable with no runtime Rust dependency. Qt6 shared libraries are bundled by the platform packager.

## Feature Parity

| Feature | Python Pengy | PengyR |
|---------|:---:|:---:|
| OpenAI-compatible LLM API | ✅ | ✅ |
| 11 tools (bash, python, files, web, etc.) | ✅ | ✅ |
| Three-pane Qt6 desktop GUI | ✅ | ✅ |
| Markdown rendering + syntax highlighting | ✅ | ✅ (Qt native markdown) |
| Table rendering (hack for Qt border-collapse) | ✅ | ✅ |
| Collapsible tool call blocks | ✅ | ✅ |
| Chat history sidebar (CRUD) | ✅ | ✅ |
| Settings dialog + Fetch Models | ✅ | ✅ |
| Tool confirmation (YOLO / Safe / None) | ✅ | ✅ |
| Sudo password support | ✅ | ✅ |
| File attachments (GUI) | ✅ | ✅ |
| Image paste from clipboard | ✅ | ✅ |
| Image download rendering (Qt) | ✅ | ✅ |
| CLI (rich-based terminal REPL) | ✅ | ❌ |
| Web UI (Flask + SSE) | ✅ | ❌ |

## Interoperability

PengyR shares the same `~/.config/pengy/` directory as the Python Pengy:
- **`settings.json`** — Same format, both read/write it
- **`chats.json`** — Same message schema. Chats created in one app can be loaded in the other

## Development

```bash
# Build just the Rust core
cargo build --release

# Build just the GUI
cd gui/build && cmake .. -DCMAKE_BUILD_TYPE=Release && make -j$(nproc)

# Run from build directory
./run_pengy.sh

# Format code
cargo fmt              # Rust
clang-format -i gui/*.cpp gui/*.h  # C++
```

## License

MIT
