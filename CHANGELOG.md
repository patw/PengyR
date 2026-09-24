# Changelog

## v1.9.3

- **Image previews and save controls.** Drag and drop image files into the chat input, preview attached images before sending, remove individual images, and save images shown in chat to disk. Includes regression tests for drag-and-drop and image preview.
- Coordinated v1.9.3 release across the Python, Rust, and C++ editions.

## v1.9.2

- **Restore Linux downloads.** The v1.9.1 Linux release job built the .deb but failed before uploading either Linux asset because linuxdeploy-plugin-qt could not find `qmlimportscanner`. Install `qt6-declarative-dev-tools` on the release runner, then rebuild and upload the AppImage and .deb together. The v1.9.1 tag remains unchanged.

## v1.9.1

- **Recover from aggregate context-limit errors without rerunning tools.** Explicit provider context-overflow responses (HTTP 400/413/422 with recognized code or message) trigger at most four request-only tool-result reductions: retain head/tail previews first, then stub older results, protecting the latest tool result until necessary. Full tool events and saved history stay intact, including assistant calls and matching tool IDs. The CLI, GUI, and web surface `context_compacted` progress. Stub-server tests cover successful recovery, original history, no repeated tools, unrelated 400s, and failures with no eligible tool output. The per-tool output cap remains a separate safety limit; proactive aggregate budgeting is not part of this release.
- **Fix: AppImage Wayland plugins now use only the bundled Qt runtime.** Manually
  bundled Wayland plugins did not receive the AppImage-local RPATH that
  `linuxdeploy` gives its auto-discovered XCB plugin. On rolling-release hosts
  with a newer system Qt (such as CachyOS/Arch Qt 6.11), the loader mixed the
  host `libQt6WaylandClient` with PengyR's Qt 6.4 runtime and native Wayland
  startup aborted. The package now patches the Wayland plugin RPATHs, bundles
  the complete Qt Wayland library family (including EGL hardware integration),
  and verifies both conditions during the build.
- **CI cache isolation.** Separate Cargo cache keys for check, tests, and GUI prevent jobs with different profiles from sharing build outputs. The tests job now caches dependencies only (not compiled `target/ci` objects); prior Rust test jobs had spent over 45 minutes in the build/test step and timed out. The release workflow is unchanged.

## v1.9.0

- **Remote sudo over ssh (ported from Python Pengy).** `run_bash` takes an optional `host`: the command runs on that machine over ssh (key-based login required), and `sudo` there works exactly like local sudo — explicit `sudo` plus `elevated=true`, a password prompt that names the host in the GUI, Web, and CLI, and delivery through a single-use `SUDO_ASKPASS` helper on the remote side, so the password is never on an argv or in the command's environment. Passwords are cached per host for the run and never offered to another host; a failed sudo authentication (local or remote) now discards the cached password instead of replaying it. Stop kills the remote command and cleans up its askpass directory. The wrapper script is byte-identical to Python's (a test pins its hash); placeholder substitution is single-pass, so a password or command containing placeholder text can't break the quoting. The FFI `SudoState` struct gained a `host` field.
- **Actually fixed web sudo password prompts hanging.** `run_bash` called the sudo password provider directly on a Tokio worker thread, and the web provider blocks that thread on a `Condvar` until the browser answers. Publishing `sudo_request` had just woken the SSE stream task, and a task woken from a worker thread goes into that worker's LIFO slot, which other workers cannot steal. The stream that should deliver the prompt was therefore stuck behind the blocked thread, and the UI spun forever. Reopening the chat "fixed" it only because the new stream ran on a free worker and replayed the event log. The provider now runs via `spawn_blocking`. `ask_user_question` was never affected because it waits with `.await`. A multi-thread regression test (`sudo_provider_block_does_not_starve_woken_tasks`) fails without the fix. The v1.8.7 SSE padding has been removed: it was not the cause, and the hang reproduces with plain `curl`.

## v1.8.7

- **Attempted fix for web sudo password prompts; did not fix them.** Added a 2KB SSE comment prelude on the theory that browsers buffered the first small event. That theory was wrong, and the prelude was removed in the next release (see Unreleased).

## v1.8.6

- **CLI panel layout and emoji handling are reliable.** ANSI styling is measured as zero-width and is kept intact across wraps, so Rust's Markdown/tool panels retain aligned right borders. Complex UTF-8 emoji (including joined emoji, flags, variation selectors, and keycaps) are preserved in all CLI editions.
- **Fixed a critical fast-web-turn replay race.** The web event log is authoritative even before a browser subscribes: event counts are retained and SSE replay no longer loses a completed response when it finishes before `EventSource` attaches. The web UI also defines its wake-lock helpers before send-state code uses them.
- **Skills and specs are current.** README and skills documentation now direct users to [BotSkills](https://skills.catbee.ca) for inspectable reusable skill packages. Cross-edition specs now document durable attachment storage, per-chat files/index, current Tasks surfaces, and SSE replay requirements.

- **The defaults are local now, not OpenAI.** `base_url` is
  `http://127.0.0.1:11434/v1` — Ollama's OpenAI-compatible port, which needs no
  API key — and `model` is **empty**, because a local server ships no model of
  its own (a fresh `ollama list` is empty, so naming one would just fail on the
  user's first message). This is the audience the app is for: people running
  Ollama or llama.cpp on their own machine, not people with an OpenAI account.
  Existing `settings.json` files are untouched — anything already configured
  wins, and nothing is rewritten.
- **An unset model explains itself instead of being sent.** Until a model is
  chosen, `llm_client::chat` emits an `error` event with `kind: "config"`
  carrying `no_model_help()`: `/models` to list what the endpoint offers,
  `/model <name>` to pick one, `ollama pull <name>` if the list is empty, or
  Fetch Models in the GUI/Web UI. No request is made, so the endpoint never sees
  `model: ""`. The GUI sidebar and Web UI fall back to the first model the
  endpoint last advertised, so what they show is what a send will use.
- **An endpoint that never answers says so usefully.** With a local default, a
  connection failure is the likeliest first-run mistake, and `API error: error
  sending request for url (...)` is not an instruction. Local endpoints now get
  "Nothing answered at <url> — is your local model server running?" plus
  `ollama serve` and `/baseurl`; remote endpoints get "Could not reach <url>".
- `credential_help` now says the default local endpoint needs no key.

- **A failed API call is no longer shown — or stored — as the model's answer.**
  A non-2xx response (a fresh install's `401`, a `500`, a transport failure, an
  unparseable body) used to be delivered as a `FinalResponse` whose `content`
  was the error text. Every frontend treats a final response as the assistant's
  reply, so the CLI drew it inside the assistant's own box, the GUI rendered it
  as a message from the model, and all three **wrote it into the chat file as an
  assistant turn** — where `/show`, `/export` and the other editions' UIs read
  it back later as something the model had said. Failures now travel as a
  dedicated `LlmEvent::Error { kind, message }`, are reported on **stderr**, and
  are persisted as nothing. Interactive mode still keeps running.
- **Missing or rejected credentials explain Pengy's own configuration.** The
  endpoint's message is not actionable for a Pengy user: a fresh install gets
  "You didn't provide an API key. You need to provide your API key in an
  Authorization header using Bearer auth" (and the Python SDK's client-side
  error told users to set `OPENAI_API_KEY`, an environment variable no edition
  of Pengy reads). `401`/`403`, or any credential wording in the body, is now
  translated into instructions naming `pengy-cli /apikey`, `/baseurl`, `/model`,
  `/config`, the shared `settings.json` and the Web UI Settings page, plus an
  explicit note that environment variables are not used. The translation lives
  in `llm_client`, so the CLI, GUI and Web UI all get it. Wording is shared with
  the Python and C++ editions.
- Token usage is no longer reported for a turn that failed. Accumulated usage
  used to ride along on the error's final response; it does not now, which
  matches the Python edition (an error raises there, so no usage is recorded
  either).

Tests: `cargo test --workspace` — 277 passing (5 new in `llm_client`,
`tests/ffi_event_contract.rs` for the GUI's FFI path, and a black-box CLI case
covering a rejected credential), plus the two GUI `ctest` suites.

## v1.8.4

- **Fix: `run_bash` `elevated=true` without a `sudo` invocation no longer runs
  silently unprivileged.** `elevated` was only a guard for a literal `sudo`
  command word; a command with `elevated=true` and no `sudo` fell through and
  ran as the ordinary user — no password prompt, no elevation, no error. It now
  fails loudly with an explicit message, so every elevation stays an explicit,
  auditable `sudo` call. The `run_bash` tool description no longer implies that
  `elevated=true` elevates on its own. Regression tests added.

## v1.8.3

- **Fix: image attachment import on macOS and every other platform.** Durable
  attachment objects use extensionless SHA-256 filenames; derivative generation
  previously reopened those objects with filename-based format detection and
  rejected valid PNGs as an unknown image type. Preprocessing now identifies the
  format from image bytes, and the regression suite covers extensionless objects.
- **More reliable macOS image paste.** The Qt input accepts both `QPixmap` and
  `QImage` clipboard payloads, accommodating the native `NSImage` representation
  commonly supplied by macOS. Image-import errors now include the underlying
  reason instead of only a generic failure dialog.

## v1.8.2

- **Fix: AppImage crashed on startup on Wayland-only compositors (niri/sway/Hyprland).**
  The Linux AppImage shipped with only the `xcb` Qt platform plugin, so on a
  Wayland-only session Qt selected the `wayland` plugin, found it missing, and
  aborted (`qFatal` → SIGABRT) before the window opened. `appimage/build.sh` now
  bundles `libqwayland.so` + `libQt6WaylandClient.so.6` and the wayland
  shell/graphics/decoration plugins, **fails the build** (instead of silently
  warning) if the wayland plugin is unavailable, and verifies it landed in the
  artifact. The release workflow installs `qt6-wayland` on the Linux runner.

## v1.8.1

- **CLI: sanitize ANSI/control chars in tool & error display.** Untrusted
  tool/compiler output was echoed to the terminal verbatim, so ANSI escape
  sequences could move the cursor, clear the screen, or skew the `print_box`
  width math. A new `sanitize_display()` strips CSI/OSC/DCS/single-byte escapes
  and non-`\n`/`\t` C0/DEL control chars from the tool-result, tool-request, and
  error-display paths. Display-only: raw bytes to the model are unchanged, and
  emoji/wide glyphs pass through untouched. Covered by `sanitize_display` unit
  tests in `cli/src/main.rs`.
- **About window & page.** A new `About` tab (via the `pengy_about_json` FFI) and
  an `/about` route — backed by a shared `src/about.rs` — show the edition name +
  version, a short description, the project repo/website/license links, and a
  Catbee attribution, consistent across the CLI, GUI, and web.

## v1.8.0

- **Durable, content-addressed attachment storage** (attachment schema v1):
  chat JSON now stores small `sha256:` references; validated source bytes and
  bounded image derivatives live under `<config>/attachments/objects|derivatives`,
  written atomically and never stored as base64/data URLs.
- **Attachment rendering across the CLI, desktop GUI, and web UI.** CLI shows
  `[image: …]`/`[attachment: …]` labels and a read-only `/attachments` storage
  report; GUI and web render image thumbnails from a new
  `/attachments/<digest>/<derivative>` route, with an "attachment unavailable"
  placeholder for missing or unknown kinds.
- **Provider message handling improvements.** Provider data URLs are derived
  transiently only during request assembly, and only for the most recent
  `attachment_context_keep_turns` user turns (new config option, default 4).
  Legacy `[Image: filename]` strings remain plain text and are never inferred as
  attachment records.
- **Web chat export** now supports self-contained HTML and ZIP bundles
  (`/chat/<id>/export/html`, `/export/zip`) in addition to Markdown; both embed
  attachment images instead of referencing external paths.
- **Expanded attachment tests** (storage layout, missing/unknown kinds, legacy
  placeholders, markdown/html/zip exports).

## v1.7.3

- **Safer sudo handling.** `run_bash` now requires explicit `elevated=true` before invoking sudo, ignores sudo mentions in quotes/comments/data, and scopes cached sudo credentials to each web worker. Rust and C++ editions flush tool output before the hidden-password prompt for reliable terminal ordering.
- **Sudo password no longer sits in the child environment.** The `SUDO_ASKPASS` helper no longer receives the password through the environment; it's written to a private `0600` file that only the `0700` askpass script reads, so grandchildren / `printenv` can't observe it. Single-use and cleaned up on exit.
- **Binary guard for file/URL tool reads.** `read_file`, `read_multiple_files`, and `fetch_url` now reject content that decodes as UTF-8 but is actually binary (e.g. UTF-16 NUL-interleaved text) that would flood context, on top of the v1.7.2 command-output guard.
- **CLI `/download-max` command** added, matching the `download_max_mb` setting.
- **The LLM "retrying" (429/529) backoff event is now surfaced.** The CLI, Qt GUI, and web UI show it instead of hanging silently.
- **Delete-chat hygiene.** Deleting a chat now also trims it out of the legacy `chats.json` seed, so a later legacy re-import can't silently resurrect it.
- Tests added/updated for all of the above (including the resolve-attachments missing-path warning).

## v1.7.2

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
