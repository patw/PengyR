//! LLM client for OpenAI-compatible APIs.
//!
//! The Python generator becomes an async function that sends events via a channel
//! and receives tool confirmations via a separate channel.
//! This is the heart of the app — all three frontends drive the same logic.

use crate::chat_manager::ChatMessage;
use crate::tools;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

// ── Graceful image-stripping helpers ────────────────────────────

const IMAGE_ERROR_KEYWORDS: &[&str] = &[
    "image",
    "multimodal",
    "vision",
    "not support",
    "unsupported",
];

fn has_image_url_parts(messages: &[ChatMessage]) -> bool {
    for msg in messages {
        if let Some(content) = &msg.content {
            if let Some(parts) = content.as_array() {
                for part in parts {
                    if part.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn strip_image_url_parts(messages: &mut [ChatMessage]) {
    for msg in messages.iter_mut() {
        if let Some(content) = msg.content.take() {
            if let Some(parts) = content.as_array() {
                let kept: Vec<&serde_json::Value> = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) != Some("image_url"))
                    .collect();
                if kept.len() == 1 {
                    if let Some(text) = kept[0].get("text").and_then(|t| t.as_str()) {
                        msg.content = Some(serde_json::Value::String(text.to_string()));
                    } else {
                        msg.content = Some(kept[0].clone());
                    }
                } else if kept.is_empty() {
                    msg.content = Some(serde_json::Value::String(
                        "[Empty — image content was removed]".into(),
                    ));
                } else {
                    msg.content = Some(serde_json::Value::Array(
                        kept.into_iter().cloned().collect(),
                    ));
                }
            } else {
                msg.content = Some(content);
            }
        }
    }
}

fn is_image_input_error(status_code: u16, error_text: &str) -> bool {
    if status_code != 400 {
        return false;
    }
    let lower = error_text.to_lowercase();
    IMAGE_ERROR_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

// ── 429 / 529 backoff ────────────────────────────────────────────
const MAX_RETRIES: u32 = 5;
const BASE_DELAY_SECS: f64 = 1.0;
const MAX_DELAY_SECS: f64 = 60.0;
const JITTER: f64 = 0.25;
const RETRYABLE_STATUSES: &[u16] = &[429, 529];

// Context errors get size-reduction retries, never generic bad requests.
const MAX_CONTEXT_RETRIES: u32 = 4;
const CONTEXT_PREVIEW: usize = 1500;
const CONTEXT_STUB: &str = "[tool output omitted from provider request to fit context; original remains in chat history]";
const CONTEXT_ERROR_CODES: &[&str] = &[
    "context_length_exceeded", "context_window_exceeded", "prompt_too_long",
    "input_too_long", "max_context_length_exceeded", "token_limit_exceeded",
];
const CONTEXT_ERROR_PHRASES: &[&str] = &[
    "context length", "context window", "context limit", "maximum context",
    "prompt too long", "input too long", "too many tokens", "token limit exceeded",
    "exceeds the model's context", "exceeds the model context",
    "exceeds the context", "context size", "context_length_exceeded",
    "exceeds the maximum allowed number of tokens", "maximum number of tokens",
];

fn is_context_limit_error(status: u16, body: &serde_json::Value, detail: &str) -> bool {
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }
    let error = body.get("error").unwrap_or(body);
    let codes = [error.get("code"), error.get("type"), body.get("code")];
    if codes.iter().flatten().filter_map(|v| v.as_str()).any(|s|
        CONTEXT_ERROR_CODES.contains(&s.to_ascii_lowercase().as_str())) {
        return true;
    }
    let text = error.get("message").and_then(|v| v.as_str())
        .or_else(|| error.as_str()).unwrap_or(detail).to_lowercase();
    CONTEXT_ERROR_PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// Compact only the provider copy. Keep the latest tool result when older
/// candidates exist; a second failure replaces previews with short stubs.
fn compact_tool_results(messages: &mut [ChatMessage], stage: u32) -> usize {
    let newest = messages.iter().rposition(|m| m.role == "tool");
    let eligible = |msg: &ChatMessage| {
        let Some(text) = msg.content.as_ref().and_then(|v| v.as_str()) else { return false; };
        msg.role == "tool" && !text.starts_with(CONTEXT_STUB)
            && !text.starts_with("Tool execution was declined")
            && !text.starts_with("User cancelled")
            && text.chars().count() >= if stage == 1 { 2 * CONTEXT_PREVIEW + 200 } else { 256 }
    };
    let protect_newest = messages.iter().enumerate()
        .any(|(i, m)| Some(i) != newest && eligible(m));
    let mut saved = 0;
    for (i, msg) in messages.iter_mut().enumerate() {
        if (protect_newest && Some(i) == newest) || !eligible(msg) { continue; }
        let text = msg.content.as_ref().and_then(|v| v.as_str()).unwrap();
        let chars = text.chars().count();
        let replacement = if stage == 1 {
            format!("{}\n\n[... {} characters omitted from provider request; original remains in chat history ...]\n\n{}",
                text.chars().take(CONTEXT_PREVIEW).collect::<String>(),
                chars - 2 * CONTEXT_PREVIEW,
                text.chars().skip(chars - CONTEXT_PREVIEW).collect::<String>())
        } else {
            CONTEXT_STUB.to_string()
        };
        let reduction = chars.saturating_sub(replacement.chars().count());
        if reduction > 0 {
            saved += reduction;
            msg.content = Some(serde_json::Value::String(replacement));
        }
    }
    saved
}

fn backoff_delay(attempt: u32, retry_after: Option<f64>) -> f64 {
    let base = match retry_after {
        Some(ra) => ra.min(MAX_DELAY_SECS),
        None => (BASE_DELAY_SECS * (2u32.pow(attempt) as f64)).min(MAX_DELAY_SECS),
    };
    let jitter = base * JITTER * (rand::random::<f64>() * 2.0 - 1.0);
    base + jitter.max(-base * JITTER)
}

fn extract_retry_after(headers: &reqwest::header::HeaderMap) -> Option<f64> {
    // OpenAI-specific: retry-after-ms (integer milliseconds)
    if let Some(ms) = headers.get("retry-after-ms") {
        if let Ok(ms_str) = ms.to_str() {
            if let Ok(ms_val) = ms_str.parse::<f64>() {
                return Some(ms_val / 1000.0);
            }
        }
    }
    // Standard Retry-After (seconds)
    if let Some(ra) = headers.get("retry-after") {
        if let Ok(ra_str) = ra.to_str() {
            if let Ok(secs) = ra_str.parse::<f64>() {
                return Some(secs);
            }
        }
    }
    None
}

/// Events emitted by the LLM chat loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LlmEvent {
    #[serde(rename = "assistant_tool_calls")]
    AssistantToolCalls { message: ChatMessage },
    #[serde(rename = "tool_request")]
    ToolRequest {
        name: String,
        args: serde_json::Value,
        tool_call_id: String,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_call_id: String,
        name: String,
        args: serde_json::Value,
        content: String,
        declined: bool,
    },
    #[serde(rename = "question_request")]
    QuestionRequest {
        name: String,
        args: serde_json::Value,
        tool_call_id: String,
        questions: serde_json::Value,
    },
    #[serde(rename = "question_result")]
    QuestionResult {
        tool_call_id: String,
        name: String,
        content: String,
    },
    #[serde(rename = "final_response")]
    FinalResponse {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<ChatMessage>,
        usage: Usage,
    },
    #[serde(rename = "retrying")]
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_secs: f64,
        status_code: u16,
        message: String,
    },
    #[serde(rename = "context_compacted")]
    ContextCompacted {
        attempt: u32,
        max_attempts: u32,
        chars_removed: usize,
    },
    /// A turn that failed for a reason the user has to act on.
    ///
    /// Deliberately *not* a [`LlmEvent::FinalResponse`]: this text is Pengy's or
    /// the endpoint's, never the model's, so frontends must not render it as the
    /// assistant's answer -- and must not persist it as one.  Before this
    /// existed, a 401 arrived as a final response, so it was drawn inside the
    /// "Assistant" box, written into `chats.json` as an assistant message, and
    /// read back by `/show`, the GUI and the Web UI as if the model had said it.
    ///
    /// `kind` is [`ERROR_KIND_CREDENTIALS`] when the endpoint rejected our
    /// credentials (in which case `message` is already the user-facing
    /// instructions), [`ERROR_KIND_CONFIG`] when the turn could not be attempted
    /// at all (no model selected), or [`ERROR_KIND_ERROR`] otherwise.
    #[serde(rename = "error")]
    Error {
        kind: String,
        message: String,
    },
}

/// `kind` for an error the user fixes by configuring credentials.
pub const ERROR_KIND_CREDENTIALS: &str = "credentials";

/// `kind` for every other failed turn.
pub const ERROR_KIND_ERROR: &str = "error";

/// `kind` for a turn that cannot even be attempted until the user chooses
/// something.  Today that means one case: no model is selected.
pub const ERROR_KIND_CONFIG: &str = "config";

/// True when `base_url` points at this machine.
pub fn is_local_endpoint(base_url: &str) -> bool {
    let rest = base_url.split("//").nth(1).unwrap_or(base_url);
    let authority = rest.split('/').next().unwrap_or("");
    // Strip any userinfo, then the port, taking IPv6 brackets into account.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or("").to_string(),
        None => host.split(':').next().unwrap_or("").to_string(),
    };
    let host = host.to_lowercase();
    matches!(host.as_str(), "localhost" | "::1" | "0.0.0.0") || host.starts_with("127.")
}

/// The instructions a user needs when no model is selected.
///
/// A local endpoint ships no model of its own (a fresh Ollama has an empty model
/// list), so the useful answer is how to choose one -- not the endpoint's
/// complaint about an empty model field.  Wording is shared with the Python and
/// C++ editions.
pub fn no_model_help(base_url: &str) -> String {
    format!(
        "No model is selected for {base_url}.\n\nPengy's default endpoint is a local server, which has no model of its own:\n    pengy-cli /models               list the models this endpoint offers\n    pengy-cli /model <name>         select one\n    ollama pull <name>              (Ollama) download one first, if the list is empty\n  Or open Settings in the GUI / Web UI and use Fetch Models."
    )
}

/// What to say when the endpoint did not answer at all.
///
/// With a local default this is the likeliest first-run failure, and a bare
/// transport error ("error sending request for url …") does not tell a new user
/// that the fix is to start their own server.  Wording is shared with the Python
/// and C++ editions.
pub fn unreachable_help(base_url: &str, detail: &str) -> String {
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    if is_local_endpoint(base_url) {
        format!(
            "Nothing answered at {base_url}{suffix}.\n\nIs your local model server running?\n    ollama serve                    (Ollama) start the server, then: ollama pull <name>\n    pengy-cli /models               list the models it offers\n    pengy-cli /baseurl <url>        point Pengy at a different endpoint\n    pengy-cli /config               review the current settings"
        )
    } else {
        format!(
            "Could not reach {base_url}{suffix}. Check the endpoint with pengy-cli /baseurl <url>."
        )
    }
}

/// A short description of a transport failure, for [`unreachable_help`].
fn transport_detail(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "connection refused".into()
    } else {
        error.to_string().chars().take(120).collect()
    }
}

/// Emit \"no model is selected\" — a configuration problem, not a failure of the
/// request, since no request is made.
fn emit_config_error(event_tx: &mpsc::UnboundedSender<LlmEvent>, message: String) {
    let _ = event_tx.send(LlmEvent::Error {
        kind: ERROR_KIND_CONFIG.to_string(),
        message,
    });
}

/// Emit an endpoint that never answered.
fn emit_unreachable_error(
    event_tx: &mpsc::UnboundedSender<LlmEvent>,
    error: &reqwest::Error,
    base_url: &str,
) {
    // A proxy or tunnel in front of a hosted API can fail the transport while
    // still speaking credential language, so that classification stays first.
    let detail = transport_detail(error);
    if looks_like_credential_problem(None, &detail) {
        emit_turn_error(event_tx, None, format!("API error: {error}"), base_url);
        return;
    }
    let _ = event_tx.send(LlmEvent::Error {
        kind: ERROR_KIND_ERROR.to_string(),
        message: unreachable_help(base_url, &detail),
    });
}

/// Phrases OpenAI-compatible endpoints use when a request cannot be
/// authenticated.  Checked in addition to the status code because several
/// compatible servers answer `400` with "api key is required" rather than a
/// `401` -- the same reason the Python edition matches text as well as types.
const CREDENTIAL_PHRASES: [&str; 14] = [
    "missing credentials",
    "no api key",
    "api key is required",
    "api_key is required",
    "api key must be set",
    "api_key client option must be set",
    "invalid api key",
    "invalid_api_key",
    "incorrect api key",
    "invalid authentication",
    "authentication failed",
    "unauthorized",
    "credentials not found",
    "you didn't provide an api key",
];

/// Does this failed request look like a credentials problem?
pub fn looks_like_credential_problem(status: Option<u16>, detail: &str) -> bool {
    if matches!(status, Some(401) | Some(403)) {
        return true;
    }
    let text = detail.to_lowercase();
    CREDENTIAL_PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// The instructions a user actually needs when credentials are missing.
///
/// The endpoint's own text is not actionable here: OpenAI answers a fresh
/// install with "provide your API key in an Authorization header using Bearer
/// auth", and the Python SDK's client-side error told users to set
/// `OPENAI_API_KEY` -- an environment variable no edition of Pengy reads.
/// Wording is shared with the Python and C++ editions.
pub fn credential_help(base_url: &str) -> String {
    let settings = crate::config::pengy_config_dir().join("settings.json");
    format!(
        "No API credentials are configured for {base_url}.\n\nConfigure Pengy (the CLI, Web UI and GUI all share {settings}):\n    pengy-cli /apikey <your-key>    set the API key\n    pengy-cli /baseurl <url>        change the endpoint (a local Ollama/vLLM needs no key)\n    pengy-cli /model <name>         choose a model\n    pengy-cli /config               review the current settings\n  Or run pengy-web and open Settings (http://127.0.0.1:5000/settings).\n\nNote: Pengy reads credentials from its own settings file. OPENAI_API_KEY\nand similar environment variables are NOT used, whatever the API error says.",
        settings = settings.display()
    )
}

/// Emit a failed turn instead of a final response.
///
/// Credential failures are replaced by [`credential_help`]; everything else
/// keeps the provider's detail but still travels as an *error*, so no frontend
/// can mistake it for something the model said.
fn emit_turn_error(
    event_tx: &mpsc::UnboundedSender<LlmEvent>,
    status: Option<u16>,
    detail: String,
    base_url: &str,
) {
    let credentials = looks_like_credential_problem(status, &detail);
    let kind = if credentials {
        ERROR_KIND_CREDENTIALS
    } else {
        ERROR_KIND_ERROR
    };
    let message = if credentials {
        credential_help(base_url)
    } else {
        detail
    };
    let _ = event_tx.send(LlmEvent::Error {
        kind: kind.to_string(),
        message,
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Confirmation from the UI for a tool call.
#[derive(Debug, Clone)]
pub struct Confirmation {
    pub tool_call_id: String,
    pub confirmed: bool,
    /// If true, auto-approve all remaining tools this turn.
    pub yolo_turn: bool,
    /// User's answers for ask_user_question (if this is a question confirmation).
    pub answers: Option<Vec<String>>,
}

/// Tool confirmation mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToolConfirmation {
    /// Execute every tool without asking.
    All,
    /// Auto-approve read-only tools; confirm write/execute.
    Safe,
    /// Confirm every tool call.
    None,
}

impl ToolConfirmation {
    pub fn from_str(s: &str) -> Self {
        match s {
            "all" => Self::All,
            "safe" => Self::Safe,
            _ => Self::None,
        }
    }
}

/// The main chat loop.
///
/// Drives the LLM conversation:
/// - Sends messages to the OpenAI-compatible API
/// - Handles tool calls
/// - Emits events via `event_tx`
/// - Receives tool confirmations via `confirm_rx`
/// - Checks `cancel` flag before each API call
/// Convert persisted messages into provider-safe messages. Local attachment
/// references are always removed; image bytes are resolved only for retained
/// recent user turns.
fn provider_messages(messages: &[ChatMessage], keep_turns: usize) -> Vec<ChatMessage> {
    let user_indices: Vec<usize> = messages.iter().enumerate()
        .filter_map(|(i, m)| (m.role == "user").then_some(i)).collect();
    let keep_from = if keep_turns == 0 { 0 } else { user_indices.len().saturating_sub(keep_turns) };
    messages.iter().enumerate().map(|(index, original)| {
        let mut msg = original.clone();
        let refs = std::mem::take(&mut msg.attachments);
        let retained = msg.role == "user" && !refs.is_empty()
            && user_indices.iter().position(|i| *i == index).map(|n| n >= keep_from).unwrap_or(false);
        if retained {
            let mut parts = Vec::new();
            for reference in &refs {
                if let Some(url) = crate::attachments::image_data_url(reference,
                    *tools::IMAGE_MAX_DIMENSION.lock().unwrap(),
                    *tools::IMAGE_MAX_MB.lock().unwrap(),
                    *tools::IMAGE_QUALITY.lock().unwrap()) {
                    parts.push(serde_json::json!({"type":"image_url","image_url":{"url":url}}));
                }
            }
            if let Some(serde_json::Value::String(text)) = &msg.content {
                if !text.is_empty() { parts.push(serde_json::json!({"type":"text","text":text})); }
            }
            if !parts.is_empty() { msg.content = Some(serde_json::Value::Array(parts)); }
        }
        msg
    }).collect()
}

fn format_question_answers(questions: &serde_json::Value, answers: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(qs) = questions.as_array() {
        for (i, q) in qs.iter().enumerate() {
            let header = q.get("header").and_then(|v| v.as_str()).unwrap_or("Q");
            let answer = answers.get(i).map(|s| s.as_str()).unwrap_or("(no answer)");
            let mut detail = String::new();
            if let Some(opts) = q.get("options").and_then(|v| v.as_array()) {
                for opt in opts {
                    if opt.get("label").and_then(|v| v.as_str()) == Some(answer) {
                        if let Some(desc) = opt.get("description").and_then(|v| v.as_str()) {
                            detail = format!(" — {desc}");
                        }
                        break;
                    }
                }
            }
            lines.push(format!("**{header}**: {answer}{detail}"));
        }
    }
    lines.join("\n")
}

pub async fn chat(
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    tool_confirmation: ToolConfirmation,
    reasoning_effort: &str,
    preserve_reasoning: bool,
    llm_timeout: u64,
    attachment_context_keep_turns: usize,
    event_tx: mpsc::UnboundedSender<LlmEvent>,
    mut confirm_rx: mpsc::UnboundedReceiver<Confirmation>,
    cancel: Arc<AtomicBool>,
    tool_ctx: Arc<tools::ToolContext>,
) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(llm_timeout))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Checked here rather than in each frontend so the CLI, GUI and Web UI cannot
    // disagree -- and because an empty model name would otherwise be sent to the
    // endpoint, whose complaint about it is not an instruction.  No request is
    // made, so nothing is charged or logged anywhere.
    if model.trim().is_empty() {
        emit_config_error(&event_tx, no_model_help(base_url));
        return;
    }
    let base_url = base_url.trim_end_matches('/');
    let url = format!("{base_url}/chat/completions");

    let mut current_messages: Vec<ChatMessage> = messages;
    #[allow(unused_assignments)]
    let mut yolo_this_turn = false;
    let mut accumulated_usage = Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    };

    'outer: loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        // Attachment refs are local history only. Resolve image derivatives at
        // request time so provider payloads never leak back into chat JSON.
        let api_messages = provider_messages(&current_messages, attachment_context_keep_turns);
        // Only this provider request is compacted. Events and persisted history
        // continue to use the unmodified current_messages.
        let mut request_messages = api_messages;
        let mut context_retries = 0;
        // Build API request payload
        let mut payload = serde_json::json!({
            "model": model,
            "messages": request_messages,
            "tools": tools::tool_definitions_json(),
            "tool_choice": "auto",
        });
        if !reasoning_effort.is_empty() {
            payload["reasoning_effort"] = serde_json::Value::String(reasoning_effort.to_string());
        }

        // ── API call with 429 / 529 exponential backoff ──────────
        let resp = {
            let mut last_status: Option<reqwest::StatusCode> = None;
            let mut last_body: Option<serde_json::Value> = None;
            let mut success = None;
            let mut rate_retries = 0;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    // Cancelled during backoff — emit nothing, just return
                    return;
                }
                match client
                    .post(&url)
                    .header("Authorization", format!("Bearer {api_key}"))
                    .header("api-key", api_key)
                    .header("Content-Type", "application/json")
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(r) => {
                        let status = r.status();
                        if status.is_success() {
                            success = Some(r);
                            break;
                        }
                        let code = status.as_u16();
                        let headers = r.headers().clone();
                        let body_text = r.text().await.unwrap_or_default();
                        let body: serde_json::Value =
                            serde_json::from_str(&body_text).unwrap_or(serde_json::json!({}));
                        last_status = Some(status);
                        last_body = Some(body.clone());
                        // ── Graceful handling: model doesn't support images ──
                        let detail = body["error"]["message"]
                            .as_str()
                            .or_else(|| body["error"].as_str())
                            .or_else(|| body["message"].as_str())
                            .unwrap_or(body_text.as_str());
                        if is_image_input_error(code, detail)
                            && !is_context_limit_error(code, &body, detail)
                            && has_image_url_parts(&current_messages)
                        {
                            strip_image_url_parts(&mut current_messages);
                            current_messages.push(ChatMessage {
                                role: "user".into(),
                                content: Some(serde_json::Value::String(
                                    "[This AI model does not support image/vision inputs, \
                                     so the image could not be attached. \
                                     The file metadata was returned above.]"
                                        .into(),
                                )),
                                attachments: vec![],
                                tool_calls: vec![],
                                tool_call_id: None,
                                reasoning_content: None,
                                reasoning: None,
                                reasoning_details: None,
                            });
                            continue 'outer;
                        }

                        if is_context_limit_error(code, &body, detail) {
                            if context_retries < MAX_CONTEXT_RETRIES {
                                let saved = compact_tool_results(
                                    &mut request_messages, if context_retries == 0 { 1 } else { 2 });
                                if saved > 0 {
                                    context_retries += 1;
                                    payload["messages"] = serde_json::to_value(&request_messages).unwrap();
                                    let _ = event_tx.send(LlmEvent::ContextCompacted {
                                        attempt: context_retries,
                                        max_attempts: MAX_CONTEXT_RETRIES,
                                        chars_removed: saved,
                                    });
                                    continue;
                                }
                            }
                            let _ = event_tx.send(LlmEvent::Error {
                                kind: ERROR_KIND_ERROR.into(),
                                message: format!("Model context limit reached; could not fit this request after {context_retries} tool-output reductions. The full tool outputs remain in chat history. Try a shorter request or a larger-context model."),
                            });
                            return;
                        }

                        if RETRYABLE_STATUSES.contains(&code) && rate_retries < MAX_RETRIES {
                            let ra = extract_retry_after(&headers);
                            let delay = backoff_delay(rate_retries, ra);
                            rate_retries += 1;
                            let detail = body["error"]["message"]
                                .as_str()
                                .or_else(|| body["error"].as_str())
                                .or_else(|| body["message"].as_str())
                                .unwrap_or(body_text.as_str())
                                .to_string();
                            let _ = event_tx.send(LlmEvent::Retrying {
                                attempt: rate_retries,
                                max_attempts: MAX_RETRIES,
                                delay_secs: (delay * 10.0).round() / 10.0,
                                status_code: code,
                                message: detail,
                            });
                            // Sleep in 500ms slices so cancel is responsive
                            let deadline = tokio::time::Instant::now()
                                + tokio::time::Duration::from_secs_f64(delay);
                            loop {
                                if cancel.load(Ordering::Relaxed) {
                                    return;
                                }
                                let now = tokio::time::Instant::now();
                                if now >= deadline {
                                    break;
                                }
                                let remaining = deadline - now;
                                let slice = remaining.min(tokio::time::Duration::from_millis(500));
                                tokio::time::sleep(slice).await;
                            }
                            continue;
                        }
                    }
                    Err(e) => {
                        emit_unreachable_error(&event_tx, &e, base_url);
                        return;
                    }
                }
                break; // non-retryable status or final attempt — handled below
            }
            if let Some(r) = success {
                r
            } else {
                let status = last_status.unwrap();
                let body = last_body.unwrap();
                let body_text = serde_json::to_string(&body).unwrap_or_default();
                let detail = body["error"]["message"]
                    .as_str()
                    .or_else(|| body["error"].as_str())
                    .or_else(|| body["message"].as_str())
                    .unwrap_or(body_text.as_str());
                emit_turn_error(
                    &event_tx,
                    Some(status.as_u16()),
                    format!("API error (HTTP {status}): {detail}"),
                    base_url,
                );
                return;
            }
        };

        let body_text = resp.text().await.unwrap_or_default();
        let body: serde_json::Value =
            serde_json::from_str(&body_text).unwrap_or(serde_json::json!({}));

        // Parse the response
        let choice = match body["choices"].as_array().and_then(|a| a.first()) {
            Some(c) => c,
            None => {
                emit_turn_error(
                    &event_tx,
                    None,
                    format!(
                        "No choices in API response: {}",
                        serde_json::to_string_pretty(&body).unwrap_or_default()
                    ),
                    base_url,
                );
                return;
            }
        };

        // Accumulate usage
        if let Some(usage) = body["usage"].as_object() {
            accumulated_usage.prompt_tokens += usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            accumulated_usage.completion_tokens += usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            accumulated_usage.total_tokens += usage
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }

        let msg = &choice["message"];
        let content = msg["content"].as_str().unwrap_or("").to_string();
        let tool_calls = msg["tool_calls"].as_array();

        if let Some(tool_calls) = tool_calls {
            if !tool_calls.is_empty() {
                // Build the assistant message for history
                let assistant_msg = ChatMessage {
                    role: "assistant".into(),
                    content: Some(serde_json::Value::String(content.clone())),
                    attachments: vec![],
                    tool_calls: tool_calls
                        .iter()
                        .map(|tc| crate::chat_manager::ToolCall {
                            id: tc["id"].as_str().unwrap_or("").into(),
                            call_type: "function".into(),
                            function: crate::chat_manager::FunctionCall {
                                name: tc["function"]["name"].as_str().unwrap_or("").into(),
                                arguments: tc["function"]["arguments"]
                                    .as_str()
                                    .unwrap_or("{}")
                                    .into(),
                            },
                        })
                        .collect(),
                    tool_call_id: None,
                    reasoning_content: if preserve_reasoning {
                        msg.get("reasoning_content").cloned()
                    } else {
                        None
                    },
                    reasoning: if preserve_reasoning {
                        msg.get("reasoning").cloned()
                    } else {
                        None
                    },
                    reasoning_details: if preserve_reasoning {
                        msg.get("reasoning_details").cloned()
                    } else {
                        None
                    },
                };

                let _ = event_tx.send(LlmEvent::AssistantToolCalls {
                    message: assistant_msg.clone(),
                });

                current_messages.push(assistant_msg);

                // Set up per-turn YOLO
                yolo_this_turn = false;

                for tc in tool_calls {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }

                    let tc_id = tc["id"].as_str().unwrap_or("").to_string();
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                    let args: serde_json::Value =
                        serde_json::from_str(args_str).unwrap_or_default();

                    // ask_user_question is a special harness-level tool
                    if name == "ask_user_question" {
                        let questions = args
                            .get("questions")
                            .cloned()
                            .unwrap_or(serde_json::Value::Array(vec![]));
                        let _ = event_tx.send(LlmEvent::QuestionRequest {
                            name: name.clone(),
                            args: args.clone(),
                            tool_call_id: tc_id.clone(),
                            questions: questions.clone(),
                        });

                        match confirm_rx.recv().await {
                            Some(conf) if conf.confirmed => {
                                let answers = conf.answers.unwrap_or_default();
                                let result_text = format_question_answers(&questions, &answers);
                                current_messages.push(ChatMessage {
                                    role: "tool".into(),
                                    content: Some(serde_json::Value::String(result_text.clone())),
                                    attachments: vec![],
                                    tool_calls: vec![],
                                    tool_call_id: Some(tc_id.clone()),
                                    reasoning_content: None,
                                    reasoning: None,
                                    reasoning_details: None,
                                });
                                let _ = event_tx.send(LlmEvent::QuestionResult {
                                    tool_call_id: tc_id.clone(),
                                    name: name.clone(),
                                    content: result_text,
                                });
                            }
                            _ => {
                                let declined_msg = "User cancelled the question.".to_string();
                                current_messages.push(ChatMessage {
                                    role: "tool".into(),
                                    content: Some(serde_json::Value::String(declined_msg.clone())),
                                    attachments: vec![],
                                    tool_calls: vec![],
                                    tool_call_id: Some(tc_id.clone()),
                                    reasoning_content: None,
                                    reasoning: None,
                                    reasoning_details: None,
                                });
                                let _ = event_tx.send(LlmEvent::ToolResult {
                                    tool_call_id: tc_id.clone(),
                                    name: name.clone(),
                                    args: args.clone(),
                                    content: declined_msg,
                                    declined: true,
                                });
                            }
                        }
                        continue;
                    }

                    // Decide if we need user confirmation
                    let skip_confirm = tool_confirmation == ToolConfirmation::All
                        || (tool_confirmation == ToolConfirmation::Safe
                            && tools::is_readonly_tool(&name))
                        || yolo_this_turn;

                    if skip_confirm {
                        // Execute immediately
                        let _ = event_tx.send(LlmEvent::ToolRequest {
                            name: name.clone(),
                            args: args.clone(),
                            tool_call_id: tc_id.clone(),
                        });

                        let result = tools::execute_tool(&name, &args, &tool_ctx).await;

                        current_messages.push(ChatMessage {
                            role: "tool".into(),
                            content: Some(serde_json::Value::String(result.clone())),
                            attachments: vec![],
                            tool_calls: vec![],
                            tool_call_id: Some(tc_id.clone()),
                            reasoning_content: None,
                            reasoning: None,
                            reasoning_details: None,
                        });

                        let _ = event_tx.send(LlmEvent::ToolResult {
                            tool_call_id: tc_id.clone(),
                            name: name.clone(),
                            args: args.clone(),
                            content: result,
                            declined: false,
                        });
                    } else {
                        // Ask UI for confirmation
                        let _ = event_tx.send(LlmEvent::ToolRequest {
                            name: name.clone(),
                            args: args.clone(),
                            tool_call_id: tc_id.clone(),
                        });

                        // Wait for confirmation
                        match confirm_rx.recv().await {
                            Some(conf) if conf.confirmed => {
                                if conf.yolo_turn {
                                    yolo_this_turn = true;
                                }

                                let result = tools::execute_tool(&name, &args, &tool_ctx).await;

                                current_messages.push(ChatMessage {
                                    role: "tool".into(),
                                    content: Some(serde_json::Value::String(result.clone())),
                                    attachments: vec![],
                                    tool_calls: vec![],
                                    tool_call_id: Some(tc_id.clone()),
                                    reasoning_content: None,
                                    reasoning: None,
                                    reasoning_details: None,
                                });

                                let _ = event_tx.send(LlmEvent::ToolResult {
                                    tool_call_id: tc_id.clone(),
                                    name: name.clone(),
                                    args: args.clone(),
                                    content: result,
                                    declined: false,
                                });
                            }
                            _ => {
                                // Declined or channel closed
                                let declined_msg =
                                    "Tool execution was declined by user.".to_string();
                                current_messages.push(ChatMessage {
                                    role: "tool".into(),
                                    content: Some(serde_json::Value::String(declined_msg.clone())),
                                    attachments: vec![],
                                    tool_calls: vec![],
                                    tool_call_id: Some(tc_id.clone()),
                                    reasoning_content: None,
                                    reasoning: None,
                                    reasoning_details: None,
                                });

                                let _ = event_tx.send(LlmEvent::ToolResult {
                                    tool_call_id: tc_id.clone(),
                                    name: name.clone(),
                                    args: args.clone(),
                                    content: declined_msg,
                                    declined: true,
                                });
                            }
                        }
                    }
                }

                // read_image parks its picture on the tool context because a
                // role:"tool" message only accepts string content on
                // OpenAI-compatible APIs.  Attach anything queued as a follow-up
                // user message — after the loop, so every tool_call keeps its
                // matching tool message immediately behind the assistant one.
                let pending_images = tool_ctx.take_pending_images();
                if !pending_images.is_empty() {
                    let mut parts: Vec<serde_json::Value> = Vec::new();
                    for image in &pending_images {
                        parts.push(serde_json::json!({
                            "type": "text",
                            "text": format!("Image loaded by read_image: {}", image.path),
                        }));
                        parts.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", image.mime, image.b64),
                            },
                        }));
                    }
                    current_messages.push(ChatMessage {
                        role: "user".into(),
                        content: Some(serde_json::Value::Array(parts)),
                        attachments: vec![],
                        tool_calls: vec![],
                        tool_call_id: None,
                        reasoning_content: None,
                        reasoning: None,
                        reasoning_details: None,
                    });
                }

                // Loop back for the next API call (the LLM will respond to tool results)
                continue;
            }
        }

        // No tool calls — this is the final response
        let final_msg = ChatMessage {
            role: "assistant".into(),
            content: Some(serde_json::Value::String(content.clone())),
            attachments: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: if preserve_reasoning {
                msg.get("reasoning_content").cloned()
            } else {
                None
            },
            reasoning: if preserve_reasoning {
                msg.get("reasoning").cloned()
            } else {
                None
            },
            reasoning_details: if preserve_reasoning {
                msg.get("reasoning_details").cloned()
            } else {
                None
            },
        };
        let _ = event_tx.send(LlmEvent::FinalResponse {
            content,
            message: Some(final_msg),
            usage: accumulated_usage,
        });
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_error_requires_explicit_signal() {
        assert!(is_context_limit_error(400, &serde_json::json!({
            "error": {"message": "This model's maximum context length is 1024 tokens"}
        }), ""));
        assert!(is_context_limit_error(413, &serde_json::json!({
            "error": {"code": "context_length_exceeded", "message": "request rejected"}
        }), ""));
        assert!(!is_context_limit_error(400, &serde_json::json!({
            "error": {"message": "Invalid model; images unsupported"}
        }), ""));
        assert!(!is_context_limit_error(500, &serde_json::json!({
            "error": {"message": "context length exceeded"}
        }), ""));
    }

    #[test]
    fn context_compaction_only_changes_tool_body_and_protects_latest() {
        let mut history = vec![
            ChatMessage::new("user", Some(serde_json::json!("question"))),
            ChatMessage::new("assistant", Some(serde_json::json!("answer"))),
            ChatMessage::new("tool", Some(serde_json::json!("a".repeat(12000)))),
            ChatMessage::new("tool", Some(serde_json::json!("b".repeat(12000)))),
        ];
        history[2].tool_call_id = Some("a".into());
        history[3].tool_call_id = Some("b".into());
        let original = history.clone();
        let saved = compact_tool_results(&mut history, 1);
        assert!(saved > 8000);
        assert!(history[2].content.as_ref().unwrap().as_str().unwrap().len() < 4000);
        assert_eq!(history[2].tool_call_id, original[2].tool_call_id);
        assert_eq!(history[3].content, original[3].content);
        let saved = compact_tool_results(&mut history, 2);
        assert!(saved > 0);
        assert_eq!(history[2].content.as_ref().unwrap(), CONTEXT_STUB);
        assert_eq!(history[0].content, original[0].content);
        assert_eq!(history[1].content, original[1].content);
    }

    #[test]
    fn context_event_serde() {
        let event = LlmEvent::ContextCompacted { attempt: 1, max_attempts: 4, chars_removed: 9000 };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"context_compacted","attempt":1,"max_attempts":4,"chars_removed":9000}"#);
        assert!(matches!(serde_json::from_str::<LlmEvent>(&json).unwrap(), LlmEvent::ContextCompacted { .. }));
    }

    #[test]
    fn tool_confirmation_from_str_all() {
        assert_eq!(ToolConfirmation::from_str("all"), ToolConfirmation::All);
    }

    #[test]
    fn tool_confirmation_from_str_safe() {
        assert_eq!(ToolConfirmation::from_str("safe"), ToolConfirmation::Safe);
    }

    #[test]
    fn tool_confirmation_from_str_none() {
        assert_eq!(ToolConfirmation::from_str("none"), ToolConfirmation::None);
    }

    #[test]
    fn tool_confirmation_from_str_unknown_defaults_to_none() {
        assert_eq!(ToolConfirmation::from_str(""), ToolConfirmation::None);
        assert_eq!(
            ToolConfirmation::from_str("garbage"),
            ToolConfirmation::None
        );
    }

    #[test]
    fn llm_event_tool_request_serde() {
        let event = LlmEvent::ToolRequest {
            name: "read_file".into(),
            args: serde_json::json!({"path": "/tmp/test"}),
            tool_call_id: "tc-123".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tool_request\""));
        let parsed: LlmEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            LlmEvent::ToolRequest {
                name, tool_call_id, ..
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(tool_call_id, "tc-123");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn llm_event_final_response_serde() {
        let event = LlmEvent::FinalResponse {
            content: "Hello!".into(),
            message: None,
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: LlmEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            LlmEvent::FinalResponse { content, usage, .. } => {
                assert_eq!(content, "Hello!");
                assert_eq!(usage.prompt_tokens, 100);
                assert_eq!(usage.completion_tokens, 50);
                assert_eq!(usage.total_tokens, 150);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn llm_event_tool_result_serde() {
        let event = LlmEvent::ToolResult {
            tool_call_id: "tc-1".into(),
            name: "run_bash".into(),
            args: serde_json::json!({"command": "ls"}),
            content: "file.txt\n".into(),
            declined: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: LlmEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            LlmEvent::ToolResult {
                declined, content, ..
            } => {
                assert!(!declined);
                assert_eq!(content, "file.txt\n");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn credential_detection_covers_status_and_wording() {
        // 401/403 are unambiguous.
        assert!(looks_like_credential_problem(Some(401), ""));
        assert!(looks_like_credential_problem(Some(403), "nope"));
        // Compatible servers answer 400 with their own wording.
        assert!(looks_like_credential_problem(Some(400), "api key is required"));
        assert!(looks_like_credential_problem(
            None,
            "Incorrect API key provided: sk-xxx"
        ));
        assert!(looks_like_credential_problem(None, "Unauthorized"));
        // ...and ordinary failures are not credential failures, or every error
        // would tell the user to configure a key they already have.
        assert!(!looks_like_credential_problem(Some(500), "boom"));
        assert!(!looks_like_credential_problem(Some(404), "model not found"));
        assert!(!looks_like_credential_problem(None, "connection refused"));
    }

    #[test]
    fn credential_help_names_the_real_controls_not_env_vars() {
        let help = credential_help("https://api.openai.com/v1");
        assert!(help.contains("https://api.openai.com/v1"), "{help}");
        for expected in [
            "/apikey",
            "/baseurl",
            "/model",
            "/config",
            "settings.json",
            "pengy-web",
            "NOT used",
        ] {
            assert!(help.contains(expected), "missing {expected} in {help}");
        }
    }

    #[test]
    fn llm_event_error_serde() {
        // The FFI (GUI) and SSE (web) contracts are this JSON, so pin it.
        let event = LlmEvent::Error {
            kind: ERROR_KIND_CREDENTIALS.into(),
            message: "no key".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"error\""), "{json}");
        match serde_json::from_str::<LlmEvent>(&json).unwrap() {
            LlmEvent::Error { kind, message } => {
                assert_eq!(kind, ERROR_KIND_CREDENTIALS);
                assert_eq!(message, "no key");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn usage_default_values() {
        let u = Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        };
        let json = serde_json::to_string(&u).unwrap();
        let parsed: Usage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_tokens, 0);
    }

    fn attached_user(text: &str, id: &str) -> ChatMessage {
        let mut msg = ChatMessage::new("user", Some(serde_json::Value::String(text.into())));
        msg.attachments.push(crate::attachments::AttachmentRef {
            v: 1,
            id: id.into(),
            kind: "image".into(),
            name: "missing.png".into(),
            media_type: "image/png".into(),
            byte_size: 1,
            created_at: "now".into(),
            image: None,
            extra: std::collections::BTreeMap::new(),
        });
        msg
    }

    #[test]
    fn provider_messages_strip_attachment_metadata_from_old_turns() {
        let messages = vec![attached_user("old", "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), attached_user("new", "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")];
        let provider = provider_messages(&messages, 1);
        assert!(provider.iter().all(|message| message.attachments.is_empty()));
        assert_eq!(provider[0].content.as_ref().unwrap(), "old");
        // The newest retained image is unavailable in this fixture, but its
        // local metadata is still stripped rather than sent to the provider.
        assert_eq!(provider[1].content.as_ref().unwrap(), &serde_json::json!([{"type":"text","text":"new"}]));
    }

    #[test]
    fn provider_messages_zero_keeps_all_turns_but_strips_metadata() {
        let messages = vec![attached_user("one", "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), attached_user("two", "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")];
        let provider = provider_messages(&messages, 0);
        assert_eq!(provider.len(), 2);
        assert!(provider.iter().all(|message| message.attachments.is_empty()));
    }
}

#[cfg(test)]
mod loop_tests {
    //! Conversation-loop tests against a canned stub HTTP server.
    //! Mirrors Pengy's Python tests/test_llm_loop.py — keep scenarios in sync.
    use super::*;
    use crate::chat_manager::ChatMessage;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String(text.into())),
            attachments: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        }
    }

    fn completion(
        content: &str,
        tool_calls: serde_json::Value,
        usage: (u64, u64),
    ) -> serde_json::Value {
        let mut message = serde_json::json!({"role": "assistant", "content": content});
        if !tool_calls.is_null() {
            message["tool_calls"] = tool_calls;
        }
        serde_json::json!({
            "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": usage.0,
                "completion_tokens": usage.1,
                "total_tokens": usage.0 + usage.1,
            }
        })
    }

    fn tool_call(id: &str, name: &str, args: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": args.to_string()}
        })
    }

    /// Serve `responses` in order on an ephemeral port; record request bodies.
    fn stub_server(
        responses: Vec<serde_json::Value>,
    ) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
        stub_server_status(200, responses)
    }

    /// Serve explicit success/error sequences to exercise retries end to end.
    fn stub_server_sequence(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        std::thread::spawn(move || {
            for (status, response) in responses {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (start, length) = loop {
                    let count = sock.read(&mut tmp).unwrap();
                    if count == 0 { return; }
                    buf.extend_from_slice(&tmp[..count]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        let len = headers.lines().find_map(|line|
                            line.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())
                        ).unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < start + length {
                    let count = sock.read(&mut tmp).unwrap();
                    if count == 0 { return; }
                    buf.extend_from_slice(&tmp[..count]);
                }
                recorded.lock().unwrap().push(
                    serde_json::from_slice(&buf[start..start + length]).unwrap());
                let data = response.to_string();
                let wire = format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len());
                sock.write_all(wire.as_bytes()).unwrap();
            }
        });
        (base_url, requests)
    }

    /// Same, but every response is served with `status`.
    ///
    /// The original stub only ever answered 200, so neither a rejected
    /// credential nor a server error could be exercised end to end.
    fn stub_server_status(
        status: u16,
        responses: Vec<serde_json::Value>,
    ) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            403 => "Forbidden",
            _ => "Internal Server Error",
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![]));
        let requests_clone = requests.clone();

        std::thread::spawn(move || {
            for response in responses {
                let (mut sock, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (headers_end, content_length) = loop {
                    let n = match sock.read(&mut tmp) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        let cl = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (pos + 4, cl);
                    }
                };
                while buf.len() < headers_end + content_length {
                    let n = match sock.read(&mut tmp) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&buf[headers_end..headers_end + content_length])
                        .unwrap_or_default();
                requests_clone.lock().unwrap().push(body);

                let payload = response.to_string();
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });

        (base_url, requests)
    }

    struct Driver {
        rx: mpsc::UnboundedReceiver<LlmEvent>,
        confirm_tx: mpsc::UnboundedSender<Confirmation>,
        handle: tokio::task::JoinHandle<()>,
    }

    fn start_chat(
        base_url: &str,
        messages: Vec<ChatMessage>,
        mode: ToolConfirmation,
        reasoning_effort: &str,
        preserve_reasoning: bool,
    ) -> Driver {
        start_chat_full(base_url, "stub-model", messages, mode, reasoning_effort, preserve_reasoning)
    }

    /// Same, with an explicit model name (the default is deliberately empty).
    fn start_chat_with_model(base_url: &str, model: &str, messages: Vec<ChatMessage>) -> Driver {
        start_chat_full(base_url, model, messages, ToolConfirmation::None, "", false)
    }

    fn start_chat_full(
        base_url: &str,
        model: &str,
        messages: Vec<ChatMessage>,
        mode: ToolConfirmation,
        reasoning_effort: &str,
        preserve_reasoning: bool,
    ) -> Driver {
        let (event_tx, rx) = mpsc::unbounded_channel();
        let (confirm_tx, confirm_rx) = mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let base_url = base_url.to_string();
        let model = model.to_string();
        let effort = reasoning_effort.to_string();
        let handle = tokio::spawn(async move {
            chat(
                &base_url,
                "test-key",
                &model,
                messages,
                mode,
                &effort,
                preserve_reasoning,
                300,
                4,
                event_tx,
                confirm_rx,
                cancel,
                Arc::new(tools::ToolContext::new()),
            )
            .await;
        });
        Driver {
            rx,
            confirm_tx,
            handle,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejected_credentials_become_pengy_instructions() {
        // A fresh install: no key, default base_url. The endpoint answers 401
        // with advice a Pengy user cannot act on ("provide your API key in an
        // Authorization header"), so the turn must be reported as a credentials
        // error carrying Pengy's own controls instead.
        let (base, requests) = stub_server_status(
            401,
            vec![serde_json::json!({
                "error": {"message": "You didn't provide an API key."}
            })],
        );
        let mut d = start_chat(
            &base,
            vec![user_msg("hi")],
            ToolConfirmation::None,
            "",
            false,
        );

        match d.rx.recv().await.expect("an event") {
            LlmEvent::Error { kind, message } => {
                assert_eq!(kind, ERROR_KIND_CREDENTIALS);
                assert!(
                    message.contains("No API credentials are configured"),
                    "{message}"
                );
                assert!(message.contains("/apikey"), "{message}");
                // The endpoint's advice is replaced, not merely prefixed.
                assert!(!message.contains("Authorization header"), "{message}");
            }
            other => panic!("expected an error event, got {other:?}"),
        }

        // Nothing follows it. A trailing final response is exactly what made a
        // 401 render (and persist) as the assistant's answer.
        assert!(d.rx.try_recv().is_err(), "no event may follow the error");
        assert_eq!(requests.lock().unwrap().len(), 1, "one attempt, no retries");
    }

    #[test]
    fn local_endpoints_are_recognised() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://127.0.0.1:8080/v1",
            "http://localhost:11434/v1",
            "http://0.0.0.0:11434/v1",
            "http://[::1]:11434/v1",
            "127.0.0.1:11434",
        ] {
            assert!(is_local_endpoint(url), "{url} should be local");
        }
        for url in [
            "https://api.openai.com/v1",
            "https://api.groq.com/openai/v1",
            "http://192.168.1.50:11434/v1",
            "",
        ] {
            assert!(!is_local_endpoint(url), "{url} should not be local");
        }
    }

    #[test]
    fn no_model_help_says_how_to_choose_one() {
        let help = no_model_help("http://127.0.0.1:11434/v1");
        for expected in [
            "No model is selected",
            "http://127.0.0.1:11434/v1",
            "/models",
            "/model ",
            "ollama pull",
            "Fetch Models",
        ] {
            assert!(help.contains(expected), "missing {expected} in {help}");
        }
    }

    #[test]
    fn unreachable_help_adapts_to_the_endpoint() {
        let local = unreachable_help("http://127.0.0.1:11434/v1", "connection refused");
        assert!(local.contains("Nothing answered at http://127.0.0.1:11434/v1"));
        assert!(local.contains("connection refused"));
        assert!(local.contains("ollama serve"));
        assert!(local.contains("/baseurl"));

        let remote = unreachable_help("https://api.example.com/v1", "timed out");
        assert!(remote.contains("Could not reach https://api.example.com/v1"));
        assert!(!remote.to_lowercase().contains("ollama"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_model_is_reported_without_touching_the_endpoint() {
        // The shipped default: a local endpoint and no model, because a local
        // server ships none of its own.  The turn must not become a request that
        // asks the endpoint what it thinks of an empty model name.
        let (base, requests) = stub_server(vec![]);
        let mut d = start_chat_with_model(&base, "   ", vec![user_msg("hi")]);

        match d.rx.recv().await.expect("an event") {
            LlmEvent::Error { kind, message } => {
                assert_eq!(kind, ERROR_KIND_CONFIG);
                assert!(message.contains("No model is selected"), "{message}");
                assert!(message.contains("/models"), "{message}");
            }
            other => panic!("expected a config error, got {other:?}"),
        }
        assert!(d.rx.try_recv().is_err());
        assert!(
            requests.lock().unwrap().is_empty(),
            "no request may reach the endpoint"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_endpoint_that_never_answers_explains_itself() {
        // Loopback port 1: nothing listens.  With a local default this is the
        // likeliest first-run failure, so it must name the URL and point at the
        // server the user has to start, not just relay a transport error.
        let mut d = start_chat(
            "http://127.0.0.1:1",
            vec![user_msg("hi")],
            ToolConfirmation::None,
            "",
            false,
        );

        match d.rx.recv().await.expect("an event") {
            LlmEvent::Error { kind, message } => {
                assert_eq!(kind, ERROR_KIND_ERROR);
                assert!(
                    message.contains("Nothing answered at http://127.0.0.1:1"),
                    "{message}"
                );
                assert!(message.contains("ollama serve"), "{message}");
                assert!(message.contains("/baseurl"), "{message}");
            }
            other => panic!("expected an error event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn final_response_no_tools() {
        let (base, requests) = stub_server(vec![completion(
            "hello there",
            serde_json::Value::Null,
            (10, 5),
        )]);
        let mut d = start_chat(
            &base,
            vec![user_msg("hi")],
            ToolConfirmation::None,
            "",
            false,
        );

        match d.rx.recv().await.unwrap() {
            LlmEvent::FinalResponse { content, usage, .. } => {
                assert_eq!(content, "hello there");
                assert_eq!(usage.total_tokens, 15);
            }
            other => panic!("expected FinalResponse, got {other:?}"),
        }
        d.handle.await.unwrap();

        let reqs = requests.lock().unwrap();
        assert_eq!(reqs[0]["model"], "stub-model");
        assert!(reqs[0]["tools"].as_array().unwrap().len() == 16);
        assert!(reqs[0].get("reasoning_effort").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reasoning_effort_included_when_set() {
        let (base, requests) = stub_server(vec![completion("ok", serde_json::Value::Null, (1, 1))]);
        let mut d = start_chat(
            &base,
            vec![user_msg("hi")],
            ToolConfirmation::None,
            "high",
            false,
        );
        d.rx.recv().await.unwrap();
        d.handle.await.unwrap();
        assert_eq!(requests.lock().unwrap()[0]["reasoning_effort"], "high");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn context_overflow_after_real_tool_keeps_full_event_and_does_not_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        let text = "source-data-".repeat(1000);
        std::fs::write(&file, &text).unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()});
        let (base, requests) = stub_server_sequence(vec![
            (200, completion("", serde_json::json!([tool_call("tc1", "read_file", &args)]), (10, 5))),
            (400, serde_json::json!({"error": {"message": "maximum context length exceeded"}})),
            (200, completion("OK", serde_json::Value::Null, (10, 5))),
        ]);
        let mut d = start_chat(&base, vec![user_msg("read it")], ToolConfirmation::All, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::AssistantToolCalls { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ToolRequest { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ToolResult { content, .. }
            if content == text));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ContextCompacted { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::FinalResponse { content, .. }
            if content == "OK"));
        d.handle.await.unwrap();
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[1]["messages"][2]["content"], text);
        assert!(reqs[2]["messages"][2]["content"].as_str().unwrap().len() < 4000);
        assert_eq!(reqs[2]["messages"][2]["tool_call_id"], "tc1");
    }

    fn tool_history() -> Vec<ChatMessage> {
        let mut messages = vec![user_msg("Say OK")];
        for (id, text) in [("a", "A".repeat(12000)), ("b", "B".repeat(12000))] {
            let mut assistant = ChatMessage::new("assistant", Some(serde_json::json!("")));
            assistant.tool_calls.push(crate::chat_manager::ToolCall {
                id: id.into(), call_type: "function".into(),
                function: crate::chat_manager::FunctionCall {
                    name: "run_python".into(), arguments: "{\"code\":\"print(1)\"}".into(),
                },
            });
            messages.push(assistant);
            let mut tool = ChatMessage::new("tool", Some(serde_json::json!(text)));
            tool.tool_call_id = Some(id.into());
            messages.push(tool);
        }
        messages
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn context_overflow_retries_with_provider_only_compaction() {
        let overflow = serde_json::json!({"error": {"code": "context_length_exceeded",
            "message": "maximum context length exceeded"}});
        let (base, requests) = stub_server_sequence(vec![
            (400, overflow), (200, completion("OK", serde_json::Value::Null, (10, 5))),
        ]);
        let original = tool_history();
        let mut d = start_chat(&base, original.clone(), ToolConfirmation::All, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(),
            LlmEvent::ContextCompacted { attempt: 1, chars_removed, .. } if chars_removed > 8000));
        assert!(matches!(d.rx.recv().await.unwrap(),
            LlmEvent::FinalResponse { content, .. } if content == "OK"));
        d.handle.await.unwrap();
        let req = requests.lock().unwrap();
        assert_eq!(req.len(), 2);
        let before = req[0]["messages"].as_array().unwrap();
        let after = req[1]["messages"].as_array().unwrap();
        assert_eq!(before[2]["content"], original[2].content.as_ref().unwrap().clone());
        assert!(after[2]["content"].as_str().unwrap().len() < 4000);
        assert_eq!(after[4]["content"], before[4]["content"]);
        assert_eq!(after[2]["tool_call_id"], "a");
        assert_eq!(after[3]["tool_calls"], before[3]["tool_calls"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn context_overflow_without_tools_is_not_retried() {
        let (base, requests) = stub_server_sequence(vec![
            (400, serde_json::json!({"error": {"message": "context length exceeded"}})),
        ]);
        let mut d = start_chat(&base, vec![user_msg("hello")], ToolConfirmation::None, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(),
            LlmEvent::Error { message, .. } if message.contains("Model context limit reached")));
        d.handle.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unrelated_bad_request_is_not_retried() {
        let (base, requests) = stub_server_sequence(vec![
            (400, serde_json::json!({"error": {"message": "Invalid model; images unsupported"}})),
        ]);
        let mut d = start_chat(&base, vec![user_msg("hello")], ToolConfirmation::None, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::Error { message, .. }
            if message.contains("Invalid model")));
        d.handle.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_mode_executes_tool_and_feeds_result() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "file body here").unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()});

        let (base, requests) = stub_server(vec![
            completion(
                "",
                serde_json::json!([tool_call("tc1", "read_file", &args)]),
                (100, 20),
            ),
            completion("done", serde_json::Value::Null, (200, 30)),
        ]);
        let mut d = start_chat(
            &base,
            vec![user_msg("read it")],
            ToolConfirmation::All,
            "",
            false,
        );

        assert!(matches!(
            d.rx.recv().await.unwrap(),
            LlmEvent::AssistantToolCalls { .. }
        ));
        assert!(matches!(
            d.rx.recv().await.unwrap(),
            LlmEvent::ToolRequest { .. }
        ));
        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult {
                content, declined, ..
            } => {
                assert!(!declined);
                assert!(content.contains("file body here"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        match d.rx.recv().await.unwrap() {
            LlmEvent::FinalResponse { usage, .. } => {
                assert_eq!(usage.prompt_tokens, 300);
                assert_eq!(usage.completion_tokens, 50);
            }
            other => panic!("expected FinalResponse, got {other:?}"),
        }
        d.handle.await.unwrap();

        let reqs = requests.lock().unwrap();
        let msgs = reqs[1]["messages"].as_array().unwrap();
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last["role"], "tool");
        assert_eq!(last["tool_call_id"], "tc1");
    }

    /// read_image can't return a picture through a role:"tool" message, so the
    /// loop attaches it as a follow-up user message.  Mirrors Python's
    /// TestReadImageAttachment — keep the two in sync.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_image_attaches_picture_as_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shot.png");
        image::RgbImage::from_pixel(48, 32, image::Rgb([10, 120, 200]))
            .save(&file)
            .unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()});

        let (base, requests) = stub_server(vec![
            completion(
                "",
                serde_json::json!([tool_call("tc1", "read_image", &args)]),
                (100, 20),
            ),
            completion("a blue rectangle", serde_json::Value::Null, (200, 30)),
        ]);
        let mut d = start_chat(
            &base,
            vec![user_msg("what is in it?")],
            ToolConfirmation::All,
            "",
            false,
        );
        while let Some(ev) = d.rx.recv().await {
            if let LlmEvent::ToolResult { content, .. } = &ev {
                assert!(content.contains("Loaded shot.png"), "{content}");
                assert!(content.contains("48×32"), "{content}");
            }
            if matches!(ev, LlmEvent::FinalResponse { .. }) {
                break;
            }
        }
        d.handle.await.unwrap();

        let reqs = requests.lock().unwrap();
        let msgs = reqs[1]["messages"].as_array().unwrap();

        // The tool message must stay a plain string and stay adjacent to the
        // assistant message that requested it.
        let tool_idx = msgs
            .iter()
            .position(|m| m["role"] == "tool")
            .expect("a tool message");
        assert!(msgs[tool_idx]["content"].is_string());
        assert_eq!(msgs[tool_idx - 1]["role"], "assistant");

        // The picture rides in a trailing user message instead.
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last["role"], "user");
        let parts = last["content"].as_array().expect("array content");
        let images: Vec<_> = parts.iter().filter(|p| p["type"] == "image_url").collect();
        assert_eq!(images.len(), 1);
        let url = images[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/"), "{url}");
        assert!(url.contains(";base64,"), "{url}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_image_error_attaches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"path": dir.path().join("nope.png").to_str().unwrap()});
        let (base, requests) = stub_server(vec![
            completion(
                "",
                serde_json::json!([tool_call("tc1", "read_image", &args)]),
                (10, 5),
            ),
            completion("ok", serde_json::Value::Null, (10, 5)),
        ]);
        let mut d = start_chat(
            &base,
            vec![user_msg("look")],
            ToolConfirmation::All,
            "",
            false,
        );
        while let Some(ev) = d.rx.recv().await {
            if matches!(ev, LlmEvent::FinalResponse { .. }) {
                break;
            }
        }
        d.handle.await.unwrap();

        let reqs = requests.lock().unwrap();
        for m in reqs[1]["messages"].as_array().unwrap() {
            if m["role"] == "user" {
                assert!(m["content"].is_string(), "nothing should be attached");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn safe_mode_pauses_for_write_tool_until_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        let args = serde_json::json!({"path": target.to_str().unwrap(), "content": "written!"});

        let (base, _requests) = stub_server(vec![
            completion(
                "",
                serde_json::json!([tool_call("tc1", "write_file", &args)]),
                (1, 1),
            ),
            completion("done", serde_json::Value::Null, (1, 1)),
        ]);
        let mut d = start_chat(
            &base,
            vec![user_msg("write")],
            ToolConfirmation::Safe,
            "",
            false,
        );

        assert!(matches!(
            d.rx.recv().await.unwrap(),
            LlmEvent::AssistantToolCalls { .. }
        ));
        assert!(matches!(
            d.rx.recv().await.unwrap(),
            LlmEvent::ToolRequest { .. }
        ));
        assert!(!target.exists(), "tool must not run before confirmation");

        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "tc1".into(),
                confirmed: true,
                yolo_turn: false,
                answers: None,
            })
            .unwrap();

        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult { declined, .. } => assert!(!declined),
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "written!");
        assert!(matches!(
            d.rx.recv().await.unwrap(),
            LlmEvent::FinalResponse { .. }
        ));
        d.handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn decline_feeds_declined_message_to_model() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        let args = serde_json::json!({"path": target.to_str().unwrap(), "content": "x"});

        let (base, requests) = stub_server(vec![
            completion(
                "",
                serde_json::json!([tool_call("tc1", "write_file", &args)]),
                (1, 1),
            ),
            completion("understood", serde_json::Value::Null, (1, 1)),
        ]);
        let mut d = start_chat(
            &base,
            vec![user_msg("write")],
            ToolConfirmation::None,
            "",
            false,
        );

        d.rx.recv().await.unwrap(); // AssistantToolCalls
        d.rx.recv().await.unwrap(); // ToolRequest
        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "tc1".into(),
                confirmed: false,
                yolo_turn: false,
                answers: None,
            })
            .unwrap();

        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult {
                declined, content, ..
            } => {
                assert!(declined);
                assert_eq!(content, "Tool execution was declined by user.");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert!(!target.exists());
        d.rx.recv().await.unwrap(); // FinalResponse
        d.handle.await.unwrap();

        let reqs = requests.lock().unwrap();
        let msgs = reqs[1]["messages"].as_array().unwrap();
        assert_eq!(
            msgs[msgs.len() - 1]["content"],
            "Tool execution was declined by user."
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn yolo_turn_approves_remaining_tools_in_round() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("a.txt");
        let f2 = dir.path().join("b.txt");
        let args1 = serde_json::json!({"path": f1.to_str().unwrap(), "content": "one"});
        let args2 = serde_json::json!({"path": f2.to_str().unwrap(), "content": "two"});

        let (base, _requests) = stub_server(vec![
            completion(
                "",
                serde_json::json!([
                    tool_call("tc1", "write_file", &args1),
                    tool_call("tc2", "write_file", &args2),
                ]),
                (1, 1),
            ),
            completion("done", serde_json::Value::Null, (1, 1)),
        ]);
        let mut d = start_chat(
            &base,
            vec![user_msg("write both")],
            ToolConfirmation::None,
            "",
            false,
        );

        d.rx.recv().await.unwrap(); // AssistantToolCalls
        d.rx.recv().await.unwrap(); // ToolRequest tc1
        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "tc1".into(),
                confirmed: true,
                yolo_turn: true,
                answers: None,
            })
            .unwrap();
        d.rx.recv().await.unwrap(); // ToolResult tc1

        // tc2 must run WITHOUT another confirmation being sent
        assert!(matches!(
            d.rx.recv().await.unwrap(),
            LlmEvent::ToolRequest { .. }
        ));
        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult { declined, .. } => assert!(!declined),
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "two");
        d.rx.recv().await.unwrap(); // FinalResponse
        d.handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn yolo_turn_resets_on_next_assistant_round() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("a.txt");
        let f2 = dir.path().join("b.txt");
        let args1 = serde_json::json!({"path": f1.to_str().unwrap(), "content": "one"});
        let args2 = serde_json::json!({"path": f2.to_str().unwrap(), "content": "two"});

        let (base, _requests) = stub_server(vec![
            completion(
                "",
                serde_json::json!([tool_call("tc1", "write_file", &args1)]),
                (1, 1),
            ),
            completion(
                "",
                serde_json::json!([tool_call("tc2", "write_file", &args2)]),
                (1, 1),
            ),
            completion("done", serde_json::Value::Null, (1, 1)),
        ]);
        let mut d = start_chat(
            &base,
            vec![user_msg("write twice")],
            ToolConfirmation::None,
            "",
            false,
        );

        d.rx.recv().await.unwrap(); // AssistantToolCalls round 1
        d.rx.recv().await.unwrap(); // ToolRequest tc1
        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "tc1".into(),
                confirmed: true,
                yolo_turn: true,
                answers: None,
            })
            .unwrap();
        d.rx.recv().await.unwrap(); // ToolResult tc1

        // Round 2: yolo must have reset — tc2 needs a fresh confirmation.
        d.rx.recv().await.unwrap(); // AssistantToolCalls round 2
        d.rx.recv().await.unwrap(); // ToolRequest tc2
        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "tc2".into(),
                confirmed: false,
                yolo_turn: false,
                answers: None,
            })
            .unwrap();
        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult { declined, .. } => {
                assert!(declined, "yolo_turn must not leak into the next round");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert!(!f2.exists());
        d.rx.recv().await.unwrap(); // FinalResponse
        d.handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_error_produces_an_error_event_not_a_final_response() {
        // Renamed from `http_error_produces_api_error_final_response`: a 500 used
        // to arrive as a *final response*, which every frontend then drew in the
        // assistant's own box and wrote into chats.json as an assistant message --
        // a server fault became a permanent thing the model had "said". It is an
        // error event now, and it stays an error event.
        let (base, _requests) = stub_server_status(
            500,
            vec![serde_json::json!({"error": {"message": "boom"}})],
        );

        let mut d = start_chat(
            &base,
            vec![user_msg("hi")],
            ToolConfirmation::None,
            "",
            false,
        );
        match d.rx.recv().await.unwrap() {
            LlmEvent::Error { kind, message } => {
                assert_eq!(kind, ERROR_KIND_ERROR);
                assert!(message.contains("API error"), "got: {message}");
                assert!(message.contains("boom"), "got: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(d.rx.try_recv().is_err(), "no final response may follow");
        d.handle.await.unwrap();
    }
}
