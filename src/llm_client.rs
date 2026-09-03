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
        // Build API request payload
        let mut payload = serde_json::json!({
            "model": model,
            "messages": api_messages,
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
            for attempt in 0..=MAX_RETRIES {
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

                        if RETRYABLE_STATUSES.contains(&code) && attempt < MAX_RETRIES {
                            let ra = extract_retry_after(&headers);
                            let delay = backoff_delay(attempt, ra);
                            let detail = body["error"]["message"]
                                .as_str()
                                .or_else(|| body["error"].as_str())
                                .or_else(|| body["message"].as_str())
                                .unwrap_or(body_text.as_str())
                                .to_string();
                            let _ = event_tx.send(LlmEvent::Retrying {
                                attempt: attempt + 1,
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
                        let _ = event_tx.send(LlmEvent::FinalResponse {
                            content: format!("API error: {e}"),
                            message: None,
                            usage: accumulated_usage,
                        });
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
                let _ = event_tx.send(LlmEvent::FinalResponse {
                    content: format!("API error (HTTP {status}): {detail}"),
                    message: None,
                    usage: accumulated_usage,
                });
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
                let _ = event_tx.send(LlmEvent::FinalResponse {
                    content: format!(
                        "No choices in API response: {}",
                        serde_json::to_string_pretty(&body).unwrap_or_default()
                    ),
                    message: None,
                    usage: accumulated_usage,
                });
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
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
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
        let (event_tx, rx) = mpsc::unbounded_channel();
        let (confirm_tx, confirm_rx) = mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let base_url = base_url.to_string();
        let effort = reasoning_effort.to_string();
        let handle = tokio::spawn(async move {
            chat(
                &base_url,
                "test-key",
                "stub-model",
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
    async fn http_error_produces_api_error_final_response() {
        // Server that always answers 500 with an error body
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut tmp = [0u8; 65536];
                let _ = sock.read(&mut tmp);
                let body = r#"{"error": {"message": "boom"}}"#;
                let resp = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });

        let mut d = start_chat(
            &base,
            vec![user_msg("hi")],
            ToolConfirmation::None,
            "",
            false,
        );
        match d.rx.recv().await.unwrap() {
            LlmEvent::FinalResponse { content, .. } => {
                assert!(content.contains("API error"), "got: {content}");
                assert!(content.contains("boom"), "got: {content}");
            }
            other => panic!("expected FinalResponse, got {other:?}"),
        }
        d.handle.await.unwrap();
    }
}
