//! LLM client for OpenAI-compatible APIs.
//!
//! The Python generator becomes an async function that sends events via a channel
//! and receives tool confirmations via a separate channel.
//! This is the heart of the app — all three frontends drive the same logic.

use crate::chat_manager::ChatMessage;
use crate::tools;
use crate::context_recovery::{Recovery, SUMMARY_PROMPT};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

// ── Graceful image-stripping helpers ────────────────────────────

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

fn is_image_input_error(status_code: u16, body: &serde_json::Value, error_text: &str) -> bool {
    if status_code != 400 {
        return false;
    }
    let error = &body["error"];
    // Structured local errors are authoritative. Invalid images, unsupported
    // audio/options/roles/detail, etc. must not be hidden by stripping images.
    if error["source"].as_str() == Some("openai-proxy") {
        return error["code"].as_str() == Some("unsupported_content_type")
            && error["content_type"].as_str() == Some("image_url");
    }
    let lower = error_text.to_lowercase();
    // Compatibility with the original text-only adapter; do not mistake this
    // endpoint limitation for proof that the underlying model lacks vision.
    if lower.trim_end_matches('.') == "only text content parts are supported by this upstream format" {
        return true;
    }
    let unsupported_input_phrases = [
        "does not support image", "doesn't support image", "do not support image",
        "does not support vision", "does not support multimodal",
        "image inputs are not supported", "image input is not supported",
        "images are not supported", "image_url is not supported",
        "unsupported image input", "unsupported vision input",
    ];
    unsupported_input_phrases.iter().any(|phrase| lower.contains(phrase))
        || ((lower.contains("text-only") || lower.contains("only text"))
            && ["image", "vision", "multimodal"].iter().any(|kw| lower.contains(kw)))
}

fn image_rejection_notice(body: &serde_json::Value, detail: &str) -> &'static str {
    if body["error"]["source"].as_str() == Some("openai-proxy")
        || detail.trim_end_matches('.') == "Only text content parts are supported by this upstream format" {
        "[The proxy adapter cannot translate image inputs for this route. Images were omitted from this request; only file metadata is available. This is not evidence that the underlying model lacks vision. Do not claim to have inspected the images.]"
    } else {
        "[The API endpoint rejected image/vision inputs as unsupported. Images were omitted from this request; only file metadata is available. Do not claim to have inspected the images.]"
    }
}

// ── 429 / 529 backoff ────────────────────────────────────────────
const MAX_RETRIES: u32 = 5;
const BASE_DELAY_SECS: f64 = 1.0;
const MAX_DELAY_SECS: f64 = 60.0;
const JITTER: f64 = 0.25;
const RETRYABLE_STATUSES: &[u16] = &[429, 529];

// Context errors get size-reduction retries, never generic bad requests.
const MAX_CONTEXT_RETRIES: u32 = 4;
/// A length stop this short is context starvation, not a plausible output cap;
/// larger (or unreported) completions keep the safe truncation failure.
const SHORT_LENGTH_COMPLETION_TOKENS: u64 = 1024;
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
    "leaves no room to answer in the context",
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
        /// Running turn usage so a frontend can advance its token count before
        /// the turn ends, instead of waiting for [`LlmEvent::FinalResponse`].
        /// Same accumulator the final response reports, so the live total and
        /// the terminal total agree. `#[serde(default)]` keeps older event
        /// consumers (and the FFI's legacy tests) deserialising.
        #[serde(default)]
        usage: Usage,
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
        /// Running turn usage (see [`LlmEvent::ToolRequest`]). The
        /// ask_user_question round emits no tool_request, so it carries the
        /// live total itself.
        #[serde(default)]
        usage: Usage,
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
        /// Final response output tokens / successful HTTP request wall time.
        /// Includes latency/prefill/reasoning, excludes tools and retry waits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_per_second: Option<f64>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

/// Keep only our tagged proxy's opaque replay state independent of the UI's
/// optional preservation of ordinary, possibly human-readable reasoning.
/// The proxy itself checks both provider and model before replaying it.
fn strip_cross_model_proxy_state(messages: &mut [ChatMessage], model: &str) {
    for message in messages {
        if message.reasoning_details.as_ref().is_some_and(|details|
            details.get("format").and_then(|v| v.as_str()) == Some("openai-proxy/reasoning-v1")
            && details.get("proxy_model").and_then(|v| v.as_str()) != Some(model)
        ) {
            message.reasoning_details = None;
        }
    }
}

fn preserved_reasoning_details(msg: &serde_json::Value, preserve_reasoning: bool) -> Option<serde_json::Value> {
    let details = msg.get("reasoning_details")?;
    if preserve_reasoning || details.get("format").and_then(|v| v.as_str()) == Some("openai-proxy/reasoning-v1") {
        return Some(details.clone());
    }
    None
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

    let options = tool_ctx.recovery.lock().unwrap().clone();
    if !["max_tokens", "max_completion_tokens"].contains(&options.output_parameter.as_str()) {
        emit_config_error(&event_tx, "output_token_parameter must be max_tokens or max_completion_tokens".into());
        return;
    }
    let mut recovery = Recovery::new(&serde_json::to_value(&current_messages).unwrap().as_array().unwrap(), base_url, model, options);
    let mut image_rejected = false;
    let mut rejection_notice = String::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        // Attachment refs are local history only. Resolve image derivatives at
        // request time so provider payloads never leak back into chat JSON.
        let raw = serde_json::to_value(&current_messages).unwrap();
        let reduced: Vec<ChatMessage> = serde_json::from_value(serde_json::Value::Array(recovery.apply(raw.as_array().unwrap()))).unwrap();
        let mut api_messages = provider_messages(&reduced, attachment_context_keep_turns);
        if image_rejected { strip_image_url_parts(&mut api_messages); api_messages.push(ChatMessage::new("user", Some(serde_json::json!(rejection_notice)))); }
        // Proxy envelopes are opaque state tied to one model and upstream.
        // Keep them in persisted chat history, but don't leak them to a
        // different selected model (including model overrides in a tab).
        strip_cross_model_proxy_state(&mut api_messages, model);
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
        if recovery.options.output_limit > 0 { payload[&recovery.options.output_parameter] = serde_json::json!(recovery.options.output_limit); }

        // ── API call with 429 / 529 exponential backoff ──────────
        let (resp, request_started) = {
            let mut last_status: Option<reqwest::StatusCode> = None;
            let mut last_body: Option<serde_json::Value> = None;
            let mut success = None;
            let mut rate_retries = 0;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    // Cancelled during backoff — emit nothing, just return
                    return;
                }
                let request_started = std::time::Instant::now();
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
                            success = Some((r, request_started));
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
                        if is_image_input_error(code, &body, detail)
                            && !is_context_limit_error(code, &body, detail)
                            && has_image_url_parts(&request_messages)
                        {
                            // Retry the provider copy only. Stripping stored
                            // messages would miss attachment refs, which get
                            // resolved back into image parts on the next turn.
                            image_rejected = true;
                            rejection_notice = image_rejection_notice(&body, detail).into();
                            strip_image_url_parts(&mut request_messages);
                            request_messages.push(ChatMessage::new("user", Some(
                                serde_json::Value::String(image_rejection_notice(&body, detail).into()),
                            )));
                            payload["messages"] = serde_json::to_value(&request_messages).unwrap();
                            continue;
                        }

                        if is_context_limit_error(code, &body, detail) {
                            if recovery.options.enabled {
                                match recover(&mut recovery, &current_messages, &client, &url, api_key, model, &cancel, &mut accumulated_usage).await {
                                    Ok(Some(event)) => {
                                        let _ = event_tx.send(event);
                                        let raw = serde_json::to_value(&current_messages).unwrap();
                                        let reduced: Vec<ChatMessage> = serde_json::from_value(serde_json::Value::Array(recovery.apply(raw.as_array().unwrap()))).unwrap();
                                        request_messages = provider_messages(&reduced, attachment_context_keep_turns);
                                        strip_cross_model_proxy_state(&mut request_messages, model);
                                        if image_rejected { strip_image_url_parts(&mut request_messages); request_messages.push(ChatMessage::new("user", Some(serde_json::json!(rejection_notice)))); }
                                        payload["messages"] = serde_json::to_value(&request_messages).unwrap();
                                        continue;
                                    }
                                    result => {
                                        let detail = result.err().unwrap_or_else(|| "Model context limit reached; could not fit the protected task after bounded recovery. Full history retained; try a shorter request or a new chat.".into());
                                        let _ = event_tx.send(LlmEvent::Error { kind: "error".into(), message: detail });
                                        return;
                                    }
                                }
                            }
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
                                        message: None,
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
        let request_seconds = request_started.elapsed().as_secs_f64();
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

        // Fail before emitting/persisting assistant messages or executing tools.
        // A length-truncated tool sequence is unsafe even if its JSON parses.
        if choice["finish_reason"].as_str() == Some("length") {
            let has_tool_calls = tool_calls.is_some_and(|calls| !calls.is_empty());
            // The truncated reply is discarded unexecuted; a retry regenerates
            // it against a smaller provider view.
            if length_suggests_context_pressure(&content, has_tool_calls, &body["usage"], recovery.options.output_limit) {
                match recover(&mut recovery, &current_messages, &client, &url, api_key, model, &cancel, &mut accumulated_usage).await {
                    Ok(Some(event)) => { let _ = event_tx.send(event); continue; }
                    Err(detail) => { let _ = event_tx.send(LlmEvent::Error { kind: "error".into(), message: detail }); return; }
                    _ => {}
                }
            }
            let _ = event_tx.send(LlmEvent::Error {
                kind: "truncated".into(),
                message: generation_limit_message(&content, has_tool_calls, &body["usage"], recovery.attempts),
            });
            return;
        }

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
                    // Native proxy state is opaque and required to resume tool turns,
                    // regardless of the UI preference for ordinary reasoning fields.
                    reasoning_details: preserved_reasoning_details(msg, preserve_reasoning),
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
                            usage: accumulated_usage.clone(),
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
                            usage: accumulated_usage.clone(),
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
                            usage: accumulated_usage.clone(),
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
            reasoning_details: preserved_reasoning_details(msg, preserve_reasoning),
        };
        let _ = event_tx.send(LlmEvent::FinalResponse {
            content,
            message: Some(final_msg),
            usage: accumulated_usage,
            tokens_per_second: response_tokens_per_second(&body, request_seconds),
        });
        return;
    }
}

/// Execute bounded, tool-free summary calls; commit only a smaller complete plan.
async fn recover(recovery: &mut Recovery, messages: &[ChatMessage], client: &reqwest::Client,
                 url: &str, key: &str, model: &str, cancel: &AtomicBool, usage: &mut Usage) -> Result<Option<LlmEvent>, String> {
    let raw = serde_json::to_value(messages).unwrap();
    let Some(plan) = recovery.plan(raw.as_array().unwrap()) else { return Ok(None); };
    let mut summaries = vec![];
    for chunk in &plan.chunks {
        if cancel.load(Ordering::Relaxed) { return Err("Context recovery cancelled.".into()); }
        recovery.summary_calls += 1;
        let mut payload = serde_json::json!({"model": model, "messages":[{"role":"system","content":SUMMARY_PROMPT},{"role":"user","content":chunk}]});
        payload[&recovery.options.output_parameter] = serde_json::json!(2048);
        let body: serde_json::Value = client.post(url).bearer_auth(key).header("api-key",key).json(&payload).send().await.map_err(|e| e.to_string())?
            .error_for_status().map_err(|e| format!("Context summary failed; history retained: {e}"))?.json().await.map_err(|e| e.to_string())?;
        if let Some(u) = body["usage"].as_object() {
            usage.prompt_tokens += u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            usage.completion_tokens += u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            usage.total_tokens += u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        }
        if cancel.load(Ordering::Relaxed) { return Err("Context recovery cancelled.".into()); }
        let choice = &body["choices"][0];
        let text = choice["message"]["content"].as_str().unwrap_or("");
        if choice["finish_reason"] != "stop" || text.trim().is_empty() || choice["message"]["tool_calls"].as_array().is_some_and(|a| !a.is_empty()) {
            return Err("Context summary was incomplete; original history retained.".into());
        }
        summaries.push(text.to_string());
    }
    Ok(recovery.commit(plan,summaries,raw.as_array().unwrap())?.map(|v| LlmEvent::ContextCompacted {
        attempt:v["attempt"].as_u64().unwrap() as u32, max_attempts:4,
        chars_removed:v["chars_removed"].as_u64().unwrap() as usize,
        message:v["message"].as_str().map(String::from),
    }))
}

/// Whether a length stop is worth a context-reduction retry.
///
/// An empty answer always qualifies. A partial answer or truncated tool call
/// qualifies only when the provider reports a completion too short to be an
/// output cap -- typically a lead-in like "Redoing it properly:" cut off just
/// before its tool call because the window was nearly full.
fn length_suggests_context_pressure(content: &str, has_tool_calls: bool, usage: &serde_json::Value, output_limit: u64) -> bool {
    if !has_tool_calls && content.trim().is_empty() {
        return true;
    }
    let limit = if output_limit > 0 { SHORT_LENGTH_COMPLETION_TOKENS.min(output_limit) } else { SHORT_LENGTH_COMPLETION_TOKENS };
    usage["completion_tokens"].as_u64().is_some_and(|tokens| tokens < limit)
}

/// `1234567` -> `"1,234,567"`, matching Python's `{:,}` in the shared message.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 { out.push(','); }
        out.push(c);
    }
    out
}

/// Length means generation exhaustion, not necessarily a full context window.
fn generation_limit_message(content: &str, has_tool_calls: bool, usage: &serde_json::Value, recovery_attempts: u32) -> String {
    let detail = if has_tool_calls {
        "Generation limit reached during tool calls; no tools from this response were executed."
    } else if content.trim().is_empty() {
        "Generation limit reached before an answer was produced."
    } else {
        "Generation limit reached; the answer is incomplete."
    };
    let counts = match (usage["prompt_tokens"].as_u64(), usage["completion_tokens"].as_u64()) {
        (Some(prompt), Some(completion)) => format!(" (prompt {} tokens, completion {} tokens)", thousands(prompt), thousands(completion)),
        _ => String::new(),
    };
    let retried = if recovery_attempts > 0 {
        format!("Pengy retried after {recovery_attempts} context reduction(s) without success.")
    } else {
        "Pengy did not retry automatically.".to_string()
    };
    let mut message = format!(
        "{detail} The provider reported finish_reason=length{counts}. This can mean an output-token cap or insufficient remaining context. Try a shorter conversation, a larger output allowance, or a reasoning budget that leaves room for an answer. {retried}"
    );
    if !content.trim().is_empty() {
        message.push_str(&format!("\n\nPartial response (incomplete, not saved as an answer):\n{content}"));
    }
    message
}

fn response_tokens_per_second(body: &serde_json::Value, seconds: f64) -> Option<f64> {
    let tokens = body["usage"]["completion_tokens"].as_u64()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let rate = tokens as f64 / seconds;
    rate.is_finite().then_some(rate)
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
        let event = LlmEvent::ContextCompacted { attempt: 1, max_attempts: 4, chars_removed: 9000, message: None };
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
            usage: Usage {
                prompt_tokens: 40,
                completion_tokens: 8,
                total_tokens: 48,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tool_request\""));
        let parsed: LlmEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            LlmEvent::ToolRequest {
                name,
                tool_call_id,
                usage,
                ..
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(tool_call_id, "tc-123");
                assert_eq!(usage.total_tokens, 48);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tool_request_without_usage_deserialises_as_zero() {
        // The field is additive: an older producer that omits it must not break
        // the consumer (the GUI parses these events from the FFI).
        let legacy = r#"{"type":"tool_request","name":"read_file","args":{},"tool_call_id":"tc-1"}"#;
        match serde_json::from_str::<LlmEvent>(legacy).unwrap() {
            LlmEvent::ToolRequest { usage, .. } => assert_eq!(usage.total_tokens, 0),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn final_response_rate_uses_only_output_tokens() {
        let body = serde_json::json!({"usage":{"prompt_tokens":1000,"completion_tokens":30,"total_tokens":1030}});
        assert_eq!(response_tokens_per_second(&body, 2.0), Some(15.0));
        assert_eq!(response_tokens_per_second(&body, 0.0), None);
        assert_eq!(response_tokens_per_second(&body, f64::NAN), None);
        assert_eq!(response_tokens_per_second(&serde_json::json!({}), 2.0), None);
        assert_eq!(response_tokens_per_second(&serde_json::json!({"usage":{"completion_tokens":0}}), 2.0), Some(0.0));
        let legacy = r#"{"type":"final_response","content":"ok","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        assert!(matches!(serde_json::from_str::<LlmEvent>(legacy).unwrap(), LlmEvent::FinalResponse {tokens_per_second: None, ..}));
    }

    #[test]
    fn llm_event_final_response_serde() {
        let event = LlmEvent::FinalResponse {
            content: "Hello!".into(),
            message: None,
            tokens_per_second: Some(25.0),
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
    fn native_proxy_reasoning_is_preserved_without_user_toggle() {
        let opaque = serde_json::json!({"format":"openai-proxy/reasoning-v1",
            "provider":"openai_responses", "proxy_model":"astra", "model":"upstream-astra",
            "blocks":[{"type":"reasoning", "encrypted_content":"opaque"}]});
        let response = serde_json::json!({"reasoning_details":opaque});
        assert_eq!(preserved_reasoning_details(&response, false), Some(opaque.clone()));
        let mut assistant = ChatMessage::new("assistant", Some(serde_json::json!("Hi")));
        assistant.reasoning_details = Some(opaque.clone());
        let history = vec![assistant];
        let mut same_model = provider_messages(&history, 0);
        strip_cross_model_proxy_state(&mut same_model, "astra");
        assert_eq!(same_model[0].reasoning_details, Some(opaque));
        let mut changed_model = provider_messages(&history, 0);
        strip_cross_model_proxy_state(&mut changed_model, "different");
        assert!(changed_model[0].reasoning_details.is_none());
        assert!(history[0].reasoning_details.is_some(), "stored history must remain intact");
    }

    #[test]
    fn foreign_reasoning_respects_user_toggle() {
        let response = serde_json::json!({"reasoning_details":[{"type":"other","text":"x"}]});
        assert!(preserved_reasoning_details(&response, false).is_none());
        assert!(preserved_reasoning_details(&response, true).is_some());
    }

    #[test]
    fn image_recovery_requires_explicit_unsupported_input_signal() {
        let empty = serde_json::json!({});
        assert!(is_image_input_error(400, &empty, "Only text content parts are supported by this upstream format"));
        assert!(is_image_input_error(400, &empty, "This model does not support image inputs"));
        for message in ["Unsupported parameter: temperature", "Invalid image URL",
            "Unsupported image format", "Unsupported image detail", "context length exceeded"] {
            assert!(!is_image_input_error(400, &empty, message), "{message}");
        }
        let mut body = serde_json::json!({"error":{"source":"openai-proxy",
            "code":"unsupported_content_type", "content_type":"image_url"}});
        assert!(is_image_input_error(400, &body, "cannot translate"));
        assert!(!is_image_input_error(500, &body, "cannot translate"));
        body["error"]["content_type"] = serde_json::json!("input_audio");
        assert!(!is_image_input_error(400, &body, "images unsupported"));
        body["error"]["code"] = serde_json::json!("invalid_chat_request");
        assert!(!is_image_input_error(400, &body, "images unsupported"));
    }

    #[test]
    fn provider_image_stripping_preserves_local_history_and_attachment_refs() {
        let mut original = attached_user("look", "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        original.content = Some(serde_json::json!([
            {"type":"text", "text":"look"},
            {"type":"image_url", "image_url":{"url":"data:image/png;base64,aW1hZ2U="}}
        ]));
        let history = vec![original];
        let mut outgoing = history.clone();
        outgoing[0].attachments.clear();
        strip_image_url_parts(&mut outgoing);
        assert!(!has_image_url_parts(&outgoing));
        assert_eq!(outgoing[0].content.as_ref().unwrap(), "look");
        assert!(has_image_url_parts(&history));
        assert_eq!(history[0].attachments.len(), 1);
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
    async fn fourth_recovery_attempt_reaches_summary_for_context_and_empty_length() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("../tests/fixtures/context_recovery_reserve.json")).unwrap();
        let fixture = &cases[0];
        for empty_length in [false, true] {
            let messages: Vec<ChatMessage> = serde_json::from_value(fixture["messages"].clone()).unwrap();
            let original = serde_json::to_value(&messages).unwrap();
            let mut sequence = vec![];
            for _ in 0..4 {
                if empty_length {
                    let mut blank = completion("", serde_json::Value::Null, (10, 48));
                    blank["choices"][0]["finish_reason"] = serde_json::json!("length");
                    sequence.push((200, blank));
                } else {
                    sequence.push((400, serde_json::json!({"error":{"code":"context_length_exceeded","message":"context length exceeded"}})));
                }
            }
            sequence.push((200, completion(fixture["summary"].as_str().unwrap(), serde_json::Value::Null, (10, 5))));
            sequence.push((200, completion("HARBOR_17 /tmp/harbor-17 audit pending", serde_json::Value::Null, (10, 5))));
            let (base, requests) = stub_server_sequence(sequence);
            let mut d = start_chat(&base, messages.clone(), ToolConfirmation::All, "", false);
            for attempt in 1..=4 {
                assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ContextCompacted { attempt: n, .. } if n == attempt));
            }
            assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::FinalResponse { content, .. } if content.contains("HARBOR_17")));
            d.handle.await.unwrap();
            assert!(d.rx.recv().await.is_none());
            let req = requests.lock().unwrap();
            assert_eq!(req.len(), 6);
            assert!(req[4].get("tools").is_none());
            assert!(req[4]["messages"][1]["content"].as_str().unwrap().contains("HARBOR_17"));
            let outgoing = req[5]["messages"].as_array().unwrap();
            assert!(outgoing[1]["content"].as_str().unwrap().contains("HARBOR_17"));
            assert!(outgoing.last().unwrap()["content"].as_str().unwrap().starts_with('B'));
            assert_eq!(outgoing[outgoing.len()-2]["tool_calls"][0]["id"], "newest");
            assert_eq!(serde_json::to_value(&messages).unwrap(), original);
        }
    }

    fn recoverable_history() -> Vec<ChatMessage> {
        let mut messages = vec![user_msg(&format!("Old requirement: HARBOR_17. {}", "history ".repeat(1000))), ChatMessage::new("assistant", Some(serde_json::json!("done")))];
        for _ in 0..4 { messages.push(user_msg("recent")); messages.push(ChatMessage::new("assistant", Some(serde_json::json!("ok")))); }
        messages.push(user_msg("current"));
        messages
    }

    fn length_stop(content: &str, tool_calls: serde_json::Value, completion_tokens: u64) -> serde_json::Value {
        let mut reply = completion(content, tool_calls, (250000, completion_tokens));
        reply["choices"][0]["finish_reason"] = serde_json::json!("length");
        reply
    }

    #[test]
    fn short_length_completions_suggest_context_pressure() {
        let usage = |n: u64| serde_json::json!({"prompt_tokens": 250000, "completion_tokens": n});
        assert!(length_suggests_context_pressure(" ", false, &serde_json::Value::Null, 0));
        assert!(length_suggests_context_pressure("Redoing it properly:", false, &usage(14), 0));
        assert!(length_suggests_context_pressure("Writing:", true, &usage(40), 0));
        assert!(!length_suggests_context_pressure("A long partial answer", false, &usage(4096), 0));
        assert!(!length_suggests_context_pressure("Capped", false, &usage(256), 256));
        assert!(!length_suggests_context_pressure("Unreported", false, &serde_json::Value::Null, 0));
        let message = generation_limit_message("partial", false, &usage(4096), 0);
        assert!(message.contains("finish_reason=length (prompt 250,000 tokens, completion 4,096 tokens)"), "{message}");
        assert!(message.contains("did not retry automatically"));
        assert!(generation_limit_message("partial", false, &usage(8), 3).contains("retried after 3 context reduction(s) without success"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn short_partial_length_recovers_and_discards_lead_in() {
        let lead_in = "The mutations never applied — redoing it properly:";
        let messages = recoverable_history();
        let original = serde_json::to_value(&messages).unwrap();
        let (base, requests) = stub_server(vec![length_stop(lead_in, serde_json::Value::Null, 14),
            completion("HARBOR_17", serde_json::Value::Null, (20, 10)), completion("recovered", serde_json::Value::Null, (30, 5))]);
        let mut d = start_chat(&base, messages.clone(), ToolConfirmation::All, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ContextCompacted { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::FinalResponse { content, .. } if content == "recovered"));
        d.handle.await.unwrap();
        assert!(d.rx.recv().await.is_none());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1..].iter().all(|r| !r.to_string().contains(lead_in)));
        assert_eq!(serde_json::to_value(&messages).unwrap(), original);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn short_truncated_tool_call_recovers_without_executing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("must-not-exist");
        let mut call = tool_call("tc1", "write_file", &serde_json::json!({"path": target, "content": "unsafe"}));
        call["function"]["arguments"] = serde_json::json!("{\"path\":");
        let (base, _requests) = stub_server(vec![length_stop("Writing:", serde_json::json!([call]), 40),
            completion("HARBOR_17", serde_json::Value::Null, (20, 10)), completion("recovered", serde_json::Value::Null, (30, 5))]);
        let mut d = start_chat(&base, recoverable_history(), ToolConfirmation::All, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ContextCompacted { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::FinalResponse { content, .. } if content == "recovered"));
        d.handle.await.unwrap();
        assert!(!target.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn long_partial_length_still_fails_safely_with_token_counts() {
        let (base, requests) = stub_server(vec![length_stop("A long partial answer", serde_json::Value::Null, 4096)]);
        let mut d = start_chat(&base, recoverable_history(), ToolConfirmation::All, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::Error { kind, message }
            if kind == "truncated" && message.contains("completion 4,096 tokens") && message.contains("did not retry automatically")));
        d.handle.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_length_summarizes_history_without_emitting_blank() {
        let mut messages=vec![user_msg(&format!("Old requirement: HARBOR_17; /tmp/harbor-17; audit pending. {}","history ".repeat(1000))), ChatMessage::new("assistant",Some(serde_json::json!("done")))];
        for _ in 0..4 { messages.push(user_msg("recent")); messages.push(ChatMessage::new("assistant",Some(serde_json::json!("ok")))); }
        messages.push(user_msg("current"));
        let original=serde_json::to_value(&messages).unwrap();
        let mut blank=completion("",serde_json::Value::Null,(10,48));blank["choices"][0]["finish_reason"]=serde_json::json!("length");
        let (base,requests)=stub_server(vec![blank,completion("HARBOR_17; /tmp/harbor-17; audit pending",serde_json::Value::Null,(20,10)),completion("recovered",serde_json::Value::Null,(30,5))]);
        let mut d=start_chat(&base,messages.clone(),ToolConfirmation::All,"",false);
        assert!(matches!(d.rx.recv().await.unwrap(),LlmEvent::ContextCompacted{message:Some(_),..}));
        assert!(matches!(d.rx.recv().await.unwrap(),LlmEvent::FinalResponse{content,usage,..} if content=="recovered" && usage.total_tokens==123));
        d.handle.await.unwrap();assert!(d.rx.recv().await.is_none());
        let requests=requests.lock().unwrap();assert_eq!(requests.len(),3);assert!(requests[1].get("tools").is_none());
        assert_eq!(requests[1]["messages"][0]["content"],SUMMARY_PROMPT);
        assert_eq!(serde_json::to_value(&messages).unwrap(),original);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn length_completions_fail_once_without_assistant_events() {
        for content in [serde_json::Value::Null, serde_json::json!(""), serde_json::json!(" \n\t"), serde_json::json!("## Partial plan… 🐧")] {
            let mut reply = completion("", serde_json::Value::Null, (10, 48));
            reply["choices"][0]["finish_reason"] = serde_json::json!("length");
            reply["choices"][0]["message"]["content"] = content.clone();
            let (base, requests) = stub_server(vec![reply]);
            let history = vec![user_msg("think hard")];
            let original = serde_json::to_value(&history).unwrap();
            let mut d = start_chat(&base, history.clone(), ToolConfirmation::All, "", false);
            match d.rx.recv().await.unwrap() {
                LlmEvent::Error { kind, message } => {
                    assert_eq!(kind, "truncated");
                    assert!(message.contains("finish_reason=length"));
                    assert!(message.contains("output-token cap or insufficient remaining context"));
                    assert!(message.contains("did not retry automatically"));
                    if content.as_str().is_some_and(|text| !text.trim().is_empty()) {
                        assert!(message.contains("answer is incomplete"));
                        assert!(message.contains(content.as_str().unwrap()));
                        assert!(message.contains("not saved as an answer"));
                    } else {
                        assert!(message.contains("before an answer was produced"));
                    }
                }
                other => panic!("truncation must only emit an error, got {other:?}"),
            }
            d.handle.await.unwrap();
            assert!(d.rx.recv().await.is_none());
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert_eq!(serde_json::to_value(&history).unwrap(), original);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn length_tool_calls_never_execute_even_when_arguments_parse() {
        for malformed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("must-not-exist");
            let args = serde_json::json!({"path": target, "content": "unsafe"});
            let mut call = tool_call("tc1", "write_file", &args);
            if malformed { call["function"]["arguments"] = serde_json::json!("{\"path\":"); }
            let mut reply = completion("About to write", serde_json::json!([call]), (10, 48));
            reply["choices"][0]["finish_reason"] = serde_json::json!("length");
            let (base, requests) = stub_server(vec![reply]);
            let mut d = start_chat(&base, vec![user_msg("write")], ToolConfirmation::All, "", false);
            assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::Error { kind, message }
                if kind == "truncated" && message.contains("no tools from this response were executed")));
            d.handle.await.unwrap();
            assert!(d.rx.recv().await.is_none());
            assert!(!target.exists());
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn length_after_completed_tool_does_not_rerun_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("done.txt");
        let args = serde_json::json!({"path": target, "content": "done"});
        let first = completion("", serde_json::json!([tool_call("tc1", "write_file", &args)]), (10, 5));
        let mut last = completion("", serde_json::Value::Null, (20, 48));
        last["choices"][0]["finish_reason"] = serde_json::json!("length");
        let (base, requests) = stub_server(vec![first, last]);
        let mut d = start_chat(&base, vec![user_msg("write")], ToolConfirmation::All, "", false);
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::AssistantToolCalls { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ToolRequest { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ToolResult { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::Error { kind, .. } if kind == "truncated"));
        d.handle.await.unwrap();
        assert!(d.rx.recv().await.is_none());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "done");
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_tool_only_responses_with_missing_or_tool_finish_reason_still_run() {
        for reason in [None, Some("tool_calls")] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("safe.txt");
            let args = serde_json::json!({"path": target, "content": "done"});
            let mut first = completion("", serde_json::json!([tool_call("tc1", "write_file", &args)]), (10, 5));
            if let Some(reason) = reason {
                first["choices"][0]["finish_reason"] = serde_json::json!(reason);
            } else {
                first["choices"][0].as_object_mut().unwrap().remove("finish_reason");
            }
            let (base, requests) = stub_server(vec![first, completion("written", serde_json::Value::Null, (20, 5))]);
            let mut d = start_chat(&base, vec![user_msg("write")], ToolConfirmation::All, "", false);
            assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::AssistantToolCalls { .. }));
            assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ToolRequest { .. }));
            assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ToolResult { .. }));
            assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::FinalResponse { content, .. } if content == "written"));
            d.handle.await.unwrap();
            assert_eq!(std::fs::read_to_string(target).unwrap(), "done");
            assert_eq!(requests.lock().unwrap().len(), 2);
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
            LlmEvent::FinalResponse { content, usage, tokens_per_second, .. } => {
                assert!(tokens_per_second.unwrap() > 0.0);
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
    async fn tagged_proxy_state_survives_tool_loop_without_reasoning_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello").unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()});
        let envelope = serde_json::json!({"format":"openai-proxy/reasoning-v1",
            "provider":"openai_responses", "proxy_model":"stub-model", "model":"upstream-stub",
            "blocks":[{"type":"reasoning", "encrypted_content":"opaque"}]});
        let first = serde_json::json!({"choices":[{"message":{
            "role":"assistant", "content":"", "tool_calls":[tool_call("tc1", "read_file", &args)],
            "reasoning_details":envelope}}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}});
        let (base, requests) = stub_server(vec![first, completion("done", serde_json::Value::Null, (5, 1))]);
        let mut d = start_chat(&base, vec![user_msg("read")], ToolConfirmation::All, "", false);
        while let Some(event) = d.rx.recv().await {
            if matches!(event, LlmEvent::FinalResponse { .. } | LlmEvent::Error { .. }) { break; }
        }
        d.handle.await.unwrap();
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[1]["messages"][1]["reasoning_details"], envelope);
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
    async fn read_image_unsupported_input_recovers_once_without_losing_tool_state() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shot.png");
        image::RgbImage::from_pixel(48, 32, image::Rgb([10, 120, 200])).save(&file).unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()});
        let envelope = serde_json::json!({"format":"openai-proxy/reasoning-v1",
            "provider":"anthropic_messages", "proxy_model":"stub-model", "model":"upstream",
            "blocks":[{"type":"tool_use", "id":"tc1", "name":"read_image", "input":args}]});
        for error in [
            serde_json::json!({"error":{"source":"openai-proxy", "code":"unsupported_content_type",
                "content_type":"image_url", "message":"Adapter cannot translate pictures"}}),
            serde_json::json!({"error":"Only text content parts are supported by this upstream format"}),
        ] {
            let mut first = completion("", serde_json::json!([tool_call("tc1", "read_image", &args)]), (10, 5));
            first["choices"][0]["message"]["reasoning_details"] = envelope.clone();
            let (base, requests) = stub_server_sequence(vec![
                (200, first), (400, error),
                (200, completion("I could not inspect the image", serde_json::Value::Null, (10, 5)))]);
            let mut d = start_chat(&base, vec![user_msg("look")], ToolConfirmation::All, "", false);
            let mut tool_results = 0;
            let mut final_response = false;
            while let Some(event) = d.rx.recv().await {
                match event {
                    LlmEvent::ToolResult { .. } => tool_results += 1,
                    LlmEvent::FinalResponse { .. } => { final_response = true; break; },
                    LlmEvent::Error { message, .. } => panic!("unexpected failure: {message}"),
                    _ => {}
                }
            }
            d.handle.await.unwrap();
            assert!(final_response);
            assert_eq!(tool_results, 1);
            let req = requests.lock().unwrap();
            assert_eq!(req.len(), 3);
            let before: Vec<ChatMessage> = serde_json::from_value(req[1]["messages"].clone()).unwrap();
            let after: Vec<ChatMessage> = serde_json::from_value(req[2]["messages"].clone()).unwrap();
            assert!(has_image_url_parts(&before));
            assert!(!has_image_url_parts(&after));
            assert_eq!(req[2]["messages"][1]["reasoning_details"], envelope);
            assert_eq!(req[2]["messages"][2]["tool_call_id"], "tc1");
            let notice = after.last().unwrap().content.as_ref().unwrap().as_str().unwrap();
            assert!(notice.contains("proxy adapter"));
            assert!(notice.contains("not evidence"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_image_bad_request_is_not_hidden_by_image_recovery() {
        for message in ["Unsupported parameter: temperature", "Invalid image URL", "Unsupported image format"] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("shot.png");
            image::RgbImage::from_pixel(48, 32, image::Rgb([10, 120, 200])).save(&file).unwrap();
            let args = serde_json::json!({"path": file.to_str().unwrap()});
            let (base, requests) = stub_server_sequence(vec![
                (200, completion("", serde_json::json!([tool_call("tc1", "read_image", &args)]), (10, 5))),
                (400, serde_json::json!({"error":{"message":message}}))]);
            let mut d = start_chat(&base, vec![user_msg("look")], ToolConfirmation::All, "", false);
            let mut failed = false;
            while let Some(event) = d.rx.recv().await {
                match event {
                    LlmEvent::Error { message: detail, .. } => { assert!(detail.contains(message)); failed = true; break; },
                    LlmEvent::FinalResponse { .. } => panic!("error must not become an answer"),
                    _ => {}
                }
            }
            d.handle.await.unwrap();
            assert!(failed);
            assert_eq!(requests.lock().unwrap().len(), 2);
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
