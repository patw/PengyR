# PengyR — Application Specification

## Overview

PengyR is a Rust + Qt6 rewrite of [Pengy](https://github.com/patw/pengy) — a local-first AI agent application that connects to any OpenAI-compatible LLM API and gives the model a set of tools to operate on the user's machine. Three frontends: Qt6 desktop GUI, CLI, and Web UI.

> PengyR shares `~/.config/pengy/` with the Python Pengy and PengyCPP. Settings and chat history are fully interoperable between all three applications.

---

## Technology Stack

- **Core Language:** Rust (stable, edition 2021)
- **GUI Framework:** Qt6 via C++17 (CMake build)
- **CLI:** Rust binary with ANSI terminal output, rpassword for sudo
- **Web UI:** Axum + SSE streaming, Bootstrap 5 (CDN)
- **Async Runtime:** tokio (multi-threaded, 4 workers)
- **LLM Client:** `reqwest` (HTTP/HTTPS) + `serde_json` (OpenAI API types); `primp` for browser-grade TLS fingerprinting on web_search
- **Markdown Rendering:** Qt's built-in QTextBrowser markdown subset + custom regex transforms (GUI); simple regex-based converter (Web)
- **FFI Boundary:** Rust `extern "C"` → C++ header `pengy_ffi.h`
- **Storage:** JSON files in `~/.config/pengy/` (shared with Python Pengy and PengyCPP)

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│  Frontends                                                          │
│                                                                     │
│  ┌─ Qt6 GUI (C++17) ─────────────┐  ┌─ CLI (Rust) ──────────────┐ │
│  │ ChatHistory / ChatView /       │  │ Interactive REPL           │ │
│  │ ChatInput / SettingsDialog     │  │ Single-shot mode           │ │
│  │ ChatWorker (QThread → FFI)     │  │ 18 slash commands          │ │
│  └────────────┬───────────────────┘  └────────────┬──────────────┘ │
│               │ C FFI (20 extern "C")             │ direct Rust    │
│               │                                    │                │
│  ┌─ Web UI (Rust/Axum) ──────────┐                │                │
│  │ Bootstrap 5 + SSE streaming    │                │                │
│  │ WebWorker per active chat      │                │                │
│  └────────────┬───────────────────┘                │                │
│               │ direct Rust                        │                │
├───────────────┴────────────────────────────────────┴────────────────┤
│  Rust Core Library (pengy_core)                                     │
│  ┌──────────┐ ┌──────────────┐ ┌──────────┐                      │
│  │ config   │ │ chat_manager │ │ tools    │                      │
│  └──────────┘ └──────────────┘ └──────────┘                      │
│  ┌──────────────────────────────────────────┐                      │
│  │ llm_client (tokio async chat loop)       │                      │
│  └──────────────────────────────────────────┘                      │
└─────────────────────────────────────────────────────────────────────┘
```

### Source Layout

```
PengyR/
├── Cargo.toml                  # Workspace root + Rust core library
├── src/
│   ├── lib.rs                  # Public modules + C FFI exports + global tokio runtime
│   ├── config.rs               # Settings load/save + system message rendering
│   ├── chat_manager.rs         # Chat session CRUD + message cleaning
│   ├── tools.rs                # 11 OpenAI function-calling tools
│   └── llm_client.rs           # Async LLM chat generator (tokio channels)
├── cli/                        # CLI binary (pengy-cli)
│   ├── Cargo.toml
│   └── src/main.rs             # Interactive REPL + single-shot mode + slash commands
├── web/                        # Web UI binary (pengy-web)
│   ├── Cargo.toml
│   └── src/main.rs             # Axum server + SSE + Bootstrap 5 UI
├── gui/
│   ├── CMakeLists.txt          # Cross-platform CMake (finds Qt6 + Rust lib)
│   ├── pengy_ffi.h             # C type declarations matching Rust FFI
│   ├── main.cpp                # Entry point — QApplication setup
│   ├── mainwindow.cpp/h        # Three-pane main window, confirmation dialogs
│   ├── chathistory.cpp/h       # Left sidebar — chat list, quick settings
│   ├── chatview.cpp/h          # Right-top — QTextBrowser markdown, tables, tool blocks
│   ├── chatinput.cpp/h         # Right-bottom — message input
│   ├── chatworker.cpp/h        # QThread worker — calls pengy_llm_chat_run()
│   └── settingsdialog.cpp/h    # Settings modal + Fetch Models button
├── appimage/
│   ├── build.sh                # Bundles PengyR-x86_64.AppImage
│   ├── pengy.desktop           # Linux desktop entry
│   └── pengy.png               # App icon (256×256)
└── install.sh                  # Install CLI + Web to ~/.local/bin/
```

---

## FFI Design

The Rust core exposes 20 C functions via `extern "C"`. The C++ GUI includes `pengy_ffi.h` and links the static library.

### Config Functions

| Function | Signature | Returns |
|----------|-----------|---------|
| `pengy_config_load` | `() → *mut c_char` | JSON string of current config |
| `pengy_config_save` | `(json: *const c_char) → bool` | Success |
| `pengy_config_render` | `(template: *const c_char) → *mut c_char` | Rendered system message |

### Chat Functions

| Function | Signature | Returns |
|----------|-----------|---------|
| `pengy_chats_load` | `() → *mut c_char` | JSON array of all chats |
| `pengy_chat_create` | `(title: *const c_char) → *mut c_char` | New chat JSON |
| `pengy_chat_delete` | `(id: *const c_char) → bool` | Success |
| `pengy_chat_save` | `(json: *const c_char) → bool` | Success |
| `pengy_chat_get` | `(id: *const c_char) → *mut c_char` | Chat JSON or NULL |
| `pengy_clean_messages` | `(json: *const c_char) → *mut c_char` | Cleaned message array |

### Tool Functions

| Function | Signature | Returns |
|----------|-----------|---------|
| `pengy_tool_is_readonly` | `(name: *const c_char) → bool` | Whether tool is read-only |
| `pengy_tool_set_user_agent` | `(ua: *const c_char) → void` | Set HTTP User-Agent |
| `pengy_tool_set_timeout` | `(secs: u64) → void` | Set tool execution timeout |

### LLM Chat Function

| Function | Signature |
|----------|-----------|
| `pengy_llm_chat_run` | `(base_url, api_key, model, messages_json, tool_confirmation, confirm_state, sudo_state, on_event_cb, userdata) → bool` |

This is the FFI conversation driver used by the Qt GUI. Called from a `QThread`, it blocks until the conversation completes. Events (`tool_request`, `tool_result`, `assistant_tool_calls`, `final_response`) are reported via the C callback `on_event`. Tool confirmation uses a shared `ConfirmState` struct that the Qt main thread signals via `sendConfirmation()`. The CLI and Web frontends bypass this FFI layer and call `llm_client::chat()` directly via tokio channels.

```c
typedef struct {
    int32_t status;     // 0=idle, 1=pending, 2=confirmed, 3=declined
    bool yolo_turn;     // "Yes to all this turn" flag
} ConfirmState;

typedef struct {
    int32_t status;         // 0=idle, 1=pending, 2=provided, 3=cancelled
    uint8_t password[256];  // null-terminated password buffer
} SudoState;
```

### Memory Management

`pengy_free(ptr)` must be called on every non-NULL string returned by the FFI. These strings are allocated by Rust (`CString::into_raw`) and freed by calling `CString::from_raw` + `drop`.

---

## Desktop UI Layout

```
┌────────────────────┬──────────────────────────────────────────────────┐
│  + New Chat        │                                                  │
│  ⚙ Settings        │           Chat View (Markdown)                   │
│                    │                                                  │
│  ─────────────     │  🧑 You                                          │
│  Chat 1            │  Can you list files in /tmp?                     │
│  Chat 2            │                                                  │
│  Chat 3            │  ┌─ Tool block (collapsed) ──────────────────┐  │
│                    │  │ ▶ Tool: run_bash [command='ls /tmp']       │  │
│  ─────────────     │  └──────────────────────────────────────────┘  │
│  Model: gpt-4o     │                                                  │
│  Tool Confirm: None│  🤖 Assistant                                    │
│                    │  Here are the files in /tmp: ...                 │
│                    │                                                  │
│                    │  ─────────────────────────────────────────────   │
│                    │  [Type a message...                ] [⏹ Stop]  │
└────────────────────┴──────────────────────────────────────────────────┘
```

### Left Pane (Sidebar)
- **+ New Chat button** — Creates a new chat session
- **⚙ Settings button** — Opens the settings dialog
- **Chat history list** — Scrollable, sorted newest first; click to load; chat items have a delete button
- **Quick settings panel** — Shows current model name, tool confirmation mode (YOLO/Safe/None)

### Right-Top Pane (Chat View)
- Markdown-rendered chat messages via `QTextBrowser`
- **User messages:** bold dark-blue `🧑 You` label, plain body text
- **Assistant messages:** bold dark-green `🤖 Assistant` label, markdown-converted body
- **Tool blocks:** collapsed by default (`▶ Tool: name [args…]`); click to expand and show args + result
- Syntax-highlighted code blocks via custom HTML rendering
- Auto-scrolls to bottom on new content

### Right-Bottom Pane (Chat Input)
- **Attach button** — Opens file picker; supports text files and images
- **Image paste** — Clipboard images pasted directly into the chat
- **Text input** — Multi-line text area; Enter to send, Shift+Enter for newline
- On send: attached file contents are injected into the message

---

## ChatView Rendering

The `ChatView` widget (`QTextBrowser`) renders messages as HTML with inline CSS:

| Message type | Appearance |
|---|---|
| User | Bold dark-blue `🧑 You` label, plain body (HTML-escaped, `white-space: pre-wrap`) |
| Assistant | Bold dark-green `🤖 Assistant` label, markdown-converted body |
| Tool block (collapsed) | `▶ Tool: name [args…] (running…)` — clickable toggle link |
| Tool block (expanded) | `▼ Tool: name` + **Arguments:** `<pre>` block + **Result:** `<pre>` block |

### Markdown Pipeline

`markdownToHtml()` applies transforms in order:

1. **HTML-escape** the entire input (`toHtmlEscaped()`) — prevents literal `< > " &` from breaking QTextBrowser's HTML parser
2. **Fenced code blocks** — ` ```lang\n...\n``` ` → `<pre><code>...</code></pre>`
3. **Tables** — `| a | b |` rows → `<table>` HTML
4. **Inline code** — `` `code` `` → `<code>code</code>`
5. **Bold** — `**text**` → `<b>text</b>`
6. **Italic** — `*text*` → `<i>text</i>`
7. **Qt table hack** — `<table>` → `<table cellspacing="0">` (Qt doesn't support `border-collapse: collapse`)
8. **Paragraphize** — split on `\n\n`, wrap plain text in `<p>`, convert `\n` to `<br>`; leave block HTML elements untouched

### Tool Block Toggling

Tool calls are stored as unified `tool_block` messages (not separate `tool_request` + `tool_result`). An `m_expandedTools` QSet tracks which blocks are expanded. Clicking the `▶/▼` toggle link calls `mousePressEvent` which flips the set and re-renders.

---

## CLI Interface

The CLI binary (`pengy-cli`) provides an interactive REPL and single-shot mode. It uses the Rust core directly (no FFI).

### Entry Points

```bash
# Interactive REPL
pengy-cli

# Single-shot
pengy-cli "What is the capital of France?"

# Single-shot without saving
pengy-cli --no-save "quick question"
```

### Interactive Mode

On startup:
1. Loads the most recent chat from `chats.json` (or creates a new one if none exist)
2. Shows a welcome banner with model name and tool confirmation status
3. Enters the REPL loop: prompt → send → stream events → loop

The main thread drives the tokio channel receiver. Tool confirmation blocks on user input with a 3-choice menu: Execute / Yes to all this turn / Decline.

### Single-Shot Mode

1. Creates a throw-away chat (persisted unless `--no-save` is passed)
2. Sends the prompt, drives the conversation to completion, and exits
3. Useful for scripting: `pengy-cli "summarize this file" && pengy-cli "translate to French"`

### Slash Commands

| Command | Description |
|---------|-------------|
| `/help` | Show the command reference table |
| `/new` | Start a new chat session |
| `/yolo [all\|safe\|none]` | Set tool confirmation: all (YOLO), safe (read-only), none — cycles if no arg |
| `/config` | Show current configuration (base URL, model, timeout, etc.) |
| `/model <name>` | Switch models (e.g. `/model gpt-4o`) |
| `/models` | Fetch available models from the endpoint's `GET /v1/models` |
| `/baseurl <url>` | Change the API base URL |
| `/apikey <key>` | Set the API key |
| `/timeout <sec>` | Set tool execution timeout |
| `/agent <string>` | Set the user agent string |
| `/context-keep <n>` | Set context elision keep-turns (0 = keep all) |
| `/system [message]` | Show or set the system message template |
| `/list` | List recent chats with index, title, message count, and creation date |
| `/load <index>` | Load a chat by its `/list` index |
| `/delete <index>` | Delete a chat by its `/list` index |
| `/compact` | Elide old tool results to free context window space |
| `/attach` | Attach a text file (or use `@path` inline in your prompt) |
| `/quit`, `/exit`, `/q` | Exit the CLI |

### File Attachments

The `@path/to/file` syntax anywhere in a message reads the file's contents and injects it as a fenced code block before the user's prompt.

---

## Web Interface

The Web binary (`pengy-web [port]`) runs an Axum HTTP server (default port 5000) with a Bootstrap 5 UI. Intended for single-user personal use; SSL and authentication are expected to be handled by a reverse proxy (nginx).

### Entry Points

```bash
pengy-web                            # localhost:5000
pengy-web 8080                       # custom port
```

### Layout

```
┌──navbar: 🐧 PengyR  [model] [Confirm badge]  [⚙]──────────┐
│                                                             │
│  ┌─sidebar (260px)─┐  ┌─chat area──────────────────────┐  │
│  │  [+ New Chat]   │  │  message history (scrollable)  │  │
│  │                 │  │                                │  │
│  │  Chat 1   [×]  │  │  User bubble (right-aligned)   │  │
│  │  Chat 2   [×]  │  │  🔧 tool card (collapsed)       │  │
│  │  Chat 3   [×]  │  │  Assistant bubble (markdown)   │  │
│  └─────────────────┘  │                                │  │
│   (offcanvas on mob.) │  ┌──input + [Send]────────────┐│  │
│                        │  └────────────────────────────┘│  │
│                        └────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

### Routes

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Redirect to most recent chat (or create new) |
| POST | `/chat/new` | Create a new chat session |
| GET | `/chat/:id` | Render chat page (server-side history) |
| POST | `/chat/:id/send` | Append user message, start WebWorker |
| GET | `/chat/:id/stream` | SSE endpoint — streams events until final response |
| POST | `/chat/:id/confirm` | Unblock tool confirmation (confirmed/declined/yolo) |
| POST | `/chat/:id/sudo` | Provide sudo password to blocked worker |
| POST | `/chat/:id/delete` | Delete chat and redirect to index |
| GET/POST | `/settings` | View/update all config fields |

### SSE Event Types

| Type | Payload | Browser action |
|------|---------|---------------|
| `tool_request` | `name`, `args`, `auto_approved` | Append tool card; if not auto-approved, show confirmation modal |
| `tool_result` | `content`, `declined` | Update tool card body and badge |
| `final_response` | `html`, `usage` | Append assistant bubble |
| `sudo_request` | — | Show sudo password modal |
| `error` | `message` | Append error alert, re-enable input |
| `keepalive` | — | SSE comment (`: keepalive`); browser ignores |

### Tool Confirmation Flow (Web)

```
SSE sends tool_request (auto_approved=false)
       │
       ▼
Browser shows Bootstrap modal (tool name + args JSON)
       │
       ├── Execute              → POST /confirm {confirmed: true}
       ├── Yes to all this turn → POST /confirm {confirmed: true, yolo_turn: true}
       └── Decline              → POST /confirm {confirmed: false}
              │
              ▼
       WebWorker command channel receives → generator resumes
```

### Worker Model

Each active chat gets a `WebWorker` that spawns a tokio task driving `llm_client::chat()`. The worker forwards `LlmEvent`s to an SSE channel and receives confirmations/sudo passwords via a command channel. Workers are stored in a shared `HashMap<String, Arc<WebWorker>>` and cleaned up when the SSE stream drains.

---

## Message Flow

```
User types message → Enter
       │
       ▼
User message appended to chat view and message history
       │
       ▼
System message rendered (templates filled) and prepended
       │
       ▼
LLM API call (non-streaming, full response at once)
       │
       ├── No tool calls → render final response → save chat
       │
       └── Tool call(s) → confirm/execute loop → final response → save chat
```

**Note:** The system message is **not** stored in `chat["messages"]` — it is prepended at request time so templates are always fresh.

---

## Tool Confirmation Flow

```
LLM responds with tool_calls
       │
       ├─ tool_confirmation = "all" ──► auto-approve → execute → loop
       │
       ├─ tool_confirmation = "safe" & tool is read-only ──► auto-approve → loop
       │
       └─ Otherwise
              │
              ▼
        Modal dialog (tool name + full JSON args)
              │
              ├── Execute              → execute → feed result → loop
              ├── Yes to All This Turn → execute + yolo for rest of turn → loop
              └── Decline              → "Tool execution was declined by user." → loop
```

Each frontend handles tool confirmation differently:

- **GUI:** The `ChatWorker` (on a `QThread`) calls `pengy_llm_chat_run()` which blocks when it needs tool confirmation. The Rust side sets `ConfirmState.status = 1` (pending) and spins (5ms). The Qt main thread shows a modal dialog and on user choice sets `status = 2` (confirmed) or `3` (declined).
- **CLI:** The main thread calls `blocking_recv()` on the event channel. When a `ToolRequest` arrives that needs confirmation, it prompts the user (1/2/3) and sends the result on the confirm channel.
- **Web:** The `WebWorker` task receives events and forwards them as SSE. When confirmation is needed, it blocks on a command channel. The browser POSTs to `/chat/:id/confirm` which sends the command.

---

## Settings Dialog (Desktop)

| Field | Widget | Notes |
|-------|--------|-------|
| Base URL | QLineEdit | OpenAI-compatible endpoint |
| API Key | QLineEdit (masked) | Stored in settings.json (plaintext) |
| Model | QComboBox (editable) | Pre-populated with current model; "Fetch" button calls `GET /v1/models` |
| System Message | QTextEdit | Supports `{date}`, `{username}`, etc. templates |
| Tool Confirmation | QComboBox | "YOLO (All)", "Safe Only", "None" |
| Context Keep Turns | QSpinBox | Number of recent turns to keep tool results for (0 = keep all) |
| UI Scale | QComboBox | 75%, 100%, 125%, 200% — takes effect on relaunch |
| Tool Timeout | QSpinBox | Seconds (-1 = no timeout) |

---

## Data Storage

Shared with Python Pengy and PengyCPP at `~/.config/pengy/`.

### Settings File: `~/.config/pengy/settings.json`

```json
{
  "base_url": "https://api.openai.com/v1",
  "api_key": "",
  "model": "gpt-4o",
  "system_message": "You are a helpful assistant. The current date is {date} and the user is {username} on host {hostname} which is {osinfo}.",
  "tool_confirmation": "none",
  "ui_scale": 100,
  "user_agent": "PengyAgent/1.0",
  "tool_timeout": 60,
  "context_keep_turns": 0
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `base_url` | string | `https://api.openai.com/v1` | OpenAI-compatible API endpoint |
| `api_key` | string | (empty) | API key |
| `model` | string | `gpt-4o` | Model name |
| `system_message` | string | (see above) | Template; `{date}`, `{username}`, `{hostname}`, `{osinfo}` filled at send time |
| `tool_confirmation` | string | `"none"` | `"all"` (YOLO), `"safe"` (read-only auto), `"none"` (prompt all) |
| `ui_scale` | int | `100` | Sets `QT_SCALE_FACTOR` on next launch (75/100/125/200); CLI ignores |
| `user_agent` | string | `PengyAgent/1.0` | User-Agent header for HTTP requests |
| `tool_timeout` | int | `60` | Timeout in seconds for tool execution (-1 = no timeout) |
| `context_keep_turns` | int | `0` | Recent turns whose tool results are kept; older ones elided. 0 = keep all |

### System Message Templating

`config::render_system_message(template)` is called at send time (not at save time), so `{date}` always reflects today. Variables:

| Placeholder | Source |
|------------|--------|
| `{date}` | `chrono::Local::now().format("%B %d, %Y")` |
| `{username}` | `whoami::username()` |
| `{hostname}` | `hostname::get()` |
| `{osinfo}` | `std::env::consts::OS` + `std::env::consts::ARCH` |

### Chats File: `~/.config/pengy/chats.json`

Array of chat session objects with `user`, `assistant` (including `tool_calls`), and `tool` messages. Format is identical to Python Pengy.

---

## Tools

All 11 tools from Python Pengy are implemented in Rust (`src/tools.rs`):

| Tool | Read-only | Description |
|------|:---:|-------------|
| `read_file` | ✅ | Read a local file. Expands `~`. |
| `read_multiple_files` | ✅ | Read up to 20 files at once, each under a clear header. |
| `write_file` | ❌ | Write content to a file (creates parent dirs). |
| `replace_in_file` | ❌ | Exact string replacement; must match exactly once. |
| `run_bash` | ❌ | Execute a bash command (sudo via `-S` with cached password). |
| `run_python` | ❌ | Write code to temp file and execute with `python3`. |
| `web_search` | ✅ | DuckDuckGo search via `primp` (browser-impersonating HTTP, 5s timeout). |
| `download_file` | ❌ | Download file to `~/Downloads/`. |
| `fetch_url` | ✅ | Fetch URL text content (strips HTML, 50K char limit). |
| `directory_tree` | ✅ | Visual directory tree (Unicode box-drawing, 500 entry cap). |
| `search_content` | ✅ | Regex search in files with context lines and region grouping. |

Tool execution runs on the tokio runtime via `tokio::task::spawn_blocking` for CPU/IO-heavy operations. Sudo password is cached in memory for the session.

---

## App Identity

- **Application name:** "PengyR" (set via `QApplication::setApplicationName("PengyR")`)
- **Icon:** `pengy.png` (256×256) — loaded at startup via `QApplication::setWindowIcon`
- The desktop app shows in taskbar, alt-tab, and window decorations on X11/XWayland. On native Wayland, the provided `pengy.desktop` file may be needed for taskbar icon.
- The CLI uses no icon but displays the penguin emoji (🐧) in its welcome banner.

---

## Build & Packaging

### Linux Native

```bash
./build_linux.sh
# → gui/build/pengy          (~13 MB, links Rust statically, Qt6 dynamically)
# → target/release/pengy-cli  (standalone binary)
# → target/release/pengy-web  (standalone binary)

# Install CLI + Web to ~/.local/bin/
./install.sh --prebuilt
```

### Linux AppImage (GUI only)

```bash
./build_linux.sh
cd appimage && ./build.sh
# → PengyR-x86_64.AppImage  (~41 MB, fully portable)
```

The AppImage bundles:
- The `pengy` binary (~13 MB, Rust statically linked)
- Qt6 shared libraries + platform plugins (XCB + Wayland)
- Wayland shell integration plugins (xdg-shell, wl-shell, etc.)
- Wayland graphics integration + decoration plugins
- Image format plugins (JPEG, GIF, ICO, SVG)
- Network/SSL dependencies

### Linux .deb

```bash
./build_deb.sh
# → pengy_<version>_amd64.deb
```

### macOS

```bash
brew install qt@6 cmake
./build_macos.sh [arm64|x86_64]
# → Pengy.app
# → PengyR-macOS-<arch>.dmg
```

### Windows

```
build_windows.bat
# → gui\build_windows\Release\pengy.exe
# → PengyR-Windows\ (bundled with windeployqt)
```

---

## Design Decisions

**Rust core + C FFI instead of pure Rust GUI:** The Rust GUI ecosystem (egui, iced, slint) lacks the maturity of Qt for complex desktop applications. Qt6 via C++ provides a proven widget toolkit with native look-and-feel on all platforms. The C FFI boundary is thin — 20 functions with simple types.

**Static linking of Rust core:** The Rust library is compiled as a static archive (`.a` / `.lib`) and linked into the Qt6 binary. This eliminates runtime Rust dependencies in the final binary. The trade-off is a larger binary (~13 MB) but simpler deployment.

**Blocking LLM call on QThread:** Instead of the Python generator pattern, the Rust `pengy_llm_chat_run()` blocks the calling QThread until the conversation ends. Events are pushed via a C callback. Tool confirmation uses a shared struct + busy-wait (5ms spin) rather than a condition variable, because tokio async and blocking condvars don't compose well. The 5ms spin is negligible for an app that's already waiting on network I/O.

**Qt native markdown instead of a markdown library:** Qt's `QTextBrowser` supports a subset of Markdown natively (bold, italic, links, code). Custom regex transforms add fenced code blocks, tables, and paragraph breaks. This avoids pulling in a Rust or C++ markdown library that would need to match Python's `markdown` + `pygments` output.

**Message schema compatibility:** Chat messages use the same JSON schema as Python Pengy — `{"role": "user", "content": "..."}`, `{"role": "assistant", "content": "...", "tool_calls": [...]}`, `{"role": "tool", "tool_call_id": "...", "content": "..."}`. The internal ChatView representation uses unified `tool_block` messages (combining request + result) for rendering, but the persisted format matches the Python version exactly.

**Non-streaming API calls:** The LLM client uses non-streaming completions (no `stream: true`). Full responses render at once. This simplifies the architecture and is acceptable because tool call round-trips dominate latency for agentic workflows.

**Sudo via `-S`:** Same approach as Python Pengy — detect `sudo` in bash commands, prompt for password, pass it to `sudo -S`. Password cached in memory for the session. No PTY complexity.

**System message templating at send time:** Templates are resolved fresh on every send so `{date}` is always accurate regardless of when the config was saved.

**Cargo workspace for multi-binary:** The core is a library crate (`lib` + `staticlib` + `cdylib`) at the workspace root. CLI and Web are separate binary crates in `cli/` and `web/` that depend on the core via `path = ".."`. This lets `cargo build --release` produce all three outputs, while CMake still finds `libpengy_core.a` in `target/release/` for the GUI.

**CLI with no TUI framework:** The CLI uses raw ANSI escape codes for colors instead of a TUI library. This keeps the binary small and avoids terminal compatibility issues.

**Web with embedded templates:** The Web UI embeds all HTML as Rust string-building functions instead of using a template engine. This avoids a build-time dependency and keeps the entire web server in a single file.

---

## Feature Parity (vs Python Pengy)

| Feature | Status | Notes |
|---------|:---:|-------|
| OpenAI-compatible LLM API | ✅ | Same API format and tool calling |
| 11 tools | ✅ | All tools ported |
| Qt6 desktop GUI | ✅ | Three-pane layout, markdown, tool blocks |
| CLI (interactive REPL + single-shot) | ✅ | 18 slash commands, @path attachments |
| Web UI (SSE streaming) | ✅ | Axum + Bootstrap 5, mirrors Python Flask UI |
| File attachments (GUI) | ✅ | Image + text file support |
| Image paste from clipboard | ✅ | |
| Image download rendering | ✅ | Async HTTP fetch in QTextBrowser |
| Tool confirmation (YOLO/Safe/None) | ✅ | All three frontends |
| Sudo password support | ✅ | All three frontends |
| Context elision | ✅ | `elide_old_tool_results` wired to config |
| Chat export to Markdown | ✅ | GUI sidebar export button |
| Settings dialog + Fetch Models | ✅ | GUI dialog + Web settings page + CLI `/config` |
| Skills system | ✅ | Skills are markdown docs loaded via system message |

---

## Dependencies

### Rust Core (Cargo.toml)

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (multi-threaded, 4 workers) |
| `reqwest` | HTTP client (rustls-tls) |
| `primp` | Browser-impersonating HTTP client (TLS/JA3/JA4/HTTP2 fingerprinting) |
| `serde` / `serde_json` | JSON serialization |
| `scraper` | HTML parsing for `fetch_url` |
| `regex` | Pattern matching in tools and markdown |
| `walkdir` | Directory tree traversal |
| `chrono` | Date/time for system message templates |
| `uuid` | Chat session IDs |
| `dirs` | XDG config directory resolution |
| `once_cell` | Lazy statics |
| `futures-util` | Stream utilities for tokio channels |
| `url` | URL parsing |

### CLI (cli/Cargo.toml)

| Crate | Purpose |
|-------|---------|
| `rpassword` | Secure sudo password input (no echo) |
| `reqwest` | `/models` endpoint fetch |
| `regex` | @path file attachment resolution |

### Web (web/Cargo.toml)

| Crate | Purpose |
|-------|---------|
| `axum` | HTTP framework (routing, SSE, forms) |
| `futures-util` | Stream construction for SSE |
| `regex` | Markdown-to-HTML conversion |

### C++ GUI (CMakeLists.txt)

| Dependency | Purpose |
|-----------|---------|
| Qt6::Core | Foundation classes |
| Qt6::Widgets | GUI toolkit |
| Qt6::Network | Network information plugin |
| OpenSSL | SSL/TLS for Qt networking |

### Build Tools

| Tool | Purpose |
|------|---------|
| Rust (stable) | Compile core library |
| CMake ≥ 3.16 | Build Qt6 GUI |
| C++17 compiler | GCC ≥ 8, Clang ≥ 7, MSVC 2019+ |
| linuxdeploy + plugin-qt | AppImage bundling (Linux only) |

---

## License

MIT
