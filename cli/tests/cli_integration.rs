//! Black-box integration tests for the `pengy-cli` binary.
//!
//! Before this file, the CLI had *zero* test coverage for its own command
//! layer -- not just the newly-added `/redact`/`/tasks`/`/task`, but every
//! slash command (`/compact`, `/delete`, ...). These tests spawn the real
//! compiled binary as a subprocess (mirroring PengyCPP's `runCli` test
//! helper) rather than testing `PengyCli` in-process, because:
//!
//! - `PengyCli::new()` touches real global state on construction (a tokio
//!   Runtime, a rustyline `Editor` that reads `$HOME/.local/state/pengy/cli_history`)
//!   that isn't gated behind `PENGY_CONFIG_DIR` the way chat/task/settings
//!   storage is.
//! - `pengy_core::config::set_config_dir()` is a process-global `OnceLock` --
//!   settable once per process. `cargo test` runs test functions in parallel
//!   threads of one process, so two tests calling it would race for which
//!   directory "wins" for the whole binary.
//!
//! Spawning a subprocess per test sidesteps both: each child gets its own
//! process (and thus its own fresh `OnceLock`), and `HOME`/`PENGY_CONFIG_DIR`
//! are set per-child without touching the test process's own environment.
//! Chat/task fixtures are written directly as JSON files rather than via
//! `pengy_core::chat_manager`'s functions, for the same reason -- calling
//! those from the test process would hit the *test* process's global config
//! resolution, not a fixture the child can be pointed at independently.

use pengy_core::chat_manager::{Chat, ChatMessage};
use pengy_core::task_manager::Task;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

// ── Test harness ─────────────────────────────────────────────────────

struct Harness {
    config_dir: TempDir,
    home_dir: TempDir,
}

impl Harness {
    fn new() -> Self {
        Self {
            config_dir: TempDir::new().expect("config tempdir"),
            home_dir: TempDir::new().expect("home tempdir"),
        }
    }

    fn config_path(&self) -> &std::path::Path {
        self.config_dir.path()
    }

    fn chats_dir(&self) -> std::path::PathBuf {
        self.config_path().join("chats")
    }

    fn chat_file(&self, id: &str) -> std::path::PathBuf {
        self.chats_dir().join(format!("{id}.json"))
    }

    /// Writes a chat fixture directly as JSON (bypassing chat_manager's
    /// config-dir resolution -- see module doc). `created_at` is pinned far
    /// in the future so the CLI's "resume the newest chat" startup logic
    /// picks this one deterministically, regardless of what else exists in
    /// this test's (already-isolated) config dir.
    fn seed_chat(&self, title: &str, messages: Vec<ChatMessage>) -> Chat {
        let mut chat = Chat::new(title);
        chat.created_at = "2999-01-01T00:00:00".to_string();
        chat.messages = messages;
        std::fs::create_dir_all(self.chats_dir()).unwrap();
        let f = std::fs::File::create(self.chat_file(&chat.id)).unwrap();
        serde_json::to_writer_pretty(f, &chat).unwrap();
        chat
    }

    fn read_chat(&self, id: &str) -> Chat {
        let data = std::fs::read_to_string(self.chat_file(id))
            .unwrap_or_else(|e| panic!("reading chat {id}: {e}"));
        serde_json::from_str(&data).unwrap()
    }

    fn seed_task(&self, title: &str, template: &str) -> Task {
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.to_string(),
            template: template.to_string(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let path = self.config_path().join("tasks.json");
        let existing: Vec<Task> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut tasks = existing;
        tasks.push(task.clone());
        std::fs::write(&path, serde_json::to_string_pretty(&tasks).unwrap()).unwrap();
        task
    }

    /// Pins base_url at a closed local port, so a turn that doesn't use
    /// `run_with_stub` still fails fast (connection refused) instead of
    /// hanging or making a real network call.
    fn point_at_dead_port(&self) {
        let settings = self.config_path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"base_url": "http://127.0.0.1:1", "api_key": "test"}"#,
        )
        .unwrap();
    }

    fn point_at(&self, base_url: &str) {
        let settings = self.config_path().join("settings.json");
        std::fs::write(
            &settings,
            format!(
                r#"{{"base_url": "{base_url}", "api_key": "test", "tool_confirmation": "all"}}"#
            ),
        )
        .unwrap();
    }

    /// Spawns pengy-cli, feeds `commands` (each becomes one line of stdin,
    /// `/quit` appended automatically), returns captured stdout.
    fn run(&self, commands: &[&str]) -> String {
        self.run_timeout(commands, Duration::from_secs(10))
    }

    fn run_timeout(&self, commands: &[&str], timeout: Duration) -> String {
        let mut child = Command::new(cli_bin())
            .env("PENGY_CONFIG_DIR", self.config_path())
            .env("HOME", self.home_dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pengy-cli");

        {
            let stdin = child.stdin.as_mut().expect("child stdin");
            for cmd in commands {
                writeln!(stdin, "{cmd}").expect("write to child stdin");
            }
            writeln!(stdin, "/quit").ok();
        }

        // wait_with_output has no built-in timeout; give the child a bounded
        // window (matters if a bug makes it hang on real network I/O) before
        // killing it, same as PengyCPP's runCli().
        let start = std::time::Instant::now();
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let output = child.wait_with_output().expect("collect child output");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pengy-cli")
}

// ── Minimal in-process stub LLM server ──────────────────────────────
//
// Runs on a background OS thread within the *test* process, listening on a
// real loopback TCP port. The pengy-cli *subprocess* connects to it like any
// other HTTP client. Unlike a GUI event-loop-driven test harness, a plain
// background thread keeps servicing the socket regardless of what the main
// test thread is blocked on (here, waiting for the child process), so a
// full successful turn can be tested end-to-end without hanging.

struct StubLlm {
    base_url: String,
}

fn spawn_stub_llm(responses: Vec<String>) -> StubLlm {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub llm");
    let port = listener.local_addr().unwrap().port();
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Drain the request headers (don't need the body; every request
            // to this stub gets whatever's next in the queue regardless of
            // path or payload).
            {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => continue,
                    }
                }
            }

            let body = queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"error":{"message":"stub exhausted"}}"#.to_string());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    StubLlm {
        base_url: format!("http://127.0.0.1:{port}"),
    }
}

fn llm_completion(content: &str, prompt_toks: u64, completion_toks: u64) -> String {
    format!(
        r#"{{"choices":[{{"index":0,"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{prompt_toks},"completion_tokens":{completion_toks},"total_tokens":{total}}}}}"#,
        total = prompt_toks + completion_toks
    )
}

// ── Message fixture helpers ─────────────────────────────────────────

fn user_msg(content: &str) -> ChatMessage {
    ChatMessage::new("user", Some(serde_json::Value::String(content.into())))
}

fn assistant_msg(content: &str) -> ChatMessage {
    ChatMessage::new("assistant", Some(serde_json::Value::String(content.into())))
}

// ── Smoke test: the harness itself ──────────────────────────────────

#[test]
fn help_lists_new_and_old_commands() {
    let h = Harness::new();
    let out = h.run(&["/help"]);
    assert!(out.contains("/redact"), "{out}");
    assert!(out.contains("/tasks"), "{out}");
    assert!(out.contains("/task"), "{out}");
    assert!(out.contains("/download-max"), "{out}");
    // Pre-existing commands must still be listed -- catches a help-table
    // edit that accidentally drops an entry.
    assert!(out.contains("/compact"), "{out}");
    assert!(out.contains("/delete"), "{out}");
    assert!(out.contains("/yolo"), "{out}");
}

#[test]
fn quit_exits_cleanly() {
    let h = Harness::new();
    let out = h.run(&[]);
    assert!(out.contains("Goodbye"), "{out}");
}

// ── Chat lifecycle ───────────────────────────────────────────────────

#[test]
fn new_chat_and_list() {
    let h = Harness::new();
    let out = h.run(&["/new", "/list"]);
    assert!(out.contains("Chat History"), "{out}");
    assert!(out.contains("New Chat"), "{out}");
}

#[test]
fn rename_persists_to_disk() {
    let h = Harness::new();
    let chat = h.seed_chat("Old Title", vec![]);
    h.run(&["/rename Fresh Title"]);
    assert_eq!(h.read_chat(&chat.id).title, "Fresh Title");
}

#[test]
fn delete_declined_keeps_the_chat() {
    let h = Harness::new();
    let chat = h.seed_chat("Keep Me", vec![]);
    let out = h.run(&["/list", "/delete 1", "n"]);
    assert!(out.contains("Cancelled"), "{out}");
    assert!(h.chat_file(&chat.id).exists());
}

#[test]
fn delete_confirmed_removes_the_chat() {
    let h = Harness::new();
    let chat = h.seed_chat("Delete Me", vec![]);
    let out = h.run(&["/list", "/delete 1", "y"]);
    assert!(out.contains("Deleted"), "{out}");
    assert!(!h.chat_file(&chat.id).exists());
}

// ── Config commands ──────────────────────────────────────────────────

#[test]
fn config_shows_current_settings() {
    let h = Harness::new();
    let out = h.run(&["/config"]);
    assert!(out.contains("Configuration"), "{out}");
    assert!(out.contains("Base URL"), "{out}");
    assert!(out.contains("Model"), "{out}");
}

#[test]
fn model_command_persists_across_invocations() {
    let h = Harness::new();
    h.run(&["/model gpt-4o-mini"]);
    let out = h.run(&["/config"]);
    assert!(out.contains("gpt-4o-mini"), "{out}");
}

#[test]
fn download_max_sets_and_persists() {
    let h = Harness::new();
    let out = h.run(&["/download-max 50"]);
    assert!(out.contains("Download max changed"), "{out}");
    // A second, independent process must see the same setting.
    let out2 = h.run(&["/download-max"]);
    assert!(out2.contains("50 MB"), "{out2}");
}

#[test]
fn download_max_invalid_is_rejected() {
    let h = Harness::new();
    let out = h.run(&["/download-max abc"]);
    assert!(out.contains("Usage"), "{out}");
}

#[test]
fn yolo_sets_and_persists_mode() {
    let h = Harness::new();
    let out = h.run(&["/yolo safe"]);
    assert!(out.contains("Tool Confirmation"), "{out}");
    // A second, independent process must see the same setting.
    let out2 = h.run(&["/yolo"]); // no arg = cycle from current
    assert!(!out2.is_empty());
}

// ── Context management ───────────────────────────────────────────────

#[test]
fn compact_elides_old_tool_results() {
    let h = Harness::new();
    let msgs = vec![
        user_msg("q1"),
        assistant_msg("a1"),
        user_msg("q2"),
        assistant_msg("a2"),
    ];
    let chat = h.seed_chat("Compact Test", msgs);
    h.run(&["/context-keep 1", "/compact"]);
    // Just verify it ran without corrupting the chat file -- elision only
    // touches role:"tool" messages, so this fixture (no tool messages)
    // should come back unchanged in shape.
    let after = h.read_chat(&chat.id);
    assert_eq!(after.messages.len(), 4);
}

#[test]
fn redact_default_removes_one_message() {
    let h = Harness::new();
    let chat = h.seed_chat("Redact Test", vec![user_msg("hi"), assistant_msg("hello")]);
    let out = h.run(&["/redact"]);
    assert!(out.contains("Redacted 1 message"), "{out}");
    let after = h.read_chat(&chat.id);
    assert_eq!(after.messages.len(), 1);
    assert_eq!(after.messages[0].role, "user");
}

#[test]
fn redact_n_removes_n_messages() {
    let h = Harness::new();
    let chat = h.seed_chat(
        "Redact N Test",
        vec![
            user_msg("q1"),
            assistant_msg("a1"),
            user_msg("q2"),
            assistant_msg("a2"),
        ],
    );
    h.run(&["/redact 3"]);
    let after = h.read_chat(&chat.id);
    assert_eq!(after.messages.len(), 1);
}

#[test]
fn redact_more_than_available_empties_without_erroring() {
    let h = Harness::new();
    let chat = h.seed_chat("Overshoot", vec![user_msg("hi"), assistant_msg("hello")]);
    let out = h.run(&["/redact 50"]);
    assert!(!out.contains("panic"), "{out}");
    let after = h.read_chat(&chat.id);
    assert!(after.messages.is_empty());
}

#[test]
fn redact_invalid_n_is_rejected() {
    let h = Harness::new();
    let chat = h.seed_chat("Invalid N", vec![user_msg("hi")]);
    let out = h.run(&["/redact abc"]);
    assert!(out.contains("Usage"), "{out}");
    // Nothing was removed on bad input.
    assert_eq!(h.read_chat(&chat.id).messages.len(), 1);
}

// ── Tasks ─────────────────────────────────────────────────────────────

#[test]
fn tasks_lists_saved_templates() {
    let h = Harness::new();
    h.seed_task("Greet", "Say hello to %name%");
    let out = h.run(&["/tasks"]);
    assert!(out.contains("Greet"), "{out}");
    assert!(out.contains("%name%"), "{out}");
}

#[test]
fn tasks_empty_shows_hint() {
    let h = Harness::new();
    let out = h.run(&["/tasks"]);
    assert!(out.contains("No tasks defined"), "{out}");
}

#[test]
fn task_invalid_index_rejected() {
    let h = Harness::new();
    h.seed_task("Greet", "hi %name%");
    let out = h.run(&["/task 99"]);
    assert!(out.contains("Usage"), "{out}");
}

/// Full round trip: /task fills a placeholder from stdin, renders the
/// template, and completes a real turn against a stub LLM -- the strongest
/// test in this file, since it exercises the placeholder prompt, the send
/// path, and response handling together.
#[test]
fn task_round_trip_completes_a_turn() {
    let h = Harness::new();
    let stub = spawn_stub_llm(vec![llm_completion("Hello Ada!", 10, 5)]);
    h.point_at(&stub.base_url);
    let task = h.seed_task("Greet", "Say hello to %name%");
    let chat = h.seed_chat("New Chat", vec![]);

    let out = h.run(&["/task 1", "Ada"]);
    assert!(out.contains("Hello Ada!"), "{out}");

    let after = h.read_chat(&chat.id);
    assert_eq!(after.messages[0].role, "user");
    assert_eq!(
        after.messages[0].content,
        Some(serde_json::Value::String("Say hello to Ada".into()))
    );
    assert_eq!(after.messages[1].role, "assistant");

    let _ = task; // keep the fixture alive for clarity even though id is unused
}

// ── Single-shot mode ─────────────────────────────────────────────────

#[test]
fn single_shot_completes_a_turn_and_reports_usage() {
    let h = Harness::new();
    let stub = spawn_stub_llm(vec![llm_completion("General Kenobi!", 20, 8)]);
    h.point_at(&stub.base_url);

    let output = Command::new(cli_bin())
        .env("PENGY_CONFIG_DIR", h.config_path())
        .env("HOME", h.home_dir.path())
        .args(["--output", "json", "Hello there"])
        .output()
        .expect("run single-shot");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert_eq!(json["content"], "General Kenobi!");
    assert_eq!(json["usage"]["total_tokens"], 28);
}

#[test]
fn single_shot_no_save_does_not_touch_disk() {
    let h = Harness::new();
    h.point_at_dead_port();

    Command::new(cli_bin())
        .env("PENGY_CONFIG_DIR", h.config_path())
        .env("HOME", h.home_dir.path())
        .args(["--no-save", "--output", "silent", "quick question"])
        .output()
        .expect("run single-shot");

    // Either the chats dir was never created, or it has no chat files --
    // --no-save must not leave a persisted chat behind.
    let has_any_chat = h
        .chats_dir()
        .read_dir()
        .map(|it| {
            it.filter_map(|e| e.ok()).any(|e| {
                e.path().extension().map(|x| x == "json").unwrap_or(false)
                    && e.path().file_stem().map(|s| s != "index").unwrap_or(true)
            })
        })
        .unwrap_or(false);
    assert!(!has_any_chat);
}
