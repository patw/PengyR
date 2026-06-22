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
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);

    let state = AppState::new();

    let app = Router::new()
        .route("/", get(index))
        .route("/chat/new", post(new_chat))
        .route("/chat/:chat_id", get(chat_view))
        .route("/chat/:chat_id/send", post(chat_send))
        .route("/chat/:chat_id/stream", get(chat_stream))
        .route("/chat/:chat_id/confirm", post(chat_confirm))
        .route("/chat/:chat_id/sudo", post(chat_sudo))
        .route("/chat/:chat_id/delete", post(chat_delete))
        .route("/settings", get(settings_get).post(settings_post))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    println!("Pengy Web UI running at http://localhost:{}", port);

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
        #[allow(dead_code)]
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
    fn start(chat: Chat, config: Config, sse_tx: tokio::sync::mpsc::UnboundedSender<SseEvent>) -> Arc<Self> {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerCommand>();
        let cancel = Arc::new(AtomicBool::new(false));
        let sudo_state: Arc<(Mutex<Option<Option<String>>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        let worker = Arc::new(Self {
            sse_rx: Mutex::new(None), // set below
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
            let cancel2 = cancel.clone();

            tokio::spawn(async move {
                llm_client::chat(&bu, &ak, &md, messages, tc_mode, event_tx, confirm_rx, cancel2)
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
                            format!("{}\n... [truncated]", &content[..3000])
                        } else {
                            content.clone()
                        };
                        chat.messages.push(ChatMessage {
                            role: "tool".into(),
                            content: Some(serde_json::Value::String(content)),
                            tool_calls: vec![],
                            tool_call_id: Some(tool_call_id.clone()),
                        });
                        let _ = sse_tx2.send(SseEvent::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            safe_id: safe_id(&tool_call_id),
                            name,
                            content: display,
                            declined,
                        });
                    }
                    Some(LlmEvent::FinalResponse { content, usage }) => {
                        chat.messages.push(ChatMessage {
                            role: "assistant".into(),
                            content: Some(serde_json::Value::String(content.clone())),
                            tool_calls: vec![],
                            tool_call_id: None,
                        });
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
}

async fn chat_send(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(data): Json<SendRequest>,
) -> impl IntoResponse {
    let content = data.content.unwrap_or_default().trim().to_string();
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
    });

    if chat.title == "New Chat" {
        chat.title = if content.len() > 50 {
            format!("{}...", &content[..47])
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
    futures_util::stream::unfold(
        (sse_rx, false),
        move |(mut rx, done)| {
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
        },
    )
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

// ── Message grouping ─────────────────────────────────────────────

struct Turn {
    turn_type: TurnType,
}

enum TurnType {
    User {
        content: String,
    },
    Assistant {
        html: String,
    },
    ToolUse {
        events: Vec<ToolEvent>,
    },
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
    // Simple markdown-to-HTML: fenced code blocks, inline code, bold, italic, headers, lists, links
    let mut html = String::new();
    let mut in_code_block = false;
    let mut in_paragraph = false;
    let mut table_lines: Vec<String> = Vec::new();

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
            continue;
        }

        if trimmed.starts_with("### ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            html.push_str(&format!(
                "<h3>{}</h3>\n",
                inline_markdown(&trimmed[4..])
            ));
        } else if trimmed.starts_with("## ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            html.push_str(&format!(
                "<h2>{}</h2>\n",
                inline_markdown(&trimmed[3..])
            ));
        } else if trimmed.starts_with("# ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            html.push_str(&format!(
                "<h1>{}</h1>\n",
                inline_markdown(&trimmed[2..])
            ));
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            html.push_str(&format!(
                "<li>{}</li>\n",
                inline_markdown(&trimmed[2..])
            ));
        } else if trimmed.starts_with('|') && trimmed.ends_with('|') {
            if in_paragraph {
                html.push_str("</p>\n");
                in_paragraph = false;
            }
            table_lines.push(trimmed.to_string());
            continue;
        } else {
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
    if in_paragraph {
        html.push_str("</p>\n");
    }

    html
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

    // Inline code
    let code_re = regex::Regex::new(r"`([^`]+)`").unwrap();
    result = code_re
        .replace_all(&result, "<code>$1</code>")
        .to_string();

    // Bold
    let bold_re = regex::Regex::new(r"\*\*([^*]+)\*\*").unwrap();
    result = bold_re
        .replace_all(&result, "<strong>$1</strong>")
        .to_string();

    // Italic
    let italic_re = regex::Regex::new(r"\*([^*]+)\*").unwrap();
    result = italic_re
        .replace_all(&result, "<em>$1</em>")
        .to_string();

    // Links
    let link_re = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    result = link_re
        .replace_all(&result, r#"<a href="$2">$1</a>"#)
        .to_string();

    result
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Build messages ───────────────────────────────────────────────

fn build_messages(chat: &Chat, config: &Config) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if !config.system_message.is_empty() {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String(
                config::render_system_message(&config.system_message),
            )),
            tool_calls: vec![],
            tool_call_id: None,
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
        <span class="fw-bold text-nowrap">Pengy</span>
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
            "all" => r#"<span class="badge bg-warning text-dark small">YOLO</span>"#,
            "safe" => r#"<span class="badge bg-info text-dark small">Safe</span>"#,
            _ => r#"<span class="badge bg-secondary small">Confirm</span>"#,
        };
        let navbar_center = format!(
            r#"<span class="text-muted small d-none d-sm-inline">{}</span> {}"#,
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

                        let args_str = serde_json::to_string_pretty(&ev.args).unwrap_or_default();

                        let result_html = match &ev.result {
                            Some(r) if ev.declined => {
                                r#"<div class="text-danger small mt-1 ps-1">Declined by user</div>"#
                                    .to_string()
                            }
                            Some(r) => {
                                let display = if r.len() > 3000 {
                                    format!("{}...", &r[..3000])
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

        let main_content = format!(
            r##"<div class="messages-area" id="messagesArea">{messages_html}</div>
<div class="input-area">
  <form id="messageForm" class="d-flex gap-2" novalidate>
    <textarea id="messageInput" class="form-control" rows="1" placeholder="Message... (Enter to send, Shift+Enter for newline)" autocomplete="off" autofocus></textarea>
    <button type="submit" id="sendBtn" class="btn btn-primary align-self-end">
      <i class="bi bi-send-fill"></i>
    </button>
  </form>
</div>
<div class="modal fade" id="confirmModal" tabindex="-1" data-bs-backdrop="static">
  <div class="modal-dialog modal-lg">
    <div class="modal-content">
      <div class="modal-header">
        <h6 class="modal-title">
          <i class="bi bi-gear-fill text-warning me-2"></i>
          Tool Request: <code id="confirmToolName"></code>
        </h6>
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
</div>"##
        );

        let chat_id_json = serde_json::to_string(&chat.id).unwrap_or_default();
        let scripts = format!(
            r##"<script>
const CHAT_ID = {chat_id_json};
let isProcessing = false;
let eventSource = null;
let pendingToolCallId = null;
let thinkingEl = null;
let confirmModal, sudoModal;

document.addEventListener('DOMContentLoaded', () => {{
  scrollToBottom();
  confirmModal = new bootstrap.Modal(document.getElementById('confirmModal'));
  sudoModal    = new bootstrap.Modal(document.getElementById('sudoModal'));
  document.getElementById('sudoPasswordInput').addEventListener('keydown', e => {{
    if (e.key === 'Enter') submitSudo();
  }});
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
  if (!val) document.getElementById('messageInput').focus();
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

function doSend() {{
  if (isProcessing) return;
  const input = document.getElementById('messageInput');
  const content = input.value.trim();
  if (!content) return;
  input.value = '';
  input.style.height = 'auto';
  const placeholder = document.querySelector('#messagesArea .text-center.text-muted');
  if (placeholder) placeholder.remove();
  appendUserMessage(content);
  setProcessing(true);
  showThinking();
  fetch(`/chat/${{CHAT_ID}}/send`, {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{content}}),
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

        let tc_none_sel = if config.tool_confirmation == "none" { " selected" } else { "" };
        let tc_safe_sel = if config.tool_confirmation == "safe" { " selected" } else { "" };
        let tc_all_sel = if config.tool_confirmation == "all" { " selected" } else { "" };
        let api_key_status = if config.api_key.is_empty() { "not set" } else { "set" };

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
        <input type="text" name="model" class="form-control" value="{model}" placeholder="gpt-4o">
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
</div>"##,
            base_url = escape_html(&config.base_url),
            model = escape_html(&config.model),
            tool_timeout = config.tool_timeout,
            context_keep_turns = config.context_keep_turns,
            user_agent = escape_html(&config.user_agent),
            system_message = escape_html(&config.system_message),
        );

        base(
            "Settings — Pengy",
            &sidebar,
            "",
            &main_content,
            "",
            "",
        )
    }
}
