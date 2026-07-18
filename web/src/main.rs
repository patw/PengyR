use pengy_core::chat_manager::{self, Chat, ChatMessage};
use pengy_core::config::{self, Config};
use pengy_core::llm_client::{self, Confirmation, LlmEvent, ToolConfirmation};
use pengy_core::tools;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                println!("Pengy Web — chat with LLMs from your browser");
                println!();
                println!("Usage: pengy-web [PORT] [OPTIONS]");
                println!();
                println!("Arguments:");
                println!("  PORT               Bind port (default: 5000)");
                println!();
                println!("Options:");
                println!("  --host HOST        Bind host (default: 127.0.0.1). Pass");
                println!("                     --host 0.0.0.0 to expose beyond localhost —");
                println!("                     this app has no authentication and exposes");
                println!("                     run_bash/run_python tools, so only do this");
                println!("                     on a trusted network.");
                println!("  --config-dir PATH  Use a custom config directory.");
                println!("  -v, --version      Show version information and exit.");
                println!("  -h, --help         Show this help message and exit.");
                return;
            }
            "--host" => {
                i += 1;
                if let Some(h) = args.get(i) {
                    host = h.clone();
                }
            }
            "--config-dir" => {
                i += 1;
                if let Some(d) = args.get(i) {
                    config_dir = Some(d.clone());
                }
            }
            other => {
                if let Ok(p) = other.parse() {
                    port = p;
                }
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
        .route("/chat/:chat_id/command", post(chat_command))
        .route("/settings", get(settings_get).post(settings_post))
        .route("/models", get(models_api))
        .with_state(state);

    let addr = format!("{}:{}", host, port);
    println!("Pengy Web UI running at http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
    sse_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<SseEvent>>>,
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
        auto_approved: bool,
    },
    ToolResult {
        tool_call_id: String,
        safe_id: String,
        name: String,
        content: String,
        declined: bool,
    },
    FinalResponse {
        html: String,
        usage: llm_client::Usage,
    },
    SudoRequest,
    Error {
        message: String,
    },
}

enum WorkerCommand {
    Confirm {
        confirmed: bool,
        tool_call_id: String,
        yolo_turn: bool,
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

fn sse_event_to_json(event: &SseEvent) -> String {
    match event {
        SseEvent::ToolRequest {
            name,
            args,
            tool_call_id,
            safe_id,
            auto_approved,
        } => serde_json::json!({
            "type": "tool_request",
            "name": name,
            "args": args,
            "tool_call_id": tool_call_id,
            "safe_id": safe_id,
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
        SseEvent::FinalResponse { html, usage } => serde_json::json!({
            "type": "final_response",
            "html": html,
            "usage": {
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": usage.total_tokens,
            },
        })
        .to_string(),
        SseEvent::SudoRequest => r#"{"type":"sudo_request"}"#.to_string(),
        SseEvent::Error { message } => {
            serde_json::json!({"type": "error", "message": message}).to_string()
        }
    }
}

impl WebWorker {
    fn start(
        chat: Chat,
        config: Config,
        sse_tx: tokio::sync::mpsc::UnboundedSender<SseEvent>,
    ) -> Arc<Self> {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerCommand>();
        let cancel = Arc::new(AtomicBool::new(false));
        let sudo_state: Arc<(Mutex<Option<Option<String>>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        let worker = Arc::new(Self {
            sse_rx: Mutex::new(None),
            cmd_tx,
            cancel: cancel.clone(),
            sudo_state: sudo_state.clone(),
        });

        let sse_tx2 = sse_tx.clone();
        *tools::USER_AGENT.lock().unwrap() = config.user_agent.clone();
        *tools::TOOL_TIMEOUT.lock().unwrap() = config.tool_timeout;

        {
            let sse_tx_sudo = sse_tx.clone();
            let sudo_state_provider = sudo_state.clone();
            *tools::SUDO_PASSWORD_PROVIDER.lock().unwrap() = Some(Box::new(move || {
                let _ = sse_tx_sudo.send(SseEvent::SudoRequest);
                let (lock, cvar) = &*sudo_state_provider;
                let mut guard = lock.lock().unwrap();
                while guard.is_none() {
                    guard = cvar.wait(guard).unwrap();
                }
                guard.take().flatten()
            }));
        }

        let tc_mode = ToolConfirmation::from_str(&config.tool_confirmation);
        let mut chat = chat;

        tokio::spawn(async move {
            let messages = build_messages(&chat, &config);
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let (confirm_tx, confirm_rx) = tokio::sync::mpsc::unbounded_channel();

            let bu = config.base_url.clone();
            let ak = config.api_key.clone();
            let md = config.model.clone();
            let re = config.reasoning_effort.clone();
            let pr = config.preserve_reasoning;
            let cancel2 = cancel.clone();

            tokio::spawn(async move {
                llm_client::chat(
                    &bu, &ak, &md, messages, tc_mode, &re, pr, event_tx, confirm_rx, cancel2,
                )
                .await;
            });

            let mut yolo_this_turn = false;

            loop {
                match event_rx.recv().await {
                    Some(LlmEvent::AssistantToolCalls { message }) => {
                        yolo_this_turn = false;
                        chat.messages.push(message);
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

                        let _ = sse_tx2.send(SseEvent::ToolRequest {
                            name: name.clone(),
                            args: args.clone(),
                            tool_call_id: tool_call_id.clone(),
                            safe_id: sid,
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
                        let _ = sse_tx2.send(SseEvent::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            safe_id: safe_id(&tool_call_id),
                            name,
                            content: display,
                            declined,
                        });
                    }
                    Some(LlmEvent::FinalResponse { content, message, usage }) => {
                        chat.messages.push(message.unwrap_or(ChatMessage {
                            role: "assistant".into(),
                            content: Some(serde_json::Value::String(content.clone())),
                            tool_calls: vec![],
                            tool_call_id: None,
                            reasoning_content: None,
                            reasoning: None,
                            reasoning_details: None,
                        }));
                        let _ = sse_tx2.send(SseEvent::FinalResponse {
                            html: render_markdown(&content),
                            usage,
                        });
                        chat_manager::save_chat(&chat).ok();
                        break;
                    }
                    None => {
                        let _ = sse_tx2.send(SseEvent::Error {
                            message: "Chat ended unexpectedly".into(),
                        });
                        chat_manager::save_chat(&chat).ok();
                        break;
                    }
                }
            }

            *tools::SUDO_PASSWORD_PROVIDER.lock().unwrap() = None;
        });

        worker
    }
}

// ── Routes ───────────────────────────────────────────────────────

async fn index() -> impl IntoResponse {
    let chats = chat_manager::load_chats();
    if !chats.is_empty() {
        Redirect::to(&format!("/chat/{}", chats[0].id))
    } else {
        let chat = chat_manager::create_chat("New Chat").unwrap();
        Redirect::to(&format!("/chat/{}", chat.id))
    }
}

async fn new_chat() -> impl IntoResponse {
    let chats = chat_manager::load_chats();
    if let Some(first) = chats.first() {
        if first.title == "New Chat" && first.messages.is_empty() {
            return Redirect::to(&format!("/chat/{}", first.id));
        }
    }
    let chat = chat_manager::create_chat("New Chat").unwrap();
    Redirect::to(&format!("/chat/{}", chat.id))
}

async fn chat_view(Path(chat_id): Path<String>) -> impl IntoResponse {
    let chat = match chat_manager::get_chat(&chat_id) {
        Some(c) => c,
        None => return Redirect::to("/").into_response(),
    };
    let chats = chat_manager::load_chats();
    let config = config::load_config();
    let turns = group_messages(&chat.messages);

    Html(templates::chat_page(&chat, &chats, &config, &turns)).into_response()
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
    let mut content = data.content.unwrap_or_default().trim().to_string();

    // Handle file attachments
    if let Some(files) = &data.files {
        let mut blocks = Vec::new();
        for f in files {
            if let Ok(decoded) = base64_decode(&f.data) {
                if let Ok(text) = String::from_utf8(decoded) {
                    blocks.push(format!("[File: {}]\n```\n{}\n```", f.name, text));
                }
            }
        }
        if !blocks.is_empty() {
            content = format!("{}\n{}", blocks.join("\n\n"), content);
        }
    }

    if content.is_empty() {
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
        content: Some(serde_json::Value::String(content.clone())),
        tool_calls: vec![],
        tool_call_id: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_details: None,
    });

    if chat.title == "New Chat" {
        chat.title = if content.len() > 50 {
            format!("{}...", truncate_on_char_boundary(&content, 47))
        } else {
            content.clone()
        };
    }
    chat_manager::save_chat(&chat).ok();

    let config = config::load_config();
    let (sse_tx, sse_rx) = tokio::sync::mpsc::unbounded_channel();

    let worker = WebWorker::start(chat.clone(), config, sse_tx);
    *worker.sse_rx.lock().unwrap() = Some(sse_rx);

    state
        .workers
        .lock()
        .unwrap()
        .insert(chat_id.clone(), worker);

    Json(serde_json::json!({"status": "ok", "title": chat.title})).into_response()
}

async fn chat_stream(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let worker = {
        let workers = state.workers.lock().unwrap();
        workers.get(&chat_id).cloned()
    };

    let sse_rx = worker.and_then(|w| w.sse_rx.lock().unwrap().take());

    let workers_ref = state.workers.clone();
    let chat_id_clone = chat_id.clone();

    let stream = async_stream(sse_rx, workers_ref, chat_id_clone);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn async_stream(
    sse_rx: Option<tokio::sync::mpsc::UnboundedReceiver<SseEvent>>,
    workers: Arc<Mutex<HashMap<String, Arc<WebWorker>>>>,
    chat_id: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold((sse_rx, false), move |(mut rx, done)| {
        let workers = workers.clone();
        let chat_id = chat_id.clone();
        async move {
            if done {
                return None;
            }

            let rx_ref = rx.as_mut()?;

            match rx_ref.recv().await {
                Some(event) => {
                    let json = sse_event_to_json(&event);
                    let is_final = matches!(
                        event,
                        SseEvent::FinalResponse { .. } | SseEvent::Error { .. }
                    );

                    if is_final {
                        workers.lock().unwrap().remove(&chat_id);
                    }

                    Some((Ok(Event::default().data(json)), (rx, is_final)))
                }
                None => {
                    workers.lock().unwrap().remove(&chat_id);
                    let event = Event::default()
                        .data(r#"{"type":"error","message":"Worker disconnected"}"#);
                    Some((Ok(event), (rx, true)))
                }
            }
        }
    })
}

#[derive(Deserialize)]
struct ConfirmRequest {
    confirmed: Option<bool>,
    tool_call_id: Option<String>,
    yolo_turn: Option<bool>,
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
                    lines.push(format!(
                        "  ```json\n  {}\n  ```",
                        tc.function.arguments
                    ));
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
            _ => "None",
        };
        return Json(serde_json::json!({
            "type": "config",
            "message": format!("Tool Confirmation: {}", label),
            "config": {
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

// ── Settings ─────────────────────────────────────────────────────

async fn settings_get() -> impl IntoResponse {
    let config = config::load_config();
    let chats = chat_manager::load_chats();
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
    tool_timeout: Option<String>,
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
        if ["", "none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&v.as_str())
        {
            config.reasoning_effort = v.clone();
        }
    }
    config.preserve_reasoning = form.preserve_reasoning.is_some();
    if let Some(v) = &form.tool_timeout {
        if let Ok(n) = v.parse::<u64>() {
            config.tool_timeout = n.max(1);
        }
    }
    if let Some(v) = &form.context_keep_turns {
        if let Ok(n) = v.parse::<usize>() {
            config.context_keep_turns = n;
        }
    }

    config::save_config(&config).ok();

    let chats = chat_manager::load_chats();
    Html(templates::settings_page(&config, &chats, true))
}

// ── Base64 decode helper ─────────────────────────────────────────

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|e| e.to_string())
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
                            args,
                            tool_call_id: tc.id.clone(),
                            safe_id: safe_id(&tc.id),
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

    html
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

fn inline_markdown(text: &str) -> String {
    let escaped = escape_html(text);
    let mut result = escaped;

    let code_re = regex::Regex::new(r"`([^`]+)`").unwrap();
    result = code_re.replace_all(&result, "<code>$1</code>").to_string();

    let bold_re = regex::Regex::new(r"\*\*([^*]+)\*\*").unwrap();
    result = bold_re
        .replace_all(&result, "<strong>$1</strong>")
        .to_string();

    let italic_re = regex::Regex::new(r"\*([^*]+)\*").unwrap();
    result = italic_re.replace_all(&result, "<em>$1</em>").to_string();

    let link_re = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    result = link_re
        .replace_all(&result, r#"<a href="$2">$1</a>"#)
        .to_string();

    result
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
    html, body {{ height: 100%; overflow: hidden; }}
    .app-shell {{ display: flex; flex-direction: column; height: 100vh; padding-bottom: env(safe-area-inset-bottom, 0px); }}
    .app-body {{ display: flex; flex: 1; overflow: hidden; }}
    @media (min-width: 768px) {{
      body {{ display: flex; flex-direction: row; }}
      #sidebarOffcanvas {{
        position: relative !important; transform: none !important; visibility: visible !important;
        width: 260px !important; height: 100vh !important; flex-shrink: 0;
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
    .messages-area {{ flex: 1; overflow-y: auto; padding: 1rem 0.75rem; }}
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
    .tool-args, .tool-output {{ background: #f6f8fa; border-radius: 4px; padding: 0.5rem 0.6rem; margin-top: 0.4rem; max-height: 250px; overflow-y: auto; white-space: pre-wrap; word-break: break-all; font-size: 0.78rem; color: #24292e; }}
    .msg-thinking {{ display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.85rem; color: var(--bs-secondary-color); font-size: 0.85rem; }}
    .input-area {{ padding: 0.6rem 0.75rem; border-top: 1px solid var(--bs-border-color); background: var(--bs-body-bg); padding-bottom: calc(0.6rem + env(safe-area-inset-bottom, 0px)); }}
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
      <div class="mt-auto border-top p-2 d-md-none">
        <a href="/settings" class="btn btn-outline-secondary w-100" data-bs-dismiss="offcanvas">
          <i class="bi bi-gear me-1"></i> Settings
        </a>
      </div>
    </div>
  </div>
  <div class="app-shell">
    <nav class="navbar navbar-light bg-light border-bottom px-2 py-2 flex-shrink-0">
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

    fn render_sidebar_chats(chats: &[Chat], active_id: &str) -> String {
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

    pub fn chat_page(chat: &Chat, chats: &[Chat], config: &Config, turns: &[Turn]) -> String {
        let sidebar = render_sidebar_chats(chats, &chat.id);

        let tc_badge = match config.tool_confirmation.as_str() {
            "all" => r#"<span class="badge bg-warning text-dark small" id="navConfirmBadge">YOLO</span>"#,
            "safe" => r#"<span class="badge bg-info text-dark small" id="navConfirmBadge">Safe</span>"#,
            _ => r#"<span class="badge bg-secondary small" id="navConfirmBadge">None</span>"#,
        };
        let navbar_center = format!(
            r#"<span class="text-muted small d-none d-sm-inline" id="navModel">{}</span> {}
<button class="btn btn-outline-secondary btn-sm ms-1" onclick="exportChat()" title="Export chat as Markdown">
  <i class="bi bi-download"></i>
</button>"#,
            escape_html(&config.model),
            tc_badge
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

                        let args_str =
                            serde_json::to_string_pretty(&ev.args).unwrap_or_default();

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
</div>"##
        );

        let scripts = format!(
            r##"<script>
const CHAT_ID = {chat_id_json};
const CHAT_TITLE = {chat_title_json};
let isProcessing = false;
let eventSource = null;
let pendingToolCallId = null;
let thinkingEl = null;
let confirmModal, sudoModal, renameModal;
let pendingFiles = [];

document.addEventListener('DOMContentLoaded', () => {{
  scrollToBottom();
  confirmModal = new bootstrap.Modal(document.getElementById('confirmModal'));
  sudoModal    = new bootstrap.Modal(document.getElementById('sudoModal'));
  renameModal  = new bootstrap.Modal(document.getElementById('renameModal'));
  document.title = CHAT_TITLE + ' — Pengy';
  document.getElementById('navTitle').textContent = 'Pengy';
  document.getElementById('sudoPasswordInput').addEventListener('keydown', e => {{
    if (e.key === 'Enter') submitSudo();
  }});
  document.getElementById('renameInput').addEventListener('keydown', e => {{
    if (e.key === 'Enter') doRename();
  }});
  document.getElementById('stopBtn').addEventListener('click', stopGeneration);
  document.getElementById('navTitle').addEventListener('dblclick', showRename);
}});

function scrollToBottom() {{
  const area = document.getElementById('messagesArea');
  area.scrollTop = area.scrollHeight;
}}

function escHtml(text) {{
  const d = document.createElement('div');
  d.textContent = text;
  return d.innerHTML;
}}

function safeId(toolCallId) {{
  return 'tc_' + toolCallId.replace(/[^a-zA-Z0-9]/g, '');
}}

function setProcessing(val) {{
  isProcessing = val;
  document.getElementById('messageInput').disabled = val;
  document.getElementById('sendBtn').disabled = val;
  document.getElementById('stopBtn').classList.toggle('d-none', !val);
  if (!val) document.getElementById('messageInput').focus();
}}

function handleFiles(files) {{
  for (const f of files) {{
    const reader = new FileReader();
    reader.onload = (e) => {{
      const base64 = e.target.result.split(',')[1];
      pendingFiles.push({{name: f.name, data: base64}});
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
  document.getElementById('messagesArea').appendChild(thinkingEl);
  scrollToBottom();
}}

function hideThinking() {{
  if (thinkingEl) {{ thinkingEl.remove(); thinkingEl = null; }}
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
          if (tc === 'all') {{ badge.className = 'badge bg-warning text-dark small'; badge.textContent = 'YOLO'; }}
          else if (tc === 'safe') {{ badge.className = 'badge bg-info text-dark small'; badge.textContent = 'Safe'; }}
          else {{ badge.className = 'badge bg-secondary small'; badge.textContent = 'None'; }}
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
  document.getElementById('messagesArea').appendChild(el);
  scrollToBottom();
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

function openSSE() {{
  if (eventSource) {{ eventSource.close(); eventSource = null; }}
  eventSource = new EventSource(`/chat/${{CHAT_ID}}/stream`);
  eventSource.onmessage = e => handleEvent(JSON.parse(e.data));
  eventSource.onerror = () => {{
    eventSource.close(); eventSource = null;
    hideThinking();
    setProcessing(false);
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
    case 'final_response':
      hideThinking();
      appendAssistantMessage(data.html, data.usage);
      eventSource.close(); eventSource = null;
      setProcessing(false);
      break;
    case 'sudo_request':
      hideThinking();
      document.getElementById('sudoPasswordInput').value = '';
      sudoModal.show();
      setTimeout(() => document.getElementById('sudoPasswordInput').focus(), 300);
      break;
    case 'error':
      hideThinking();
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
  const el = document.createElement('div');
  el.className = 'msg-tool';
  el.innerHTML = `
    <div class="tool-card mb-1" id="${{sid}}">
      <div class="tool-header" data-bs-toggle="collapse" data-bs-target="#body-${{sid}}">
        <i class="bi bi-gear-fill text-warning" style="font-size:.8rem"></i>
        <code class="fw-semibold text-warning">${{escHtml(data.name)}}</code>
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
  document.getElementById('messagesArea').appendChild(el);
  scrollToBottom();
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
  const resultArea = document.getElementById(`result-${{sid}}`);
  if (resultArea) {{
    if (data.declined) {{
      resultArea.innerHTML = '<div class="text-danger small mt-1 ps-1">Declined by user</div>';
    }} else {{
      resultArea.innerHTML = `<pre class="tool-output">${{escHtml(data.content)}}</pre>`;
    }}
  }}
  scrollToBottom();
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
  document.getElementById('messagesArea').appendChild(el);
  scrollToBottom();
}}

function appendError(message) {{
  const el = document.createElement('div');
  el.className = 'mb-3';
  el.innerHTML = `<div class="alert alert-danger py-2 mb-0"><i class="bi bi-exclamation-triangle me-2"></i>${{escHtml(message)}}</div>`;
  document.getElementById('messagesArea').appendChild(el);
  scrollToBottom();
}}

function confirmTool(confirmed, yoloTurn = false) {{
  confirmModal.hide();
  if (!pendingToolCallId) return;
  const sid = safeId(pendingToolCallId);
  const badge = document.getElementById(`badge-${{sid}}`);
  if (badge) {{
    if (confirmed) {{
      badge.className = 'badge ms-1 ' + (yoloTurn ? 'bg-warning text-dark' : 'bg-secondary');
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

    pub fn settings_page(config: &Config, chats: &[Chat], saved: bool) -> String {
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
        let reasoning_default_sel = if config.reasoning_effort.is_empty() { " selected" } else { "" };
        let reasoning_none_sel = if config.reasoning_effort == "none" { " selected" } else { "" };
        let reasoning_minimal_sel = if config.reasoning_effort == "minimal" { " selected" } else { "" };
        let reasoning_low_sel = if config.reasoning_effort == "low" { " selected" } else { "" };
        let reasoning_medium_sel = if config.reasoning_effort == "medium" { " selected" } else { "" };
        let reasoning_high_sel = if config.reasoning_effort == "high" { " selected" } else { "" };
        let reasoning_xhigh_sel = if config.reasoning_effort == "xhigh" { " selected" } else { "" };
        let reasoning_max_sel = if config.reasoning_effort == "max" { " selected" } else { "" };
        let preserve_reasoning_checked = if config.preserve_reasoning { " checked" } else { "" };
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
        <label class="form-label fw-semibold">Tool Timeout (seconds)</label>
        <input type="number" name="tool_timeout" class="form-control" value="{tool_timeout}" min="1" max="3600">
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
      for (const m of data.models) {{
        const active = m === current ? ' active fw-bold' : '';
        const escaped = m.replace(/'/g, "&#39;");
        html += `<span class="badge bg-light text-dark me-1 mb-1${{active}}" style="cursor:pointer;font-size:0.8rem"
                 onclick="document.getElementById('modelInput').value='${{escaped}}';
                         document.querySelectorAll('#modelsList .badge').forEach(b=>b.classList.remove('active','fw-bold'));
                         this.classList.add('active','fw-bold')">${{m}}</span>`;
      }}
      list.innerHTML = html;
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
            tool_timeout = config.tool_timeout,
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
}
