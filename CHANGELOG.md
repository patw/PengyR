# Changelog

## v1.8.0 (current)

- Attachment storage and rendering support across the CLI, desktop GUI, and web UI.
- Provider message handling improvements and expanded attachment tests.

## v1.7.3 (current)

- **Safer sudo handling.** `run_bash` now requires explicit `elevated=true` before invoking sudo, ignores sudo mentions in quotes/comments/data, and scopes cached sudo credentials to each web worker. Rust and C++ editions flush tool output before the hidden-password prompt for reliable terminal ordering.


Ported from the Python edition, keeping the three editions in feature sync.

- **CLI integration tests.** `cli/tests/cli_integration.rs` adds 20 tests
  covering the CLI's own command layer for the first time (previously zero
  coverage for any slash command, not just the new ones). Spawns the real
  `pengy-cli` binary as a subprocess per test with an isolated config dir,
  including one full end-to-end turn against an in-process stub LLM server.
- **Binary guard.** `snip_tool_output()` (the shared choke point for
  `run_bash`, `run_python`, `directory_tree`, `search_content`, and `glob`)
  now runs a `looks_binary()` heuristic first: a NUL byte anywhere in the
  first 4KB, or a non-printable/control-char ratio over ~25%, blocks the
  output outright with a short diagnostic instead of loading it into context.
  `read_and_remove()` (used by `run_bash`/`run_python`) also switched from
  `fs::read_to_string`'s strict decoding — which silently turned any
  invalid-UTF-8 output into an *empty string* via `unwrap_or_default()` — to
  lossy decoding, so that content reaches the guard as text instead of
  vanishing without a trace.
- **Redact last message.** `chat_manager::redact_last_message()` pops exactly
  one raw message off the end of a chat per call — a tool result, an
  assistant `tool_calls` request, or a final response — repeatable all the
  way to an empty chat. A popped tool result strikes its id directly from the
  assistant's `tool_calls` list rather than falling through to
  `clean_dangling_tool_calls()`'s "cancelled" synthesis, which would
  regenerate an identical stub forever and never let redaction advance.
  Wired as `/redact [N]` in the CLI, a redact button in the Web navbar
  (`POST /chat/:id/redact`, refused with 409 while a turn is in flight), and
  a "Redact" button in the GUI input row (`pengy_messages_redact_last` FFI).
- **Tasks in the CLI and Web UI.** Previously GUI-only; `/tasks` and
  `/task <#>` in the CLI, and a Tasks modal (`GET /tasks`, `POST
  /tasks/render`) in the Web UI, both routing the rendered prompt through the
  normal send path.
- **Cumulative token usage.** `chat_manager::add_usage()` accumulates each
  turn's token counts into `chat.usage` (persisted, not session-only state),
  so the running total for a chat survives reloads and tab switches instead
  of only ever showing the last turn's numbers. All three frontends show it
  next to the model/tool-confirmation status.
- **GUI: "New Chat" sidebar performance.** Two stacked costs scaling with
  total chat count made "New Chat" visibly slow with more than a couple dozen
  chats: `pengyIcon()` rebuilt a 15-pixmap `QIcon` from scratch on every call
  even though every sidebar row requests the same `(name, color)` (fixed with
  a cache), and `createNewChat()` called `loadChats()`'s full
  clear-and-rebuild on every click (fixed with `ChatHistoryWidget::addChat()`,
  a single-row insert). Fixing the full rebuild uncovered a real regression:
  `closeTab()`/`loadIntoNewTab()` delete an abandoned empty "New Chat" from
  disk but never removed its sidebar row, previously masked by the full
  rebuild that ran right after — without it, closing an empty chat and
  clicking New Chat again left a permanent ghost row each time. Fixed with
  the matching `ChatHistoryWidget::removeChat()`.
- **GUI: quick-settings whitespace gap.** The "no cached model list" hint
  label was only text-cleared once populated, not hidden — an empty `QLabel`
  still claims a line of layout height, leaving a permanent gap above "Tool
  Confirm:". Now hidden outright when a model list exists.
- **Settings: two more UI scale options.** 110% and 135% added alongside the
  existing 75/100/125/150/175/200% steps.

## v1.7.0

- **Ask the user a question, interactively.** The web UI now surfaces
  `ask_user_question` in an interactive modal showing the model's options and a
  free-text "Other" field, and routes the answers (submit or cancel) back through
  a new `POST /chat/<id>/answer` endpoint. The assistant's preamble narration is
  also streamed live instead of only appearing after a reload.
- **Narration now renders before the tool cards.** The text the model writes
  alongside its tool calls is persisted but was dropped from the live run — and
  the reload path put it *after* the tool cards. CLI, desktop GUI, and web now
  render it live, and the reload path renders it first (shared
  `assistantDisplayMessage` helper).
- **`PENGY_CONFIG_DIR` for built binaries.** Anything driving a built pingy-cli /
  pengy-web binary can now point it at a scratch config instead of silently using
  the real settings (and API key). It sits between the explicit override and the
  default `~/.config/pengy`; a leading `~` is expanded, matching the Python and
  C++ editions. Resolution is factored into a pure `resolve_config_dir` so tests
  pass every input in.
- **Web hardening:** tool cards are de-duplicated on SSE reconnect, and
  attribute content is escaped (`escAttr`) so model-supplied text can't break
  out of `title="…"`.

## v1.6.4

- **Incremental persistence — a turn reaches disk before it finishes.** The CLI
  (`save_progress()`) and web worker (`chat_manager::save_chat_progress`) write
  after every message a run produces (assistant tool calls, tool results,
  question answers, final reply) instead of only when it finishes, and the user
  message is persisted up front. A crash, cancel, or API error mid-tool-loop
  used to silently drop the whole turn's tool calls while the user message
  stayed on disk.
- **Mid-run renames are preserved.** `save_chat_progress` re-reads the on-disk
  title before each write, so a rename landing mid-run is no longer clobbered by
  the worker's stale in-memory snapshot.
- **Dangling tool calls are repaired on any run end.** Every run-ending path
  (final response, error, cancel) runs `clean_dangling_tool_calls` before the
  last save, synthesizing a placeholder tool message for any orphaned assistant
  `tool_calls` so the next request does not go wrong.
- Extended `chat_storage` tests cover `save_chat_progress` keeping out-of-band
  title edits.

## v1.6.3

- **Fix: Stop button left the sidebar status bubble stuck.** Pressing Stop cleared
the tab's thinking/tool-running state but never refreshed the quick-settings
status dot, so the bubble stayed on "Thinking…" (blinking red) or
"Running Tool…" (orange) instead of returning to green "Idle". The Stop handler
now repaints the status bubble, matching the normal completion and error paths.
Fixed in all three editions (Python, C++, Rust).

## v1.6.2

- **Persistent model list and per-tab model selection (desktop GUI):** the sidebar
  "Model:" field is now an editable dropdown, pre-populated from a persistent model
  list cached in `~/.config/pengy/models_cache.json` (shared across the Python, Rust
  and C++ editions). Each chat tab remembers its own model — stored on the chat
  record — overriding the global default, and the dropdown follows the active tab.
  Settings → Fetch refreshes and re-persists the list; a hint appears under the
  dropdown when no cache has been fetched yet.

## v1.6.1

- **Qt local-image rendering fix:** raw HTML `<img>` tags now render canonical
  `file:///…` local URLs correctly in the desktop chat view. The loader also
  accepts absolute local paths emitted by models.

- **Tooling updates:**
  - `download_file` now streams directly to a configurable directory (default
    `~/Downloads`), returns the saved path and byte size, overwrites same-name
    files, supports per-call `max_size_mb` limits (`0` = unlimited), and uses a
    120-second no-data stall timeout so large transfers can finish.
  - `fetch_url` and `read_multiple_files` now follow the configured global tool
    output limit; `fetch_url` also accepts a `max_chars` override.
  - `run_bash` and `run_python` accept an optional `cwd` working directory.
  - `search_content` matches literal text by default; pass `regex=true` for
    regular-expression searches. Tool descriptions now document their limits,
    safety behavior, and argument semantics more precisely.
- **Tool defaults and controls:** tool execution now defaults to 300 seconds
  (matching the documented setting), and the new `download_max_mb` setting
  controls the default download cap (100 MB by default, `0` = unlimited).

## v1.6.0

- **New `read_image` tool** — the agent can inspect local images (screenshots,
  photos, diagrams, charts, rendered plots) and attach them to the conversation
  so vision-capable models can describe what they show.
  - Images decoded, preprocessed (resize/compress to configurable limits), and
    base64-encoded via Rust `image` crate
  - Parked on `ToolContext` (not the tool return value) because the API only
    accepts string content in `role: "tool"` messages
  - Attached as a follow-up user message with `image_url` parts after the tool
    loop completes
  - Added to `is_readonly_tool()` safe-list for auto-approval in "safe" mode
  - Limits backed by `IMAGE_MAX_DIMENSION`, `IMAGE_MAX_MB`, `IMAGE_QUALITY`
    statics, shared across all frontends
  - Tests: image attachment in LLM loop, error handling, chat storage
    round-trips for multipart image content
- **Graceful degradation for text-only models**: if the API returns HTTP 400
  because the model doesn't support vision inputs, the `image_url` parts are
  automatically stripped from all messages, a clarifying note is appended, and
  the conversation retries without the image — instead of emitting a
  `LlmEvent::FinalResponse` with "API error (HTTP 400)" and ending the chat.
  Implemented in all three editions (Python, C++, Rust).
- **Fix: tool output truncation now cuts on line boundaries and separates file
  reads from command output**:
  - `read_file` / `read_multiple_files`: truncate from the head only
    (contiguous, no middle gap) — the head has imports/declarations, the rest
    can be paged via `offset`
  - `run_bash` / `run_python`: remain tail-biased (head + tail, middle snipped)
    — command echo at the start, errors at the end, disposable middle
  - Both seams cut on full line boundaries so the model never sees a broken
    half-line fragment
  - Whole-file reads that fit within the limit stay bare — no `[Lines X-Y]`
    header to parse
  - Truncated file headers show the exact continuation offset for easy paging
  - Giant single-line files fall back to character-boundary cutting
- **Updated README screenshots** — new settings, templates, and main UI images.

## v1.5.9

- Fix web SSE reconnect race: replaced the single-use mpsc receiver with an
  append-only event log + `tokio::sync::watch` channel. SSE events now carry
  monotonic IDs and reconnects resume via `Last-Event-ID`, so a phone sleep /
  tab switch can no longer drop the `final_response` and leave the UI stuck on
  "Thinking…".
- Mobile web layout fixes: remove the double-counted safe-area padding that
  created a gap below the input bar, allow Firefox Android to scroll a focused
  prompt above its software keyboard, and explicitly bring that prompt into
  view on focus.

## v1.5.7

- `run_bash` now authenticates sudo via `SUDO_ASKPASS` instead of piping the password to stdin — fixes sudo in pipelines (`echo x | sudo tee f`), with redirected stdin, after a command that reads stdin, and for the second and later `sudo` in one command
- Fixed `search_content` tool output limits — wasn't respecting the global snip setting
- Added missing `qt6-svg-dev` to CI workflows (Linux build fix)
- `glob` tool now auto-extracts directory prefix from patterns like `~/src/*.rs`
- Added `QuestionState` for `ask_user_question` in the Qt6 GUI

## v1.5.4

- Fixed `todowrite` and `apply_changes` tool schemas so the LLM generates valid calls
- Added schema-content tests to catch this class of bug automatically

## v1.5.3

- Fixed scrollbar jumping in chat view when new content arrived
- Refreshed the UI with consistent icon set
- Harmonized output limits across tools

## v1.5.2

- Added `apply_changes` tool — multi-file transactional edits with dry-run diff preview
- Raised default tool output limit from 50 KB to 250 KB
- Harmonized `directory_tree` and `read_multiple_files` limit handling

## v1.5.1

- Added origin guard for web UI (CSRF/DNS rebinding protection) with `--trusted-host` flag
- Added `ask_user_question` support to the web worker
- Robust CLI argument parsing across all entry points
- Status dot in GUI sidebar shows live connection state

## v1.5.0

- **Three new tools** — `glob`, `todowrite`, `ask_user_question`
- **Tabbed chat** — multiple concurrent sessions, each with its own worker thread
- Fixed threading: `m_cancelled` is now atomic, and `pengy_llm_cancel` actually reaches the running thread
- Per-tab tool context means stopping one tab won't kill another

## v1.4.5

- Context management: tool results snipped (head + tail) when they exceed the configured limit
- CI split into separate Rust test and GUI build stages

## v1.4.4

- Chat history rewritten: per-chat files + index.json for faster loading at scale
- HTML render cache turned O(n²) re-renders into O(n)
- Sidebar performance improvements

## v1.4.3

- UI audit parity with Python edition: confirmation labels, delete confirmations, auto-grow input, CLI tab completion, web sticky scroll, navbar/badge theme prep

## v1.4.2 – v1.4.1

- Performance: faster chat load and render, cleaned up hardcoded paths

## v1.4.0

- Configurable LLM timeout setting
- Mobile-friendly web UI
- Default tool timeout bumped from 60s to 300s

## v1.3.11

- Image preprocessing for LLM vision APIs
- Web UI renders local images via `/files` route with `![alt](url)` markdown support
- Exponential backoff on 429/529 HTTP status from LLM providers

## v1.3.9 – v1.3.7

- Added `--config-dir`, `--version`, `--no-browser` flags to `pengy-web`
- Fixed tool call display, better 500 handling
- Bugfixes and quality-of-life improvements

## v1.3.5 – v1.3.4

- Reasoning traces displayed for models that emit them
- CLI and Web UX/UI improvements
- Added `--model`, `--output`, `--config-dir`, `--system`, `--no-browser` CLI flags

## v1.3.3 – v1.3.1

- Stop button in web UI
- Many GUI fixes (wider dropdowns, shorter task previews, themed dialogs)
- Fixed Qt theme application

## v1.3.0

- **Theme system** — light/dark/system modes with accent colours, font scaling
- **Tasks** — reusable prompt templates with `%placeholder%` tokens
- Font scaling fix — markdown and code fonts now track the configured UI scale

## v1.2.x

- Reasoning effort and reasoning history options for compatible models
- CI/CD fixes for Windows (MSVC compat, Qt version pinning), macOS (Homebrew trust), Linux (thin LTO to prevent OOM)
- Cross-edition documentation and interop testing
