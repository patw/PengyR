//! Chat history management.
//!
//! Stores chat sessions as a JSON array at `~/.config/pengy/chats.json`.
//! Shared between the GUI and any future CLI/web frontends.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;
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
    pub content: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<serde_json::Value>,
}

impl ChatMessage {
    pub fn new(role: impl Into<String>, content: Option<serde_json::Value>) -> Self {
        Self {
            role: role.into(),
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(serde_json::Value::String(content.into())),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        }
    }
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
    let mut p = crate::config::pengy_config_dir();
    p.push(CHATS_FILE);
    p
}

// ---------------------------------------------------------------------------
// in-memory cache
// ---------------------------------------------------------------------------
// chats.json is a single (potentially large) file that was fully re-parsed on
// every read: the GUI loads the list, then `get_chat` re-loads it; `save_chat`
// loads it again before writing. We cache the parsed Vec keyed by the file's
// (mtime, size). Any external writer (the CLI, or the Python/C++ editions
// sharing ~/.config/pengy/) bumps mtime and transparently invalidates us.
struct ChatCache {
    key: Option<(u128, u64)>, // (mtime nanos since epoch, size)
    chats: Vec<Chat>,
}

static CHAT_CACHE: LazyLock<Mutex<ChatCache>> = LazyLock::new(|| {
    Mutex::new(ChatCache {
        key: None,
        chats: Vec::new(),
    })
});

fn stat_key(path: &std::path::Path) -> Option<(u128, u64)> {
    let md = fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime, md.len()))
}

/// Drop the in-memory chats cache (forces a re-read on the next load).
pub fn invalidate_cache() {
    if let Ok(mut c) = CHAT_CACHE.lock() {
        c.key = None;
        c.chats = Vec::new();
    }
}

/// Run `f` against the cached chat list without cloning the whole Vec.
///
/// This is the cheap path for lookups like [`get_chat`], which previously
/// parsed the entire file just to return one chat.
fn with_chats<R>(f: impl FnOnce(&[Chat]) -> R) -> R {
    let path = chats_path();
    let mut cache = match CHAT_CACHE.lock() {
        Ok(c) => c,
        Err(p) => p.into_inner(), // a poisoned lock still holds usable data
    };

    let key = stat_key(&path);
    if key.is_some() && key == cache.key {
        return f(&cache.chats);
    }

    let chats = match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Vec<Chat>>(&text) {
            Ok(chats) => chats,
            Err(_) => {
                backup_corrupt_file(&path);
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    };
    cache.chats = chats;
    // Re-stat: backup_corrupt_file may have moved the file aside.
    cache.key = stat_key(&path);
    f(&cache.chats)
}

/// Load all chat sessions from disk.
pub fn load_chats() -> Vec<Chat> {
    with_chats(|chats| chats.to_vec())
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

    // Prime the cache with what we just wrote so the next load (e.g. the
    // load->mutate->save cycle in `save_chat`) skips a re-parse.
    if let Ok(mut cache) = CHAT_CACHE.lock() {
        cache.chats = chats.to_vec();
        cache.key = stat_key(&path);
    }
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
    // Clone just the one chat, not the whole list.
    with_chats(|chats| chats.iter().find(|c| c.id == chat_id).cloned())
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
                    content: Some(serde_json::Value::String(
                        "Tool execution was cancelled by user.".into(),
                    )),
                    tool_calls: vec![],
                    tool_call_id: Some(missing_id),
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
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
                    content: Some(serde_json::Value::String(
                        "[tool output from earlier turn elided]".into(),
                    )),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String(content.into())),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        }
    }

    fn assistant_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: Some(serde_json::Value::String(content.into())),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        }
    }

    fn assistant_with_tools(tool_ids: &[&str]) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: Some(serde_json::Value::String(String::new())),
            tool_calls: tool_ids
                .iter()
                .map(|id| ToolCall {
                    id: id.to_string(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "test_tool".into(),
                        arguments: "{}".into(),
                    },
                })
                .collect(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        }
    }

    fn tool_msg(tool_call_id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".into(),
            content: Some(serde_json::Value::String(content.into())),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        }
    }

    // ── Chat struct tests ──────────────────────────────────────────

    #[test]
    fn chat_new_generates_unique_ids() {
        let c1 = Chat::new("Chat 1");
        let c2 = Chat::new("Chat 2");
        assert_ne!(c1.id, c2.id);
        assert_eq!(c1.title, "Chat 1");
        assert!(c1.messages.is_empty());
    }

    #[test]
    fn chat_serde_round_trip() {
        let mut chat = Chat::new("Test");
        chat.messages.push(user_msg("hello"));
        chat.messages.push(assistant_msg("hi there"));
        let json = serde_json::to_string(&chat).unwrap();
        let chat2: Chat = serde_json::from_str(&json).unwrap();
        assert_eq!(chat2.id, chat.id);
        assert_eq!(chat2.title, "Test");
        assert_eq!(chat2.messages.len(), 2);
    }

    #[test]
    fn chat_message_with_tool_calls_round_trip() {
        let msg = assistant_with_tools(&["tc-1", "tc-2"]);
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg2.tool_calls.len(), 2);
        assert_eq!(msg2.tool_calls[0].id, "tc-1");
        assert_eq!(msg2.tool_calls[1].id, "tc-2");
        assert_eq!(msg2.tool_calls[0].function.name, "test_tool");
    }

    #[test]
    fn chat_message_without_tool_calls_omits_field() {
        let msg = user_msg("hello");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("tool_calls"));
        assert!(!json.contains("tool_call_id"));
    }

    // ── clean_dangling_tool_calls tests ────────────────────────────

    #[test]
    fn clean_no_tool_calls_unchanged() {
        let msgs = vec![user_msg("hi"), assistant_msg("hello")];
        let cleaned = clean_dangling_tool_calls(&msgs);
        assert_eq!(cleaned.len(), 2);
    }

    #[test]
    fn clean_complete_tool_call_unchanged() {
        let msgs = vec![
            user_msg("do something"),
            assistant_with_tools(&["tc-1"]),
            tool_msg("tc-1", "result"),
            assistant_msg("done"),
        ];
        let cleaned = clean_dangling_tool_calls(&msgs);
        assert_eq!(cleaned.len(), 4);
        assert_eq!(cleaned[2].role, "tool");
        assert_eq!(
            cleaned[2].content.as_ref().unwrap().as_str().unwrap(),
            "result"
        );
    }

    #[test]
    fn clean_dangling_tool_call_synthesizes_cancelled() {
        let msgs = vec![
            user_msg("do something"),
            assistant_with_tools(&["tc-1"]),
            // missing tool result for tc-1
            user_msg("next question"),
        ];
        let cleaned = clean_dangling_tool_calls(&msgs);
        // Should be: user, assistant_with_tools, synthesized tool result, user
        assert_eq!(cleaned.len(), 4);
        assert_eq!(cleaned[2].role, "tool");
        assert_eq!(cleaned[2].tool_call_id.as_deref(), Some("tc-1"));
        assert!(cleaned[2]
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("cancelled"));
    }

    #[test]
    fn clean_orphan_tool_message_dropped() {
        let msgs = vec![
            user_msg("hi"),
            tool_msg("orphan-id", "stale result"),
            assistant_msg("hello"),
        ];
        let cleaned = clean_dangling_tool_calls(&msgs);
        assert_eq!(cleaned.len(), 2);
        assert_eq!(cleaned[0].role, "user");
        assert_eq!(cleaned[1].role, "assistant");
    }

    #[test]
    fn clean_multiple_tool_calls_partial_results() {
        let msgs = vec![
            user_msg("do two things"),
            assistant_with_tools(&["tc-1", "tc-2"]),
            tool_msg("tc-1", "result 1"),
            // tc-2 missing
        ];
        let cleaned = clean_dangling_tool_calls(&msgs);
        // user, assistant, tool(tc-1), synthesized tool(tc-2)
        assert_eq!(cleaned.len(), 4);
        assert_eq!(cleaned[2].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(cleaned[3].role, "tool");
        assert_eq!(cleaned[3].tool_call_id.as_deref(), Some("tc-2"));
        assert!(cleaned[3]
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("cancelled"));
    }

    #[test]
    fn clean_multiple_tool_calls_all_satisfied() {
        let msgs = vec![
            assistant_with_tools(&["tc-1", "tc-2", "tc-3"]),
            tool_msg("tc-1", "r1"),
            tool_msg("tc-2", "r2"),
            tool_msg("tc-3", "r3"),
        ];
        let cleaned = clean_dangling_tool_calls(&msgs);
        assert_eq!(cleaned.len(), 4);
        assert!(cleaned
            .iter()
            .all(|m| m.role == "assistant" || m.role == "tool"));
    }

    #[test]
    fn clean_empty_messages() {
        let cleaned = clean_dangling_tool_calls(&[]);
        assert!(cleaned.is_empty());
    }

    // ── elide_old_tool_results tests ───────────────────────────────

    #[test]
    fn elide_keep_zero_returns_unchanged() {
        let msgs = vec![
            user_msg("q1"),
            assistant_with_tools(&["tc-1"]),
            tool_msg("tc-1", "long result data"),
            assistant_msg("done"),
        ];
        let elided = elide_old_tool_results(&msgs, 0);
        assert_eq!(elided.len(), msgs.len());
        assert_eq!(
            elided[2].content.as_ref().unwrap().as_str().unwrap(),
            "long result data"
        );
    }

    #[test]
    fn elide_keeps_recent_turn_intact() {
        let msgs = vec![
            user_msg("old question"),
            assistant_with_tools(&["tc-old"]),
            tool_msg("tc-old", "old tool output"),
            assistant_msg("old answer"),
            user_msg("new question"),
            assistant_with_tools(&["tc-new"]),
            tool_msg("tc-new", "new tool output"),
            assistant_msg("new answer"),
        ];
        let elided = elide_old_tool_results(&msgs, 1);
        // Old tool result should be elided
        assert!(elided[2]
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("elided"));
        // New tool result should be preserved
        assert_eq!(
            elided[6].content.as_ref().unwrap().as_str().unwrap(),
            "new tool output"
        );
    }

    #[test]
    fn elide_no_user_messages_returns_unchanged() {
        let msgs = vec![assistant_msg("system init")];
        let elided = elide_old_tool_results(&msgs, 1);
        assert_eq!(elided.len(), 1);
    }

    #[test]
    fn elide_keep_all_turns() {
        let msgs = vec![
            user_msg("q1"),
            tool_msg("tc-1", "result 1"),
            user_msg("q2"),
            tool_msg("tc-2", "result 2"),
        ];
        let elided = elide_old_tool_results(&msgs, 10);
        // All turns kept since keep_turns > actual turns
        assert_eq!(
            elided[1].content.as_ref().unwrap().as_str().unwrap(),
            "result 1"
        );
        assert_eq!(
            elided[3].content.as_ref().unwrap().as_str().unwrap(),
            "result 2"
        );
    }

    #[test]
    fn elide_non_tool_messages_never_modified() {
        let msgs = vec![
            user_msg("old"),
            assistant_msg("old answer"),
            user_msg("new"),
            assistant_msg("new answer"),
        ];
        let elided = elide_old_tool_results(&msgs, 1);
        assert_eq!(
            elided[1].content.as_ref().unwrap().as_str().unwrap(),
            "old answer"
        );
    }

    #[test]
    fn multipart_image_content_roundtrips_through_serialization() {
        let json = r#"[
            {"role": "system", "content": "You are helpful"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                {"type": "text", "text": "What is this?"}
            ]}
        ]"#;

        let msgs: Vec<ChatMessage> = serde_json::from_str(json).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content.as_ref().unwrap().is_string());
        assert!(msgs[1].content.as_ref().unwrap().is_array());

        let payload = serde_json::json!({
            "model": "test",
            "messages": msgs,
        });

        let out = serde_json::to_string(&payload).unwrap();
        assert!(out.contains(r#""type":"image_url"#));
        assert!(out.contains("iVBORw0KGgo="));
        assert!(out.contains(r#""type":"text"#));
        assert!(!out.contains("tool_calls"));
    }
}
