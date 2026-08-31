use pengy_core::chat_manager::{self, Chat, ChatMessage, ChatSummary};
use pengy_core::config::{self, Config};
use pengy_core::llm_client::{self, Confirmation, LlmEvent, ToolConfirmation};
use pengy_core::task_manager;
use pengy_core::tools;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use futures_util::stream::Stream;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use std::pin::Pin;
use std::time::Duration;

const EVENT_LOG_GRACE: Duration = Duration::from_secs(10 * 60);

/// Report a command-line usage error and exit 2, matching the other frontends.
fn arg_error(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    eprintln!("Try 'pengy-web --help' for more information.");
    std::process::exit(2);
}

/// Consume the value following a flag, or fail if it is missing.
fn require_value(args: &[String], i: &mut usize, flag: &str) -> String {
    *i += 1;
    match args.get(*i) {
        Some(v) => v.clone(),
        None => arg_error(&format!("option '{}' requires a value", flag)),
    }
}

fn parse_port(value: &str) -> u16 {
    match value.parse::<u16>() {
        Ok(p) if p > 0 => p,
        _ => arg_error(&format!("invalid port '{}' (expected 1-65535)", value)),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Handle --version / -v before anything else
    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("Pengy v{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let mut host = String::from("127.0.0.1");
    let mut port: u16 = 5000;
    let mut config_dir: Option<String> = None;
    let mut trusted_hosts: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                println!("Pengy Web — chat with LLMs from your browser");
                println!();
                println!("Usage: pengy-web [OPTIONS]");
                println!();
                println!("Options:");
                println!("  --port PORT        Bind port (default: 5000)");
                println!("  --host HOST        Bind host (default: 127.0.0.1). Pass");
                println!("                     --host 0.0.0.0 to expose beyond localhost —");
                println!("                     this app has no authentication and exposes");
                println!("                     run_bash/run_python tools, so only do this");
                println!("                     on a trusted network.");
                println!("  --trusted-host HOST  Public hostname this server is reached");
                println!("                     as when behind a reverse proxy (e.g.");
                println!("                     pengy.example). Repeatable. Needed only");
                println!("                     for a proxy in front of a loopback bind.");
                println!("  --config-dir PATH  Use a custom config directory.");
                println!("  -v, --version      Show version information and exit.");
                println!("  -h, --help         Show this help message and exit.");
                return;
            }
            "--host" => host = require_value(&args, &mut i, "--host"),
            "--port" => {
                let v = require_value(&args, &mut i, "--port");
                port = parse_port(&v);
            }
            "--trusted-host" => trusted_hosts.push(require_value(&args, &mut i, "--trusted-host")),
            "--config-dir" => config_dir = Some(require_value(&args, &mut i, "--config-dir")),
            other => {
                // Unrecognised flags used to be discarded silently, so a typo
                // like --prot 8080 started the server on defaults.
                if other.starts_with('-') {
                    arg_error(&format!("unknown option '{}'", other));
                }
                // Bare number: the legacy positional PORT, still accepted.
                port = parse_port(other);
            }
        }
        i += 1;
    }

    if let Some(ref dir) = config_dir {
        config::set_config_dir(dir);
    }

    let state = AppState::new();

    let app = Router::new()
        .route("/", get(index))
        .route("/chat/new", post(new_chat))
        .route("/chat/:chat_id", get(chat_view))
        .route("/chat/:chat_id/send", post(chat_send))
        .route("/chat/:chat_id/stream", get(chat_stream))
        .route("/chat/:chat_id/confirm", post(chat_confirm))
        .route("/chat/:chat_id/sudo", post(chat_sudo))
        .route("/chat/:chat_id/stop", post(chat_stop))
        .route("/chat/:chat_id/delete", post(chat_delete))
        .route("/chat/:chat_id/export", get(chat_export))
        .route("/chat/:chat_id/rename", post(chat_rename))
        .route("/chat/:chat_id/redact", post(chat_redact))
        .route("/chat/:chat_id/command", post(chat_command))
        .route("/tasks", get(list_tasks))
        .route("/tasks/render", post(render_task))
        .route("/settings", get(settings_get).post(settings_post))
        .route("/models", get(models_api))
        .route("/files", get(serve_file))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            OriginGuard {
                bound_host: host.clone(),
                trusted_hosts: trusted_hosts
                    .iter()
                    .filter(|h| !h.trim().is_empty())
                    .map(|h| host_only(h))
                    .collect(),
            },
            origin_guard,
        ));

    // Bind before announcing, so a failure reports the failure rather than
    // printing a success banner and then panicking.
    let addr = format!("{}:{}", host, port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind on {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    println!("Pengy Web UI running at http://{}", addr);
    if !is_loopback_host(&host_only(&host)) {
        println!(
            "  note: bound beyond loopback — Pengy Web has no auth of its own, \
             so put it behind a proxy or a VM boundary."
        );
    }

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server error: {}", e);
        std::process::exit(1);
    }
}

// ── Request origin guard ─────────────────────────────────────────
//
// Pengy Web has no authentication: anything that can reach it runs tools as
// the current user. Two browser-driven attacks defeat a loopback bind, since
// both are issued by the user's own browser:
//
//   CSRF          — a page on any origin auto-submits a form to 127.0.0.1 and
//                   rewrites settings (base_url, system_message, YOLO mode).
//   DNS rebinding — an attacker domain re-resolves to 127.0.0.1, so the
//                   browser treats it as same-origin and can *read* replies.
//
// Two cheap checks close both, with no tokens or sessions to thread through
// the templates.

#[derive(Clone)]
struct OriginGuard {
    bound_host: String,
    /// Extra hostnames accepted in Host *and* Origin, from --trusted-host.
    /// Required when running behind a reverse proxy on a loopback bind: nginx
    /// either forwards the public domain as Host (proxy_set_header Host $host)
    /// or forwards its own upstream address and leaves the browser's public
    /// Origin unmatched (nginx's default, Host $proxy_host). Naming the public
    /// hostname covers both.
    trusted_hosts: std::collections::HashSet<String>,
}

/// Strip any `:port` from a Host/Origin authority, keeping `[::1]` intact.
fn host_only(value: &str) -> String {
    let v = value.trim();
    if let Some(stripped) = v.strip_prefix('[') {
        // Bracketed IPv6 literal
        if let Some(end) = stripped.find(']') {
            return v[..end + 2].to_lowercase();
        }
    }
    // Only strip a trailing :port when it is unambiguous. A bare IPv6 literal
    // such as "::1" (from --host ::1) has many colons and no port.
    if v.matches(':').count() == 1 {
        return v[..v.find(':').unwrap()].to_lowercase();
    }
    v.to_lowercase()
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Authority portion of an Origin header (`http://evil.com:5000` → `evil.com:5000`).
fn origin_authority(origin: &str) -> &str {
    match origin.find("://") {
        Some(i) => &origin[i + 3..],
        None => origin,
    }
}

async fn origin_guard(
    State(guard): State<OriginGuard>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // 1. DNS rebinding: when bound to loopback the browser should only ever
    //    address us as localhost (or a --trusted-host name, for a proxy). An
    //    attacker-controlled name resolving to 127.0.0.1 arrives with that
    //    name in Host, so it fails here. Skipped
    //    when the operator explicitly bound a non-loopback address — they are
    //    fronting this with a proxy or VM boundary, and Host is then some
    //    arbitrary domain of their choosing.
    let host = host_only(host);
    if is_loopback_host(&host_only(&guard.bound_host))
        && !is_loopback_host(&host)
        && !guard.trusted_hosts.contains(&host)
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // 2. CSRF: accept an Origin matching the Host the request was actually
    //    sent to, or any --trusted-host (a proxy may forward its own upstream
    //    Host while the browser reports the public origin). An attacker page's
    //    Origin is its own, and never either of those. Origin is absent on
    //    non-browser clients (curl) and on same-origin GETs, so only enforce
    //    it when present.
    if !matches!(req.method().as_str(), "GET" | "HEAD" | "OPTIONS") {
        if let Some(origin) = req
            .headers()
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        {
            let origin_host = host_only(origin_authority(origin));
            if origin_host != host && !guard.trusted_hosts.contains(&origin_host) {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }

    Ok(next.run(req).await)
}

// ── App State ────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    workers: Arc<Mutex<HashMap<String, Arc<WebWorker>>>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

// ── WebWorker ────────────────────────────────────────────────────

struct WebWorker {
    /// Append-only log of every SSE event produced by this worker.  Multiple
    /// concurrent SSE streams can read from it, and reconnecting clients can
    /// resume from where they left off using ``Last-Event-ID``.
    events: Arc<Mutex<Vec<SseEvent>>>,
    /// Watch channel that ticks the current length of ``events`` so streams
    /// know when new events are available without polling.
    event_count_tx: tokio::sync::watch::Sender<usize>,
    /// Set when the worker emits a terminal event (final_response/error).
    done: Arc<AtomicBool>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<WorkerCommand>,
    cancel: Arc<AtomicBool>,
    sudo_state: Arc<(Mutex<Option<Option<String>>>, Condvar)>,
}

#[derive(Clone)]
enum SseEvent {
    ToolRequest {
        name: String,
        args: serde_json::Value,
        tool_call_id: String,
        safe_id: String,
        summary: String,
        auto_approved: bool,
    },
    ToolResult {
        tool_call_id: String,
        safe_id: String,
        name: String,
        content: String,
        declined: bool,
    },
    /// Narration the assistant emitted alongside its tool calls.  Persisted
    /// history renders it on reload, so it has to stream too or mid-turn
    /// commentary only shows up after a refresh.
    AssistantMessage {
        html: String,
    },
    FinalResponse {
        html: String,
        usage: llm_client::Usage,
        /// Running total across every turn in this chat, not just this one
        /// (see `chat_manager::add_usage`) -- the client's navbar badge shows
        /// this, since "how much context has this whole chat burned" is a
        /// more useful signal than the last turn alone.
        cumulative_usage: llm_client::Usage,
    },
    SudoRequest,
    QuestionRequest {
        name: String,
        args: serde_json::Value,
        questions: serde_json::Value,
        tool_call_id: String,
        safe_id: String,
    },
    QuestionResult {
        tool_call_id: String,
        safe_id: String,
        name: String,
        content: String,
    },
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_secs: f64,
        status_code: u16,
        message: String,
    },
    Error {
        message: String,
    },
}

enum WorkerCommand {
    Confirm {
        confirmed: bool,
        tool_call_id: String,
        yolo_turn: bool,
        answers: Option<Vec<String>>,
    },
}

fn safe_id(tool_call_id: &str) -> String {
    format!(
        "tc_{}",
        tool_call_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
    )
}
fn tool_summary(name: &str, args: &serde_json::Value) -> String {
    let Some(obj) = args.as_object() else { return String::new() };
    let secret = |key: &str| matches!(key, "password" | "passwd" | "api_key" | "apikey" | "token" | "access_token" | "refresh_token" | "authorization" | "secret" | "private_key");
    let val = |key: &str| -> String {
        if secret(key) { return "[redacted]".into(); }
        obj.get(key).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()).replace('\n', " ").trim().to_owned()).unwrap_or_default()
    };
    let mut s = match name {
        "read_file" | "write_file" | "replace_in_file" | "directory_tree" => val("path"),
        "read_multiple_files" => obj.get("paths").and_then(|v| v.as_array()).map(|v| format!("{} files", v.len())).unwrap_or_default(),
        "web_search" => val("query"),
        "fetch_url" => val("url"),
        "download_file" => { let f = val("filename"); if f.is_empty() { val("url") } else { f } },
        "run_bash" => val("command"),
        "run_python" => val("code"),
        "search_content" | "glob" => { let p = val("pattern"); let path = val("path"); if p.is_empty() { path } else if path.is_empty() { p } else { format!("{} in {}", p, path) } },
        "apply_changes" => obj.get("changes").and_then(|v| v.as_array()).map(|v| format!("{} files", v.len())).unwrap_or_default(),
        "ask_user_question" => obj.get("questions").and_then(|v| v.as_array()).map(|v| format!("{} questions", v.len())).unwrap_or_default(),
        _ => obj.iter().find_map(|(k, v)| if secret(k) { None } else { Some(v.as_str().unwrap_or("").replace('\n', " ")) }).unwrap_or_default(),
    };
    if s.chars().count() > 100 { s = format!("{}…", s.chars().take(97).collect::<String>().trim_end()); }
    s
}

fn sse_event_to_json(event: &SseEvent) -> String {
    match event {
        SseEvent::ToolRequest {
            name,
            args,
            tool_call_id,
            safe_id,
            summary,
            auto_approved,
        } => serde_json::json!({
            "type": "tool_request",
            "name": name,
            "args": args,
            "tool_call_id": tool_call_id,
            "safe_id": safe_id,
            "summary": summary,
            "auto_approved": auto_approved,
        })
        .to_string(),
        SseEvent::ToolResult {
            tool_call_id,
            safe_id,
            name,
            content,
            declined,
        } => serde_json::json!({
            "type": "tool_result",
            "tool_call_id": tool_call_id,
            "safe_id": safe_id,
            "name": name,
            "content": content,
            "declined": declined,
        })
        .to_string(),
        SseEvent::AssistantMessage { html } => serde_json::json!({
            "type": "assistant_message",
            "html": html,
        })
        .to_string(),
        SseEvent::FinalResponse {
            html,
            usage,
            cumulative_usage,
        } => serde_json::json!({
            "type": "final_response",
            "html": html,
            "usage": {
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": usage.total_tokens,
            },
            "cumulative_usage": {
                "prompt_tokens": cumulative_usage.prompt_tokens,
                "completion_tokens": cumulative_usage.completion_tokens,
                "total_tokens": cumulative_usage.total_tokens,
            },
        })
        .to_string(),
        SseEvent::SudoRequest => r#"{"type":"sudo_request"}"#.to_string(),
        SseEvent::QuestionRequest {
            name,
            args,
            questions,
            tool_call_id,
            safe_id,
        } => serde_json::json!({
            "type": "question_request",
            "name": name,
            "args": args,
            "questions": questions,
            "tool_call_id": tool_call_id,
            "safe_id": safe_id,
        })
        .to_string(),
        SseEvent::QuestionResult {
            tool_call_id,
            safe_id,
            name,
            content,
        } => serde_json::json!({
            "type": "question_result",
            "tool_call_id": tool_call_id,
            "safe_id": safe_id,
            "name": name,
            "content": content,
        })
        .to_string(),
        SseEvent::Retrying {
            attempt,
            max_attempts,
            delay_secs,
            status_code,
            message,
        } => serde_json::json!({
            "type": "retrying",
            "attempt": attempt,
            "max_attempts": max_attempts,
            "delay_secs": delay_secs,
            "status_code": status_code,
            "message": message,
        })
        .to_string(),
        SseEvent::Error { message } => {
            serde_json::json!({"type": "error", "message": message}).to_string()
        }
    }
}

impl WebWorker {
    fn start_with_messages(
        chat: Chat,
        config: Config,
        messages: Vec<ChatMessage>,
    ) -> Arc<Self> {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerCommand>();
        let cancel = Arc::new(AtomicBool::new(false));
        let sudo_state: Arc<(Mutex<Option<Option<String>>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        // Append-only event log shared by all SSE streams, with a watch channel
        // that notifies streams when new events are appended.
        let events = Arc::new(Mutex::new(Vec::new()));
        let (event_count_tx, _) = tokio::sync::watch::channel(0usize);
        let done = Arc::new(AtomicBool::new(false));

        let worker = Arc::new(Self {
            events: events.clone(),
            event_count_tx: event_count_tx.clone(),
            done: done.clone(),
            cmd_tx,
            cancel: cancel.clone(),
            sudo_state: sudo_state.clone(),
        });

        *tools::USER_AGENT.lock().unwrap() = config.user_agent.clone();
        *tools::TOOL_TIMEOUT.lock().unwrap() = config.tool_timeout;
        *tools::TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = config.tool_output_max_chars;
        *tools::DOWNLOAD_MAX_MB.lock().unwrap() = config.download_max_mb;
        *tools::IMAGE_MAX_DIMENSION.lock().unwrap() = config.image_max_dimension;
        *tools::IMAGE_MAX_MB.lock().unwrap() = config.image_max_mb;
        *tools::IMAGE_QUALITY.lock().unwrap() = config.image_quality;

        // Per-request tool context so concurrent chats don't share a sudo
        // provider or kill each other's subprocesses.
        let tool_ctx = std::sync::Arc::new(tools::ToolContext::new());
        {
            let events_sudo = events.clone();
            let event_count_tx_sudo = event_count_tx.clone();
            let sudo_state_provider = sudo_state.clone();
            tool_ctx.set_sudo_provider(Some(Box::new(move || {
                let count;
                {
                    let mut ev = events_sudo.lock().unwrap();
                    ev.push(SseEvent::SudoRequest);
                    count = ev.len();
                }
                let _ = event_count_tx_sudo.send(count);
                let (lock, cvar) = &*sudo_state_provider;
                let mut guard = lock.lock().unwrap();
                while guard.is_none() {
                    guard = cvar.wait(guard).unwrap();
                }
                guard.take().flatten()
            })));
        }

        let tc_mode = ToolConfirmation::from_str(&config.tool_confirmation);
        let mut chat = chat;

        tokio::spawn(async move {
            let messages = messages;
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let (confirm_tx, confirm_rx) = tokio::sync::mpsc::unbounded_channel();

            let bu = config.base_url.clone();
            let ak = config.api_key.clone();
            let md = config.model.clone();
            let re = config.reasoning_effort.clone();
            let pr = config.preserve_reasoning;
            let lt = config.llm_timeout;
            let cancel2 = cancel.clone();
            let ctx_for_task = tool_ctx.clone();

            tokio::spawn(async move {
                llm_client::chat(
                    &bu,
                    &ak,
                    &md,
                    messages,
                    tc_mode,
                    &re,
                    pr,
                    lt,
                    event_tx,
                    confirm_rx,
                    cancel2,
                    ctx_for_task,
                )
                .await;
            });

            let mut yolo_this_turn = false;

            let push_event = |event: SseEvent| {
                let count;
                {
                    let mut ev = events.lock().unwrap();
                    ev.push(event);
                    count = ev.len();
                }
                let _ = event_count_tx.send(count);
            };

            loop {
                match event_rx.recv().await {
                    Some(LlmEvent::AssistantToolCalls { message }) => {
                        yolo_this_turn = false;
                        let preamble = message
                            .content
                            .as_ref()
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_owned();
                        chat.messages.push(message);
                        chat_manager::save_chat_progress(&mut chat).ok();
                        if !preamble.is_empty() {
                            push_event(SseEvent::AssistantMessage {
                                html: render_markdown(&preamble),
                            });
                        }
                    }
                    Some(LlmEvent::Retrying {
                        attempt,
                        max_attempts,
                        delay_secs,
                        status_code,
                        message,
                    }) => {
                        push_event(SseEvent::Retrying {
                            attempt,
                            max_attempts,
                            delay_secs,
                            status_code,
                            message,
                        });
                    }
                    Some(LlmEvent::ToolRequest {
                        name,
                        args,
                        tool_call_id,
                    }) => {
                        let needs_confirm = tc_mode != ToolConfirmation::All
                            && !(tc_mode == ToolConfirmation::Safe
                                && tools::is_readonly_tool(&name))
                            && !yolo_this_turn;

                        let sid = safe_id(&tool_call_id);

                        push_event(SseEvent::ToolRequest {
                            name: name.clone(),
                            args: args.clone(),
                            tool_call_id: tool_call_id.clone(),
                            safe_id: sid,
                            summary: tool_summary(&name, &args),
                            auto_approved: !needs_confirm,
                        });

                        if needs_confirm {
                            match cmd_rx.recv().await {
                                Some(WorkerCommand::Confirm {
                                    confirmed,
                                    yolo_turn,
                                    ..
                                }) => {
                                    if yolo_turn {
                                        yolo_this_turn = true;
                                    }
                                    let _ = confirm_tx.send(Confirmation {
                                        tool_call_id,
                                        confirmed,
                                        yolo_turn,
                                        answers: None,
                                    });
                                }
                                None => break,
                            }
                        }
                    }
                    Some(LlmEvent::ToolResult {
                        tool_call_id,
                        name,
                        content,
                        declined,
                        ..
                    }) => {
                        let display = if content.len() > 3000 {
                            format!(
                                "{}\n... [truncated]",
                                truncate_on_char_boundary(&content, 3000)
                            )
                        } else {
                            content.clone()
                        };
                        chat.messages.push(ChatMessage {
                            role: "tool".into(),
                            content: Some(serde_json::Value::String(content)),
                            tool_calls: vec![],
                            tool_call_id: Some(tool_call_id.clone()),
                            reasoning_content: None,
                            reasoning: None,
                            reasoning_details: None,
                        });
                        chat_manager::save_chat_progress(&mut chat).ok();
                        push_event(SseEvent::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            safe_id: safe_id(&tool_call_id),
                            name,
                            content: display,
                            declined,
                        });
                    }
                    Some(LlmEvent::QuestionRequest {
                        name,
                        args,
                        tool_call_id,
                        questions,
                    }) => {
                        push_event(SseEvent::QuestionRequest {
                            name,
                            args,
                            questions: questions.clone(),
                            tool_call_id: tool_call_id.clone(),
                            safe_id: safe_id(&tool_call_id),
                        });

                        // Always wait for user answers (ask_user_question is always interactive)
                        match cmd_rx.recv().await {
                            Some(WorkerCommand::Confirm {
                                confirmed, answers, ..
                            }) => {
                                let _ = confirm_tx.send(Confirmation {
                                    tool_call_id,
                                    confirmed,
                                    yolo_turn: false,
                                    answers,
                                });
                            }
                            None => break,
                        }
                    }
                    Some(LlmEvent::QuestionResult {
                        tool_call_id,
                        name,
                        content,
                    }) => {
                        // The generator already has this on its own message
                        // list; persist it too, or the assistant tool_calls
                        // message above is left dangling in chat history.
                        chat.messages.push(ChatMessage {
                            role: "tool".into(),
                            content: Some(serde_json::Value::String(content.clone())),
                            tool_calls: vec![],
                            tool_call_id: Some(tool_call_id.clone()),
                            reasoning_content: None,
                            reasoning: None,
                            reasoning_details: None,
                        });
                        chat_manager::save_chat_progress(&mut chat).ok();
                        push_event(SseEvent::QuestionResult {
                            safe_id: safe_id(&tool_call_id),
                            tool_call_id,
                            name,
                            content,
                        });
                    }
                    Some(LlmEvent::FinalResponse {
                        content,
                        message,
                        usage,
                    }) => {
                        chat.messages.push(message.unwrap_or(ChatMessage {
                            role: "assistant".into(),
                            content: Some(serde_json::Value::String(content.clone())),
                            tool_calls: vec![],
                            tool_call_id: None,
                            reasoning_content: None,
                            reasoning: None,
                            reasoning_details: None,
                        }));
                        let cumulative_usage = chat_manager::add_usage(&mut chat, &usage);
                        push_event(SseEvent::FinalResponse {
                            html: render_markdown(&content),
                            usage,
                            cumulative_usage,
                        });
                        chat_manager::save_chat_progress(&mut chat).ok();
                        done.store(true, Ordering::Relaxed);
                        break;
                    }
                    None => {
                        push_event(SseEvent::Error {
                            message: "Chat ended unexpectedly".into(),
                        });
                        done.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }

            // Cancel and errors both leave the loop mid-turn, where the last
            // assistant message can hold tool_calls with no result behind them
            // (the API 400s on that next request).  Repair, then persist what
            // the turn got through.
            chat.messages = chat_manager::clean_dangling_tool_calls(&chat.messages);
            chat_manager::save_chat_progress(&mut chat).ok();

            tool_ctx.set_sudo_provider(None);
        });

        worker
    }
}

// ── Routes ───────────────────────────────────────────────────────

async fn index() -> impl IntoResponse {
    let chats = chat_manager::load_index();
    if !chats.is_empty() {
        Redirect::to(&format!("/chat/{}", chats[0].id))
    } else {
        let chat = chat_manager::create_chat("New Chat").unwrap();
        Redirect::to(&format!("/chat/{}", chat.id))
    }
}

async fn new_chat() -> impl IntoResponse {
    let chats = chat_manager::load_index();
    if let Some(first) = chats.first() {
        if first.title == "New Chat" && first.msg_count == 0 {
            return Redirect::to(&format!("/chat/{}", first.id));
        }
    }
    let chat = chat_manager::create_chat("New Chat").unwrap();
    Redirect::to(&format!("/chat/{}", chat.id))
}

async fn chat_view(
    Path(chat_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let chat = match chat_manager::get_chat(&chat_id) {
        Some(c) => c,
        None => return Redirect::to("/").into_response(),
    };
    // Sidebar summaries only -- no message bodies needed to render the list.
    let chats = chat_manager::load_index();
    let config = config::load_config();
    let turns = group_messages(&chat.messages);
    let has_active_worker = state
        .workers
        .lock()
        .unwrap()
        .get(&chat_id)
        .map(|w| !w.done.load(Ordering::Relaxed))
        .unwrap_or(false);

    Html(templates::chat_page(
        &chat,
        &chats,
        &config,
        &turns,
        has_active_worker,
    ))
    .into_response()
}

#[derive(Deserialize)]
struct SendRequest {
    content: Option<String>,
    files: Option<Vec<AttachedFile>>,
}

#[derive(Deserialize)]
struct AttachedFile {
    name: String,
    data: String,
}

async fn chat_send(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(data): Json<SendRequest>,
) -> impl IntoResponse {
    let content = data.content.unwrap_or_default().trim().to_string();
    let config = config::load_config();

    // Handle file attachments — detect images vs text
    let mut text_blocks = Vec::new();
    let mut image_parts: Vec<serde_json::Value> = Vec::new();
    let mut display_parts = Vec::new();

    if let Some(files) = &data.files {
        for f in files {
            let is_image = is_image_filename(&f.name);
            if is_image {
                if let Ok(decoded) = base64_decode(&f.data) {
                    // Write to temp file for preprocessing
                    let ext = std::path::Path::new(&f.name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("png");
                    let tmp = std::env::temp_dir().join(format!(
                        "pengy_web_{}.{}",
                        std::process::id(),
                        ext
                    ));
                    if std::fs::write(&tmp, &decoded).is_ok() {
                        if let Ok(result) = pengy_core::image_utils::preprocess(
                            &tmp,
                            config.image_max_dimension,
                            config.image_max_mb,
                            config.image_quality,
                        ) {
                            use base64::Engine;
                            let b64 =
                                base64::engine::general_purpose::STANDARD.encode(&result.bytes);
                            image_parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{};base64,{}", result.mime, b64)}
                            }));
                            display_parts.push(format!("[Image: {}]", f.name));
                        }
                        let _ = std::fs::remove_file(&tmp);
                    }
                }
            } else if let Ok(decoded) = base64_decode(&f.data) {
                if let Ok(text) = String::from_utf8(decoded) {
                    text_blocks.push(format!("[File: {}]\n```\n{}\n```", f.name, text));
                }
            }
        }
    }

    // Build display content and API message content
    let (display_content, api_user_content) = if !image_parts.is_empty() {
        if !text_blocks.is_empty() {
            display_parts.push(text_blocks.join("\n\n"));
        }
        if !content.is_empty() {
            display_parts.push(content.clone());
        }
        let mut parts = image_parts.clone();
        if !text_blocks.is_empty() || !content.is_empty() {
            let combined = if !text_blocks.is_empty() {
                format!("{}\n{}", text_blocks.join("\n\n"), content)
            } else {
                content.clone()
            };
            parts.push(serde_json::json!({"type": "text", "text": combined}));
        }
        (display_parts.join("\n"), serde_json::Value::Array(parts))
    } else if !text_blocks.is_empty() {
        let combined = format!("{}\n{}", text_blocks.join("\n\n"), content);
        (combined.clone(), serde_json::Value::String(combined))
    } else {
        (content.clone(), serde_json::Value::String(content.clone()))
    };

    if content.is_empty() && text_blocks.is_empty() && image_parts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Empty message"})),
        )
            .into_response();
    }

    let mut chat = match chat_manager::get_chat(&chat_id) {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Chat not found"})),
            )
                .into_response()
        }
    };

    {
        let mut workers = state.workers.lock().unwrap();
        if let Some(existing) = workers.remove(&chat_id) {
            existing.cancel.store(true, Ordering::Relaxed);
        }
    }

    chat.messages.push(ChatMessage {
        role: "user".into(),
        content: Some(serde_json::Value::String(display_content.clone())),
        tool_calls: vec![],
        tool_call_id: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_details: None,
    });

    if chat.title == "New Chat" {
        chat.title = if display_content.len() > 50 {
            format!("{}...", truncate_on_char_boundary(&display_content, 47))
        } else {
            display_content.clone()
        };
    }
    chat_manager::save_chat(&chat).ok();

    // Build API messages — the last user message gets the real multimodal content
    let mut api_messages = build_messages(&chat, &config);
    // Replace the last user message's content with the real API payload
    for msg in api_messages.iter_mut().rev() {
        if msg.role == "user" {
            msg.content = Some(api_user_content.clone());
            break;
        }
    }

    let config = config::load_config();
    let worker = WebWorker::start_with_messages(chat.clone(), config, api_messages);

    state
        .workers
        .lock()
        .unwrap()
        .insert(chat_id.clone(), worker.clone());

    // A completed worker remains replayable for a bounded grace period, then
    // persisted chat history is authoritative. This cleanup is independent of
    // whether an SSE client reconnects.
    let workers = state.workers.clone();
    let cleanup_chat_id = chat_id.clone();
    let cleanup_worker = worker.clone();
    tokio::spawn(async move {
        while !cleanup_worker.done.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        tokio::time::sleep(EVENT_LOG_GRACE).await;
        let mut workers = workers.lock().unwrap();
        if workers
            .get(&cleanup_chat_id)
            .is_some_and(|current| Arc::ptr_eq(current, &cleanup_worker))
        {
            workers.remove(&cleanup_chat_id);
        }
    });

    Json(serde_json::json!({"status": "ok", "title": chat.title})).into_response()
}

async fn chat_stream(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
) -> Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>> {
    // Browser sends Last-Event-ID on auto-reconnect. Resume right after it
    // so no messages are replayed twice.
    let (events, event_count_rx, done, start_index) = {
        let workers = state.workers.lock().unwrap();
        match workers.get(&chat_id) {
            Some(w) => {
                // Script-constructed EventSources cannot attach headers, so
                // accept its persisted cursor as `?after=`. Native automatic
                // reconnects continue to use Last-Event-ID.
                let last_id = query
                    .get("after")
                    .and_then(|v| v.parse::<usize>().ok())
                    .or_else(|| {
                        headers
                            .get("Last-Event-ID")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<usize>().ok())
                    });
                let start_index = match last_id {
                    Some(id) => id.saturating_add(1),
                    None => {
                        if w.done.load(Ordering::Relaxed) {
                            // Fresh connection to a finished worker: the chat
                            // page already rendered history server-side, so
                            // only replay the terminal event.
                            w.events.lock().unwrap().len().saturating_sub(1)
                        } else {
                            0
                        }
                    }
                };
                (
                    w.events.clone(),
                    w.event_count_tx.subscribe(),
                    w.done.clone(),
                    start_index,
                )
            }
            None => {
                let error_stream = futures_util::stream::once(async move {
                    Ok::<_, Infallible>(Event::default().data(
                        r#"{"type":"error","message":"No active task"}"#,
                    ))
                });
                return Sse::new(Box::pin(error_stream) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
                    .keep_alive(KeepAlive::default());
            }
        }
    };

    let stream = async_stream(start_index, events, event_count_rx, done);
    Sse::new(Box::pin(stream) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(KeepAlive::default())
}

fn async_stream(
    start_index: usize,
    events: Arc<Mutex<Vec<SseEvent>>>,
    event_count_rx: tokio::sync::watch::Receiver<usize>,
    done: Arc<AtomicBool>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    // Send retry:1000 first so browsers reconnect in 1s instead of default 3s
    let retry_event = Ok(Event::default().retry(std::time::Duration::from_millis(1000)));
    let retry_stream = futures_util::stream::once(async move { retry_event });

    let main_stream = futures_util::stream::unfold(
        (start_index, event_count_rx),
        move |(mut index, mut rx)| {
            let events = events.clone();
            let done = done.clone();
            async move {
                loop {
                    let current_count = *rx.borrow();
                    {
                        let events = events.lock().unwrap();
                        if index < events.len() && index < current_count {
                            let ev = &events[index];
                            let json = sse_event_to_json(ev);
                            let event_id = index;
                            index += 1;
                            return Some((
                                Ok(Event::default().id(event_id.to_string()).data(json)),
                                (index, rx),
                            ));
                        }
                    }
                    if done.load(Ordering::Relaxed) && index >= current_count {
                        return None;
                    }
                    // Wait for new events or emit a keepalive comment so
                    // proxies / mobile browsers don't drop the long-running SSE.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(25),
                        rx.changed(),
                    )
                    .await
                    {
                        Ok(Ok(_)) => continue,
                        Ok(Err(_)) => return None, // sender dropped
                        Err(_) => {
                            return Some((
                                Ok(Event::default().comment("keepalive")),
                                (index, rx),
                            ));
                        }
                    }
                }
            }
        },
    );

    retry_stream.chain(main_stream)
}

#[derive(Deserialize)]
struct ConfirmRequest {
    confirmed: Option<bool>,
    tool_call_id: Option<String>,
    yolo_turn: Option<bool>,
    answers: Option<Vec<String>>,
}

async fn chat_confirm(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(data): Json<ConfirmRequest>,
) -> impl IntoResponse {
    let worker = {
        let workers = state.workers.lock().unwrap();
        workers.get(&chat_id).cloned()
    };

    match worker {
        Some(w) => {
            let _ = w.cmd_tx.send(WorkerCommand::Confirm {
                confirmed: data.confirmed.unwrap_or(false),
                tool_call_id: data.tool_call_id.unwrap_or_default(),
                yolo_turn: data.yolo_turn.unwrap_or(false),
                answers: data.answers,
            });
            Json(serde_json::json!({"status": "ok"}))
        }
        None => Json(serde_json::json!({"error": "No active task"})),
    }
}

#[derive(Deserialize)]
struct SudoRequest {
    password: Option<String>,
}

async fn chat_sudo(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(data): Json<SudoRequest>,
) -> impl IntoResponse {
    let worker = {
        let workers = state.workers.lock().unwrap();
        workers.get(&chat_id).cloned()
    };

    match worker {
        Some(w) => {
            let (lock, cvar) = &*w.sudo_state;
            *lock.lock().unwrap() = Some(data.password);
            cvar.notify_one();
            Json(serde_json::json!({"status": "ok"}))
        }
        None => Json(serde_json::json!({"error": "No active task"})),
    }
}

async fn chat_delete(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
) -> impl IntoResponse {
    {
        let mut workers = state.workers.lock().unwrap();
        if let Some(w) = workers.remove(&chat_id) {
            w.cancel.store(true, Ordering::Relaxed);
        }
    }
    chat_manager::delete_chat(&chat_id).ok();
    Redirect::to("/")
}

async fn chat_stop(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
) -> impl IntoResponse {
    {
        let mut workers = state.workers.lock().unwrap();
        if let Some(w) = workers.remove(&chat_id) {
            w.cancel.store(true, Ordering::Relaxed);
        }
    }
    Json(serde_json::json!({"status": "stopped"}))
}

// ── NEW: Export ──────────────────────────────────────────────────

async fn chat_export(Path(chat_id): Path<String>) -> impl IntoResponse {
    let chat = match chat_manager::get_chat(&chat_id) {
        Some(c) => c,
        None => return (StatusCode::NOT_FOUND, "Chat not found").into_response(),
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# {}", chat.title));
    lines.push(format!(
        "*Exported {}*",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    lines.push(String::new());

    for msg in &chat.messages {
        let role = &msg.role;
        let content = msg
            .content
            .as_ref()
            .and_then(|v| {
                if v.is_string() {
                    v.as_str().map(String::from)
                } else if v.is_array() {
                    let parts: Vec<String> = v
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| {
                            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                                t.to_string()
                            } else if p.get("image_url").is_some() {
                                "[image]".to_string()
                            } else {
                                String::new()
                            }
                        })
                        .collect();
                    Some(parts.join(" "))
                } else {
                    Some(v.to_string())
                }
            })
            .unwrap_or_default();

        if role == "user" {
            lines.push("### 🧑 You".to_string());
            lines.push(content);
            lines.push(String::new());
        } else if role == "assistant" {
            if !msg.tool_calls.is_empty() {
                lines.push("### 🤖 Assistant (tool calls)".to_string());
                for tc in &msg.tool_calls {
                    lines.push(format!("- **{}**", tc.function.name));
                    lines.push(format!("  ```json\n  {}\n  ```", tc.function.arguments));
                }
                lines.push(String::new());
            }
            if !content.is_empty() {
                lines.push("### 🤖 Assistant".to_string());
                lines.push(content);
                lines.push(String::new());
            }
        } else if role == "tool" {
            let tc_id = msg.tool_call_id.as_deref().unwrap_or("?");
            lines.push(format!("#### 🔧 Tool result (`{}`)", tc_id));
            lines.push("```".to_string());
            lines.push(content);
            lines.push("```".to_string());
            lines.push(String::new());
        } else if role == "system" {
            let truncated = if content.len() > 200 {
                format!("{}...", &content[..200])
            } else {
                content
            };
            lines.push(format!("*System: {}*", truncated));
            lines.push(String::new());
        }
    }

    let safe_title: String = chat
        .title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe_title = safe_title.trim().chars().take(50).collect::<String>();
    let filename = if safe_title.is_empty() {
        "chat.md".to_string()
    } else {
        format!("{}.md", safe_title)
    };

    (
        StatusCode::OK,
        [
            ("Content-Type", "text/markdown; charset=utf-8"),
            (
                "Content-Disposition",
                &format!("attachment; filename=\"{}\"", filename),
            ),
        ],
        lines.join("\n"),
    )
        .into_response()
}

// ── NEW: Rename ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct RenameRequest {
    title: Option<String>,
}

async fn chat_rename(
    Path(chat_id): Path<String>,
    Json(data): Json<RenameRequest>,
) -> impl IntoResponse {
    let new_title = data.title.unwrap_or_default().trim().to_string();
    if new_title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Empty title"})),
        )
            .into_response();
    }

    let mut chat = match chat_manager::get_chat(&chat_id) {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Chat not found"})),
            )
                .into_response()
        }
    };

    chat.title = new_title.clone();
    chat_manager::save_chat(&chat).ok();
    Json(serde_json::json!({"status": "ok", "title": new_title})).into_response()
}

// ── Redact ─────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct RedactRequest {
    count: Option<serde_json::Value>,
}

/// Delete the last N raw messages (default 1) from the chat.
///
/// The context-pruning "undo" button: repeatable all the way to an empty
/// chat. Refused while a turn is in flight for this chat, since popping
/// messages out from under an in-progress LlmClient run (which holds its own
/// snapshot mid-run) would race with the worker's own saves.
async fn chat_redact(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    body: Option<Json<RedactRequest>>,
) -> impl IntoResponse {
    let active = {
        let workers = state.workers.lock().unwrap();
        workers
            .get(&chat_id)
            .map(|w| !w.done.load(Ordering::Relaxed))
            .unwrap_or(false)
    };
    if active {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Cannot redact while a response is in progress"})),
        )
            .into_response();
    }

    let count_value = body.and_then(|Json(b)| b.count).unwrap_or(serde_json::json!(1));
    let n = match count_value.as_u64().or_else(|| {
        count_value
            .as_i64()
            .filter(|v| *v >= 0)
            .map(|v| v as u64)
    }) {
        Some(n) if n >= 1 => n as usize,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "count must be an integer"})),
            )
                .into_response()
        }
    };

    let mut chat = match chat_manager::get_chat(&chat_id) {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Chat not found"})),
            )
                .into_response()
        }
    };

    let before = chat.messages.len();
    for _ in 0..n.min(before) {
        chat.messages = chat_manager::redact_last_message(&chat.messages);
    }
    chat_manager::save_chat(&chat).ok();

    Json(serde_json::json!({
        "status": "ok",
        "removed": before - chat.messages.len(),
        "message_count": chat.messages.len(),
    }))
    .into_response()
}

// ── Tasks (prompt templates) ──────────────────────────────────────
// Chat-independent: same store (tasks.json) as the GUI's Tasks dialog. The
// client fills placeholders, renders here, then feeds the result through the
// normal /send path.

async fn list_tasks() -> impl IntoResponse {
    let tasks: Vec<serde_json::Value> = task_manager::load_tasks()
        .into_iter()
        .map(|t| {
            let placeholders = task_manager::extract_placeholders(&t.template);
            serde_json::json!({
                "id": t.id,
                "title": t.title,
                "template": t.template,
                "placeholders": placeholders,
            })
        })
        .collect();
    Json(serde_json::json!({"tasks": tasks}))
}

#[derive(Deserialize)]
struct RenderTaskRequest {
    id: String,
    values: serde_json::Value,
}

async fn render_task(Json(data): Json<RenderTaskRequest>) -> impl IntoResponse {
    let task = match task_manager::get_task(&data.id) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Task not found"})),
            )
                .into_response()
        }
    };

    let values: std::collections::HashMap<String, String> = match data.values {
        serde_json::Value::Object(map) => map
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    v.as_str().map(String::from).unwrap_or_else(|| v.to_string()),
                )
            })
            .collect(),
        serde_json::Value::Null => std::collections::HashMap::new(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "values must be an object"})),
            )
                .into_response()
        }
    };

    let prompt = task_manager::render_template(&task.template, &values)
        .trim()
        .to_string();
    Json(serde_json::json!({"prompt": prompt})).into_response()
}

// ── NEW: Slash Commands ──────────────────────────────────────────

#[derive(Deserialize)]
struct CommandRequest {
    command: Option<String>,
}

async fn chat_command(
    Path(chat_id): Path<String>,
    Json(data): Json<CommandRequest>,
) -> impl IntoResponse {
    let cmd_text = data.command.unwrap_or_default().trim().to_string();
    if !cmd_text.starts_with('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Not a command"})),
        )
            .into_response();
    }

    let parts: Vec<&str> = cmd_text.split_whitespace().collect();
    let cmd = parts[0].to_lowercase();
    let args: Vec<&str> = parts[1..].to_vec();

    let mut config = config::load_config();

    if cmd == "/yolo" {
        let modes = ["none", "safe", "all"];
        let current = &config.tool_confirmation;
        let new_mode = if !args.is_empty() && modes.contains(&args[0]) {
            args[0].to_string()
        } else {
            let idx = modes.iter().position(|m| m == current).unwrap_or(2);
            modes[(idx + 1) % 3].to_string()
        };
        config.tool_confirmation = new_mode.clone();
        config::save_config(&config).ok();

        let label = match new_mode.as_str() {
            "all" => "YOLO",
            "safe" => "Safe",
            _ => "Confirm All",
        };
        return Json(serde_json::json!({
            "type": "config",
            "message": format!("Tool Confirmation: {}", label),
            "config": {
                "model": config.model,
                "tool_confirmation": new_mode,
            }
        }))
        .into_response();
    }

    if cmd == "/model" && !args.is_empty() {
        config.model = args[0].to_string();
        config::save_config(&config).ok();
        return Json(serde_json::json!({
            "type": "config",
            "message": format!("Model: {}", args[0]),
            "config": {
                "model": args[0],
                "tool_confirmation": config.tool_confirmation,
            }
        }))
        .into_response();
    }

    if cmd == "/new" {
        let chat = chat_manager::create_chat("New Chat").unwrap();
        return Json(serde_json::json!({
            "type": "redirect",
            "url": format!("/chat/{}", chat.id),
        }))
        .into_response();
    }

    if cmd == "/export" {
        return Json(serde_json::json!({
            "type": "redirect",
            "url": format!("/chat/{}/export", chat_id),
        }))
        .into_response();
    }

    if cmd == "/rename" && !args.is_empty() {
        let new_title = args.join(" ");
        if let Some(mut chat) = chat_manager::get_chat(&chat_id) {
            chat.title = new_title.clone();
            chat_manager::save_chat(&chat).ok();
            return Json(serde_json::json!({
                "type": "rename",
                "title": new_title,
            }))
            .into_response();
        }
    }

    if cmd == "/help" {
        return Json(serde_json::json!({
            "type": "message",
            "message": "Slash commands: /new /yolo [none|safe|all] /model <name> /rename <title> /export /help",
        }))
        .into_response();
    }

    Json(serde_json::json!({
        "type": "message",
        "message": format!("Unknown command: {}. Try /help.", cmd),
    }))
    .into_response()
}

// ── NEW: Fetch Models API ────────────────────────────────────────

async fn models_api() -> impl IntoResponse {
    let config = config::load_config();
    let base_url = config.base_url.trim_end_matches('/');
    let url = format!("{}/models", base_url);

    let client = reqwest::Client::new();
    match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("api-key", &config.api_key)
        .header("User-Agent", &config.user_agent)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => {
                let mut model_ids: Vec<String> = data["data"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                model_ids.sort();
                Json(serde_json::json!({"models": model_ids}))
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        },
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

// ── File serving for local images ───────────────────────────────────

async fn serve_file(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let raw_path = params.get("path").map(String::as_str).unwrap_or("");
    if raw_path.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing path parameter").into_response();
    }

    // Expand ~ and resolve symlinks
    let path = match std::path::Path::new(raw_path).canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    if !path.is_file() {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    }

    // Security: only serve files under allowed directories
    let home = match std::env::var_os("HOME") {
        Some(h) => std::path::PathBuf::from(h),
        None => return (StatusCode::FORBIDDEN, "Access denied").into_response(),
    };

    let allowed = [
        home.join("Pictures"),
        home.join("Downloads"),
        home.join("Desktop"),
        std::path::PathBuf::from("/tmp"),
    ];

    let is_allowed = allowed
        .iter()
        .any(|root| path == *root || path.starts_with(root));

    if !is_allowed {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }

    // Guess MIME type from extension
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        _ => "image/png", // default for unknown (most plots are PNG)
    };

    match tokio::fs::read(&path).await {
        Ok(data) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, mime)],
            data,
        )
            .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Cannot read file").into_response(),
    }
}

// ── Settings ─────────────────────────────────────────────────────

async fn settings_get() -> impl IntoResponse {
    let config = config::load_config();
    let chats = chat_manager::load_index();
    Html(templates::settings_page(&config, &chats, false))
}

#[derive(Deserialize)]
struct SettingsForm {
    base_url: Option<String>,
    model: Option<String>,
    system_message: Option<String>,
    user_agent: Option<String>,
    api_key: Option<String>,
    tool_confirmation: Option<String>,
    reasoning_effort: Option<String>,
    preserve_reasoning: Option<String>,
    llm_timeout: Option<String>,
    tool_timeout: Option<String>,
    tool_output_max_chars: Option<String>,
    download_max_mb: Option<String>,
    context_keep_turns: Option<String>,
}

async fn settings_post(Form(form): Form<SettingsForm>) -> impl IntoResponse {
    let mut config = config::load_config();

    if let Some(v) = form.base_url {
        config.base_url = v.trim().to_string();
    }
    if let Some(v) = form.model {
        config.model = v.trim().to_string();
    }
    if let Some(v) = form.system_message {
        config.system_message = v.trim().to_string();
    }
    if let Some(v) = form.user_agent {
        config.user_agent = v.trim().to_string();
    }
    if let Some(v) = &form.api_key {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            config.api_key = trimmed.to_string();
        }
    }
    if let Some(v) = &form.tool_confirmation {
        if ["all", "safe", "none"].contains(&v.as_str()) {
            config.tool_confirmation = v.clone();
        }
    }
    if let Some(v) = &form.reasoning_effort {
        if [
            "", "none", "minimal", "low", "medium", "high", "xhigh", "max",
        ]
        .contains(&v.as_str())
        {
            config.reasoning_effort = v.clone();
        }
    }
    config.preserve_reasoning = form.preserve_reasoning.is_some();
    if let Some(v) = &form.llm_timeout {
        if let Ok(n) = v.parse::<u64>() {
            config.llm_timeout = n.max(1);
        }
    }
    if let Some(v) = &form.tool_timeout {
        if let Ok(n) = v.parse::<u64>() {
            config.tool_timeout = n.max(1);
        }
    }
    if let Some(v) = &form.tool_output_max_chars {
        if let Ok(n) = v.parse::<usize>() {
            config.tool_output_max_chars = n;
        }
    }
    if let Some(v) = &form.download_max_mb {
        if let Ok(n) = v.parse::<u64>() {
            config.download_max_mb = n;
        }
    }
    if let Some(v) = &form.context_keep_turns {
        if let Ok(n) = v.parse::<usize>() {
            config.context_keep_turns = n;
        }
    }

    config::save_config(&config).ok();

    let chats = chat_manager::load_index();
    Html(templates::settings_page(&config, &chats, true))
}

// ── Base64 decode helper ─────────────────────────────────────────

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|e| e.to_string())
}

fn is_image_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
}

// ── Message grouping ─────────────────────────────────────────────

struct Turn {
    turn_type: TurnType,
}

enum TurnType {
    User { content: String },
    Assistant { html: String },
    ToolUse { events: Vec<ToolEvent> },
}

struct ToolEvent {
    name: String,
    args: serde_json::Value,
    tool_call_id: String,
    safe_id: String,
    summary: String,
    result: Option<String>,
    declined: bool,
}

fn group_messages(raw_messages: &[ChatMessage]) -> Vec<Turn> {
    let messages = chat_manager::clean_dangling_tool_calls(raw_messages);
    let mut turns = Vec::new();
    let mut i = 0;

    while i < messages.len() {
        let msg = &messages[i];

        if msg.role == "user" {
            let content = msg
                .content
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            turns.push(Turn {
                turn_type: TurnType::User { content },
            });
            i += 1;
        } else if msg.role == "assistant" {
            let content = msg
                .content
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !msg.tool_calls.is_empty() {
                if !content.is_empty() {
                    turns.push(Turn {
                        turn_type: TurnType::Assistant {
                            html: render_markdown(&content),
                        },
                    });
                }

                let mut events: Vec<ToolEvent> = msg
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        let args: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        ToolEvent {
                            name: tc.function.name.clone(),
                            args: args.clone(),
                            tool_call_id: tc.id.clone(),
                            safe_id: safe_id(&tc.id),
                            summary: tool_summary(&tc.function.name, &args),
                            result: None,
                            declined: false,
                        }
                    })
                    .collect();

                i += 1;
                while i < messages.len() && messages[i].role == "tool" {
                    let tc_id = messages[i]
                        .tool_call_id
                        .as_deref()
                        .unwrap_or("")
                        .to_string();
                    let result = messages[i]
                        .content
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    for ev in &mut events {
                        if ev.tool_call_id == tc_id {
                            ev.declined = result == "Tool execution was declined by user."
                                || result == "Tool execution was cancelled by user.";
                            ev.result = Some(result.clone());
                        }
                    }
                    i += 1;
                }

                turns.push(Turn {
                    turn_type: TurnType::ToolUse { events },
                });
            } else {
                if !content.is_empty() {
                    turns.push(Turn {
                        turn_type: TurnType::Assistant {
                            html: render_markdown(&content),
                        },
                    });
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    turns
}

// ── Markdown rendering ───────────────────────────────────────────

fn render_markdown(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut html = String::new();
    let mut in_code_block = false;
    let mut in_paragraph = false;
    let mut in_ul = false;
    let mut in_ol = false;
    let mut in_blockquote = false;
    let mut table_lines: Vec<String> = Vec::new();

    let close_lists = |in_ul: &mut bool, in_ol: &mut bool, html: &mut String| {
        if *in_ul {
            html.push_str("</ul>\n");
            *in_ul = false;
        }
        if *in_ol {
            html.push_str("</ol>\n");
            *in_ol = false;
        }
    };

    for line in text.lines() {
        if !table_lines.is_empty()
            && !in_code_block
            && !(line.trim().starts_with('|') && line.trim().ends_with('|'))
        {
            html.push_str(&render_table(&table_lines));
            table_lines.clear();
        }
        if line.starts_with("```") {
            if in_code_block {
                html.push_str("</code></pre>\n");
                in_code_block = false;
            } else {
                if in_paragraph {
                    html.push_str("</p>\n");
                    in_paragraph = false;
                }
                close_lists(&mut in_ul, &mut in_ol, &mut html);
                if in_blockquote {
                    html.push_str("</blockquote>\n");
                    in_blockquote = false;
                }
                html.push_str("<pre><code>");
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            html.push_str(&escape_html(line));
            html.push('\n');
            continue;
        }

        let trimmed = line.trim();

        if trimmed.is_empty() {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            close_lists(&mut in_ul, &mut in_ol, &mut html);
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            continue;
        }

        if trimmed.starts_with("### ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            close_lists(&mut in_ul, &mut in_ol, &mut html);
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            html.push_str(&format!("<h3>{}</h3>\n", inline_markdown(&trimmed[4..])));
        } else if trimmed.starts_with("## ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            close_lists(&mut in_ul, &mut in_ol, &mut html);
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            html.push_str(&format!("<h2>{}</h2>\n", inline_markdown(&trimmed[3..])));
        } else if trimmed.starts_with("# ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            close_lists(&mut in_ul, &mut in_ol, &mut html);
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            html.push_str(&format!("<h1>{}</h1>\n", inline_markdown(&trimmed[2..])));
        } else if trimmed.starts_with("> ") || trimmed == ">" {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            close_lists(&mut in_ul, &mut in_ol, &mut html);
            if !in_blockquote {
                html.push_str("<blockquote>\n");
                in_blockquote = true;
            }
            let content = trimmed.strip_prefix("> ").unwrap_or("");
            if !content.is_empty() {
                html.push_str(&format!("<p>{}</p>\n", inline_markdown(content)));
            }
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            if in_ol {
                html.push_str("</ol>\n");
                in_ol = false;
            }
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            if !in_ul {
                html.push_str("<ul>\n");
                in_ul = true;
            }
            html.push_str(&format!("<li>{}</li>\n", inline_markdown(&trimmed[2..])));
        } else if is_ordered_list_item(trimmed) {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            if in_ul {
                html.push_str("</ul>\n");
                in_ul = false;
            }
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            if !in_ol {
                html.push_str("<ol>\n");
                in_ol = true;
            }
            let content = trimmed.splitn(2, ". ").nth(1).unwrap_or("");
            html.push_str(&format!("<li>{}</li>\n", inline_markdown(content)));
        } else if trimmed.starts_with('|') && trimmed.ends_with('|') {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            close_lists(&mut in_ul, &mut in_ol, &mut html);
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            table_lines.push(trimmed.to_string());
            continue;
        } else {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            close_lists(&mut in_ul, &mut in_ol, &mut html);
            if in_blockquote {
                html.push_str("</blockquote>\n");
                in_blockquote = false;
            }
            if !in_paragraph {
                html.push_str("<p>");
                in_paragraph = true;
            } else {
                html.push_str("<br>");
            }
            html.push_str(&inline_markdown(trimmed));
            html.push('\n');
        }
    }

    if !table_lines.is_empty() {
        html.push_str(&render_table(&table_lines));
    }
    if in_code_block {
        html.push_str("</code></pre>\n");
    }
    if in_ul {
        html.push_str("</ul>\n");
    }
    if in_ol {
        html.push_str("</ol>\n");
    }
    if in_blockquote {
        html.push_str("</blockquote>\n");
    }
    if in_paragraph {
        html.push_str("</p>\n");
    }

    fix_file_urls(&html)
}

fn is_ordered_list_item(line: &str) -> bool {
    let dot_pos = match line.find(". ") {
        Some(p) => p,
        None => return false,
    };
    let prefix = &line[..dot_pos];
    !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit())
}

fn render_table(lines: &[String]) -> String {
    fn parse_row(line: &str) -> Vec<String> {
        let trimmed = line.trim().trim_matches('|');
        trimmed.split('|').map(|c| c.trim().to_string()).collect()
    }

    fn is_separator(line: &str) -> bool {
        let trimmed = line.trim().trim_matches('|');
        trimmed
            .split('|')
            .all(|c| c.trim().chars().all(|ch| ch == '-' || ch == ':') && !c.trim().is_empty())
    }

    if lines.is_empty() {
        return String::new();
    }

    let mut html = String::from("<table>\n");

    let has_header = lines.len() >= 2 && is_separator(&lines[1]);

    if has_header {
        let cells = parse_row(&lines[0]);
        html.push_str("<thead><tr>");
        for cell in &cells {
            html.push_str(&format!("<th>{}</th>", inline_markdown(cell)));
        }
        html.push_str("</tr></thead>\n");
    }

    let body_start = if has_header { 2 } else { 0 };
    if body_start < lines.len() {
        html.push_str("<tbody>\n");
        for line in &lines[body_start..] {
            if is_separator(line) {
                continue;
            }
            let cells = parse_row(line);
            html.push_str("<tr>");
            for cell in &cells {
                html.push_str(&format!("<td>{}</td>", inline_markdown(cell)));
            }
            html.push_str("</tr>\n");
        }
        html.push_str("</tbody>\n");
    }

    html.push_str("</table>\n");
    html
}

// Compiled once. `inline_markdown` runs for every heading, paragraph, list item
// and table cell, so building these per call recompiled five regexes thousands
// of times when rendering a long chat.
static IMG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"!\[([^\]]*)\]\(([^)]+)\)").unwrap());
static CODE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"`([^`]+)`").unwrap());
static BOLD_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\*\*([^*]+)\*\*").unwrap());
static ITALIC_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\*([^*]+)\*").unwrap());
static LINK_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());

fn inline_markdown(text: &str) -> String {
    let escaped = escape_html(text);
    let mut result = escaped;

    // Images — must come before link regex so ![alt](url) isn't partially matched
    result = IMG_RE
        .replace_all(&result, r#"<img src="$2" alt="$1">"#)
        .to_string();

    result = CODE_RE.replace_all(&result, "<code>$1</code>").to_string();

    result = BOLD_RE
        .replace_all(&result, "<strong>$1</strong>")
        .to_string();

    result = ITALIC_RE.replace_all(&result, "<em>$1</em>").to_string();

    result = LINK_RE
        .replace_all(&result, r#"<a href="$2">$1</a>"#)
        .to_string();

    result
}

/// Percent-encode a file path for use in a query string.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Replace ``file://`` image URLs with ``/files?path=`` URLs that browsers
/// can load.  Browsers block ``file://`` from HTTP pages as a security
/// restriction.
static FILE_URL_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"src="file://([^"]+)""#).unwrap());

fn fix_file_urls(html: &str) -> String {
    let re: &regex::Regex = &FILE_URL_RE;
    re.replace_all(html, |caps: &regex::Captures| {
        let path = &caps[1];
        // Expand ~ to the user's home directory
        let expanded = if let (Some(home), Some(suffix)) =
            (std::env::var_os("HOME"), path.strip_prefix('~'))
        {
            format!("{}{}", home.to_string_lossy(), suffix)
        } else {
            path.to_string()
        };
        let encoded = percent_encode_path(&expanded);
        format!(r#"src="/files?path={}""#, encoded)
    })
    .to_string()
}

fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

// ── Build messages ───────────────────────────────────────────────

fn build_messages(chat: &Chat, config: &Config) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if !config.system_message.is_empty() {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String(config::render_system_message(
                &config.system_message,
            ))),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        });
    }
    let raw = chat_manager::clean_dangling_tool_calls(&chat.messages);
    let raw = chat_manager::elide_old_tool_results(&raw, config.context_keep_turns);
    messages.extend(raw);
    messages
}

// ── HTML Templates ───────────────────────────────────────────────

mod templates {
    use super::*;

    fn base(
        title: &str,
        sidebar_chats: &str,
        navbar_center: &str,
        main: &str,
        extra_style: &str,
        scripts: &str,
    ) -> String {
        format!(
            r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0, viewport-fit=cover">
  <title>{title}</title>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bootstrap@5.3.3/dist/css/bootstrap.min.css">
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bootstrap-icons@1.11.3/font/bootstrap-icons.min.css">
  <style>
    html, body {{ height: 100%; }}
    .app-shell {{ display: flex; flex-direction: column; height: 100vh; height: 100dvh; }}
    .app-body {{ display: flex; flex: 1; min-height: 0; overflow: hidden; }}
    @media (min-width: 768px) {{
      body {{ display: flex; flex-direction: row; }}
      #sidebarOffcanvas {{
        position: relative !important; transform: none !important; visibility: visible !important;
        width: 260px !important; height: 100vh !important; height: 100dvh !important; flex-shrink: 0;
        border-right: 1px solid var(--bs-border-color); display: flex !important;
        flex-direction: column; z-index: auto !important; top: auto !important;
        bottom: auto !important; left: auto !important;
      }}
      #sidebarOffcanvas ~ .offcanvas-backdrop {{ display: none !important; }}
      .app-shell {{ flex: 1; min-width: 0; }}
      #sidebarOffcanvas .offcanvas-header {{ display: none; }}
      #sidebarOffcanvas .offcanvas-body {{ padding: 0; display: flex; flex-direction: column; overflow: hidden; }}
    }}
    #sidebarOffcanvas .offcanvas-header {{ border-bottom: 1px solid var(--bs-border-color); }}
    #sidebarOffcanvas .offcanvas-body {{ display: flex; flex-direction: column; padding: 0; overflow: hidden; }}
    .chat-list {{ overflow-y: auto; flex: 1; }}
    .chat-list-item {{ display: flex; align-items: center; padding: 0.5rem 0.75rem; border-radius: 6px; text-decoration: none; color: var(--bs-body-color); gap: 0.4rem; min-height: 44px; }}
    .chat-list-item:hover {{ background: var(--bs-secondary-bg); color: var(--bs-body-color); }}
    .chat-list-item.active {{ background: var(--bs-tertiary-bg); }}
    .chat-list-item .chat-title {{ overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 0.85rem; flex: 1; }}
    .chat-list-item .delete-btn {{ opacity: 0; border: none; background: none; color: var(--bs-secondary-color); padding: 0.25rem 0.35rem; font-size: 0.8rem; flex-shrink: 0; min-width: 28px; min-height: 28px; }}
    @media (hover: none) and (pointer: coarse) {{ .chat-list-item .delete-btn {{ opacity: 0.5; }} }}
    .chat-list-item:hover .delete-btn {{ opacity: 1; }}
    .chat-list-item .delete-btn:hover {{ color: var(--bs-danger); }}
    .messages-area {{ flex: 1; overflow-y: auto; -webkit-overflow-scrolling: touch; padding: 1rem 0.75rem; }}
    @media (min-width: 576px) {{ .messages-area {{ padding: 1.25rem 1rem; }} }}
    .msg-user {{ display: flex; justify-content: flex-end; margin-bottom: 0.85rem; }}
    .bubble-user {{ max-width: 85%; background: var(--bs-secondary-bg); border-radius: 18px 18px 4px 18px; padding: 0.6rem 1rem; white-space: pre-wrap; word-break: break-word; font-size: 0.9rem; }}
    .msg-assistant {{ display: flex; justify-content: flex-start; margin-bottom: 0.85rem; }}
    .bubble-assistant {{ max-width: 90%; background: var(--bs-tertiary-bg); border-radius: 4px 18px 18px 18px; padding: 0.75rem 1rem; font-size: 0.9rem; word-break: break-word; }}
    .msg-tool {{ margin-bottom: 0.5rem; }}
    .tool-card {{ background: var(--bs-body-bg); border: 1px solid var(--bs-border-color); border-left: 3px solid #f9e2af; border-radius: 6px; padding: 0.5rem 0.75rem; max-width: 90%; font-size: 0.82rem; }}
    .tool-card.declined {{ border-left-color: var(--bs-danger); }}
    .tool-card.done {{ border-left-color: var(--bs-success); }}
    .tool-header {{ cursor: pointer; user-select: none; display: flex; align-items: center; gap: 0.4rem; }}
    .tool-summary {{ min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; font-size: 0.78rem; }}
    .tool-args, .tool-output {{ background: #f6f8fa; border-radius: 4px; padding: 0.5rem 0.6rem; margin-top: 0.4rem; max-height: 250px; overflow-y: auto; white-space: pre-wrap; word-break: break-all; font-size: 0.78rem; color: #24292e; }}
    .msg-thinking {{ display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.85rem; color: var(--bs-secondary-color); font-size: 0.85rem; }}
    /* ── Jump to bottom ──────────────────────────────────────── */
    #jumpBottomBtn {{
      position: absolute;
      right: 1rem;
      top: -3rem;
      z-index: 5;
      border-radius: 999px;
      width: 2.5rem;
      height: 2.5rem;
      padding: 0;
      display: none;
      box-shadow: 0 2px 8px rgba(0,0,0,.25);
    }}
    #jumpBottomBtn.show {{ display: block; }}
    .input-area {{
      position: relative;
      padding: 0.6rem 0.75rem; border-top: 1px solid var(--bs-border-color); background: var(--bs-body-bg);
      padding-bottom: max(0.6rem, env(safe-area-inset-bottom, 0px)); }}
    @media (min-width: 576px) {{ .input-area {{ padding: 0.75rem 1rem; }} }}
    #messageInput {{ resize: none; max-height: 180px; overflow-y: auto; font-size: 16px; }}
    .markdown-body {{ line-height: 1.6; }}
    .markdown-body p {{ margin-bottom: 0.5rem; }}
    .markdown-body p:last-child {{ margin-bottom: 0; }}
    .markdown-body h1,.markdown-body h2,.markdown-body h3 {{ margin-top:0.75rem; margin-bottom:0.4rem; }}
    .markdown-body pre {{ background: #f6f8fa; border-radius: 4px; padding: 0.75rem; overflow-x: auto; }}
    .markdown-body code {{ font-size: 0.84em; }}
    .markdown-body :not(pre)>code {{ background: #f6f8fa; color: #d63384; padding: 0.1em 0.3em; border-radius: 3px; }}
    .markdown-body table {{ width:100%; border-collapse:collapse; margin-bottom:0.5rem; }}
    .markdown-body th,.markdown-body td {{ border:1px solid var(--bs-border-color); padding:0.25rem 0.5rem; }}
    .markdown-body ul,.markdown-body ol {{ padding-left:1.5rem; margin-bottom:0.5rem; }}
    .markdown-body blockquote {{ border-left: 3px solid var(--bs-border-color); margin: 0.5rem 0; padding-left: 0.75rem; color: var(--bs-secondary-color); }}
    .usage-line {{ font-size: 0.75rem; color: var(--bs-secondary-color); margin-top: 0.4rem; }}
    {extra_style}
  </style>
</head>
<body>
  <div class="offcanvas offcanvas-start" tabindex="-1" id="sidebarOffcanvas" aria-labelledby="sidebarTitle">
    <div class="offcanvas-header">
      <h6 class="offcanvas-title fw-bold mb-0" id="sidebarTitle">Pengy</h6>
      <button type="button" class="btn-close" data-bs-dismiss="offcanvas" aria-label="Close"></button>
    </div>
    <div class="offcanvas-body">
      <div class="p-2">
        <form action="/chat/new" method="post">
          <button type="submit" class="btn btn-outline-primary w-100" data-bs-dismiss="offcanvas">
            <i class="bi bi-plus-lg"></i> New Chat
          </button>
        </form>
      </div>
      <div class="chat-list px-2 pb-2">
        {sidebar_chats}
      </div>
      <div class="mt-auto border-top p-2">
        <a href="/settings" class="btn btn-outline-secondary w-100"
           onclick="dismissSidebar(event)">
          <i class="bi bi-gear me-1"></i> Settings
        </a>
      </div>
    </div>
  </div>
  <div class="app-shell">
    <nav class="navbar bg-body-tertiary border-bottom px-2 py-2 flex-shrink-0">
      <div class="d-flex align-items-center gap-1">
        <button class="btn btn-outline-secondary d-md-none" type="button" data-bs-toggle="offcanvas" data-bs-target="#sidebarOffcanvas" aria-label="Open sidebar">
          <i class="bi bi-list"></i>
        </button>
        <span class="fw-bold text-nowrap" style="cursor:pointer" title="Double-click to rename" id="navTitle">Pengy</span>
        <form action="/chat/new" method="post" class="d-md-none ms-1">
          <button type="submit" class="btn btn-outline-primary" title="New Chat" aria-label="New Chat">
            <i class="bi bi-plus-lg"></i>
          </button>
        </form>
      </div>
      <div class="d-flex align-items-center gap-1">
        {navbar_center}
        <a href="/settings" class="btn btn-outline-secondary" aria-label="Settings">
          <i class="bi bi-gear"></i><span class="d-none d-sm-inline ms-1">Settings</span>
        </a>
      </div>
    </nav>
    <div class="app-body">
      <div class="flex-grow-1 d-flex flex-column overflow-hidden">
        {main}
      </div>
    </div>
  </div>
  <script src="https://cdn.jsdelivr.net/npm/bootstrap@5.3.3/dist/js/bootstrap.bundle.min.js"></script>
  <script>
    function dismissSidebar(event) {{
      const offcanvas = document.getElementById('sidebarOffcanvas');
      if (!offcanvas) return;
      const bsOffcanvas = bootstrap.Offcanvas.getInstance(offcanvas);
      if (bsOffcanvas && offcanvas.classList.contains('show')) {{
        event.preventDefault();
        bsOffcanvas.hide();
        offcanvas.addEventListener('hidden.bs.offcanvas', function() {{
          window.location.href = event.currentTarget.href;
        }}, {{ once: true }});
      }}
    }}

    document.addEventListener('click', function(e) {{
      const link = e.target.closest('.chat-list-item');
      if (!link) return;
      const offcanvas = document.getElementById('sidebarOffcanvas');
      if (!offcanvas) return;
      const bsOffcanvas = bootstrap.Offcanvas.getInstance(offcanvas);
      if (bsOffcanvas && offcanvas.classList.contains('show')) {{ bsOffcanvas.hide(); }}
    }});
  </script>
  {scripts}
</body>
</html>"##
        )
    }

    fn render_sidebar_chats(chats: &[ChatSummary], active_id: &str) -> String {
        let mut html = String::new();
        for c in chats {
            let active_class = if c.id == active_id { " active" } else { "" };
            html.push_str(&format!(
                r##"<a href="/chat/{id}" class="chat-list-item{active_class}" data-chat-id="{id}">
  <i class="bi bi-chat-left-text small text-muted flex-shrink-0"></i>
  <span class="chat-title">{title}</span>
  <form action="/chat/{id}/delete" method="post" style="display:inline" onclick="event.stopPropagation()">
    <button type="submit" class="delete-btn" title="Delete chat" onclick="return confirm('Delete this chat?')">
      <i class="bi bi-x"></i>
    </button>
  </form>
</a>"##,
                id = c.id,
                title = escape_html(&c.title),
            ));
        }
        html
    }

    pub fn chat_page(
        chat: &Chat,
        chats: &[ChatSummary],
        config: &Config,
        turns: &[Turn],
        has_active_worker: bool,
    ) -> String {
        let sidebar = render_sidebar_chats(chats, &chat.id);

        let tc_badge = match config.tool_confirmation.as_str() {
            "all" => {
                r#"<span class="badge text-bg-warning small" id="navConfirmBadge">YOLO</span>"#
            }
            "safe" => r#"<span class="badge text-bg-info small" id="navConfirmBadge">Safe</span>"#,
            _ => {
                r#"<span class="badge text-bg-secondary small" id="navConfirmBadge">Confirm All</span>"#
            }
        };
        let tokens_badge = match &chat.usage {
            Some(u) if u.total_tokens > 0 => format!(
                r#"<span class="text-muted small d-none d-md-inline ms-2" id="navTokens" title="Cumulative token usage for this chat">{} tokens</span>"#,
                u.total_tokens
            ),
            _ => r#"<span class="text-muted small d-none d-md-inline ms-2" id="navTokens" title="Cumulative token usage for this chat" hidden>0 tokens</span>"#.to_string(),
        };
        let navbar_center = format!(
            r#"<span class="text-muted small d-none d-sm-inline" id="navModel">{}</span> {}
{}
<button class="btn btn-outline-secondary btn-sm ms-1" onclick="openTasks()" title="Run a saved prompt template">
  <i class="bi bi-list-task"></i>
</button>
<button class="btn btn-outline-secondary btn-sm ms-1" onclick="exportChat()" title="Export chat as Markdown">
  <i class="bi bi-download"></i>
</button>
<button class="btn btn-outline-secondary btn-sm ms-1" onclick="redactLast()" title="Redact last message — delete the last message from context (repeatable)">
  <i class="bi bi-eraser"></i>
</button>"#,
            escape_html(&config.model),
            tc_badge,
            tokens_badge
        );

        let mut messages_html = String::new();

        if turns.is_empty() {
            messages_html.push_str(
                r#"<div class="text-center text-muted py-5">
  <div style="font-size:2.5rem">&#x1F427;</div>
  <div class="mt-2">Start a conversation</div>
</div>"#,
            );
        }

        for turn in turns {
            match &turn.turn_type {
                TurnType::User { content } => {
                    messages_html.push_str(&format!(
                        r#"<div class="msg-user"><div class="bubble-user">{}</div></div>"#,
                        escape_html(content)
                    ));
                }
                TurnType::Assistant { html } => {
                    messages_html.push_str(&format!(
                        r#"<div class="msg-assistant"><div class="bubble-assistant"><div class="markdown-body">{}</div></div></div>"#,
                        html
                    ));
                }
                TurnType::ToolUse { events } => {
                    messages_html.push_str(r#"<div class="msg-tool">"#);
                    for ev in events {
                        let status_class = if ev.declined {
                            "declined"
                        } else if ev.result.is_some() {
                            "done"
                        } else {
                            ""
                        };
                        let badge = if ev.declined {
                            r#"<span class="badge bg-danger ms-1">declined</span>"#
                        } else if ev.result.is_some() {
                            r#"<span class="badge bg-success ms-1">done</span>"#
                        } else {
                            r#"<span class="badge bg-secondary ms-1">?</span>"#
                        };

                        let args_str = serde_json::to_string_pretty(&ev.args).unwrap_or_default();

                        let result_html = match &ev.result {
                            Some(r) if ev.declined => {
                                r#"<div class="text-danger small mt-1 ps-1">Declined by user</div>"#
                                    .to_string()
                            }
                            Some(r) => {
                                let display = if r.len() > 3000 {
                                    format!("{}...", truncate_on_char_boundary(r, 3000))
                                } else {
                                    r.clone()
                                };
                                format!(
                                    r#"<pre class="tool-output">{}</pre>"#,
                                    escape_html(&display)
                                )
                            }
                            None => String::new(),
                        };

                        messages_html.push_str(&format!(
                            r##"<div class="tool-card mb-1 {status_class}" id="{safe_id}">
  <div class="tool-header" data-bs-toggle="collapse" data-bs-target="#body-{safe_id}">
    <i class="bi bi-gear-fill text-warning" style="font-size:0.8rem"></i>
    <code class="fw-semibold text-warning">{name}</code>
    {summary}
    {badge}
    <i class="bi bi-chevron-down ms-auto" style="font-size:0.7rem"></i>
  </div>
  <div class="collapse" id="body-{safe_id}">
    <pre class="tool-args">{args}</pre>
    {result_html}
  </div>
</div>"##,
                            safe_id = ev.safe_id,
                            name = escape_html(&ev.name),
                            summary = if ev.summary.is_empty() { String::new() } else { format!(r#"<span class="tool-summary text-muted" title="{}">{}</span>"#, escape_html(&ev.summary), escape_html(&ev.summary)) },
                            args = escape_html(&args_str),
                        ));
                    }
                    messages_html.push_str("</div>");
                }
            }
        }

        let chat_id_json = serde_json::to_string(&chat.id).unwrap_or_default();
        let chat_title_json = serde_json::to_string(&chat.title).unwrap_or_default();

        let main_content = format!(
            r##"<div class="messages-area" id="messagesArea">{messages_html}</div>
<div class="input-area">
  <button type="button" id="jumpBottomBtn" class="btn btn-secondary"
          title="Jump to latest" aria-label="Jump to latest">
    <i class="bi bi-arrow-down"></i>
  </button>
  <form id="messageForm" class="d-flex gap-2" novalidate>
    <input type="file" id="fileInput" style="display:none" multiple onchange="handleFiles(this.files)">
    <button type="button" id="attachBtn" class="btn btn-outline-secondary align-self-end"
            title="Attach files" onclick="document.getElementById('fileInput').click()">
      <i class="bi bi-paperclip"></i>
    </button>
    <textarea id="messageInput" class="form-control" rows="1" placeholder="Message... (Enter to send, Shift+Enter for newline, / for commands)" autocomplete="off" autofocus></textarea>
    <button type="submit" id="sendBtn" class="btn btn-primary align-self-end">
      <i class="bi bi-send-fill"></i>
    </button>
    <button type="button" id="stopBtn" class="btn btn-danger align-self-end d-none">
      <i class="bi bi-stop-fill"></i>
    </button>
  </form>
  <div id="filePreview" class="mt-1 d-none">
    <small class="text-muted">Attached: <span id="fileNames"></span>
      <a href="#" onclick="clearFiles()" class="text-danger ms-1">clear</a></small>
  </div>
</div>
<div class="modal fade" id="confirmModal" tabindex="-1" data-bs-backdrop="static">
  <div class="modal-dialog modal-lg">
    <div class="modal-content">
      <div class="modal-header">
        <h6 class="modal-title">
          <i class="bi bi-gear-fill text-warning me-2"></i>
          Tool Request: <code id="confirmToolName"></code>
        </h6>
        <button type="button" class="btn-close" onclick="confirmTool(false)" aria-label="Decline and close"></button>
      </div>
      <div class="modal-body">
        <pre id="confirmToolArgs" class="tool-args" style="max-height:300px"></pre>
      </div>
      <div class="modal-footer">
        <button class="btn btn-sm btn-outline-danger" onclick="confirmTool(false)">
          <i class="bi bi-x-circle me-1"></i>Decline
        </button>
        <button class="btn btn-sm btn-outline-warning" onclick="confirmTool(true, true)">
          <i class="bi bi-lightning-fill me-1"></i>Yes to All This Turn
        </button>
        <button class="btn btn-sm btn-success" onclick="confirmTool(true)">
          <i class="bi bi-check-circle me-1"></i>Execute
        </button>
      </div>
    </div>
  </div>
</div>
<div class="modal fade" id="sudoModal" tabindex="-1" data-bs-backdrop="static">
  <div class="modal-dialog modal-sm">
    <div class="modal-content">
      <div class="modal-header">
        <h6 class="modal-title">
          <i class="bi bi-shield-lock text-warning me-2"></i>sudo password
        </h6>
      </div>
      <div class="modal-body">
        <input type="password" id="sudoPasswordInput" class="form-control" placeholder="Enter password...">
      </div>
      <div class="modal-footer">
        <button class="btn btn-sm btn-outline-secondary" onclick="submitSudo(null)">Cancel</button>
        <button class="btn btn-sm btn-warning" onclick="submitSudo()">Submit</button>
      </div>
    </div>
  </div>
</div>
<div class="modal fade" id="renameModal" tabindex="-1">
  <div class="modal-dialog modal-sm">
    <div class="modal-content">
      <div class="modal-header">
        <h6 class="modal-title">Rename Chat</h6>
      </div>
      <div class="modal-body">
        <input type="text" id="renameInput" class="form-control" value="" placeholder="Chat title...">
      </div>
      <div class="modal-footer">
        <button class="btn btn-sm btn-outline-secondary" data-bs-dismiss="modal">Cancel</button>
        <button class="btn btn-sm btn-primary" onclick="doRename()">Rename</button>
      </div>
    </div>
  </div>
</div>
<div class="modal fade" id="tasksModal" tabindex="-1">
  <div class="modal-dialog">
    <div class="modal-content">
      <div class="modal-header">
        <h6 class="modal-title">Tasks</h6>
        <button type="button" class="btn-close" data-bs-dismiss="modal" aria-label="Close"></button>
      </div>
      <div class="modal-body">
        <div id="tasksListView">
          <p class="text-muted small" id="tasksEmptyHint" hidden>
            No tasks saved yet — create one in the desktop app's Tasks dialog
            (they're shared across all three frontends).
          </p>
          <div id="tasksList" class="list-group"></div>
        </div>
        <div id="tasksFormView" hidden>
          <button type="button" class="btn btn-sm btn-link ps-0 mb-2" onclick="showTasksList()">&larr; Back to Tasks</button>
          <h6 id="tasksFormTitle" class="mb-3"></h6>
          <div id="tasksFormFields"></div>
        </div>
      </div>
      <div class="modal-footer">
        <button class="btn btn-sm btn-outline-secondary" data-bs-dismiss="modal">Cancel</button>
        <button id="tasksRunBtn" class="btn btn-sm btn-primary" onclick="runSelectedTask()" hidden>Run</button>
      </div>
    </div>
  </div>
</div>
<div class="modal fade" id="questionModal" tabindex="-1" data-bs-backdrop="static">
  <div class="modal-dialog modal-lg">
    <div class="modal-content">
      <div class="modal-header">
        <h6 class="modal-title">
          <i class="bi bi-question-circle text-info me-2"></i>
          Question
        </h6>
      </div>
      <div class="modal-body" id="questionModalBody"></div>
      <div class="modal-footer">
        <button class="btn btn-sm btn-outline-danger" onclick="submitQuestion(null)">
          <i class="bi bi-x-circle me-1"></i>Cancel
        </button>
        <button class="btn btn-sm btn-success" onclick="submitQuestion()">
          <i class="bi bi-check-circle me-1"></i>Submit Answers
        </button>
      </div>
    </div>
  </div>
</div>"##
        );

        let scripts = format!(
            r##"<script>
const CHAT_ID = {chat_id_json};
const CHAT_TITLE = {chat_title_json};
const HAS_ACTIVE_WORKER = {has_active_worker};
let isProcessing = false;
let eventSource = null;
let sseCursor = sessionStorage.getItem('pengy_sse_cursor_' + CHAT_ID) || '';
let pendingToolCallId = null;
let pendingQuestionId = null;
let pendingQuestions = [];
let thinkingEl = null;
let confirmModal, sudoModal, renameModal, questionModal, tasksModal;
let tasksCache = [];
let selectedTask = null;
let pendingFiles = [];
let wakeLock = null;

document.addEventListener('DOMContentLoaded', () => {{
  scrollToBottom();
  confirmModal = new bootstrap.Modal(document.getElementById('confirmModal'));
  sudoModal    = new bootstrap.Modal(document.getElementById('sudoModal'));
  renameModal  = new bootstrap.Modal(document.getElementById('renameModal'));
  questionModal = new bootstrap.Modal(document.getElementById('questionModal'));
  tasksModal   = new bootstrap.Modal(document.getElementById('tasksModal'));
  document.title = CHAT_TITLE + ' — Pengy';
  document.getElementById('navTitle').textContent = 'Pengy';
  document.getElementById('sudoPasswordInput').addEventListener('keydown', e => {{
    if (e.key === 'Enter') submitSudo();
  }});
  document.getElementById('renameInput').addEventListener('keydown', e => {{
    if (e.key === 'Enter') doRename();
  }});
  document.getElementById('stopBtn').addEventListener('click', stopGeneration);
  document.getElementById('jumpBottomBtn').addEventListener('click', scrollToBottom);
  document.getElementById('messagesArea').addEventListener('scroll', updateJumpBtn, {{ passive: true }});
  document.getElementById('navTitle').addEventListener('dblclick', showRename);

  // ── Mobile resilience: visibility & bfcache ───────────────────
  document.addEventListener('visibilitychange', () => {{
    if (document.visibilityState === 'visible' && isProcessing) {{
      acquireWakeLock();
      // CONNECTING is EventSource's normal automatic reconnect state.
      if (!eventSource || eventSource.readyState === EventSource.CLOSED) {{
        openSSE();
      }}
    }}
  }});
  window.addEventListener('pageshow', (event) => {{
    if (event.persisted && isProcessing) {{
      acquireWakeLock();
      // CONNECTING is EventSource's normal automatic reconnect state.
      if (!eventSource || eventSource.readyState === EventSource.CLOSED) {{
        openSSE();
      }}
    }}
  }});
  const draft = sessionStorage.getItem('pengy_draft_' + CHAT_ID);
  if (draft) {{
    document.getElementById('messageInput').value = draft;
    sessionStorage.removeItem('pengy_draft_' + CHAT_ID);
  }}

  // Auto-resume SSE if page loaded with active worker
  if (HAS_ACTIVE_WORKER) {{
    setProcessing(true);
    showThinking();
    openSSE();
  }}
}});

function scrollToBottom() {{
  const area = document.getElementById('messagesArea');
  area.scrollTop = area.scrollHeight;
  updateJumpBtn();
}}

// ── Sticky scroll: only follow new content if user was at bottom ─
const NEAR_BOTTOM_PX = 60;

function isNearBottom() {{
  const area = document.getElementById('messagesArea');
  return area.scrollHeight - area.scrollTop - area.clientHeight <= NEAR_BOTTOM_PX;
}}

function updateJumpBtn() {{
  const btn = document.getElementById('jumpBottomBtn');
  if (btn) btn.classList.toggle('show', !isNearBottom());
}}

function appendToArea(el) {{
  const pinned = isNearBottom();
  document.getElementById('messagesArea').appendChild(el);
  if (pinned) scrollToBottom(); else updateJumpBtn();
}}

function escHtml(text) {{
  const d = document.createElement('div');
  d.textContent = text;
  return d.innerHTML;
}}

// escHtml is safe for element content but leaves quotes intact, which would
// let model-supplied text break out of an attribute. Use this inside "...".
function escAttr(text) {{
  return escHtml(text).replace(/"/g, '&quot;');
}}

function safeId(toolCallId) {{
  return 'tc_' + toolCallId.replace(/[^a-zA-Z0-9]/g, '');
}}

const SUMMARY_SECRET_KEYS = new Set(['password','passwd','api_key','apikey','token','access_token','refresh_token','authorization','secret','private_key']);
function toolSummary(name, args) {{
  if (!args || typeof args !== 'object' || Array.isArray(args)) return '';
  const val = key => SUMMARY_SECRET_KEYS.has(key.toLowerCase()) ? '[redacted]' : (typeof args[key] === 'string' ? args[key].replace(/\n/g, ' ').trim() : args[key] == null ? '' : String(args[key]));
  let s = '';
  if (['read_file','write_file','replace_in_file','directory_tree'].includes(name)) s = val('path');
  else if (name === 'read_multiple_files') s = Array.isArray(args.paths) ? `${{args.paths.length}} files` : '';
  else if (name === 'web_search') s = val('query'); else if (name === 'fetch_url') s = val('url');
  else if (name === 'download_file') s = val('filename') || val('url'); else if (name === 'run_bash') s = val('command'); else if (name === 'run_python') s = val('code');
  else if (['search_content','glob'].includes(name)) s = val('pattern') + (val('path') ? ` in ${{val('path')}}` : '');
  else if (name === 'apply_changes') s = Array.isArray(args.changes) ? `${{args.changes.length}} files` : '';
  else if (name === 'ask_user_question') s = Array.isArray(args.questions) ? `${{args.questions.length}} questions` : '';
  return s.length <= 100 ? s : s.slice(0, 97).trimEnd() + '…';
}}

function setProcessing(val) {{
  isProcessing = val;
  document.getElementById('messageInput').disabled = val;
  document.getElementById('sendBtn').disabled = val;
  document.getElementById('stopBtn').classList.toggle('d-none', !val);
  if (val) {{
    acquireWakeLock();
  }} else {{
    releaseWakeLock();
  }}
  if (!val) document.getElementById('messageInput').focus();
}}

function handleFiles(files) {{
  for (const f of files) {{
    const reader = new FileReader();
    reader.onload = (e) => {{
      const base64 = e.target.result.split(',')[1];
      const mimeMatch = e.target.result.match(/^data:([^;]+);/);
      const mime = mimeMatch ? mimeMatch[1] : f.type || '';
      pendingFiles.push({{name: f.name, data: base64, mime: mime}});
      showFilePreview();
    }};
    reader.readAsDataURL(f);
  }}
  document.getElementById('fileInput').value = '';
}}

function showFilePreview() {{
  const names = pendingFiles.map(f => f.name).join(', ');
  document.getElementById('fileNames').textContent = names;
  document.getElementById('filePreview').classList.remove('d-none');
  document.getElementById('attachBtn').classList.add('active');
}}

function clearFiles() {{
  pendingFiles = [];
  document.getElementById('filePreview').classList.add('d-none');
  document.getElementById('attachBtn').classList.remove('active');
}}

function stopGeneration() {{
  if (confirmModal._isShown) {{
    confirmModal.hide();
    if (pendingToolCallId) {{
      fetch(`/chat/${{CHAT_ID}}/confirm`, {{
        method: 'POST',
        headers: {{'Content-Type': 'application/json'}},
        body: JSON.stringify({{confirmed: false, tool_call_id: pendingToolCallId, yolo_turn: false}}),
      }});
      pendingToolCallId = null;
    }}
  }}
  // The worker blocks on the answer channel — unblock it before stopping.
  if (questionModal._isShown) {{
    submitQuestion(null);
  }}
  if (sudoModal._isShown) {{
    sudoModal.hide();
    fetch(`/chat/${{CHAT_ID}}/sudo`, {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{password: null}}),
    }});
  }}
  fetch(`/chat/${{CHAT_ID}}/stop`, {{ method: 'POST' }})
    .catch(err => console.error('Stop error:', err));
  if (eventSource) {{ eventSource.close(); eventSource = null; }}
  hideThinking();
  setProcessing(false);
}}

function showThinking() {{
  hideThinking();
  thinkingEl = document.createElement('div');
  thinkingEl.className = 'msg-thinking';
  thinkingEl.innerHTML = '<div class="spinner-border spinner-border-sm" role="status"></div><span>Thinking...</span>';
  appendToArea(thinkingEl);
}}

function hideThinking() {{
  if (thinkingEl) {{ thinkingEl.remove(); thinkingEl = null; }}
}}

function showRetrying(data) {{
  hideThinking();
  thinkingEl = document.createElement('div');
  thinkingEl.className = 'msg-thinking';
  const delay = (data.delay_secs != null ? data.delay_secs : 0);
  thinkingEl.innerHTML = '<div class="spinner-border spinner-border-sm" role="status"></div>' +
    `<span>Overloaded — retrying in ${{delay.toFixed(1)}}s (${{data.attempt}}/${{data.max_attempts}})</span>`;
  appendToArea(thinkingEl);
}}

function exportChat() {{
  window.open(`/chat/${{CHAT_ID}}/export`, '_blank');
}}

function showRename() {{
  document.getElementById('renameInput').value = document.title.replace(' — Pengy', '');
  renameModal.show();
  setTimeout(() => document.getElementById('renameInput').focus(), 300);
}}

function doRename() {{
  const newTitle = document.getElementById('renameInput').value.trim();
  if (!newTitle) return;
  renameModal.hide();
  fetch(`/chat/${{CHAT_ID}}/rename`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{title: newTitle}}),
  }})
  .then(r => r.json())
  .then(data => {{
    if (data.title) {{
      const el = document.querySelector(`[data-chat-id="${{CHAT_ID}}"] .chat-title`);
      if (el) el.textContent = data.title;
      document.title = data.title + ' — Pengy';
    }}
  }});
}}

// ── Redact last ──────────────────────────────────────────────
// Context-pruning "undo": deletes the last raw message from the chat.
// Repeatable straight up to an empty chat — each click just reloads the
// page against the freshly-trimmed history, same as a normal chat load.

function redactLast() {{
  if (isProcessing) {{
    alert('Cannot redact while a response is in progress.');
    return;
  }}
  fetch(`/chat/${{CHAT_ID}}/redact`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{count: 1}}),
  }})
  .then(r => r.json().then(data => ({{ok: r.ok, data}})))
  .then(({{ok, data}}) => {{
    if (!ok) {{
      alert(data.error || 'Redact failed.');
      return;
    }}
    location.reload();
  }})
  .catch(err => console.error('Redact error:', err));
}}

// ── Tasks ─────────────────────────────────────────────────────
// Prompt templates shared with the GUI's Tasks dialog (tasks.json). Selecting
// one fills in its %placeholders%, renders it server-side, then routes the
// result through the normal send path — same shape as messageInput -> doSend().

function openTasks() {{
  fetch('/tasks')
    .then(r => r.json())
    .then(data => {{
      tasksCache = data.tasks || [];
      renderTasksList();
      showTasksList();
      tasksModal.show();
    }})
    .catch(err => console.error('Failed to load tasks:', err));
}}

function renderTasksList() {{
  const list = document.getElementById('tasksList');
  const empty = document.getElementById('tasksEmptyHint');
  list.innerHTML = '';
  empty.hidden = tasksCache.length > 0;

  for (const task of tasksCache) {{
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'list-group-item list-group-item-action';
    const title = document.createElement('div');
    title.className = 'fw-semibold';
    title.textContent = task.title;
    const preview = document.createElement('div');
    preview.className = 'text-muted small text-truncate';
    preview.textContent = task.template;
    btn.appendChild(title);
    btn.appendChild(preview);
    btn.addEventListener('click', () => selectTask(task.id));
    list.appendChild(btn);
  }}
}}

function showTasksList() {{
  document.getElementById('tasksListView').hidden = false;
  document.getElementById('tasksFormView').hidden = true;
  document.getElementById('tasksRunBtn').hidden = true;
  selectedTask = null;
}}

function selectTask(id) {{
  selectedTask = tasksCache.find(t => t.id === id);
  if (!selectedTask) return;

  document.getElementById('tasksFormTitle').textContent = selectedTask.title;
  const fields = document.getElementById('tasksFormFields');
  fields.innerHTML = '';
  for (const name of selectedTask.placeholders) {{
    const wrap = document.createElement('div');
    wrap.className = 'mb-2';
    const label = document.createElement('label');
    label.className = 'form-label small mb-1';
    label.textContent = name;
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'form-control form-control-sm task-field-input';
    input.dataset.name = name;
    wrap.appendChild(label);
    wrap.appendChild(input);
    fields.appendChild(wrap);
  }}

  document.getElementById('tasksListView').hidden = true;
  document.getElementById('tasksFormView').hidden = false;
  document.getElementById('tasksRunBtn').hidden = false;
  if (selectedTask.placeholders.length > 0) {{
    fields.querySelector('.task-field-input').focus();
  }}
}}

function runSelectedTask() {{
  if (!selectedTask) return;
  const values = {{}};
  document.querySelectorAll('#tasksFormFields .task-field-input').forEach(input => {{
    values[input.dataset.name] = input.value;
  }});

  fetch('/tasks/render', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{id: selectedTask.id, values}}),
  }})
  .then(r => r.json().then(data => ({{ok: r.ok, data}})))
  .then(({{ok, data}}) => {{
    if (!ok) {{
      alert(data.error || 'Failed to run task.');
      return;
    }}
    if (!data.prompt) {{
      alert('This task produced an empty prompt.');
      return;
    }}
    tasksModal.hide();
    document.getElementById('messageInput').value = data.prompt;
    doSend();
  }})
  .catch(err => console.error('Task render error:', err));
}}

// ── Cumulative token usage ───────────────────────────────────

function updateCumulativeTokens(cumulativeUsage) {{
  const el = document.getElementById('navTokens');
  const total = cumulativeUsage && cumulativeUsage.total_tokens;
  if (!total) return;
  el.textContent = `${{total.toLocaleString()}} tokens`;
  el.hidden = false;
}}

function doSend() {{
  if (isProcessing) return;
  const input = document.getElementById('messageInput');
  const content = input.value.trim();
  if (!content && pendingFiles.length === 0) return;

  if (content.startsWith('/') && pendingFiles.length === 0) {{
    handleSlashCommand(content);
    input.value = '';
    input.style.height = 'auto';
    return;
  }}

  input.value = '';
  input.style.height = 'auto';
  sseCursor = '';
  sessionStorage.removeItem('pengy_sse_cursor_' + CHAT_ID);

  const placeholder = document.querySelector('#messagesArea .text-center.text-muted');
  if (placeholder) placeholder.remove();

  const displayContent = content + (pendingFiles.length > 0 ? ' [attached: ' + pendingFiles.map(f => f.name).join(', ') + ']' : '');
  appendUserMessage(displayContent);
  setProcessing(true);
  showThinking();

  const body = {{content}};
  if (pendingFiles.length > 0) {{
    body.files = pendingFiles;
    pendingFiles = [];
    clearFiles();
  }}

  fetch(`/chat/${{CHAT_ID}}/send`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify(body),
  }})
  .then(r => r.json())
  .then(data => {{
    if (data.title) {{
      const el = document.querySelector(`[data-chat-id="${{CHAT_ID}}"] .chat-title`);
      if (el) el.textContent = data.title;
      document.title = data.title + ' — Pengy';
    }}
    openSSE();
  }})
  .catch(err => {{
    console.error('Send error:', err);
    hideThinking();
    appendError('Failed to send: ' + err);
    setProcessing(false);
  }});
}}

function handleSlashCommand(text) {{
  fetch(`/chat/${{CHAT_ID}}/command`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{command: text}}),
  }})
  .then(r => r.json())
  .then(data => {{
    switch (data.type) {{
      case 'config':
        appendSystemMessage(data.message);
        if (data.config) {{
          if (data.config.model) {{
            document.getElementById('navModel').textContent = data.config.model;
          }}
          const badge = document.getElementById('navConfirmBadge');
          const tc = data.config.tool_confirmation;
          if (tc === 'all') {{ badge.className = 'badge text-bg-warning small'; badge.textContent = 'YOLO'; }}
          else if (tc === 'safe') {{ badge.className = 'badge text-bg-info small'; badge.textContent = 'Safe'; }}
          else {{ badge.className = 'badge text-bg-secondary small'; badge.textContent = 'Confirm All'; }}
        }}
        break;
      case 'redirect':
        window.location.href = data.url;
        break;
      case 'rename':
        if (data.title) {{
          const el = document.querySelector(`[data-chat-id="${{CHAT_ID}}"] .chat-title`);
          if (el) el.textContent = data.title;
          document.title = data.title + ' — Pengy';
        }}
        appendSystemMessage('Chat renamed to: ' + data.title);
        break;
      case 'message':
        appendSystemMessage(data.message);
        break;
    }}
  }});
}}

function appendSystemMessage(msg) {{
  const el = document.createElement('div');
  el.className = 'mb-2';
  el.innerHTML = `<div class="alert alert-info py-1 px-2 mb-0 small"><i class="bi bi-info-circle me-1"></i>${{escHtml(msg)}}</div>`;
  appendToArea(el);
}}

document.getElementById('messageForm').addEventListener('submit', e => {{
  e.preventDefault();
  doSend();
}});

document.getElementById('messageInput').addEventListener('keydown', e => {{
  if (e.key === 'Enter' && !e.shiftKey) {{
    e.preventDefault();
    doSend();
  }}
}});

document.getElementById('messageInput').addEventListener('input', function() {{
  this.style.height = 'auto';
  this.style.height = Math.min(this.scrollHeight, 180) + 'px';
}});

// Mobile Firefox (and some other browsers) can leave the input hidden behind
// the on-screen keyboard when it first gains focus. Force it into view.
document.getElementById('messageInput').addEventListener('focus', () => {{
  setTimeout(() => {{
    const input = document.getElementById('messageInput');
    if (window.visualViewport) {{
      const bottom = input.getBoundingClientRect().bottom;
      const viewportBottom = window.visualViewport.height + window.visualViewport.offsetTop;
      if (bottom > viewportBottom) {{
        window.scrollBy({{ top: bottom - viewportBottom + 16, behavior: 'auto' }});
      }}
    }} else {{
      input.scrollIntoView({{ block: 'nearest', behavior: 'auto' }});
    }}
  }}, 50);
}});

function openSSE() {{
  // Preserve CONNECTING: it retains browser-managed Last-Event-ID.
  if (eventSource && eventSource.readyState !== EventSource.CLOSED) return;
  if (eventSource) {{ eventSource.close(); eventSource = null; }}
  const url = `/chat/${{CHAT_ID}}/stream` + (sseCursor ? `?after=${{encodeURIComponent(sseCursor)}}` : '');
  eventSource = new EventSource(url);
  eventSource.onmessage = e => {{
    if (e.lastEventId) {{
      sseCursor = e.lastEventId;
      sessionStorage.setItem('pengy_sse_cursor_' + CHAT_ID, sseCursor);
    }}
    handleEvent(JSON.parse(e.data));
  }};
  eventSource.onerror = () => {{
    // Don't close immediately — EventSource auto-reconnects. Only act
    // if CLOSED (non-retryable HTTP status), then reload to sync.
    if (eventSource && eventSource.readyState === EventSource.CLOSED) {{
      eventSource.close();
      eventSource = null;
      if (isProcessing) {{
        reloadToSync();
      }} else {{
        hideThinking();
        setProcessing(false);
      }}
    }}
    // If CONNECTING, let the browser auto-reconnect.
  }};
}}

function handleEvent(data) {{
  switch (data.type) {{
    case 'tool_request':
      hideThinking();
      appendToolRequest(data);
      if (!data.auto_approved) {{
        pendingToolCallId = data.tool_call_id;
        document.getElementById('confirmToolName').textContent = data.name;
        document.getElementById('confirmToolArgs').textContent = JSON.stringify(data.args, null, 2);
        confirmModal.show();
      }} else {{
        showThinking();
      }}
      break;
    case 'tool_result':
      hideThinking();
      updateToolResult(data);
      showThinking();
      break;
    case 'retrying':
      showRetrying(data);
      break;
    case 'final_response':
      hideThinking();
      appendAssistantMessage(data.html, data.usage);
      updateCumulativeTokens(data.cumulative_usage);
      eventSource.close(); eventSource = null;
      sessionStorage.removeItem('pengy_sse_cursor_' + CHAT_ID);
      setProcessing(false);
      break;
    case 'question_request':
      hideThinking();
      appendToolRequest(Object.assign({{}}, data, {{auto_approved: false}}));
      showQuestionModal(data);
      break;
    case 'question_result':
      hideThinking();
      updateToolResult(data);
      showThinking();
      break;
    case 'assistant_message':
      // Mid-turn narration that arrives before the tool calls it precedes.
      hideThinking();
      appendAssistantMessage(data.html, null);
      showThinking();
      break;
    case 'sudo_request':
      hideThinking();
      document.getElementById('sudoPasswordInput').value = '';
      sudoModal.show();
      setTimeout(() => document.getElementById('sudoPasswordInput').focus(), 300);
      break;
    case 'error':
      hideThinking();
      if (data.message === 'No active task') {{
        if (eventSource) {{ eventSource.close(); eventSource = null; }}
        if (isProcessing) {{
          reloadToSync();
        }} else {{
          setProcessing(false);
        }}
        return;
      }}
      appendError(data.message || 'Unknown error');
      if (eventSource) {{ eventSource.close(); eventSource = null; }}
      setProcessing(false);
      break;
  }}
}}

function appendUserMessage(content) {{
  const el = document.createElement('div');
  el.className = 'msg-user';
  el.innerHTML = `<div class="bubble-user">${{escHtml(content)}}</div>`;
  document.getElementById('messagesArea').appendChild(el);
  scrollToBottom();
}}

function appendToolRequest(data) {{
  const sid = safeId(data.tool_call_id);
  // A replayed event (reconnect) must not duplicate a card already on screen.
  if (document.getElementById(sid)) return;
  data.summary = data.summary || toolSummary(data.name, data.args);
  const el = document.createElement('div');
  el.className = 'msg-tool';
  el.innerHTML = `
    <div class="tool-card mb-1" id="${{sid}}">
      <div class="tool-header" data-bs-toggle="collapse" data-bs-target="#body-${{sid}}">
        <i class="bi bi-gear-fill text-warning" style="font-size:.8rem"></i>
        <code class="fw-semibold text-warning">${{escHtml(data.name)}}</code>
        ${{data.summary ? `<span class="tool-summary text-muted" title="${{escAttr(data.summary)}}">${{escHtml(data.summary)}}</span>` : ''}}
        <span class="badge bg-secondary ms-1" id="badge-${{sid}}">
          ${{data.auto_approved ? 'running...' : 'pending'}}
        </span>
        <i class="bi bi-chevron-down ms-auto" style="font-size:.7rem"></i>
      </div>
      <div class="collapse" id="body-${{sid}}">
        <pre class="tool-args">${{escHtml(JSON.stringify(data.args, null, 2))}}</pre>
        <div id="result-${{sid}}">
          <span class="text-muted small">${{data.auto_approved ? 'Running...' : 'Awaiting confirmation...'}}</span>
        </div>
      </div>
    </div>`;
  appendToArea(el);
}}

function updateToolResult(data) {{
  const sid = safeId(data.tool_call_id);
  const card = document.getElementById(sid);
  if (card) {{
    card.classList.remove('declined', 'done');
    card.classList.add(data.declined ? 'declined' : 'done');
    const badge = document.getElementById(`badge-${{sid}}`);
    if (badge) {{
      badge.className = `badge ms-1 ${{data.declined ? 'bg-danger' : 'bg-success'}}`;
      badge.textContent = data.declined ? 'declined' : 'done';
    }}
  }}
  const pinned = isNearBottom();
  const resultArea = document.getElementById(`result-${{sid}}`);
  if (resultArea) {{
    if (data.declined) {{
      resultArea.innerHTML = '<div class="text-danger small mt-1 ps-1">Declined by user</div>';
    }} else {{
      resultArea.innerHTML = `<pre class="tool-output">${{escHtml(data.content)}}</pre>`;
    }}
  }}
  if (pinned) scrollToBottom(); else updateJumpBtn();
}}

function appendAssistantMessage(html, usage) {{
  const el = document.createElement('div');
  el.className = 'msg-assistant';
  let usageHtml = '';
  if (usage && (usage.prompt_tokens || usage.completion_tokens)) {{
    const tot = (usage.prompt_tokens || 0) + (usage.completion_tokens || 0);
    usageHtml = `<div class="usage-line">
      ${{(usage.prompt_tokens||0).toLocaleString()}} in /
      ${{(usage.completion_tokens||0).toLocaleString()}} out
      (${{tot.toLocaleString()}} total)
    </div>`;
  }}
  el.innerHTML = `
    <div class="bubble-assistant">
      <div class="markdown-body">${{html || '<em class="text-muted">(empty response)</em>'}}</div>
      ${{usageHtml}}
    </div>`;
  appendToArea(el);
}}

function appendError(message) {{
  const el = document.createElement('div');
  el.className = 'mb-3';
  el.innerHTML = `<div class="alert alert-danger py-2 mb-0"><i class="bi bi-exclamation-triangle me-2"></i>${{escHtml(message)}}</div>`;
  appendToArea(el);
}}

function confirmTool(confirmed, yoloTurn = false) {{
  confirmModal.hide();
  if (!pendingToolCallId) return;
  const sid = safeId(pendingToolCallId);
  const badge = document.getElementById(`badge-${{sid}}`);
  if (badge) {{
    if (confirmed) {{
      badge.className = 'badge ms-1 ' + (yoloTurn ? 'text-bg-warning' : 'text-bg-secondary');
      badge.textContent = yoloTurn ? 'yolo' : 'running...';
    }} else {{
      badge.className = 'badge ms-1 bg-danger';
      badge.textContent = 'declined';
    }}
  }}
  fetch(`/chat/${{CHAT_ID}}/confirm`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{confirmed, tool_call_id: pendingToolCallId, yolo_turn: yoloTurn}}),
  }});
  if (confirmed) showThinking();
  pendingToolCallId = null;
}}

function submitSudo(override) {{
  sudoModal.hide();
  const password = override !== undefined ? override
    : document.getElementById('sudoPasswordInput').value || null;
  document.getElementById('sudoPasswordInput').value = '';
  fetch(`/chat/${{CHAT_ID}}/sudo`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{password}}),
  }});
  showThinking();
}}

function showQuestionModal(data) {{
  pendingQuestionId = data.tool_call_id;
  pendingQuestions = Array.isArray(data.questions) ? data.questions : [];

  const body = document.getElementById('questionModalBody');
  body.innerHTML = pendingQuestions.map((q, qi) => {{
    const options = Array.isArray(q.options) ? q.options : [];
    // Radio values are option indices — labels stay out of attributes and are
    // looked up from pendingQuestions on submit.
    const opts = options.map((opt, oi) => `
      <div class="form-check mb-2">
        <input class="form-check-input question-option" type="radio"
               name="q${{qi}}" id="q${{qi}}o${{oi}}" value="${{oi}}" ${{oi === 0 ? 'checked' : ''}}>
        <label class="form-check-label" for="q${{qi}}o${{oi}}">
          <span class="fw-semibold">${{escHtml(opt.label || '')}}</span>
          ${{opt.description ? `<div class="text-muted small">${{escHtml(opt.description)}}</div>` : ''}}
        </label>
      </div>`).join('');
    return `
      <div class="mb-4">
        <div class="fw-semibold text-info small text-uppercase mb-1">${{escHtml(q.header || ('Question ' + (qi + 1)))}}</div>
        <div class="mb-2">${{escHtml(q.question || '')}}</div>
        ${{opts}}
        <div class="form-check">
          <input class="form-check-input question-option" type="radio" name="q${{qi}}"
                 id="q${{qi}}other" value="__other__" ${{options.length ? '' : 'checked'}}>
          <label class="form-check-label" for="q${{qi}}other">Other</label>
          <input type="text" class="form-control form-control-sm mt-1" id="q${{qi}}otherText"
                 placeholder="Your own answer..."
                 oninput="document.getElementById('q${{qi}}other').checked = true">
        </div>
      </div>`;
  }}).join('');

  questionModal.show();
}}

function submitQuestion(override) {{
  questionModal.hide();
  if (!pendingQuestionId) return;

  const tool_call_id = pendingQuestionId;
  const sid = safeId(tool_call_id);
  let answered = override !== null;
  let answers = [];

  if (answered) {{
    answers = pendingQuestions.map((q, qi) => {{
      const picked = document.querySelector(`input[name="q${{qi}}"]:checked`);
      if (!picked) return '';
      if (picked.value === '__other__') {{
        return (document.getElementById(`q${{qi}}otherText`).value || '').trim();
      }}
      const opt = (q.options || [])[Number(picked.value)];
      return opt ? (opt.label || '') : '';
    }});
    // An empty "Other" with nothing typed is not an answer.
    if (answers.every(a => !a)) answered = false;
  }}

  const badge = document.getElementById(`badge-${{sid}}`);
  if (badge) {{
    badge.className = 'badge ms-1 ' + (answered ? 'text-bg-secondary' : 'bg-danger');
    badge.textContent = answered ? 'answering...' : 'cancelled';
  }}

  fetch(`/chat/${{CHAT_ID}}/confirm`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{confirmed: answered, tool_call_id: tool_call_id,
                           yolo_turn: false, answers: answered ? answers : []}}),
  }});

  pendingQuestionId = null;
  pendingQuestions = [];
  showThinking();
}}
</script>"##
        );

        base(
            &format!("{} — Pengy", escape_html(&chat.title)),
            &sidebar,
            &navbar_center,
            &main_content,
            "",
            &scripts,
        )
    }

    pub fn settings_page(config: &Config, chats: &[ChatSummary], saved: bool) -> String {
        let sidebar = {
            let mut html = String::new();
            for c in chats {
                html.push_str(&format!(
                    r#"<a href="/chat/{}" class="chat-list-item" data-chat-id="{}">
  <i class="bi bi-chat-left-text small text-muted flex-shrink-0"></i>
  <span class="chat-title">{}</span>
</a>"#,
                    c.id,
                    c.id,
                    escape_html(&c.title)
                ));
            }
            html
        };

        let saved_alert = if saved {
            r#"<div class="alert alert-success alert-dismissible fade show py-2" role="alert">
  <i class="bi bi-check-circle me-2"></i>Settings saved.
  <button type="button" class="btn-close btn-close-sm" data-bs-dismiss="alert"></button>
</div>"#
        } else {
            ""
        };

        let tc_none_sel = if config.tool_confirmation == "none" {
            " selected"
        } else {
            ""
        };
        let tc_safe_sel = if config.tool_confirmation == "safe" {
            " selected"
        } else {
            ""
        };
        let tc_all_sel = if config.tool_confirmation == "all" {
            " selected"
        } else {
            ""
        };
        let reasoning_default_sel = if config.reasoning_effort.is_empty() {
            " selected"
        } else {
            ""
        };
        let reasoning_none_sel = if config.reasoning_effort == "none" {
            " selected"
        } else {
            ""
        };
        let reasoning_minimal_sel = if config.reasoning_effort == "minimal" {
            " selected"
        } else {
            ""
        };
        let reasoning_low_sel = if config.reasoning_effort == "low" {
            " selected"
        } else {
            ""
        };
        let reasoning_medium_sel = if config.reasoning_effort == "medium" {
            " selected"
        } else {
            ""
        };
        let reasoning_high_sel = if config.reasoning_effort == "high" {
            " selected"
        } else {
            ""
        };
        let reasoning_xhigh_sel = if config.reasoning_effort == "xhigh" {
            " selected"
        } else {
            ""
        };
        let reasoning_max_sel = if config.reasoning_effort == "max" {
            " selected"
        } else {
            ""
        };
        let preserve_reasoning_checked = if config.preserve_reasoning {
            " checked"
        } else {
            ""
        };
        let api_key_status = if config.api_key.is_empty() {
            "not set"
        } else {
            "set"
        };

        let main_content = format!(
            r##"<div class="overflow-y-auto flex-grow-1 p-4">
  <div class="mx-auto" style="max-width:640px">
    {saved_alert}
    <h5 class="fw-bold mb-4">Settings</h5>
    <form method="post">
      <div class="mb-3">
        <label class="form-label fw-semibold">Base URL</label>
        <input type="url" name="base_url" class="form-control" value="{base_url}" placeholder="https://api.openai.com/v1">
        <div class="form-text">OpenAI-compatible API endpoint</div>
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">API Key</label>
        <input type="password" name="api_key" class="form-control" placeholder="Leave blank to keep current key" autocomplete="new-password">
        <div class="form-text">Current: {api_key_status}</div>
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">Model</label>
        <div class="input-group">
          <input type="text" name="model" id="modelInput" class="form-control" value="{model}" placeholder="gpt-4o">
          <button type="button" id="fetchModelsBtn" class="btn btn-outline-secondary" onclick="fetchModels()">
            <i class="bi bi-cloud-download me-1"></i>Fetch
          </button>
        </div>
        <div id="modelsList" class="mt-2 d-none"></div>
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">Tool Confirmation</label>
        <select name="tool_confirmation" class="form-select">
          <option value="none"{tc_none_sel}>Confirm every tool call</option>
          <option value="safe"{tc_safe_sel}>Auto-approve read-only tools (Safe)</option>
          <option value="all"{tc_all_sel}>YOLO — approve everything automatically</option>
        </select>
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">Reasoning Effort</label>
        <select name="reasoning_effort" class="form-select">
          <option value=""{reasoning_default_sel}>Provider default — do not send</option>
          <option value="none"{reasoning_none_sel}>Off / none</option>
          <option value="minimal"{reasoning_minimal_sel}>Minimal</option>
          <option value="low"{reasoning_low_sel}>Low</option>
          <option value="medium"{reasoning_medium_sel}>Medium</option>
          <option value="high"{reasoning_high_sel}>High</option>
          <option value="xhigh"{reasoning_xhigh_sel}>Extra high</option>
          <option value="max"{reasoning_max_sel}>Max</option>
        </select>
        <div class="form-text">Optional best-effort parameter. Provider default omits it.</div>
      </div>
      <div class="form-check mb-3">
        <input type="checkbox" name="preserve_reasoning" value="1" class="form-check-input" id="preserve_reasoning"{preserve_reasoning_checked}>
        <label class="form-check-label" for="preserve_reasoning">Preserve returned reasoning fields</label>
        <div class="form-text">Keeps reasoning_content/reasoning/reasoning_details when providers return them.</div>
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">LLM Timeout (seconds)</label>
        <input type="number" name="llm_timeout" class="form-control" value="{llm_timeout}" min="1" max="3600">
        <div class="form-text">HTTP timeout for each LLM API request</div>
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">Tool Timeout (seconds)</label>
        <input type="number" name="tool_timeout" class="form-control" value="{tool_timeout}" min="1" max="3600">
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">Max Tool Output (chars, 0=no limit)</label>
        <input type="number" name="tool_output_max_chars" class="form-control" value="{tool_output_max_chars}" min="0" max="500000" step="1000">
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">Max Download (MB, 0=no limit)</label>
        <input type="number" name="download_max_mb" class="form-control" value="{download_max_mb}" min="0" step="100">
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">Context Keep Turns</label>
        <input type="number" name="context_keep_turns" class="form-control" value="{context_keep_turns}" min="0">
        <div class="form-text">Elide tool results older than N turns (0 = keep all)</div>
      </div>
      <div class="mb-3">
        <label class="form-label fw-semibold">User Agent</label>
        <input type="text" name="user_agent" class="form-control" value="{user_agent}">
      </div>
      <div class="mb-4">
        <label class="form-label fw-semibold">System Message Template</label>
        <textarea name="system_message" class="form-control" rows="4" placeholder="You are a helpful assistant...">{system_message}</textarea>
        <div class="form-text">Placeholders: <code>{{date}}</code>, <code>{{username}}</code>, <code>{{hostname}}</code>, <code>{{osinfo}}</code></div>
      </div>
      <button type="submit" class="btn btn-primary">
        <i class="bi bi-floppy me-1"></i>Save Settings
      </button>
      <a href="/" class="btn btn-outline-secondary ms-2">Cancel</a>
    </form>
  </div>
</div>
<script>
async function fetchModels() {{
  const btn = document.getElementById('fetchModelsBtn');
  const list = document.getElementById('modelsList');
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner-border spinner-border-sm me-1"></span>Fetching...';
  list.classList.add('d-none');

  try {{
    const resp = await fetch('/models');
    const data = await resp.json();
    if (data.error) {{
      list.innerHTML = `<div class="text-danger small">Error: ${{data.error}}</div>`;
    }} else if (data.models && data.models.length > 0) {{
      let html = '<div class="small fw-semibold mb-1">Available models (click to select):</div>';
      const current = document.getElementById('modelInput').value;
      list.innerHTML = html;
      for (const m of data.models) {{
        const badge = document.createElement('span');
        const isActive = m === current;
        badge.className = 'badge text-bg-secondary me-1 mb-1' + (isActive ? ' active fw-bold' : '');
        badge.style.cursor = 'pointer';
        badge.style.fontSize = '0.8rem';
        badge.textContent = m;
        badge.addEventListener('click', () => {{
          document.getElementById('modelInput').value = m;
          list.querySelectorAll('.badge').forEach(b => b.classList.remove('active', 'fw-bold'));
          badge.classList.add('active', 'fw-bold');
        }});
        list.appendChild(badge);
      }}
    }} else {{
      list.innerHTML = '<div class="text-muted small">No models returned.</div>';
    }}
    list.classList.remove('d-none');
  }} catch (e) {{
    list.innerHTML = `<div class="text-danger small">Failed: ${{e}}</div>`;
    list.classList.remove('d-none');
  }} finally {{
    btn.disabled = false;
    btn.innerHTML = '<i class="bi bi-cloud-download me-1"></i>Fetch';
  }}
}}
</script>"##,
            base_url = escape_html(&config.base_url),
            model = escape_html(&config.model),
            llm_timeout = config.llm_timeout,
            tool_timeout = config.tool_timeout,
            tool_output_max_chars = config.tool_output_max_chars,
            download_max_mb = config.download_max_mb,
            context_keep_turns = config.context_keep_turns,
            user_agent = escape_html(&config.user_agent),
            system_message = escape_html(&config.system_message),
            reasoning_default_sel = reasoning_default_sel,
            reasoning_none_sel = reasoning_none_sel,
            reasoning_minimal_sel = reasoning_minimal_sel,
            reasoning_low_sel = reasoning_low_sel,
            reasoning_medium_sel = reasoning_medium_sel,
            reasoning_high_sel = reasoning_high_sel,
            reasoning_xhigh_sel = reasoning_xhigh_sel,
            reasoning_max_sel = reasoning_max_sel,
            preserve_reasoning_checked = preserve_reasoning_checked,
        );

        base("Settings — Pengy", &sidebar, "", &main_content, "", "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_ampersand() {
        assert_eq!(escape_html("a&b"), "a&amp;b");
    }

    #[test]
    fn escape_html_less_than() {
        assert_eq!(escape_html("<tag>"), "&lt;tag&gt;");
    }

    #[test]
    fn escape_html_greater_than() {
        assert_eq!(escape_html("x>y"), "x&gt;y");
    }

    #[test]
    fn escape_html_double_quote() {
        assert_eq!(escape_html(r#"say "hi""#), "say &quot;hi&quot;");
    }

    #[test]
    fn escape_html_single_quote() {
        assert_eq!(escape_html("it's"), "it&#x27;s");
    }

    #[test]
    fn escape_html_combined() {
        assert_eq!(
            escape_html(r#"<script>alert("xss&stuff")</script>"#),
            "&lt;script&gt;alert(&quot;xss&amp;stuff&quot;)&lt;/script&gt;"
        );
    }

    #[test]
    fn escape_html_combined_with_single_quote() {
        assert_eq!(
            escape_html(r#"<img src=x onerror='alert(1)'>"#),
            "&lt;img src=x onerror=&#x27;alert(1)&#x27;&gt;"
        );
    }

    #[test]
    fn truncate_on_char_boundary_within_limit() {
        assert_eq!(truncate_on_char_boundary("hello", 10), "hello");
    }

    #[test]
    fn truncate_on_char_boundary_ascii_truncates() {
        assert_eq!(truncate_on_char_boundary("hello world", 5), "hello");
    }

    #[test]
    fn truncate_on_char_boundary_multibyte_backs_up() {
        let base = "a".repeat(2999);
        let s = format!("{base}🐧tail");
        let result = truncate_on_char_boundary(&s, 3000);
        assert_eq!(result.len(), 2999);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn chat_title_truncation_emoji_start_does_not_panic() {
        let content = "🐧".repeat(20);
        let result = truncate_on_char_boundary(&content, 47);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn async_stream_replays_from_start_index_and_ends_when_done() {
        let events = Arc::new(Mutex::new(vec![
            SseEvent::SudoRequest,
            SseEvent::FinalResponse {
                html: "<p>hi</p>".into(),
                usage: llm_client::Usage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                },
                cumulative_usage: llm_client::Usage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                },
            },
        ]));
        let (_tx, rx) = tokio::sync::watch::channel(2usize);
        let done = Arc::new(AtomicBool::new(true));
        let stream = async_stream(1, events, rx, done);
        let items: Vec<_> = stream.collect().await;
        // retry directive + the FinalResponse event starting at index 1
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn async_stream_waits_for_late_events_when_not_done() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = tokio::sync::watch::channel(0usize);
        let done = Arc::new(AtomicBool::new(false));
        let events2 = events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            {
                let mut ev = events2.lock().unwrap();
                ev.push(SseEvent::SudoRequest);
            }
            let _ = tx.send(1);
        });
        let stream = async_stream(0, events, rx, done);
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 2); // retry + SudoRequest
    }

    fn event_log(events: Vec<SseEvent>) -> (Arc<Mutex<Vec<SseEvent>>>, tokio::sync::watch::Receiver<usize>, Arc<AtomicBool>) {
        let count = events.len();
        let events = Arc::new(Mutex::new(events));
        let (_tx, rx) = tokio::sync::watch::channel(count);
        (events, rx, Arc::new(AtomicBool::new(true)))
    }

    #[tokio::test]
    async fn reconnect_replays_only_events_after_cursor() {
        let (events, rx, done) = event_log(vec![
            SseEvent::ToolRequest { name: "read_file".into(), args: serde_json::json!({}), tool_call_id: "tool-1".into(), safe_id: "tc_tool1".into(), summary: String::new(), auto_approved: false },
            SseEvent::ToolResult { tool_call_id: "tool-1".into(), safe_id: "tc_tool1".into(), name: "read_file".into(), content: "ok".into(), declined: false },
            SseEvent::FinalResponse { html: "<p>done</p>".into(), usage: llm_client::Usage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 }, cumulative_usage: llm_client::Usage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 } },
        ]);
        let items: Vec<_> = async_stream(1, events, rx, done).collect().await;
        // retry directive plus result and final; event 0/tool request is not replayed.
        assert_eq!(items.len(), 3);
    }

    #[tokio::test]
    async fn disconnected_stream_can_resume_final_response_exactly_once() {
        let events = Arc::new(Mutex::new(vec![SseEvent::ToolRequest {
            name: "write_file".into(), args: serde_json::json!({}), tool_call_id: "tool-1".into(), safe_id: "tc_tool1".into(), summary: String::new(), auto_approved: false,
        }]));
        let (tx, rx) = tokio::sync::watch::channel(1usize);
        let done = Arc::new(AtomicBool::new(false));

        // The first client has seen event 0 and disconnects. The worker makes
        // progress while no stream is consuming it.
        events.lock().unwrap().push(SseEvent::ToolResult {
            tool_call_id: "tool-1".into(), safe_id: "tc_tool1".into(), name: "write_file".into(), content: "written".into(), declined: false,
        });
        events.lock().unwrap().push(SseEvent::FinalResponse {
            html: "<p>done</p>".into(), usage: llm_client::Usage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
            cumulative_usage: llm_client::Usage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
        });
        done.store(true, Ordering::Relaxed);
        let _ = tx.send(3);

        let items: Vec<_> = async_stream(1, events, rx, done).collect().await;
        // retry + exactly the missed result and final response.
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn chat_template_has_cursor_safe_mobile_reconnect_logic() {
        let chat = Chat::new("Template test");
        let html = templates::chat_page(&chat, &[], &Config::default(), &[], true);
        assert!(html.contains("sessionStorage.getItem('pengy_sse_cursor_'"));
        assert!(html.contains("?after=${encodeURIComponent(sseCursor)}"));
        assert!(html.contains("readyState === EventSource.CLOSED"));
        assert!(html.contains("behavior: 'auto'"));
        assert!(!html.contains("readyState !== EventSource.OPEN"));
        assert!(!html.contains("behavior: 'instant'"));
    }

    // Mid-turn narration is persisted (and rendered on reload), so it has to
    // reach the browser live too — it used to be saved and never streamed.
    #[test]
    fn assistant_message_event_serializes() {
        let json = sse_event_to_json(&SseEvent::AssistantMessage {
            html: "<p>checking</p>".into(),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "assistant_message");
        assert_eq!(v["html"], "<p>checking</p>");
    }

    #[test]
    fn chat_template_renders_mid_turn_assistant_text() {
        let chat = Chat::new("Template test");
        let html = templates::chat_page(&chat, &[], &Config::default(), &[], true);
        assert!(html.contains("case 'assistant_message'"));
    }

    // The question card is built from name/args like any other tool card, and
    // the result completes that same card.
    #[test]
    fn question_events_carry_card_fields() {
        let json = sse_event_to_json(&SseEvent::QuestionRequest {
            name: "ask_user_question".into(),
            args: serde_json::json!({"questions": []}),
            questions: serde_json::json!([]),
            tool_call_id: "call-q".into(),
            safe_id: safe_id("call-q"),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "question_request");
        assert_eq!(v["name"], "ask_user_question");
        assert_eq!(v["safe_id"], "tc_callq");

        let json = sse_event_to_json(&SseEvent::QuestionResult {
            tool_call_id: "call-q".into(),
            safe_id: safe_id("call-q"),
            name: "ask_user_question".into(),
            content: "**Approach**: Rebase".into(),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["safe_id"], "tc_callq");
        assert_eq!(v["content"], "**Approach**: Rebase");
    }

    // Option labels must never be interpolated into HTML attributes: radios
    // carry the option index and the label is looked up on submit.
    #[test]
    fn question_modal_uses_index_values_and_defaults_to_first_option() {
        let chat = Chat::new("Template test");
        let html = templates::chat_page(&chat, &[], &Config::default(), &[], true);
        assert!(html.contains("value=\"${oi}\""));
        assert!(html.contains("${oi === 0 ? 'checked' : ''}"));
        assert!(!html.contains("value=\"${escHtml(opt.label)}\""));
        assert!(html.contains("function escAttr"));
    }

    #[test]
    fn tool_summary_redacts_and_bounds_arguments() {
        assert_eq!(tool_summary("read_file", &serde_json::json!({"path": "/tmp/x"})), "/tmp/x");
        assert_eq!(tool_summary("custom", &serde_json::json!({"api_key": "secret"})), "");
        assert!(tool_summary("run_bash", &serde_json::json!({"command": "x".repeat(200)})).chars().count() <= 100);
    }
}
