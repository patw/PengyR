//! Chat history management.
//!
//! Stores chat sessions as a JSON array at `~/.config/pengy/chats.json`.
//! Shared between the GUI and any future CLI/web frontends.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::{fs, io};

const CHATS_FILE: &str = "chats.json";

/// A single chat session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    pub created_at: String,
}

/// A message in a chat (OpenAI-compatible format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

impl Chat {
    pub fn new(title: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.to_string(),
            messages: Vec::new(),
            created_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        }
    }
}

fn chats_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("pengy");
    p.push(CHATS_FILE);
    p
}

/// Load all chat sessions from disk.
pub fn load_chats() -> Vec<Chat> {
    let path = chats_path();
    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Vec<Chat>>(&text) {
            Ok(chats) => chats,
            Err(_) => {
                backup_corrupt_file(&path);
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Save all chat sessions to disk atomically.
pub fn save_chats(chats: &[Chat]) -> io::Result<()> {
    let path = chats_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(chats)?;
    let mut tmp = path.clone();
    tmp.set_extension("tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Create a new chat and persist.
pub fn create_chat(title: &str) -> io::Result<Chat> {
    let chat = Chat::new(title);
    let mut chats = load_chats();
    chats.insert(0, chat.clone());
    save_chats(&chats)?;
    Ok(chat)
}

/// Delete a chat by ID.
pub fn delete_chat(chat_id: &str) -> io::Result<()> {
    let mut chats = load_chats();
    chats.retain(|c| c.id != chat_id);
    save_chats(&chats)
}

/// Save a single chat (update or insert).
pub fn save_chat(chat: &Chat) -> io::Result<()> {
    let mut chats = load_chats();
    if let Some(pos) = chats.iter().position(|c| c.id == chat.id) {
        chats[pos] = chat.clone();
    } else {
        chats.insert(0, chat.clone());
    }
    save_chats(&chats)
}

/// Get a chat by ID.
pub fn get_chat(chat_id: &str) -> Option<Chat> {
    load_chats().into_iter().find(|c| c.id == chat_id)
}

/// Clean dangling tool calls so the message list is valid for the API.
///
/// Handles two corruption cases:
/// - assistant tool_calls with no following tool result → synthesizes a cancelled result
/// - orphan `role: "tool"` messages with no preceding tool_calls → dropped
pub fn clean_dangling_tool_calls(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut cleaned: Vec<ChatMessage> = Vec::new();
    let mut pending_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut i = 0;

    while i < messages.len() {
        let msg = &messages[i];
        i += 1;

        if msg.role == "tool" {
            if let Some(ref tc_id) = msg.tool_call_id {
                if pending_ids.contains(tc_id) {
                    pending_ids.remove(tc_id);
                    cleaned.push(msg.clone());
                }
                // else: orphan — drop it
            }
            continue;
        }

        cleaned.push(msg.clone());

        if msg.role == "assistant" && !msg.tool_calls.is_empty() {
            let tc_ids: std::collections::HashSet<String> =
                msg.tool_calls.iter().map(|tc| tc.id.clone()).collect();
            pending_ids.extend(tc_ids.clone());

            // Consume any following tool messages that match
            while i < messages.len() && messages[i].role == "tool" {
                if let Some(ref tc_id) = messages[i].tool_call_id {
                    if pending_ids.contains(tc_id) {
                        pending_ids.remove(tc_id);
                        cleaned.push(messages[i].clone());
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }

            // Synthesize cancelled results for unsatisfied IDs
            let unsatisfied: Vec<String> = tc_ids.intersection(&pending_ids).cloned().collect();
            for missing_id in unsatisfied {
                pending_ids.remove(&missing_id);
                cleaned.push(ChatMessage {
                    role: "tool".into(),
                    content: Some("Tool execution was cancelled by user.".into()),
                    tool_calls: vec![],
                    tool_call_id: Some(missing_id),
                });
            }
        }
    }

    cleaned
}

/// Replace tool-result content in messages older than `keep_turns` turns.
/// A "turn" is a user message and everything until the next user message.
pub fn elide_old_tool_results(messages: &[ChatMessage], keep_turns: usize) -> Vec<ChatMessage> {
    if keep_turns == 0 {
        return messages.to_vec();
    }

    // Find indices of all user messages (turn boundaries)
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user")
        .map(|(i, _)| i)
        .collect();

    if user_indices.is_empty() {
        return messages.to_vec();
    }

    // Determine which turns are recent
    let num_turns = user_indices.len();
    let mut recent_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for (turn_idx, &start) in user_indices.iter().enumerate() {
        let turns_from_end = num_turns - turn_idx;
        if turns_from_end <= keep_turns {
            let end = if turn_idx + 1 < num_turns {
                user_indices[turn_idx + 1]
            } else {
                messages.len()
            };
            for idx in start..end {
                recent_indices.insert(idx);
            }
        }
    }

    messages
        .iter()
        .enumerate()
        .map(|(idx, msg)| {
            if msg.role == "tool" && !recent_indices.contains(&idx) {
                ChatMessage {
                    content: Some("[tool output from earlier turn elided]".into()),
                    ..msg.clone()
                }
            } else {
                msg.clone()
            }
        })
        .collect()
}

fn backup_corrupt_file(path: &std::path::Path) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup = path.with_file_name(format!(
        "{}.corrupt-{}",
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown"),
        ts
    ));
    let _ = fs::rename(path, &backup);
}
