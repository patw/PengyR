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

const CACHED_IMAGE_NOTICE: &str = "[Images omitted: this endpoint/model previously rejected image inputs. Original attachments are retained; only text and file metadata are available. This is not evidence that the underlying model lacks vision. Do not claim to have inspected the omitted images.]";

fn suppress_images(messages: &mut [ChatMessage], notice: &str) {
    let first = messages.iter().position(|m| !m.attachments.is_empty() || has_image_url_parts(std::slice::from_ref(m)));
    for msg in messages.iter_mut() {
        let labels: Vec<String> = msg.attachments.iter().map(|r| format!("[image: {}]", r.name)).collect();
        msg.attachments.clear();
        if !labels.is_empty() {
            match &mut msg.content {
                Some(serde_json::Value::String(text)) => { text.push('\n'); text.push_str(&labels.join(" ")); }
                Some(serde_json::Value::Array(parts)) => parts.push(serde_json::json!({"type":"text","text":labels.join(" ")})),
                _ => msg.content = Some(serde_json::Value::String(labels.join(" "))),
            }
        }
    }
    strip_image_url_parts(messages);
    if let Some(first) = first {
        match &mut messages[first].content {
            Some(serde_json::Value::String(text)) => { text.push_str("\n\n"); text.push_str(notice); }
            Some(serde_json::Value::Array(parts)) => parts.push(serde_json::json!({"type":"text","text":notice})),
            _ => messages[first].content = Some(serde_json::Value::String(notice.into())),
        }
    }
}

fn provider_messages_with_images(messages: &[ChatMessage], keep_turns: usize, rejected: bool, notice: &str) -> Vec<ChatMessage> {
    if !rejected { return provider_messages(messages, keep_turns); }
    let mut filtered = messages.to_vec();
    suppress_images(&mut filtered, notice);
    // Suppress references before resolution: no derivative image is rehydrated.
    provider_messages(&filtered, keep_turns)
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

// ── Provider / transport timeouts ─────────────────────────────────
// A timeout usually means "the endpoint was busy", not "the request was wrong",
// so it gets a retry budget of its own.  The budget is deliberately smaller than
// MAX_RETRIES: this request is non-streaming, so the configured timeout is total
// wall clock per attempt, and five retries at the 300 s default would hang a turn
// for ~25 minutes before the user saw the timeout message.  Cancel is polled
// during every backoff sleep.
pub const MAX_TIMEOUT_RETRIES: u32 = 3;
/// 408 Request Timeout, and 504 Gateway Timeout -- a proxy answering for an
/// upstream that ran out of time, which is the same failure seen from the client.
pub const TIMEOUT_STATUSES: &[u16] = &[408, 504];

// ── Transient server errors, and routes that drop mid-task ────────
// 502/503 say "the proxy or upstream is not available right now" -- a restart,
// a rolling deploy, a worker crash.  That is the same busy-endpoint failure a
// timeout is, and a ten-second blip used to kill a multi-hour task.  500 is
// often *deterministic* (the request trips a provider bug), so it buys one
// retry rather than three.
pub const MAX_TRANSIENT_RETRIES: u32 = 3;
pub const MAX_INTERNAL_RETRIES: u32 = 1;
const TRANSIENT_STATUSES: &[u16] = &[502, 503];
/// A connection failure *after* this endpoint has answered is a dropped route
/// (server restart, OOM kill, sleep/wake, a VM's address changing), not a
/// misconfigured endpoint.  First contact keeps failing fast: that is the case
/// the "is your server running?" help is written for.
pub const MAX_UNREACHABLE_RETRIES: u32 = 3;

/// A 200 that carries no usable answer (no choices at all) is the shape a proxy
/// answers when it loses its upstream mid-generation, so it gets one retry --
/// the same reasoning as a 500.
pub const MAX_EMPTY_RETRIES: u32 = 1;
/// `kind` for a turn that ended with no answer: the endpoint answered 200 and
/// produced nothing usable. Frontends draw it as an error, never as a reply.
pub const ERROR_KIND_EMPTY: &str = "empty";

/// A 200 that carries no choices at all.
pub fn no_choices_help() -> String {
    "The endpoint answered with no choices in the response, so there is no answer to \
     deliver. Pengy retried once. This is the endpoint, not your request: the usual causes \
     are a proxy that lost its upstream mid-generation, an engine that died while \
     answering, or a model name the endpoint does not serve. Nothing was added to the chat."
        .to_string()
}

/// A 200 whose message has neither content nor tool calls.  Names the causes that
/// are actually observed: the reasoning-budget case is documented in the field
/// (BotTalk ff4f1c1f) -- a reasoning model whose whole output allowance goes to
/// reasoning tokens answers with empty content and finish_reason=stop.
fn empty_reply_help(message: &serde_json::Value, finish_reason: &str, completion: Option<u64>) -> String {
    let reasoning = message.get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let counts = completion.map(|c| format!(" (completion {c} tokens)")).unwrap_or_default();
    let mut help = format!(
        "The model returned an empty reply{counts}: no content and no tool calls, so the \
         turn has no answer. Nothing was added to the chat. Usual causes: a content filter \
         that wiped the answer; a proxy that lost its upstream mid-generation; a reasoning \
         model that spent the whole output allowance on reasoning tokens."
    );
    if !reasoning.trim().is_empty() {
        help.push_str(" This reply carried reasoning text and no answer, which points at the \
                       reasoning budget: raise the output allowance, lower the reasoning \
                       effort, or choose a non-reasoning model.");
    }
    help.push_str(&format!(
        " The provider reported finish_reason={}. Pengy did not retry: an empty answer is \
         the endpoint's shape, not a blip.",
        if finish_reason.is_empty() { "unset" } else { finish_reason }));
    help
}

/// Endpoints that have answered at least one request in this process.
/// Keyed by base_url, so a second chat/tab cannot inherit another's confidence.
fn answered_endpoints() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static ANSWERED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    ANSWERED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

fn endpoint_has_answered(base_url: &str) -> bool {
    // Poison-tolerant: a panic elsewhere must not turn a save/report call into
    // a second failure.
    let set = answered_endpoints().lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set.contains(base_url)
}

fn mark_endpoint_answered(base_url: &str) {
    let mut set = answered_endpoints().lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set.insert(base_url.to_string());
}

/// Whether to re-send a request that failed with a status code, and how many
/// times: `(budget, reason)`, or `None` to fail immediately.
///
/// 400/401/404 and friends are the request's fault -- no amount of waiting
/// makes the explanatory message arrive faster, so they must not be queued
/// behind a backoff.
fn transient_retry(status: u16) -> Option<(u32, &'static str)> {
    if TIMEOUT_STATUSES.contains(&status) {
        return Some((MAX_TIMEOUT_RETRIES, "timeout"));
    }
    if TRANSIENT_STATUSES.contains(&status) {
        return Some((MAX_TRANSIENT_RETRIES, "server"));
    }
    if status == 500 {
        return Some((MAX_INTERNAL_RETRIES, "server"));
    }
    None
}

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

/// Sleep in 500 ms slices so a cancel stays responsive.
/// Returns `false` when the user cancelled during the sleep.
async fn sleep_interruptible(delay: f64, cancel: &AtomicBool) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs_f64(delay);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return true;
        }
        let remaining = deadline - now;
        tokio::time::sleep(remaining.min(tokio::time::Duration::from_millis(500))).await;
    }
}

/// Retry decisions close 31 seconds after the first retry is approved. This
/// limits admission of additional attempts, not the total wall time: an already
/// admitted request retains its own deadline and an approved backoff may
/// outlast the window. Fast failures retain their attempt-count budgets; a
/// full retry at the default 300-second timeout prevents approving another.
const RETRY_CHAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(31);

/// Whether the chain may launch another attempt: the first decision opens the
/// clock, later ones spend it. One shared clock for every retry path (rate,
/// transient, timeout, unreachable, recovery summary) without threading a
/// deadline through each.
fn retry_chain_open(started: &mut Option<std::time::Instant>, grace: std::time::Duration) -> bool {
    match *started {
        None => { *started = Some(std::time::Instant::now()); true }
        Some(t0) => std::time::Instant::now().duration_since(t0) < grace,
    }
}

/// Announce one transient-failure retry and sleep out.
///
/// *attempt* is the number of retries already spent (0-based), so the ladder
/// matches the 429/529 path: 1s, 2s, 4s.  *reason* is what the frontends label
/// (`"timeout"`, `"server"`, `"unreachable"`).  Returns `false` when the user
/// cancelled during the sleep, in which case the caller stops the turn.
async fn pause_after_transient(
    attempt: u32,
    max_attempts: u32,
    reason: &str,
    status_code: u16,
    detail: &str,
    headers: &reqwest::header::HeaderMap,
    event_tx: &mpsc::UnboundedSender<LlmEvent>,
    cancel: &AtomicBool,
) -> bool {
    let delay = backoff_delay(attempt, extract_retry_after(headers));
    let _ = event_tx.send(LlmEvent::Retrying {
        attempt: attempt + 1,
        max_attempts,
        delay_secs: (delay * 10.0).round() / 10.0,
        status_code,
        message: detail.to_string(),
        reason: Some(reason.to_string()),
    });
    if !sleep_interruptible(delay, cancel).await {
        return false;
    }
    // The announced pause is over and the attempt is launching. Say so, or
    // every frontend keeps advertising "retrying in Xs" for the whole (up to
    // `llm_timeout`) in-flight attempt.
    let _ = event_tx.send(LlmEvent::RequestStarted {
        stage: "turn".into(),
        attempt: attempt + 1,
    });
    true
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
        /// Why the request is being re-sent: `"timeout"` for a provider 408/504
        /// answer or our own deadline, absent for the 429/529 overload path.
        /// Frontends label the two differently.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A request is (back) in flight: a retry attempt just launched after its
    /// announced backoff, or a context-recovery summary call is starting.
    /// [`LlmEvent::Retrying`] announces a pause; this announces the pause is
    /// over, so a frontend can stop advertising the finished backoff
    /// ("retrying in 0.9s") while the attempt runs -- bounded by `llm_timeout`,
    /// with no further events until it resolves.
    #[serde(rename = "request_started")]
    RequestStarted {
        /// `"turn"` for the chat-completion request, `"summary"` for a
        /// context-recovery summary call.
        stage: String,
        /// 1-based attempt number now in flight.
        attempt: u32,
    },
    #[serde(rename = "context_compacted")]
    ContextCompacted {
        attempt: u32,
        max_attempts: u32,
        chars_removed: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
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

/// Report a failed turn with a `kind` the frontends classify directly, bypassing
/// the credential sniffing that [`emit_turn_error`] applies to endpoint text.
fn emit_turn_error_kind(
    event_tx: &mpsc::UnboundedSender<LlmEvent>,
    kind: &str,
    message: String,
    _base_url: &str,
) {
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
    /// ask_user_question only: nobody can answer (headless CLI). The model is
    /// told to decide and state its assumption instead of "cancelled".
    pub unavailable: bool,
}

/// Sent back for ask_user_question when the frontend has no user to ask.
/// Identical to `NO_USER_ANSWER` in the Python and C++ editions.
pub const NO_USER_ANSWER: &str = "No user is available to answer (non-interactive run). If what you found makes one option clearly the most reasonable, take it and state that assumption in your reply. If it would be a pure guess, leave that part undone and say what you need.";

/// Sent back for a tool call that needs confirmation when the frontend has
/// nobody to ask (a CLI run with no TTY on stdin -- cron, scripts, CI).
/// "Declined by user" is a lie there and reads as a refusal, which made
/// unattended runs stop with nothing done.
/// Identical to `NO_USER_CONFIRM` in the Python and C++ editions.
pub const NO_USER_CONFIRM: &str = "No user is available to confirm this tool (non-interactive run). Do not treat this as a refusal. If running this tool is clearly the right next step, say so in your reply and continue without it; if it was optional, carry on with the rest of the task. Never claim the tool ran.";

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
    let mut window = crate::context_windows::resolve(base_url, model, api_key, &recovery.options).await;
    if !(1..=100).contains(&recovery.options.compact_at_percent) {
        emit_config_error(&event_tx, "compact_at_percent must be between 1 and 100".into()); return;
    }
    if window.is_some_and(|w| !w.input_only && recovery.options.output_limit >= w.tokens) {
        emit_config_error(&event_tx, "output_token_limit must be smaller than the combined context_window".into()); return;
    }
    let thinking_off = recovery.options.thinking_off_models.get(model).unwrap_or(&recovery.options.thinking_off).clone();
    if !["", "reasoning_effort", "enable_thinking"].contains(&thinking_off.as_str()) {
        emit_config_error(&event_tx, "empty_reply_thinking_off must be empty, reasoning_effort, or enable_thinking".into()); return;
    }
    let pressure_key = if recovery.options.chat_id.is_empty() { String::new() } else {
        serde_json::to_string(&[base_url, model, &recovery.options.chat_id]).unwrap()
    };
    let raw = serde_json::to_value(&current_messages).unwrap();
    let mut pressure = crate::context_pressure::Pressure::load(&pressure_key, raw.as_array().unwrap(), &recovery.view_identity(), window.map(|w|w.tokens).unwrap_or(0));
    let mut proactive_commits = 0u32;
    let mut proactive_failed = false;
    let mut long_length_retried = false;
    let mut thinking_disabled = false;
    let mut image_rejected = crate::image_capabilities::images_rejected(base_url, model);
    let mut rejection_notice = CACHED_IMAGE_NOTICE.to_string();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        // Attachment refs are local history only. Resolve image derivatives at
        // request time so provider payloads never leak back into chat JSON.
        let raw = serde_json::to_value(&current_messages).unwrap();
        let reduced: Vec<ChatMessage> = serde_json::from_value(serde_json::Value::Array(recovery.apply(raw.as_array().unwrap()))).unwrap();
        image_rejected = image_rejected || crate::image_capabilities::images_rejected(base_url, model);
        let mut api_messages = provider_messages_with_images(&reduced, attachment_context_keep_turns, image_rejected, &rejection_notice);
        // Proxy envelopes are opaque state tied to one model and upstream.
        // Keep them in persisted chat history, but don't leak them to a
        // different selected model (including model overrides in a tab).
        strip_cross_model_proxy_state(&mut api_messages, model);
        // Only this provider request is compacted. Events and persisted history
        // continue to use the unmodified current_messages.
        let mut request_messages = api_messages;
        let tool_definitions = tools::tool_definitions_json();
        if recovery.options.enabled && !proactive_failed && proactive_commits < 2 && recovery.attempts < 2 {
            if let Some(w) = window {
                let (threshold, hard) = crate::context_pressure::budgets(w.tokens, recovery.options.compact_at_percent, recovery.options.output_limit, w.input_only);
                let estimate = pressure.estimate(crate::context_pressure::input_chars(&serde_json::to_value(&request_messages).unwrap(), &tool_definitions));
                if pressure.should_compact(estimate, threshold, hard) {
                    match recover_with_reason(&mut recovery, &current_messages, &client, &url, base_url, api_key, model, &event_tx, &cancel, &mut accumulated_usage, true).await {
                        Ok(Some(event)) => {
                            proactive_commits += 1;
                            let _ = event_tx.send(event);
                            let reduced: Vec<ChatMessage> = serde_json::from_value(serde_json::Value::Array(recovery.apply(raw.as_array().unwrap()))).unwrap();
                            request_messages = provider_messages_with_images(&reduced, attachment_context_keep_turns, image_rejected, &rejection_notice);
                            strip_cross_model_proxy_state(&mut request_messages, model);
                            let after = pressure.estimate(crate::context_pressure::input_chars(&serde_json::to_value(&request_messages).unwrap(), &tool_definitions));
                            pressure.compacted(after, threshold);
                        }
                        Err(_) | Ok(None) => { proactive_failed = true; }
                    }
                    if cancel.load(Ordering::Relaxed) { return; }
                }
            }
        }
        let mut context_retries = 0;
        // Build API request payload
        let mut payload = serde_json::json!({
            "model": model,
            "messages": request_messages,
            "tools": tool_definitions,
            "tool_choice": "auto",
        });
        if !reasoning_effort.is_empty() {
            payload["reasoning_effort"] = serde_json::Value::String(reasoning_effort.to_string());
        }
        if recovery.options.output_limit > 0 { payload[&recovery.options.output_parameter] = serde_json::json!(recovery.options.output_limit); }
        if thinking_disabled { apply_thinking_off(&mut payload, &thinking_off); }

        // ── API call with 429 / 529 exponential backoff ──────────
        let (body_text, request_started) = {
            let mut last_status: Option<reqwest::StatusCode> = None;
            let mut last_body: Option<serde_json::Value> = None;
            let mut success: Option<(String, std::time::Instant)> = None;
            let mut rate_retries = 0;
            let mut transient_retries = 0u32;
            // The provider answer that started this retry chain.  Reported when a
            // retry itself fails with nothing more informative, so the retries
            // never change what the user is told about the failure they care
            // about (a transport failure carries no provider message).
            let mut chain_error: Option<(reqwest::StatusCode, serde_json::Value)> = None;
            let mut chain_timeout = false;
            // Shared retry-admission clock across failure categories.
            // An admitted request retains its own per-attempt deadline.
            let mut retry_chain_started: Option<std::time::Instant> = None;
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
                            // The endpoint is reachable and speaks our protocol.
                            // From here on, a connection failure means it went
                            // away mid-work, which is worth retrying.
                            mark_endpoint_answered(base_url);
                            let body_text = r.text().await.unwrap_or_default();
                            let parsed: serde_json::Value =
                                serde_json::from_str(&body_text).unwrap_or(serde_json::json!({}));
                            if parsed["choices"].as_array().and_then(|a| a.first()).is_none() {
                                // A 200 with no choices is the shape a proxy answers
                                // when it loses its upstream mid-generation: one retry
                                // (the 500 reasoning -- ride out a blip, do not pay
                                // three full-cost attempts for a deterministic answer),
                                // then a message that says what happened.
                                if transient_retries < MAX_EMPTY_RETRIES
                                    && retry_chain_open(&mut retry_chain_started, RETRY_CHAIN_GRACE)
                                {
                                    let keep_going = pause_after_transient(
                                        transient_retries, MAX_EMPTY_RETRIES, "empty",
                                        0, "The endpoint answered with no choices",
                                        &reqwest::header::HeaderMap::new(), &event_tx, &cancel).await;
                                    transient_retries += 1;
                                    if !keep_going {
                                        return;
                                    }
                                    continue;
                                }
                                emit_turn_error_kind(&event_tx, ERROR_KIND_EMPTY,
                                                     no_choices_help(), base_url);
                                return;
                            }
                            success = Some((body_text, request_started));
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
                            crate::image_capabilities::remember_image_rejection(base_url, model);
                            rejection_notice = image_rejection_notice(&body, detail).into();
                            suppress_images(&mut request_messages, &rejection_notice);
                            payload["messages"] = serde_json::to_value(&request_messages).unwrap();
                            continue;
                        }

                        if is_context_limit_error(code, &body, detail) {
                            if recovery.options.enabled && recovery.options.window_auto && recovery.options.context_window == 0 && !recovery.options.context_windows.contains_key(model) {
                                let message = body["error"]["message"].as_str().unwrap_or(detail);
                                if let Some(discovered) = crate::context_windows::remember_error(base_url, model, message) { window = Some(discovered); pressure.set_window(discovered.tokens); }
                            }
                            if recovery.options.enabled {
                                match recover(&mut recovery, &current_messages, &client, &url, base_url, api_key, model, &event_tx, &cancel, &mut accumulated_usage).await {
                                    Ok(Some(event)) => {
                                        let _ = event_tx.send(event);
                                        let raw = serde_json::to_value(&current_messages).unwrap();
                                        let reduced: Vec<ChatMessage> = serde_json::from_value(serde_json::Value::Array(recovery.apply(raw.as_array().unwrap()))).unwrap();
                                        request_messages = provider_messages_with_images(&reduced, attachment_context_keep_turns, image_rejected, &rejection_notice);
                                        strip_cross_model_proxy_state(&mut request_messages, model);
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
                                        reason: None,
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

                        if let Some((budget, reason)) = transient_retry(code) {
                            if transient_retries < budget
                                && retry_chain_open(&mut retry_chain_started, RETRY_CHAIN_GRACE)
                            {
                                // The endpoint answered, saying it is not
                                // available right now: 408/504 (out of time),
                                // 502/503 (proxy/upstream gone), or 500 (one
                                // attempt, since a 500 is often the request's
                                // own fault).  Same ladder as 429/529.
                                chain_error = Some((status, body.clone()));
                                let keep_going = pause_after_transient(
                                    transient_retries, budget, reason, code, detail,
                                    &headers, &event_tx, &cancel).await;
                                transient_retries += 1;
                                if !keep_going {
                                    return;
                                }
                                continue;
                            }
                        }

                        if RETRYABLE_STATUSES.contains(&code) && rate_retries < MAX_RETRIES
                            && retry_chain_open(&mut retry_chain_started, RETRY_CHAIN_GRACE)
                        {
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
                                reason: None,
                            });
                            // Sleep in 500ms slices so cancel is responsive
                            if !sleep_interruptible(delay, &cancel).await {
                                return;
                            }
                            let _ = event_tx.send(LlmEvent::RequestStarted {
                                stage: "turn".into(),
                                attempt: rate_retries,
                            });
                            continue;
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() && transient_retries < MAX_TIMEOUT_RETRIES
                            && retry_chain_open(&mut retry_chain_started, RETRY_CHAIN_GRACE)
                        {
                            // Our own deadline elapsed with no answer at all --
                            // the failure a retry actually fixes: a busy or
                            // single-slot endpoint usually answers on the next
                            // attempt.  Once the budget is spent the user gets
                            // the ordinary "timed out" message, unchanged.
                            chain_timeout = true;
                            let keep_going = pause_after_transient(
                                transient_retries, MAX_TIMEOUT_RETRIES, "timeout",
                                0, "Request timed out",
                                &reqwest::header::HeaderMap::new(), &event_tx, &cancel).await;
                            transient_retries += 1;
                            if !keep_going {
                                return;
                            }
                            continue;
                        }
                        if !e.is_timeout() && endpoint_has_answered(base_url)
                            && transient_retries < MAX_UNREACHABLE_RETRIES
                            && retry_chain_open(&mut retry_chain_started, RETRY_CHAIN_GRACE)
                        {
                            // Refused / DNS / TLS *after* this endpoint answered:
                            // a restart or a dropped route, not a bad URL.  These
                            // failures are instant, so the retries cost seconds.
                            // First contact still fails fast, because that is the
                            // case the "is your server running?" help is for.
                            let keep_going = pause_after_transient(
                                transient_retries, MAX_UNREACHABLE_RETRIES, "unreachable",
                                0, &transport_detail(&e),
                                &reqwest::header::HeaderMap::new(), &event_tx, &cancel).await;
                            transient_retries += 1;
                            if !keep_going {
                                return;
                            }
                            continue;
                        }
                        if chain_timeout && !e.is_timeout() {
                            // We were retrying a timeout; the retry hit something
                            // else.  Report the timeout -- that is the failure the
                            // user is waiting on, and the one the retries were for.
                            let _ = event_tx.send(LlmEvent::Error {
                                kind: ERROR_KIND_ERROR.into(),
                                message: unreachable_help(base_url, "timed out"),
                            });
                            return;
                        }
                        if let Some((status, body)) = &chain_error {
                            // The chain started with a provider answer; report
                            // that, not the transport failure of a retry.
                            let body_text = serde_json::to_string(body).unwrap_or_default();
                            let detail = body["error"]["message"].as_str()
                                .or_else(|| body["error"].as_str())
                                .or_else(|| body["message"].as_str())
                                .unwrap_or(body_text.as_str());
                            emit_turn_error(&event_tx, Some(status.as_u16()),
                                            format!("API error (HTTP {status}): {detail}"),
                                            base_url);
                            return;
                        }
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

        let request_seconds = request_started.elapsed().as_secs_f64();
        let body: serde_json::Value =
            serde_json::from_str(&body_text).unwrap_or(serde_json::json!({}));

        // Parse the response
        let choice = match body["choices"].as_array().and_then(|a| a.first()) {
            Some(c) => c,
            None => {
                emit_turn_error_kind(&event_tx, ERROR_KIND_EMPTY, no_choices_help(), base_url);
                return;
            }
        };

        // Pair provider usage with the exact successful payload, including inner-loop reductions.
        if recovery.options.enabled && window.is_some() {
            pressure.record(crate::context_pressure::input_chars(&payload["messages"], &payload["tools"]), body["usage"]["prompt_tokens"].as_u64());
            let raw = serde_json::to_value(&current_messages).unwrap();
            pressure.save(&pressure_key, raw.as_array().unwrap(), &recovery.view_identity());
        }
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
            let short_or_empty = length_suggests_context_pressure(&content, has_tool_calls, &body["usage"], recovery.options.output_limit);
            let extra_long_attempt = !short_or_empty && recovery.options.output_limit == 0 && !long_length_retried;
            if short_or_empty || extra_long_attempt {
                if extra_long_attempt { long_length_retried = true; }
                match recover(&mut recovery, &current_messages, &client, &url, base_url, api_key, model, &event_tx, &cancel, &mut accumulated_usage).await {
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
                    // Malformed JSON is kept (not replaced by {}) so the tool
                    // layer can tell the model its call didn't parse.
                    let args = crate::tool_args::parse_raw_arguments(args_str);

                    // ask_user_question is a special harness-level tool
                    if name == "ask_user_question" {
                        let args = match tools::validate_question_arguments(&args) {
                            Ok(v) => v,
                            Err(err) => {
                                current_messages.push(ChatMessage {
                                    role: "tool".into(),
                                    content: Some(serde_json::Value::String(err.clone())),
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
                                    content: err,
                                    declined: false,
                                });
                                continue;
                            }
                        };
                        let questions = args["questions"].clone();
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
                            other => {
                                let declined_msg = if other.as_ref().is_some_and(|c| c.unavailable) {
                                    NO_USER_ANSWER.to_string()
                                } else {
                                    "User cancelled the question.".to_string()
                                };
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
                            conf => {
                                // Declined, or nobody is available to confirm
                                // (headless CLI): "declined by user" reads as a
                                // refusal and stopped unattended runs cold.
                                let declined_msg = if conf.as_ref().is_some_and(|c| c.unavailable) {
                                    NO_USER_CONFIRM.to_string()
                                } else {
                                    "Tool execution was declined by user.".to_string()
                                };
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

        // No tool calls — this is the final response.  An empty one is not an
        // answer: it used to end the turn as a completed reply, so the chat
        // stored a blank assistant message that the next turn reads back as
        // something the model said.  Report it as the provider anomaly it is.
        if content.trim().is_empty() {
            let has_reasoning = ["reasoning", "reasoning_content"].iter().any(|k| msg[*k].as_str().is_some_and(|v| !v.trim().is_empty()));
            if choice["finish_reason"] == "stop" && has_reasoning && !thinking_disabled && !thinking_off.is_empty() {
                thinking_disabled = true;
                let _ = event_tx.send(LlmEvent::Retrying { attempt: 1, max_attempts: 1, delay_secs: 0.0, status_code: 200,
                    message: "Reasoning-only empty reply; retrying once with thinking off".into(), reason: Some("empty".into()) });
                let _ = event_tx.send(LlmEvent::RequestStarted { stage: "turn".into(), attempt: 2 });
                continue;
            }
            let _ = event_tx.send(LlmEvent::Error {
                kind: ERROR_KIND_EMPTY.to_string(),
                message: {
                    let help = empty_reply_help(
                        msg, choice["finish_reason"].as_str().unwrap_or(""),
                        body["completion_tokens"].as_u64().or_else(|| body["usage"]["completion_tokens"].as_u64()),
                    );
                    if thinking_disabled { help.replace("Pengy did not retry: an empty answer is the endpoint's shape, not a blip.", "Pengy retried once with thinking off but still received no answer.") }
                    else { help }
                },
            });
            return;
        }
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

fn apply_thinking_off(payload: &mut serde_json::Value, mechanism: &str) {
    match mechanism {
        "reasoning_effort" => { payload["reasoning_effort"] = serde_json::json!("none"); }
        "enable_thinking" => {
            payload.as_object_mut().unwrap().remove("reasoning_effort");
            payload["chat_template_kwargs"] = serde_json::json!({"enable_thinking":false});
        }
        _ => {}
    }
}

/// Execute bounded, tool-free summary calls; commit only a smaller complete plan.
async fn recover(recovery: &mut Recovery, messages: &[ChatMessage], client: &reqwest::Client,
                 url: &str, base_url: &str, key: &str, model: &str,
                 event_tx: &mpsc::UnboundedSender<LlmEvent>, cancel: &AtomicBool, usage: &mut Usage) -> Result<Option<LlmEvent>, String> {
    recover_with_reason(recovery, messages, client, url, base_url, key, model, event_tx, cancel, usage, false).await
}
async fn recover_with_reason(recovery: &mut Recovery, messages: &[ChatMessage], client: &reqwest::Client,
                 url: &str, base_url: &str, key: &str, model: &str,
                 event_tx: &mpsc::UnboundedSender<LlmEvent>, cancel: &AtomicBool, usage: &mut Usage, proactive: bool) -> Result<Option<LlmEvent>, String> {
    let raw = serde_json::to_value(messages).unwrap();
    let Some(plan) = recovery.plan(raw.as_array().unwrap()) else { return Ok(None); };
    if proactive && recovery.summary_calls + plan.chunks.len() > 8 { return Ok(None); }
    let mut summaries = vec![];
    // One wall-clock grace for the whole recovery: every chunk's summary calls
    // and their retries share it (see `RETRY_CHAIN_GRACE`).
    let mut chain_started: Option<std::time::Instant> = None;
    for chunk in &plan.chunks {
        if cancel.load(Ordering::Relaxed) { return Err("Context recovery cancelled.".into()); }
        recovery.summary_calls += 1;
        let mut payload = serde_json::json!({"model": model, "messages":[{"role":"system","content":SUMMARY_PROMPT},{"role":"user","content":chunk}]});
        payload[&recovery.options.output_parameter] = serde_json::json!(2048);
        // Bounded summaries need answer tokens, not an exhausted reasoning allowance.
        // Only use the mechanism explicitly configured for this endpoint/model.
        let mechanism = recovery.options.thinking_off_models.get(model).unwrap_or(&recovery.options.thinking_off);
        apply_thinking_off(&mut payload, mechanism);
        // The summary is part of this turn, so a provider timeout gets the same
        // retry budget here as the turn request -- a big conversation's prefill
        // is exactly what blows the configured timeout on a slow engine.
        let mut summary_retries = 0u32;
        let body: serde_json::Value = loop {
            // A summary call is an LLM request the frontends would otherwise
            // sit through blind (up to `llm_timeout` each, retries included).
            let _ = event_tx.send(LlmEvent::RequestStarted {
                stage: "summary".into(),
                attempt: summary_retries + 1,
            });
            match client.post(url).bearer_auth(key).header("api-key",key).json(&payload).send().await {
                Ok(r) => {
                    let code = r.status().as_u16();
                    if let Some((budget, reason)) = transient_retry(code) {
                        if summary_retries < budget && retry_chain_open(&mut chain_started, RETRY_CHAIN_GRACE) {
                            let delay = backoff_delay(summary_retries, None);
                            let _ = event_tx.send(LlmEvent::Retrying {
                                attempt: summary_retries + 1,
                                max_attempts: budget,
                                delay_secs: (delay * 10.0).round() / 10.0,
                                status_code: code,
                                message: format!("Context summary failed (HTTP {code}); retrying"),
                                reason: Some(reason.to_string()),
                            });
                            summary_retries += 1;
                            if !sleep_interruptible(delay, cancel).await {
                                return Err("Context recovery cancelled.".into());
                            }
                            continue;
                        }
                    }
                    if r.status().is_success() {
                        mark_endpoint_answered(base_url);
                    }
                    break r.error_for_status().map_err(|e| format!("Context summary failed; history retained: {e}"))?.json().await.map_err(|e| e.to_string())?;
                }
                Err(e) => {
                    let budget = if e.is_timeout() { MAX_TIMEOUT_RETRIES }
                        else if endpoint_has_answered(base_url) { MAX_UNREACHABLE_RETRIES }
                        else { 0 };
                    if summary_retries < budget && retry_chain_open(&mut chain_started, RETRY_CHAIN_GRACE) {
                        let delay = backoff_delay(summary_retries, None);
                        let _ = event_tx.send(LlmEvent::Retrying {
                            attempt: summary_retries + 1,
                            max_attempts: budget,
                            delay_secs: (delay * 10.0).round() / 10.0,
                            status_code: 0,
                            message: format!("Context summary failed ({e}); retrying"),
                            reason: Some(if e.is_timeout() { "timeout" } else { "unreachable" }.to_string()),
                        });
                        summary_retries += 1;
                        if !sleep_interruptible(delay, cancel).await {
                            return Err("Context recovery cancelled.".into());
                        }
                        continue;
                    }
                    return Err(format!("Context summary failed; history retained: {e}"));
                }
            }
        };
        if let Some(u) = body["usage"].as_object() {
            usage.prompt_tokens += u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            usage.completion_tokens += u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            usage.total_tokens += u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        }
        if cancel.load(Ordering::Relaxed) { return Err("Context recovery cancelled.".into()); }
        let choice = match body["choices"].as_array().and_then(|a| a.first()) {
            Some(c) => c,
            // serde_json's Index returns Null out of range, so this never
            // panicked -- it just degraded to the generic "incomplete" message.
            None => return Err("Context summary request returned no choices; original history retained.".into()),
        };
        let text = choice["message"]["content"].as_str().unwrap_or("");
        if choice["finish_reason"] != "stop" || text.trim().is_empty() || choice["message"]["tool_calls"].as_array().is_some_and(|a| !a.is_empty()) {
            return Err("Context summary was incomplete; original history retained.".into());
        }
        summaries.push(text.to_string());
    }
    Ok(recovery.commit(plan,summaries,raw.as_array().unwrap())?.map(|v| LlmEvent::ContextCompacted {
        attempt:v["attempt"].as_u64().unwrap() as u32, max_attempts:4,
        chars_removed:v["chars_removed"].as_u64().unwrap() as usize,
        reason: proactive.then(|| "proactive".into()),
        message: if proactive { Some(format!("Compacting before the window fills — {} fewer characters. Full history retained.", v["chars_removed"])) } else { v["message"].as_str().map(String::from) },
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
    fn retry_chain_grace_covers_the_longest_ladder() {
        // 1 + 2 + 4 + 8 + 16: the overload ladder's own sleeps, so every retry
        // a ladder announced still fits inside the grace, whatever
        // `llm_timeout` is configured to (the grace must not read it, or a
        // small test timeout leaks into every other chain).
        assert_eq!(RETRY_CHAIN_GRACE, std::time::Duration::from_secs(31));
    }

    #[test]
    fn retry_chain_open_opens_then_consumes() {
        let mut started = None;
        assert!(retry_chain_open(&mut started, std::time::Duration::from_millis(60)));
        assert!(started.is_some(), "the first decision opens the budget");
        // Inside the budget the chain stays open.
        assert!(retry_chain_open(&mut started, std::time::Duration::from_secs(60)));
        std::thread::sleep(std::time::Duration::from_millis(70));
        // Past the budget it closes, and stays closed.
        assert!(!retry_chain_open(&mut started, std::time::Duration::from_millis(60)));
        assert!(!retry_chain_open(&mut started, std::time::Duration::from_millis(60)));
    }

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
        let event = LlmEvent::ContextCompacted { attempt: 1, max_attempts: 4, chars_removed: 9000, message: None, reason: None };
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
        start_chat_options(base_url, model, messages, mode, reasoning_effort, preserve_reasoning, crate::context_recovery::Options::default())
    }
    fn start_chat_options(
        base_url: &str, model: &str, messages: Vec<ChatMessage>, mode: ToolConfirmation,
        reasoning_effort: &str, preserve_reasoning: bool, options: crate::context_recovery::Options,
    ) -> Driver {
        let (event_tx, rx) = mpsc::unbounded_channel();
        let (confirm_tx, confirm_rx) = mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let base_url = base_url.to_string();
        let model = model.to_string();
        let effort = reasoning_effort.to_string();
        let ctx = Arc::new(tools::ToolContext::new());
        *ctx.recovery.lock().unwrap() = options;
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
                ctx,
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
            // Attempts 1-3 replay the reserved provider-only plan with no new
            // requests; the fourth (reserved) attempt pays for the history
            // summary, and that one summary call announces itself first.
            for attempt in 1..=3 {
                assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ContextCompacted { attempt: n, .. } if n == attempt));
            }
            assert!(matches!(d.rx.recv().await.unwrap(),
                LlmEvent::RequestStarted { stage, attempt: n, .. } if stage == "summary" && n == 1));
            assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ContextCompacted { attempt: n, .. } if n == 4));
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
        // The summary call announces itself before the plan is committed.
        assert!(matches!(d.rx.recv().await.unwrap(),
            LlmEvent::RequestStarted { stage, .. } if stage == "summary"));
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
        // The summary call announces itself before the plan is committed.
        assert!(matches!(d.rx.recv().await.unwrap(),
            LlmEvent::RequestStarted { stage, .. } if stage == "summary"));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::ContextCompacted { .. }));
        assert!(matches!(d.rx.recv().await.unwrap(), LlmEvent::FinalResponse { content, .. } if content == "recovered"));
        d.handle.await.unwrap();
        assert!(!target.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_output_cap_long_partial_still_fails_safely_with_token_counts() {
        let (base, requests) = stub_server(vec![length_stop("A long partial answer", serde_json::Value::Null, 4096)]);
        let options = crate::context_recovery::Options { output_limit: 4096, ..Default::default() };
        let mut d = start_chat_options(&base, "stub-model", recoverable_history(), ToolConfirmation::All, "", false, options);
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
        assert!(matches!(d.rx.recv().await.unwrap(),LlmEvent::RequestStarted{stage,..} if stage == "summary"));
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
            assert!(!crate::image_capabilities::images_rejected(&base,"stub-model"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cached_image_rejection_omits_new_read_image_output() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.png");
        image::RgbImage::from_pixel(32,32,image::Rgb([10,120,200])).save(&file).unwrap();
        let args = serde_json::json!({"path":file.to_str().unwrap()});
        let (base, requests) = stub_server(vec![
            completion("",serde_json::json!([tool_call("tc1","read_image",&args)]),(10,5)),
            completion("metadata only",serde_json::Value::Null,(10,5))]);
        crate::image_capabilities::remember_image_rejection(&base,"stub-model");
        let mut d = start_chat(&base,vec![user_msg("look")],ToolConfirmation::All,"",false);
        while let Some(event) = d.rx.recv().await {
            if let LlmEvent::Error { message, .. } = event { panic!("{message}"); }
        }
        d.handle.await.unwrap();
        let req = requests.lock().unwrap();
        assert_eq!(req.len(),2);
        for request in req.iter() {
            let msgs: Vec<ChatMessage> = serde_json::from_value(request["messages"].clone()).unwrap();
            assert!(!has_image_url_parts(&msgs));
        }
        assert!(req[1]["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().contains("new.png"));
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
                unavailable: false,
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
    async fn unanswerable_question_tells_model_to_decide() {
        let args = serde_json::json!({"questions": [{"header": "H", "question": "Q?",
            "options": [{"label": "A", "description": "a"}]}]});
        let (base, _requests) = stub_server(vec![
            completion("", serde_json::json!([tool_call("q1", "ask_user_question", &args)]), (1, 1)),
            completion("done", serde_json::Value::Null, (1, 1)),
        ]);
        let mut d = start_chat(&base, vec![user_msg("go")], ToolConfirmation::All, "", false);
        loop {
            if let LlmEvent::QuestionRequest { .. } = d.rx.recv().await.unwrap() {
                break;
            }
        }
        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "q1".into(),
                confirmed: false,
                yolo_turn: false,
                answers: None,
                unavailable: true,
            })
            .unwrap();
        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult { content, declined, .. } => {
                assert!(declined);
                assert_eq!(content, NO_USER_ANSWER);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        d.handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_question_is_reported_not_asked() {
        // ask_user_question bypasses execute_tool, so it is schema-checked
        // separately: a malformed call goes back to the model, never to the user.
        let cases = [
            // questions as a string holding broken JSON (seen from a local model)
            serde_json::json!({"questions": "[{\"header\": \"H\", \"question\": \"Q?\", \"options\": [{\"label\": \"A\"]}]"}).to_string(),
            serde_json::json!({"questions": []}).to_string(),
            "{not json!".to_string(),
        ];
        for raw in cases {
            let call = serde_json::json!([{"id": "q1", "type": "function",
                "function": {"name": "ask_user_question", "arguments": raw}}]);
            let (base, _requests) = stub_server(vec![
                completion("", call, (1, 1)),
                completion("done", serde_json::Value::Null, (1, 1)),
            ]);
            let mut d = start_chat(&base, vec![user_msg("go")], ToolConfirmation::All, "", false);
            loop {
                match d.rx.recv().await.unwrap() {
                    LlmEvent::QuestionRequest { .. } => panic!("malformed question reached the user"),
                    LlmEvent::ToolResult { content, declined, .. } => {
                        assert!(!declined);
                        assert!(content.starts_with("Error: "), "{content}");
                        assert!(content.contains("ask_user_question"), "{content}");
                        break;
                    }
                    _ => {}
                }
            }
            d.handle.await.unwrap();
        }
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
                unavailable: false,
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
                unavailable: false,
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
                unavailable: false,
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
                unavailable: false,
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
        // A 500 now buys one retry, so the retry notice arrives first.  The
        // contract under test is unchanged: the failure is an error event, and
        // nothing terminal follows it.
        match d.rx.recv().await.unwrap() {
            LlmEvent::Retrying { status_code, reason, .. } => {
                assert_eq!(status_code, 500);
                assert_eq!(reason.as_deref(), Some("server"));
            }
            other => panic!("expected the retry notice, got {other:?}"),
        }
        // The retry's launch notice sits between the announcement and the
        // second failure that spends the 500's single-retry budget.
        match d.rx.recv().await.unwrap() {
            LlmEvent::RequestStarted { stage, attempt, .. } => {
                assert_eq!(stage, "turn");
                assert_eq!(attempt, 1);
            }
            other => panic!("expected the launch notice, got {other:?}"),
        }
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

    #[tokio::test]
    async fn a_headless_decline_tells_the_model_the_truth() {
        // A headless run (cron, script, CI) has nobody to confirm.  "Declined by
        // user" reads as a refusal and stopped unattended runs cold; the model
        // must be told that no user is available, and the run must carry on.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        let (base, _seen) = stub_server(vec![
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "",
                "tool_calls": [{"id": "tc1", "type": "function", "function": {
                    "name": "write_file",
                    "arguments": format!("{{\"path\":\"{}\",\"content\":\"x\"}}", path.display())
                }}]}, "finish_reason": "tool_calls"}]}),
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "carried on"},
                "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}),
        ]);

        let mut d = start_chat(&base, vec![user_msg("go")], ToolConfirmation::None, "", false);
        d.rx.recv().await.unwrap(); // AssistantToolCalls
        d.rx.recv().await.unwrap(); // ToolRequest
        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "tc1".into(),
                confirmed: false,
                yolo_turn: false,
                answers: None,
                unavailable: true,
            })
            .unwrap();
        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult { content, declined, .. } => {
                assert!(declined);
                assert!(content.contains("No user is available"), "{content}");
                assert!(!content.contains("declined by user"), "{content}");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // The run carries on to a further model round.
        match d.rx.recv().await.unwrap() {
            LlmEvent::FinalResponse { content, .. } => assert!(content.contains("carried on")),
            other => panic!("expected FinalResponse, got {other:?}"),
        }
        assert!(!path.exists(), "a declined tool must not run");
    }

    #[tokio::test]
    async fn a_real_decline_keeps_its_wording() {
        // Negative control: a user who really declined must not be reported as
        // "no user available".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("y.txt");
        let (base, _seen) = stub_server(vec![
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "",
                "tool_calls": [{"id": "tc1", "type": "function", "function": {
                    "name": "write_file",
                    "arguments": format!("{{\"path\":\"{}\",\"content\":\"x\"}}", path.display())
                }}]}, "finish_reason": "tool_calls"}]}),
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "stopped"},
                "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}),
        ]);

        let mut d = start_chat(&base, vec![user_msg("go")], ToolConfirmation::None, "", false);
        d.rx.recv().await.unwrap();
        d.rx.recv().await.unwrap();
        d.confirm_tx
            .send(Confirmation {
                tool_call_id: "tc1".into(),
                confirmed: false,
                yolo_turn: false,
                answers: None,
                unavailable: false,
            })
            .unwrap();
        match d.rx.recv().await.unwrap() {
            LlmEvent::ToolResult { content, .. } => {
                assert!(content.contains("declined by user"), "{content}");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }
    async fn drain(mut d: Driver) -> Vec<LlmEvent> {
        let mut events=vec![];
        while let Some(e)=tokio::time::timeout(std::time::Duration::from_secs(10),d.rx.recv()).await.expect("bounded stub turn") { events.push(e); }
        d.handle.await.unwrap(); events
    }
    fn seed_pressure(base: &str, model: &str, messages: &[ChatMessage], options: &crate::context_recovery::Options, tokens: u64) {
        let raw=serde_json::to_value(messages).unwrap();
        let recovery=Recovery::new(raw.as_array().unwrap(),base,model,options.clone());
        let provider=provider_messages_with_images(messages,4,false,CACHED_IMAGE_NOTICE);
        let chars=crate::context_pressure::input_chars(&serde_json::to_value(provider).unwrap(),&tools::tool_definitions_json()).unwrap();
        let key=serde_json::to_string(&[base,model,&options.chat_id]).unwrap();
        let mut p=crate::context_pressure::Pressure::load(&key,raw.as_array().unwrap(),&recovery.view_identity(),options.context_window);
        p.record(Some(chars),Some(tokens));p.save(&key,raw.as_array().unwrap(),&recovery.view_identity());
    }

    #[tokio::test(flavor="multi_thread")]
    async fn proactive_discovery_is_opt_in_and_matches_selected_model() {
        let (base, req)=stub_server(vec![serde_json::json!({"data":[{"id":"stub-model","context_window":1048576}]})]);
        let off=crate::context_recovery::Options::default();
        assert!(crate::context_windows::resolve(&base,"stub-model","test-key",&off).await.is_none());
        assert_eq!(req.lock().unwrap().len(),0);
        let auto=crate::context_recovery::Options{window_auto:true,..Default::default()};
        assert_eq!(crate::context_windows::resolve(&base,"stub-model","test-key",&auto).await.unwrap().tokens,1048576);
        assert_eq!(req.lock().unwrap().len(),1);
        assert_eq!(crate::context_windows::resolve(&base,"stub-model","test-key",&auto).await.unwrap().tokens,1048576);
        assert_eq!(req.lock().unwrap().len(),1);
        let explicit=crate::context_recovery::Options{context_window:262144,..auto.clone()};
        assert_eq!(crate::context_windows::resolve(&base,"stub-model","test-key",&explicit).await.unwrap().tokens,262144);
        let mut disabled=auto;disabled.context_windows.insert("stub-model".into(),0);
        assert!(crate::context_windows::resolve(&base,"stub-model","test-key",&disabled).await.is_none());
    }

    #[tokio::test(flavor="multi_thread")]
    async fn calibration_survives_user_turns_and_fast_stays_below_threshold() {
        for (window,expect) in [(262144,true),(1048576,false)] {
            let messages=recoverable_history();
            let mut replies=vec![completion("first answer",serde_json::Value::Null,(200000,5))];
            if expect{replies.push(completion("checkpoint",serde_json::Value::Null,(20,5)));}
            replies.push(completion("second answer",serde_json::Value::Null,(100,5)));
            let (base,req)=stub_server(replies);
            let options=crate::context_recovery::Options{context_window:window,chat_id:uuid::Uuid::new_v4().to_string(),checkpoint_dir:Some(tempfile::tempdir().unwrap().keep()),..Default::default()};
            let events=drain(start_chat_options(&base,"stub-model",messages.clone(),ToolConfirmation::All,"",false,options.clone())).await;
            assert!(!events.iter().any(|e|matches!(e,LlmEvent::ContextCompacted{..})));
            let mut next=messages;
            if let Some(LlmEvent::FinalResponse{message:Some(m),..})=events.last(){next.push(m.clone());}else{panic!("{events:?}");}
            next.push(user_msg("continue"));
            let events=drain(start_chat_options(&base,"stub-model",next,ToolConfirmation::All,"",false,options)).await;
            assert_eq!(events.iter().filter(|e|matches!(e,LlmEvent::ContextCompacted{..})).count(),usize::from(expect));
            assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{content,..}) if content=="second answer"));
            assert_eq!(req.lock().unwrap().len(),if expect{3}else{2});
        }
    }
    #[tokio::test(flavor="multi_thread")]
    async fn missing_usage_never_seeds_a_first_request_estimate() {
        let mut first=completion("first answer",serde_json::Value::Null,(250000,5));
        first.as_object_mut().unwrap().remove("usage");
        let (base,req)=stub_server(vec![first,completion("second answer",serde_json::Value::Null,(250000,5))]);
        let options=crate::context_recovery::Options{context_window:262144,chat_id:uuid::Uuid::new_v4().to_string(),checkpoint_dir:Some(tempfile::tempdir().unwrap().keep()),..Default::default()};
        let mut messages=recoverable_history();
        let events=drain(start_chat_options(&base,"stub-model",messages.clone(),ToolConfirmation::All,"",false,options.clone())).await;
        assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{..})));
        messages.push(ChatMessage::new("assistant",Some(serde_json::json!("first answer"))));messages.push(user_msg("continue"));
        let events=drain(start_chat_options(&base,"stub-model",messages,ToolConfirmation::All,"",false,options)).await;
        assert!(!events.iter().any(|e|matches!(e,LlmEvent::ContextCompacted{..})));
        assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{..})));assert_eq!(req.lock().unwrap().len(),2);
    }
    #[tokio::test(flavor="multi_thread")]
    async fn proactive_budget_leaves_two_reactive_attempts_across_tool_rounds() {
        let cases:Vec<serde_json::Value>=serde_json::from_str(include_str!("../tests/fixtures/context_recovery_reserve.json")).unwrap();
        let messages:Vec<ChatMessage>=serde_json::from_value(cases[0]["messages"].clone()).unwrap();
        let invalid=serde_json::json!({"path":"/file/that/does/not/exist"});
        let tools=|id|completion("",serde_json::json!([tool_call(id,"read_file",&invalid)]),(260000,5));
        let overflow=serde_json::json!({"error":{"code":"context_length_exceeded","message":"context length exceeded"}});
        let (base,req)=stub_server_sequence(vec![(200,tools("one")),(200,tools("two")),(400,overflow.clone()),(400,overflow),(200,completion("checkpoint",serde_json::Value::Null,(20,5))),(200,completion("answer",serde_json::Value::Null,(100,5)))]);
        let options=crate::context_recovery::Options{context_window:262144,chat_id:uuid::Uuid::new_v4().to_string(),checkpoint_dir:Some(tempfile::tempdir().unwrap().keep()),..Default::default()};
        seed_pressure(&base,"stub-model",&messages,&options,260000);
        let events=drain(start_chat_options(&base,"stub-model",messages,ToolConfirmation::All,"",false,options)).await;
        let compact:Vec<_>=events.iter().filter(|e|matches!(e,LlmEvent::ContextCompacted{..})).collect();
        assert_eq!(compact.len(),4,"{events:?}");
        for (i,e) in compact.iter().enumerate(){assert!(matches!(e,LlmEvent::ContextCompacted{attempt,..} if *attempt==i as u32+1));}
        assert_eq!(compact.iter().filter(|e|matches!(e,LlmEvent::ContextCompacted{reason:Some(r),..} if r=="proactive")).count(),2);
        assert_eq!(events.iter().filter(|e|matches!(e,LlmEvent::ToolRequest{..})).count(),2);
        assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{..})),"{events:?}");
        assert_eq!(req.lock().unwrap().len(),6);
    }

    #[tokio::test(flavor="multi_thread")]
    async fn impossible_output_reserve_is_rejected_before_a_request() {
        let (base,req)=stub_server(vec![]);
        let options=crate::context_recovery::Options{context_window:262144,output_limit:262144,..Default::default()};
        let events=drain(start_chat_options(&base,"stub-model",vec![user_msg("answer")],ToolConfirmation::All,"",false,options)).await;
        assert!(matches!(events.last(),Some(LlmEvent::Error{message,..}) if message.contains("must be smaller")));
        assert_eq!(req.lock().unwrap().len(),0);
    }
    #[tokio::test(flavor="multi_thread")]
    async fn proactive_threshold_edges_unknown_and_no_plan() {
        for (window,tokens,reduce) in [(262144,196608,true),(262144,196607,false),(0,250000,false)] {
            let messages=recoverable_history();
            let original=serde_json::to_value(&messages).unwrap();
            let replies=if reduce {vec![completion("checkpoint",serde_json::Value::Null,(20,5)),completion("answer",serde_json::Value::Null,(100,5))]}else{vec![completion("answer",serde_json::Value::Null,(100,5))]};
            let (base,req)=stub_server(replies);
            let options=crate::context_recovery::Options{context_window:window,chat_id:uuid::Uuid::new_v4().to_string(),checkpoint_dir:Some(tempfile::tempdir().unwrap().keep()),..Default::default()};
            let checkpoint_dir=options.checkpoint_dir.clone().unwrap();
            seed_pressure(&base,"stub-model",&messages,&options,tokens);
            let events=drain(start_chat_options(&base,"stub-model",messages.clone(),ToolConfirmation::All,"",false,options)).await;
            let compact:Vec<_>=events.iter().filter(|e|matches!(e,LlmEvent::ContextCompacted{..})).collect();
            assert_eq!(compact.len(),usize::from(reduce));
            if reduce {assert!(matches!(compact[0],LlmEvent::ContextCompacted{attempt:1,reason:Some(r),message:Some(m),..} if r=="proactive" && m.contains("before the window fills")));}
            assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{content,..}) if content=="answer"),"{events:?}");
            assert_eq!(req.lock().unwrap().len(),if reduce{2}else{1});
            if !reduce { assert_eq!(std::fs::read_dir(&checkpoint_dir).unwrap().count(),0); }
            if reduce {
                let requests=req.lock().unwrap();
                let source=requests[0]["messages"][1]["content"].as_str().unwrap();
                assert!(!source.contains("current"));
                let remaining=requests[1]["messages"].as_array().unwrap();
                assert_eq!(remaining.last().unwrap()["content"],messages.last().unwrap().content.clone().unwrap());
            }
            assert_eq!(serde_json::to_value(messages).unwrap(),original);
        }
        let messages=vec![user_msg("protected active task")];
        let (base,req)=stub_server(vec![completion("answer",serde_json::Value::Null,(200000,5))]);
        let options=crate::context_recovery::Options{context_window:262144,chat_id:uuid::Uuid::new_v4().to_string(),checkpoint_dir:Some(tempfile::tempdir().unwrap().keep()),..Default::default()};
        seed_pressure(&base,"stub-model",&messages,&options,200000);
        let events=drain(start_chat_options(&base,"stub-model",messages,ToolConfirmation::All,"",false,options)).await;
        assert!(!events.iter().any(|e|matches!(e,LlmEvent::ContextCompacted{..})));
        assert_eq!(req.lock().unwrap().len(),1);
    }
    #[tokio::test(flavor="multi_thread")]
    async fn proactive_cold_start_sends_then_tool_round_compacts_without_reexecution() {
        let dir=tempfile::tempdir().unwrap();let file=dir.path().join("logs.txt");
        let logs="log line: id=1234567890; value=abcdefghijk\n".repeat(4000);
        std::fs::write(&file,&logs).unwrap();
        let args=serde_json::json!({"path":file});
        let first=completion("",serde_json::json!([tool_call("tc1","read_file",&args)]),(180000,5));
        let (base,req)=stub_server(vec![first,completion("answer",serde_json::Value::Null,(50000,5))]);
        let messages=vec![user_msg("read logs")];
        let options=crate::context_recovery::Options{context_window:262144,..Default::default()};
        let events=drain(start_chat_options(&base,"stub-model",messages,ToolConfirmation::All,"",false,options)).await;
        assert!(matches!(events.first(),Some(LlmEvent::AssistantToolCalls{..})),"{events:?}");
        assert_eq!(events.iter().filter(|e|matches!(e,LlmEvent::ToolRequest{..})).count(),1);
        assert!(events.iter().any(|e|matches!(e,LlmEvent::ToolResult{content,..} if content.contains("log line:"))));
        assert_eq!(events.iter().filter(|e|matches!(e,LlmEvent::ContextCompacted{reason:Some(r),..} if r=="proactive")).count(),1);
        assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{..})));
        let req=req.lock().unwrap();assert_eq!(req.len(),2);
        let body=req[1]["messages"].as_array().unwrap().iter().find(|m|m["role"]=="tool").unwrap()["content"].as_str().unwrap();
        assert!(body.len()<10000);
    }
    #[tokio::test(flavor="multi_thread")]
    async fn proactive_failed_summary_sends_original_and_preserves_protected_turns() {
        let messages=recoverable_history();
        let (base,req)=stub_server_sequence(vec![(400,serde_json::json!({"error":"summary failed"})),(200,completion("answer",serde_json::Value::Null,(200000,5)))]);
        let options=crate::context_recovery::Options{context_window:262144,chat_id:uuid::Uuid::new_v4().to_string(),checkpoint_dir:Some(tempfile::tempdir().unwrap().keep()),..Default::default()};
        seed_pressure(&base,"stub-model",&messages,&options,200000);
        let events=drain(start_chat_options(&base,"stub-model",messages.clone(),ToolConfirmation::All,"",false,options)).await;
        assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{..})),"{events:?}");
        assert!(!events.iter().any(|e|matches!(e,LlmEvent::ContextCompacted{..})));
        let req=req.lock().unwrap();assert_eq!(req.len(),2);
        assert!(req[0].get("tools").is_none());
        assert_eq!(req[1]["messages"],serde_json::to_value(provider_messages_with_images(&messages,4,false,CACHED_IMAGE_NOTICE)).unwrap());
    }
    #[tokio::test(flavor="multi_thread")]
    async fn proactive_one_step_preserves_reactive_400_path() {
        let mut messages=recoverable_history();
        messages[1].reasoning=Some(serde_json::json!("historical reasoning ".repeat(2000)));
        let (base,req)=stub_server_sequence(vec![(400,serde_json::json!({"error":{"code":"context_length_exceeded","message":"maximum context length is 262144"}})),(200,completion("checkpoint",serde_json::Value::Null,(20,5))),(200,completion("answer",serde_json::Value::Null,(100,5)))]);
        let options=crate::context_recovery::Options{context_window:262144,chat_id:uuid::Uuid::new_v4().to_string(),checkpoint_dir:Some(tempfile::tempdir().unwrap().keep()),..Default::default()};
        seed_pressure(&base,"stub-model",&messages,&options,240000);
        let events=drain(start_chat_options(&base,"stub-model",messages,ToolConfirmation::All,"",false,options)).await;
        let compact:Vec<_>=events.iter().filter(|e|matches!(e,LlmEvent::ContextCompacted{..})).collect();
        assert_eq!(compact.len(),2,"{events:?}");
        assert!(matches!(compact[0],LlmEvent::ContextCompacted{attempt:1,reason:Some(r),..} if r=="proactive"));
        assert!(matches!(compact[1],LlmEvent::ContextCompacted{attempt:2,reason:None,..}));
        assert_eq!(req.lock().unwrap().len(),3);
    }
    #[tokio::test(flavor="multi_thread")]
    async fn long_length_gets_one_reduction_and_never_executes_truncated_tools() {
        let dir=tempfile::tempdir().unwrap();let target=dir.path().join("must-not-exist");
        let call=tool_call("unsafe","write_file",&serde_json::json!({"path":target,"content":"no"}));
        for success in [true,false] {
            let last=if success{completion("answer",serde_json::Value::Null,(100,5))}else{length_stop("still partial",serde_json::Value::Null,393216)};
            let (base,req)=stub_server(vec![length_stop("partial",serde_json::json!([call]),393216),completion("checkpoint",serde_json::Value::Null,(20,5)),last]);
            let events=drain(start_chat(&base,recoverable_history(),ToolConfirmation::All,"",false)).await;
            assert_eq!(events.iter().filter(|e|matches!(e,LlmEvent::ContextCompacted{..})).count(),1);
            assert!(!events.iter().any(|e|matches!(e,LlmEvent::AssistantToolCalls{..}|LlmEvent::ToolRequest{..})));
            if success{assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{..})));}
            else{assert!(matches!(events.last(),Some(LlmEvent::Error{kind,..}) if kind=="truncated"));}
            assert_eq!(req.lock().unwrap().len(),3);assert!(!target.exists());
        }
    }
    #[tokio::test(flavor="multi_thread")]
    async fn summary_uses_only_explicitly_configured_thinking_off() {
        for mechanism in ["", "reasoning_effort", "enable_thinking"] {
            let (base,req)=stub_server_sequence(vec![
                (400,serde_json::json!({"error":{"code":"context_length_exceeded"}})),
                (200,completion("checkpoint",serde_json::Value::Null,(20,5))),
                (200,completion("answer",serde_json::Value::Null,(100,5)))]);
            let options=crate::context_recovery::Options{thinking_off:mechanism.into(),..Default::default()};
            let events=drain(start_chat_options(&base,"stub-model",recoverable_history(),ToolConfirmation::All,"high",false,options)).await;
            assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{..})),"{events:?}");
            let req=req.lock().unwrap();assert_eq!(req.len(),3);
            assert!(req[1].get("tools").is_none());assert_eq!(req[1]["max_tokens"],2048);
            match mechanism {
                "reasoning_effort"=>assert_eq!(req[1]["reasoning_effort"],"none"),
                "enable_thinking"=>assert_eq!(req[1]["chat_template_kwargs"]["enable_thinking"],false),
                _=>{assert!(req[1].get("reasoning_effort").is_none());assert!(req[1].get("chat_template_kwargs").is_none());}
            }
            assert_eq!(req[2]["reasoning_effort"],"high","ordinary chat effort changed");
        }
    }
    #[tokio::test(flavor="multi_thread")]
    async fn reasoning_only_stop_retries_once_with_configured_mechanism() {
        for mechanism in ["reasoning_effort","enable_thinking",""] {
            for success in [true,false] {
                let mut empty=completion("",serde_json::Value::Null,(100,200));empty["choices"][0]["message"]["reasoning_content"]=serde_json::json!("thoughts only");
                let mut responses=vec![empty.clone()];
                if !mechanism.is_empty(){responses.push(if success{completion("ok",serde_json::Value::Null,(100,2))}else{empty});}
                let (base,req)=stub_server(responses);
                let options=crate::context_recovery::Options{thinking_off:mechanism.into(),..Default::default()};
                let events=drain(start_chat_options(&base,"stub-model",vec![user_msg("answer")],ToolConfirmation::All,"high",true,options)).await;
                assert!(!events.iter().any(|e|matches!(e,LlmEvent::AssistantToolCalls{..})));
                let req=req.lock().unwrap();
                if mechanism.is_empty(){assert_eq!(req.len(),1);assert!(matches!(events.last(),Some(LlmEvent::Error{..})));}
                else {
                    assert_eq!(req.len(),2);assert_eq!(req[0]["reasoning_effort"],"high");
                    if mechanism=="reasoning_effort"{assert_eq!(req[1]["reasoning_effort"],"none");}
                    else{assert_eq!(req[1]["chat_template_kwargs"]["enable_thinking"],false);}
                    assert_eq!(req[0]["messages"],req[1]["messages"]);
                    if success{assert!(matches!(events.last(),Some(LlmEvent::FinalResponse{content,usage,..}) if content=="ok" && usage.completion_tokens==202));}
                    else{assert!(matches!(events.last(),Some(LlmEvent::Error{kind,..}) if kind==ERROR_KIND_EMPTY));}
                }
            }
        }
    }

}
