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
    /// Per-tab model override. `None` means "follow the global default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
            model: None,
        }
    }
}

// ---------------------------------------------------------------------------
// storage layout
// ---------------------------------------------------------------------------
// Chats live one per file in `<config>/chats/<id>.json`.
//
// The previous layout was a single `<config>/chats.json` array, so every save
// rewrote the whole corpus. Per-chat files make saving and opening proportional
// to the chat you touched instead of to everything you have ever said.
//
// `<config>/chats/index.json` caches the sidebar summary (id, title,
// created_at, message count, preview) so listing chats is one small read
// instead of one per chat. It is a *cache*, never the source of truth: if it is
// missing, stale, corrupt, or loses a race between two frontends, it is rebuilt
// by scanning the directory. The per-chat files are authoritative.
//
// The legacy `chats.json` is still read, so a machine that switches between the
// Python, Rust and C++ editions doesn't appear to lose history. It is never
// written and never deleted.

const CHATS_DIR: &str = "chats";
const INDEX_FILE: &str = "index.json";
const INDEX_VERSION: u32 = 1;
const PREVIEW_CHARS: usize = 200;

/// The per-chat record cached in `index.json`. Every field is derived from the
/// chat file, so the whole index can be regenerated at any time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub msg_count: usize,
    pub preview: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    #[serde(default)]
    legacy_seen: Option<(u64, u64)>, // (mtime nanos, size)
    #[serde(default)]
    chats: Vec<ChatSummary>,
}

/// Serialises index read-modify-write within this process. Across processes the
/// id-set check in `ensure_current` repairs whatever a lost race dropped.
static INDEX_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// The pre-split single-file store. Read-only; never written or removed.
fn legacy_path() -> PathBuf {
    let mut p = crate::config::pengy_config_dir();
    p.push(CHATS_FILE);
    p
}

fn chats_dir() -> PathBuf {
    let mut p = crate::config::pengy_config_dir();
    p.push(CHATS_DIR);
    p
}

fn chat_file(chat_id: &str) -> PathBuf {
    chats_dir().join(format!("{chat_id}.json"))
}

fn index_path() -> PathBuf {
    chats_dir().join(INDEX_FILE)
}

fn stat_key(path: &std::path::Path) -> Option<(u64, u64)> {
    let md = fs::metadata(path).ok()?;
    let nanos = md
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((nanos, md.len()))
}

/// Write `value` as pretty JSON to `target` atomically (temp file + rename).
fn atomic_write<T: Serialize>(target: &std::path::Path, value: &T) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, target)
}

/// Read and parse a JSON file, moving it aside if it is corrupt.
fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<T>(&text) {
        Ok(v) => Some(v),
        Err(_) => {
            backup_corrupt_file(path);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// summaries & index
// ---------------------------------------------------------------------------

/// First user message, truncated -- what `/list` and the sidebar show.
fn preview_of(chat: &Chat) -> String {
    for m in &chat.messages {
        if m.role != "user" {
            continue;
        }
        let text = match &m.content {
            Some(serde_json::Value::String(s)) => s.clone(),
            // Multipart (image) content: use the first text part.
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .find(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        return text.chars().take(PREVIEW_CHARS).collect();
    }
    String::new()
}

fn summarize(chat: &Chat) -> ChatSummary {
    ChatSummary {
        id: chat.id.clone(),
        title: chat.title.clone(),
        created_at: chat.created_at.clone(),
        msg_count: chat.messages.len(),
        preview: preview_of(chat),
    }
}

/// Newest first. `created_at` is unique in practice; id breaks ties.
fn sort_summaries(v: &mut [ChatSummary]) {
    v.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
}

fn sort_chats(v: &mut [Chat]) {
    v.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
}

/// ids of the per-chat files, from one directory read.
fn chat_ids_on_disk() -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Ok(entries) = fs::read_dir(chats_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == INDEX_FILE || !name.ends_with(".json") {
            continue;
        }
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            out.insert(name[..name.len() - 5].to_string());
        }
    }
    out
}

/// Read every per-chat file. The fallback when the index can't be trusted.
fn scan_chats() -> Vec<Chat> {
    let mut chats: Vec<Chat> = chat_ids_on_disk()
        .iter()
        .filter_map(|id| read_json::<Chat>(&chat_file(id)))
        .collect();
    sort_chats(&mut chats);
    chats
}

fn read_index() -> Option<IndexFile> {
    let idx = read_json::<IndexFile>(&index_path())?;
    if idx.version != INDEX_VERSION {
        return None;
    }
    Some(idx)
}

fn write_index(mut entries: Vec<ChatSummary>, legacy_seen: Option<(u64, u64)>) {
    sort_summaries(&mut entries);
    let _ = atomic_write(
        &index_path(),
        &IndexFile {
            version: INDEX_VERSION,
            legacy_seen,
            chats: entries,
        },
    );
}

/// Regenerate the index from the authoritative per-chat files.
fn rebuild_index(legacy_seen: Option<(u64, u64)>) -> Vec<ChatSummary> {
    let mut entries: Vec<ChatSummary> = scan_chats().iter().map(summarize).collect();
    sort_summaries(&mut entries);
    write_index(entries.clone(), legacy_seen);
    entries
}

/// Copy `chats.json` entries that have no per-chat file yet.
///
/// Existing per-chat files always win -- this only ever adds.
fn import_legacy() {
    let Some(legacy) = read_json::<Vec<Chat>>(&legacy_path()) else {
        return;
    };
    let have = chat_ids_on_disk();
    for chat in legacy {
        if chat.id.is_empty() || have.contains(&chat.id) {
            continue;
        }
        let _ = atomic_write(&chat_file(&chat.id), &chat);
    }
}

/// Bring the index in line with disk, then return its entries.
///
/// Steady state is one directory read plus one small parse. The expensive paths
/// (importing `chats.json`, rescanning every chat) run only when the cheap
/// checks say something actually changed.
fn ensure_current() -> Vec<ChatSummary> {
    let _guard = INDEX_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _ = fs::create_dir_all(chats_dir());

    let idx = read_index();
    let legacy_now = stat_key(&legacy_path());

    // chats.json appeared or was rewritten -- most likely by the Python or C++
    // edition on a machine that runs more than one. Re-import so its chats
    // become visible here.
    if legacy_now.is_some() && idx.as_ref().and_then(|i| i.legacy_seen) != legacy_now {
        import_legacy();
        return rebuild_index(legacy_now);
    }

    let Some(idx) = idx else {
        return rebuild_index(legacy_now);
    };

    // The index is a cache: if it disagrees with the directory (a frontend
    // crashed mid-write, or two raced on index.json), rebuild from files.
    let indexed: std::collections::HashSet<String> =
        idx.chats.iter().map(|c| c.id.clone()).collect();
    if indexed != chat_ids_on_disk() {
        return rebuild_index(legacy_now);
    }

    idx.chats
}

/// Insert or replace summaries without rescanning everything.
fn update_index_entries(chats: &[Chat]) {
    if chats.is_empty() {
        return;
    }
    let _guard = INDEX_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let idx = read_index();
    let legacy_seen = idx
        .as_ref()
        .and_then(|i| i.legacy_seen)
        .or_else(|| stat_key(&legacy_path()));

    let mut entries = match idx {
        Some(i) => i.chats,
        None => scan_chats().iter().map(summarize).collect(),
    };
    let ids: std::collections::HashSet<&str> = chats.iter().map(|c| c.id.as_str()).collect();
    entries.retain(|e| !ids.contains(e.id.as_str()));
    entries.extend(chats.iter().map(summarize));
    write_index(entries, legacy_seen);
}

fn drop_index_entry(chat_id: &str) {
    let _guard = INDEX_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Some(idx) = read_index() else { return };
    let legacy_seen = idx.legacy_seen;
    let mut entries = idx.chats;
    entries.retain(|e| e.id != chat_id);
    write_index(entries, legacy_seen);
}

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// Chat summaries, newest first.
///
/// This is what sidebars and `/list` want. It reads one small file instead of
/// every chat, so prefer it over [`load_chats`] where message bodies aren't
/// actually needed.
pub fn load_index() -> Vec<ChatSummary> {
    ensure_current()
}

/// Every chat in full, newest first.
///
/// Only use this when message bodies are genuinely required; it reads every
/// chat file. [`load_index`] is far cheaper for listing.
pub fn load_chats() -> Vec<Chat> {
    ensure_current();
    scan_chats()
}

/// Save each of `chats`, leaving every other chat alone.
///
/// Additive on purpose: it writes and updates, but never deletes. Use
/// [`delete_chat`] to remove a chat.
pub fn save_chats(chats: &[Chat]) -> io::Result<()> {
    fs::create_dir_all(chats_dir())?;
    for chat in chats {
        if chat.id.is_empty() {
            continue;
        }
        atomic_write(&chat_file(&chat.id), chat)?;
    }
    update_index_entries(chats);
    Ok(())
}

/// Create a new chat and persist it.
pub fn create_chat(title: &str) -> io::Result<Chat> {
    let chat = Chat::new(title);
    ensure_current();
    fs::create_dir_all(chats_dir())?;
    atomic_write(&chat_file(&chat.id), &chat)?;
    update_index_entries(std::slice::from_ref(&chat));
    Ok(chat)
}

/// Delete a chat by ID.
pub fn delete_chat(chat_id: &str) -> io::Result<()> {
    ensure_current();
    let _ = fs::remove_file(chat_file(chat_id));
    drop_index_entry(chat_id);
    Ok(())
}

/// Save a single chat -- one small file write, not the whole store.
pub fn save_chat(chat: &Chat) -> io::Result<()> {
    if chat.id.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(chats_dir())?;
    atomic_write(&chat_file(&chat.id), chat)?;
    update_index_entries(std::slice::from_ref(chat));
    Ok(())
}

/// Get a chat by ID.
pub fn get_chat(chat_id: &str) -> Option<Chat> {
    if let Some(chat) = read_json::<Chat>(&chat_file(chat_id)) {
        return Some(chat);
    }
    // Not split out yet (first run after upgrade, or written by another
    // edition): fall back to the legacy store.
    ensure_current();
    read_json::<Chat>(&chat_file(chat_id))
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
    fn chat_model_override_is_optional_and_round_trips() {
        // Default: no override, and the field is omitted from JSON.
        let chat = Chat::new("Test");
        assert_eq!(chat.model, None);
        assert!(!serde_json::to_string(&chat).unwrap().contains("\"model\""));

        // With an override: round-trips, and an old chat without the field
        // deserialises to None (backwards compatible).
        let mut chat2 = Chat::new("Test");
        chat2.model = Some("deepseek-chat".into());
        let json = serde_json::to_string(&chat2).unwrap();
        assert_eq!(serde_json::from_str::<Chat>(&json).unwrap().model, Some("deepseek-chat".into()));

        let legacy = r#"{"id":"x","title":"old","messages":[],"created_at":"2026-01-01T00:00:00"}"#;
        assert_eq!(serde_json::from_str::<Chat>(legacy).unwrap().model, None);
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
