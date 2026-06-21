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

/// Events emitted by the LLM chat loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LlmEvent {
    #[serde(rename = "assistant_tool_calls")]
    AssistantToolCalls {
        message: ChatMessage,
    },
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
    #[serde(rename = "final_response")]
    FinalResponse {
        content: String,
        usage: Usage,
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
pub async fn chat(
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    tool_confirmation: ToolConfirmation,
    event_tx: mpsc::UnboundedSender<LlmEvent>,
    mut confirm_rx: mpsc::UnboundedReceiver<Confirmation>,
    cancel: Arc<AtomicBool>,
) {
    let client = reqwest::Client::new();
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

    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        // Build API request payload
        let payload = serde_json::json!({
            "model": model,
            "messages": current_messages,
            "tools": tools::tool_definitions(),
            "tool_choice": "auto",
        });

        let resp = match client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("api-key", api_key)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(LlmEvent::FinalResponse {
                    content: format!("API error: {e}"),
                    usage: accumulated_usage,
                });
                return;
            }
        };

        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                let _ = event_tx.send(LlmEvent::FinalResponse {
                    content: format!("Error parsing API response: {e}"),
                    usage: accumulated_usage,
                });
                return;
            }
        };

        // Parse the response
        let choice = match body["choices"].as_array().and_then(|a| a.first()) {
            Some(c) => c,
            None => {
                let _ = event_tx.send(LlmEvent::FinalResponse {
                    content: "No choices in API response.".into(),
                    usage: accumulated_usage,
                });
                return;
            }
        };

        // Accumulate usage
        if let Some(usage) = body["usage"].as_object() {
            accumulated_usage.prompt_tokens +=
                usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            accumulated_usage.completion_tokens +=
                usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            accumulated_usage.total_tokens +=
                usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
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
                    tool_calls: tool_calls
                        .iter()
                        .map(|tc| crate::chat_manager::ToolCall {
                            id: tc["id"].as_str().unwrap_or("").into(),
                            call_type: "function".into(),
                            function: crate::chat_manager::FunctionCall {
                                name: tc["function"]["name"].as_str().unwrap_or("").into(),
                                arguments: tc["function"]["arguments"].as_str().unwrap_or("{}").into(),
                            },
                        })
                        .collect(),
                    tool_call_id: None,
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
                    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or_default();

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

                        let result = tools::execute_tool(&name, &args).await;

                        current_messages.push(ChatMessage {
                            role: "tool".into(),
                            content: Some(serde_json::Value::String(result.clone())),
                            tool_calls: vec![],
                            tool_call_id: Some(tc_id.clone()),
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

                                let result = tools::execute_tool(&name, &args).await;

                                current_messages.push(ChatMessage {
                                    role: "tool".into(),
                                    content: Some(serde_json::Value::String(result.clone())),
                                    tool_calls: vec![],
                                    tool_call_id: Some(tc_id.clone()),
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
                                let declined_msg = "Tool execution was declined by user.".to_string();
                                current_messages.push(ChatMessage {
                                    role: "tool".into(),
                                    content: Some(serde_json::Value::String(declined_msg.clone())),
                                    tool_calls: vec![],
                                    tool_call_id: Some(tc_id.clone()),
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
                // Loop back for the next API call (the LLM will respond to tool results)
                continue;
            }
        }

        // No tool calls — this is the final response
        let _ = event_tx.send(LlmEvent::FinalResponse {
            content,
            usage: accumulated_usage,
        });
        return;
    }
}
