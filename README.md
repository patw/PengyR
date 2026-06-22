# PengyR 🐧

**PengyR** is a Rust + Qt6 rewrite of [Pengy](https://github.com/patw/pengy) — a local-first AI agent application that connects to any OpenAI-compatible LLM API and gives the model tools to operate on your machine. Three interfaces: Qt6 desktop GUI, CLI, and Web UI.

> **Beta** — PengyR is a Rust port of the Python Pengy. Chat history and settings are fully interoperable between the two (both use `~/.config/pengy/`), but the Rust version may be missing some features compared to the Python version.

```
PengyR/
├── Cargo.toml               # Workspace root + Rust core library
├── src/
│   ├── lib.rs               # Public modules + C FFI for Qt GUI
│   ├── config.rs            # Config: ~/.config/pengy/settings.json
│   ├── chat_manager.rs      # Chats: ~/.config/pengy/chats.json
│   ├── tools.rs             # 11 OpenAI function-calling tools
│   └── llm_client.rs        # Async LLM chat loop (tokio)
├── cli/                     # CLI binary (pengy-cli)
│   ├── Cargo.toml
│   └── src/main.rs          # Interactive REPL + single-shot mode
├── web/                     # Web UI binary (pengy-web)
│   ├── Cargo.toml
│   └── src/main.rs          # Axum server + SSE + Bootstrap UI
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
├── install.sh               # Install CLI + Web to ~/.local/bin/
└── SPEC.md                  # Full architecture specification
```

## Quick Start

### Linux

```bash
# Dependencies (Ubuntu/Debian):
sudo apt install build-essential cmake qt6-base-dev libgl-dev

# Build all (core + GUI + CLI + Web)
./build_linux.sh

# Run GUI
./gui/build/pengy

# Install CLI + Web to ~/.local/bin/
./install.sh --prebuilt
# or build + install in one step:
./install.sh

# Run CLI (now in PATH)
pengy-cli                                           # interactive REPL
pengy-cli "What is 2+2?"                            # single-shot

# Run Web UI (now in PATH)
pengy-web                                           # http://localhost:5000
pengy-web 8080                                      # custom port
```

### Linux AppImage (portable, no system deps)

```bash
./build_linux.sh
cd appimage && ./build.sh
# → PengyR-x86_64.AppImage
```

### macOS

```bash
brew install qt@6 cmake rust

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
| CLI | Rust | Interactive REPL with slash commands + single-shot mode |
| Web UI | Rust (Axum) | Bootstrap 5 UI with SSE streaming |
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
| CLI (interactive REPL + single-shot) | ✅ | ✅ |
| Web UI (SSE streaming) | ✅ | ✅ |

## Interoperability

PengyR shares the same `~/.config/pengy/` directory as the Python Pengy:
- **`settings.json`** — Same format, both read/write it
- **`chats.json`** — Same message schema. Chats created in one app can be loaded in the other

## Development

```bash
# Build all Rust targets (core + CLI + Web)
cargo build --release

# Build just the GUI
cd gui/build && cmake .. -DCMAKE_BUILD_TYPE=Release && make -j$(nproc)

# Run from build directory
./run_pengy.sh

# Run tests
cargo test

# Format code
cargo fmt              # Rust
clang-format -i gui/*.cpp gui/*.h  # C++
```

## License

MIT
