# PengyR 🐧

**A local-first AI agent with tools.** Desktop GUI, web UI, **and** command-line — all backed by the same agent core, talking to any OpenAI-compatible API. A Rust + Qt6 port of [Pengy](https://github.com/patw/pengy), sharing the same `~/.config/pengy/` data.

[![GitHub Release](https://img.shields.io/github/v/release/patw/PengyR)](https://github.com/patw/PengyR/releases)
[![License](https://img.shields.io/github/license/patw/PengyR)](https://github.com/patw/PengyR/blob/main/LICENSE)

---

## What is PengyR?

PengyR is an LLM agent that runs on your own machine. It connects to OpenAI, Ollama, vLLM, Groq, OpenRouter, or any local endpoint, and gives the model 15 built-in tools to operate on your filesystem, run code, search the web, and more — all with your approval.

Three interfaces, one agent:

| **🐧 PengyR Desktop** | **🐧 PengyR CLI** | **🐧 PengyR Web** |
|---|---|---|
| Qt6 GUI with markdown rendering, sidebar with history & quick settings, file attachments | Terminal REPL with slash commands, single-shot mode for scripting | Axum web UI with Bootstrap, responsive layout, SSE live streaming |

All three share the same core — same tools, same chat history, same config. Use whichever fits your flow.

---

## Quick Start

### Download Pre-built Releases

Pre-built binaries are on the [Releases page](https://github.com/patw/PengyR/releases):

| Platform | Format |
|----------|--------|
| **Linux** | `PengyR-x86_64.AppImage` (portable) · `.deb` (Debian/Ubuntu) |
| **macOS** | `PengyR-<arch>.dmg` (arm64 / x86_64) |
| **Windows** | `PengyR-Windows.zip` (bundled Qt DLLs) |

### Linux — Build from Source

```bash
# Dependencies
sudo apt install build-essential cmake qt6-base-dev libgl-dev
curl --proto '=https' --tls v1.2 -sSf https://sh.rustup.rs | sh

# Build everything (GUI + CLI + Web)
./build_linux.sh

# GUI
./gui/build/pengy

# CLI
./target/release/pengy-cli                                    # interactive
./target/release/pengy-cli "What is the capital of France?"   # single-shot

# Web
./target/release/pengy-web                                    # http://localhost:5000
```

The web UI is for single-user personal use. For remote access, put it behind nginx with SSL; use `--trusted-host` to set the public hostname when reverse-proxying.

---

## Features

- **OpenAI-compatible** — Works with OpenAI, Ollama, vLLM, LM Studio, OpenRouter, Groq, or any local endpoint
- **15 built-in tools** — Read, write, and edit files; run bash and Python; search the web; explore and glob your filesystem; track multi-step ops with structured to-do lists; ask clarifying questions
- **Agentic workflow** — The LLM chains multiple tool calls per turn to accomplish complex tasks
- **Tool confirmation** — Three modes: auto-approve everything, auto-approve read-only tools only, or confirm every call
- **Theme system** — System/light/dark modes plus 8 accent colours; fonts scale with the UI
- **Tasks** — Reusable prompt templates with `%placeholder%` tokens for repeat workflows
- **Model discovery** — Fetch available models from your endpoint with one click
- **File attachments** — GUI: attach files or paste images; CLI: `@path` syntax
- **Image rendering** — Pasted and downloaded images display inline in the GUI
- **Templated system message** — Auto-fills `{date}`, `{username}`, `{hostname}`, `{osinfo}`
- **Cross-version interop** — Chats created in Python Pengy or PengyCPP load seamlessly in PengyR
- **Persistent config** — Settings, tasks, and chat history in `~/.config/pengy/`, shared across all interfaces and editions

---

## Screenshots

| Main chat UI | Settings / theme controls | Tasks templates |
|---|---|---|
| ![PengyR main chat UI](pengyui.png) | ![PengyR settings and theme controls](pengyconfig.png) | ![PengyR tasks template manager](pengytasks.png) |

---

## Configuration

**Desktop:** Click ⚙ Settings in the sidebar.  
**CLI:** Run `/config` to view, `/model <name>` to switch models.  
**Web:** Click ⚙ in the top-right navbar.

| Setting | Description |
|---------|-------------|
| Base URL | API endpoint (e.g. `http://localhost:11434/v1` for Ollama) |
| API Key | Your API key (or anything for local endpoints) |
| Model | Model name, e.g. `gpt-4o`, `llama3`, `gemma` |
| System Message | Supports `{date}`, `{username}`, `{hostname}`, `{osinfo}` placeholders |
| Tool Confirmation | All / Safe / None — which tools require approval |
| Theme Mode (GUI) | System / Light / Dark — follows OS palette |
| Accent Color (GUI) | Default, Blue, Teal, Green, Orange, Red, Pink, or Purple |
| UI Scale (GUI) | 75–200% — restart for full native-widget scaling |

---

## Tasks

Tasks are reusable prompt templates for workflows you repeat often — summarizing a YouTube video, drafting a release note, or running a code-review checklist. Open **Tasks** from the desktop sidebar to create, edit, delete, or play templates.

Use `%placeholder%` tokens anywhere in the template to ask for values when the task is played:

```text
Summarize this YouTube video: %Youtube Video URL%
Always use the youtube transcription skill.
```

When you hit **▶ Play**, PengyR collects each placeholder once, renders the full prompt, and sends it through the normal chat pipeline. Tasks live in `~/.config/pengy/tasks.json`, shared across all interfaces and editions.

---

## Tools

PengyR gives the LLM these tools to operate on your machine:

| Tool | Description |
|------|-------------|
| `read_file` / `read_multiple_files` | Read one or more files at once |
| `write_file` | Write or overwrite a file |
| `replace_in_file` | Targeted text replacement (safer than full rewrites) |
| `run_bash` | Execute shell commands (configurable timeout; sudo support) |
| `run_python` | Execute Python code |
| `web_search` | DuckDuckGo web search |
| `download_file` | Download a URL to `~/Downloads/` |
| `fetch_url` | Fetch a URL's text content into context |
| `directory_tree` | Visual directory structure listing |
| `search_content` | Regex search across files in a codebase |
| `glob` | File pattern matching — respects `.gitignore`-style skips |
| `todowrite` | Structured task list for tracking multi-step operations |
| `ask_user_question` | Multi-choice questions to clarify vague requests |

---

## Skills

The 15 built-in tools cover the basics, but PengyR is designed to be extended with **skills** — your own custom instructions and scripts stored as plain markdown files.

A skill is just a `skillname/skillname_skill.md` file with instructions PengyR can read, optionally backed by a bash or Python script. No SDK, no manifest, no packaging — point PengyR at a directory and it figures out the rest.

This means your PengyR can do whatever you need it to:
- Fetch weather from an API
- Control devices on your home network
- Query your local databases
- Generate reports from your own data
- Run system administration tasks
- Send notifications, emails, or messages
- Anything you can describe in a prompt and a script

Skills are also self-authoring — ask PengyR to create one for you, and it writes the markdown, writes the script, and updates the index, all in one conversation.

**📖 Read the full guide:** [`skills/README.md`](https://github.com/patw/Pengy/blob/main/skills/README.md) — covers the philosophy, how skills work, 4 complete examples, and how to make your own.

---

## API Compatibility

| Service | Base URL |
|---------|----------|
| OpenAI | `https://api.openai.com/v1` |
| Ollama | `http://localhost:11434/v1` |
| LM Studio | `http://localhost:1234/v1` |
| vLLM | `http://localhost:8000/v1` |
| OpenRouter | `https://openrouter.ai/api/v1` |
| Groq | `https://api.groq.com/openai/v1` |

---

## Development

### Architecture

| Layer | Language | What |
|-------|----------|------|
| Core logic | Rust | Config, chat/task CRUD, 15 tools, LLM chat loop (tokio async) |
| C FFI boundary | Rust `extern "C"` | 20 functions exported for C++ consumption |
| Desktop GUI | C++17 + Qt6 | QMainWindow, QSplitter, QTextBrowser markdown rendering |
| CLI | Rust | Interactive REPL with slash commands + single-shot mode |
| Web UI | Rust (Axum) | Bootstrap 5 UI with SSE streaming |

The Rust core is **statically linked** into the Qt6 binary — a single ~13 MB executable.

### Project structure

```
PengyR/
├── Cargo.toml               # Workspace root + Rust core library
├── src/                     # Core: config, chat, tasks, tools, LLM client
├── cli/                     # CLI binary (pengy-cli)
├── web/                     # Web UI binary (pengy-web)
├── gui/                     # Qt6 GUI (CMake + C++17)
└── appimage/                # AppImage bundling scripts
```

### Build from source

```bash
git clone https://github.com/patw/PengyR.git && cd PengyR
cargo build --release                                         # Rust core + CLI + Web
mkdir -p gui/build && cd gui/build && cmake .. -DCMAKE_BUILD_TYPE=Release && make -j$(nproc)  # Qt6 GUI
cargo test
```

### Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `reqwest` | HTTP client |
| `primp` | Browser-impersonating HTTP client (TLS/JA3/JA4 fingerprinting) |
| `serde` / `serde_json` | JSON serialization |
| `scraper` | HTML parsing for `fetch_url` |
| `regex` | Pattern matching |
| `walkdir` | Directory tree traversal |
| `axum` | Web framework |
| Qt6::Core, Qt6::Widgets, Qt6::Network | GUI framework |
| CMake ≥ 3.16 + C++17 compiler | GUI build |

---

## Interoperability

PengyR shares `~/.config/pengy/` with Python Pengy and PengyCPP:
- **`settings.json`** — Same format, all versions read/write it
- **`chats.json`** — Same message schema. Chats created in any version load in any other
- **`tasks.json`** — Shared prompt-template library

---

## Also Available

| Edition | Language | Notes |
|---------|----------|-------|
| [**Pengy**](https://github.com/patw/Pengy) | Python | Reference implementation — easiest to hack on |
| [**PengyR**](https://github.com/patw/PengyR) | Rust + Qt6 | High-performance native binary, statically-linked core |
| [**PengyCPP**](https://github.com/patw/PengyCPP) | C++17 + Qt6 | Highest performance, smallest memory footprint |

All three offer the same 15 tools, desktop theme controls, reusable task templates, three interfaces (GUI/CLI/Web), and full chat/task interop. PengyR and PengyCPP ship pre-built AppImage, `.deb`, `.dmg`, and `.zip` releases.

---

## Documentation

- [Configuration reference](docs/configuration.md) — all settings.json fields explained
- [Reverse proxy setup](docs/reverse-proxy.md) — nginx, Caddy, SSH tunnels, Docker
- [Skills deep-dive](docs/skills.md) — skill patterns, `~/.secrets`, `uv` dependencies
- [API compatibility](docs/api-compatibility.md) — provider support, model discovery, local endpoints
- [FAQ](docs/faq.md) — common questions and troubleshooting
- [Building from source](docs/building.md) — platform-specific build instructions
- [Changelog](CHANGELOG.md) — version history

## License

MIT
