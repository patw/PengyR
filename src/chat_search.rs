//! Search chat history: case-insensitive word matching, newest first.
//!
//! No index and no scoring -- a scan of the per-chat files, cheap enough on
//! Enter but not per keystroke, so frontends filter titles live and run this
//! on Enter, off the UI thread.  Port of the Python edition's
//! `pengy/core/chat_search.py`; the rules are pinned in its spec.md
//! ("Chat Search") and must stay identical:
//!
//! * The query splits on whitespace into words; "double quotes" keep a phrase
//!   as one word.  Matching is lowercased substring matching.
//! * A chat matches when every word appears somewhere in it -- its title, or
//!   the text of its user and assistant messages (words may be in different
//!   messages).  Tool calls and tool output are not searched.
//! * Chats whose *title* holds every word come first (from the index, no file
//!   read), then the rest newest first, each with up to [`MAX_HITS`] snippets.
//!
//! Snippet `spans` are offsets in Unicode code points (Python `str` indexing),
//! not bytes: every frontend converts them to its own string units.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::chat_manager;

pub const MAX_RESULTS: usize = 100;
pub const MAX_HITS: usize = 3;
pub const SNIPPET_CHARS: usize = 80;
const LEAD_CHARS: usize = 30;

/// Markdown markers that only add noise to a one-line snippet.
static MARKUP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"```[\w+-]*|\*\*|__|`").expect("markup regex"));
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("ws regex"));

/// Lowercased search words; a "quoted phrase" is one word.
///
/// Same tokenising as Python's `"([^"]*)"|(\S+)`: a quote only opens a phrase
/// when a closing quote follows; otherwise it is part of an ordinary word.
pub fn parse_query(query: &str) -> Vec<String> {
    let chars: Vec<char> = query.chars().collect();
    let mut terms: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let raw: String;
        if chars[i] == '"' {
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == '"') {
                raw = chars[i + 1..i + 1 + close].iter().collect();
                i += close + 2;
            } else {
                let end = word_end(&chars, i);
                raw = chars[i..end].iter().collect();
                i = end;
            }
        } else {
            let end = word_end(&chars, i);
            raw = chars[i..end].iter().collect();
            i = end;
        }
        let term = WS_RE.replace_all(raw.trim(), " ").to_lowercase();
        if !term.is_empty() && !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn word_end(chars: &[char], start: usize) -> usize {
    chars[start..]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(chars.len(), |n| start + n)
}

/// The text of a message's content: a string, or the text parts of a list.
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Cheap pre-check on a chat file's bytes, before paying for a JSON parse.
///
/// Only plain-ASCII words without JSON-escaped characters are checked: the
/// Python edition writes non-ASCII as `\uXXXX` escapes, and quotes and
/// backslashes are escaped by every edition, so those can only be decided
/// after parsing.
pub fn could_match(raw: &[u8], terms: &[String]) -> bool {
    let lowered = raw.to_ascii_lowercase();
    terms
        .iter()
        .filter(|t| t.is_ascii() && !t.contains('"') && !t.contains('\\'))
        .all(|t| contains_bytes(&lowered, t.as_bytes()))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// Code-point index of the first `needle` in `haystack` at or after code
/// point `from`.
fn find_chars(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| haystack[i..i + needle.len()] == *needle)
}

/// ~[`SNIPPET_CHARS`] of `text` around the first matching word, Markdown
/// markers stripped and whitespace collapsed, plus `[start, end)` code-point
/// spans of every word inside it (for bolding).
pub fn make_snippet(text: &str, terms: &[String]) -> (String, Vec<[usize; 2]>) {
    let stripped = MARKUP_RE.replace_all(text, "");
    let flat: Vec<char> = WS_RE.replace_all(&stripped, " ").trim().chars().collect();
    let lowered: Vec<char> = flat.iter().collect::<String>().to_lowercase().chars().collect();
    let term_chars: Vec<Vec<char>> = terms.iter().map(|t| t.chars().collect()).collect();
    // Lowercasing can change a string's length (e.g. "İ"); positions are then
    // unreliable, so fall back to the start of the text.
    let first = if lowered.len() == flat.len() {
        term_chars.iter().filter_map(|t| find_chars(&lowered, t, 0)).min().unwrap_or(0)
    } else {
        0
    };
    let mut start = first.saturating_sub(LEAD_CHARS);
    let end = flat.len().min(start + SNIPPET_CHARS);
    start = start.min(end.saturating_sub(SNIPPET_CHARS));
    let snippet = &flat[start..end];
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < flat.len() { "…" } else { "" };
    let offset = prefix.chars().count();
    let mut spans = Vec::new();
    let low: Vec<char> = snippet.iter().collect::<String>().to_lowercase().chars().collect();
    if low.len() == snippet.len() {
        for term in &term_chars {
            let mut from = 0;
            while let Some(i) = find_chars(&low, term, from) {
                spans.push([i + offset, i + offset + term.len()]);
                from = i + term.len();
            }
        }
    }
    spans.sort();
    let body: String = snippet.iter().collect();
    (format!("{prefix}{body}{suffix}"), spans)
}

/// One message that holds search words.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    /// Position in `chat.messages`, for jumping to it.
    pub index: usize,
    pub role: String,
    pub snippet: String,
    pub spans: Vec<[usize; 2]>,
}

/// Hits for a chat (its `title` and `messages` as JSON), or `None` if some
/// word appears nowhere in it.  Messages holding more of the words are
/// preferred, then earlier ones; hits come back in message order.
pub fn match_chat(chat: &Value, terms: &[String]) -> Option<Vec<Hit>> {
    let title = chat.get("title").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let mut remaining: HashSet<&String> = terms.iter().filter(|t| !title.contains(t.as_str())).collect();
    let mut candidates: Vec<(usize, usize, String, String)> = Vec::new();
    let messages = chat.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    for (index, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = text_of(msg.get("content").unwrap_or(&Value::Null));
        if text.is_empty() {
            continue;
        }
        let lowered = text.to_lowercase();
        let present: Vec<&String> = terms.iter().filter(|t| lowered.contains(t.as_str())).collect();
        if !present.is_empty() {
            for t in &present {
                remaining.remove(t);
            }
            candidates.push((present.len(), index, role.to_string(), text));
        }
    }
    if !remaining.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .take(MAX_HITS)
        .map(|(_, index, role, text)| {
            let (snippet, spans) = make_snippet(&text, terms);
            Hit { index, role, snippet, spans }
        })
        .collect();
    hits.sort_by_key(|h| h.index);
    Some(hits)
}

/// What [`search_chats`] reports, serialised with a `type` tag exactly like
/// the Python edition's event dicts.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SearchEvent {
    Result {
        id: String,
        title: String,
        created_at: String,
        title_match: bool,
        hits: Vec<Hit>,
    },
    Progress {
        scanned: usize,
        total: usize,
    },
    Done {
        results: usize,
        truncated: bool,
        cancelled: bool,
    },
}

/// Search every stored chat, reporting events as results are found: title
/// matches first (`hits` empty), then message matches newest first, a
/// `Progress` after each chat file and `Done` last.  `cancelled` is polled
/// between chats so a frontend can drop a stale search.
pub fn search_chats(
    query: &str,
    limit: usize,
    cancelled: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(SearchEvent),
) {
    let terms = parse_query(query);
    let done = |results, truncated, cancelled| SearchEvent::Done { results, truncated, cancelled };
    if terms.is_empty() {
        on_event(done(0, false, false));
        return;
    }
    let index = chat_manager::load_index();
    let mut results = 0;

    let mut title_ids: HashSet<&str> = HashSet::new();
    for entry in &index {
        let title = entry.title.to_lowercase();
        if terms.iter().all(|t| title.contains(t.as_str())) {
            if results >= limit {
                on_event(done(results, true, false));
                return;
            }
            title_ids.insert(&entry.id);
            results += 1;
            on_event(SearchEvent::Result {
                id: entry.id.clone(),
                title: entry.title.clone(),
                created_at: entry.created_at.clone(),
                title_match: true,
                hits: Vec::new(),
            });
        }
    }

    let candidates: Vec<_> = index
        .iter()
        .filter(|e| !title_ids.contains(e.id.as_str()) && e.msg_count > 0)
        .collect();
    for (n, entry) in candidates.iter().enumerate() {
        if cancelled() {
            on_event(done(results, false, true));
            return;
        }
        let raw = chat_manager::read_chat_bytes(&entry.id).unwrap_or_default();
        if !raw.is_empty() && could_match(&raw, &terms) {
            if let Ok(chat) = serde_json::from_slice::<Value>(&raw) {
                if let Some(hits) = chat.is_object().then(|| match_chat(&chat, &terms)).flatten() {
                    if results >= limit {
                        on_event(done(results, true, false));
                        return;
                    }
                    results += 1;
                    let field = |k: &str| chat.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    on_event(SearchEvent::Result {
                        id: entry.id.clone(),
                        title: field("title"),
                        created_at: field("created_at"),
                        title_match: false,
                        hits,
                    });
                }
            }
        }
        on_event(SearchEvent::Progress { scanned: n + 1, total: candidates.len() });
    }
    on_event(done(results, false, false));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terms(q: &str) -> Vec<String> {
        parse_query(q)
    }

    #[test]
    fn parse_query_matches_python() {
        let cases: [(&str, &[&str]); 7] = [
            ("NGINX proxy", &["nginx", "proxy"]),
            ("  \"Reverse   Proxy\"  nginx ", &["reverse proxy", "nginx"]),
            ("a a A", &["a"]),
            ("\"\"", &[]),
            ("", &[]),
            ("\"open quote", &["\"open", "quote"]),
            ("run \"make all\" in C:\\src", &["run", "make all", "in", "c:\\src"]),
        ];
        for (q, expected) in cases {
            assert_eq!(parse_query(q), expected, "query {q:?}");
        }
    }

    #[test]
    fn words_may_be_in_different_messages_and_title_counts() {
        let chat = json!({"title": "t", "messages": [
            {"role": "user", "content": "set up nginx"},
            {"role": "assistant", "content": "use a proxy_pass"}]});
        let hits = match_chat(&chat, &terms("nginx proxy")).unwrap();
        assert_eq!(hits.iter().map(|h| h.index).collect::<Vec<_>>(), [0, 1]);
        assert!(match_chat(&chat, &terms("nginx apache")).is_none());
        let titled = json!({"title": "Nginx notes", "messages": [{"role": "user", "content": "proxy_pass"}]});
        assert!(match_chat(&titled, &terms("nginx proxy")).is_some());
    }

    #[test]
    fn tool_traffic_is_not_searched_but_list_content_is() {
        let chat = json!({"title": "t", "messages": [
            {"role": "user", "content": "read my config"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "1", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\": \"nginx.conf\"}"}}]},
            {"role": "tool", "tool_call_id": "1", "content": "server { nginx }"},
            {"role": "user", "content": [{"type": "text", "text": "what is in this image"},
                                         {"type": "image_url", "image_url": {}}]}]});
        assert!(match_chat(&chat, &terms("nginx")).is_none());
        assert_eq!(match_chat(&chat, &terms("image")).unwrap()[0].index, 3);
    }

    #[test]
    fn at_most_three_hits_preferring_more_words() {
        let mut msgs = vec![json!({"role": "user", "content": "nginx"}); 4];
        msgs.push(json!({"role": "assistant", "content": "nginx proxy here"}));
        let hits = match_chat(&json!({"title": "t", "messages": msgs}), &terms("nginx proxy")).unwrap();
        assert_eq!(hits.len(), MAX_HITS);
        assert!(hits.iter().any(|h| h.index == 4));
    }

    #[test]
    fn snippet_window_spans_and_markdown() {
        let text = format!("{} the NGINX config {}", "x".repeat(200), "y".repeat(200));
        let (snippet, spans) = make_snippet(&text, &terms("nginx"));
        assert!(snippet.starts_with('…') && snippet.ends_with('…'));
        let chars: Vec<char> = snippet.chars().collect();
        let [s, e] = spans[0];
        assert_eq!(chars[s..e].iter().collect::<String>(), "NGINX");

        let (snippet, spans) = make_snippet("**Use** `nginx`\n\n```bash\nreload\n```", &terms("nginx"));
        assert_eq!(snippet, "Use nginx reload");
        assert_eq!(spans, [[4, 9]]);
    }

    #[test]
    fn snippet_spans_are_code_points() {
        // Python indexes str by code point; an emoji is one, not 4 bytes or 2 UTF-16 units.
        let (snippet, spans) = make_snippet("🐧 café nginx", &terms("nginx café"));
        assert_eq!(snippet, "🐧 café nginx");
        assert_eq!(spans, [[2, 6], [7, 12]]);
    }

    #[test]
    fn pre_check_skips_only_what_it_can_see() {
        let raw = br#"{"content": "Un CAF\u00c9 cr\u00e8me, \"quoted\""}"#;
        assert!(could_match(raw, &terms("caf")));
        assert!(!could_match(raw, &terms("nginx")));
        // Non-ASCII and quoted words are left to the parsed check.
        assert!(could_match(raw, &["café".to_string(), "\"quoted\"".to_string()]));
    }

    #[test]
    fn events_serialise_like_python() {
        let event = SearchEvent::Done { results: 3, truncated: true, cancelled: false };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "done", "results": 3, "truncated": true, "cancelled": false})
        );
    }
}
