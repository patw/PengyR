//! Chat search over real chat files: ordering, the raw-bytes pre-check
//! against a Python-written (\uXXXX-escaped) file, the result limit, and
//! cancelling through the FFI the GUI uses.  Its own test binary, so it can
//! own the process-global config dir.

use std::ffi::{c_char, c_void, CStr, CString};

use pengy_core::chat_manager::{create_chat, save_chat, ChatMessage};
use pengy_core::chat_search::{search_chats, SearchEvent};
use pengy_core::config::set_config_dir;
use serde_json::json;

fn make_chat(title: &str, second: u32, messages: &[(&str, &str)]) -> String {
    let mut c = create_chat(title).unwrap();
    // created_at has one-second resolution; spell out the order instead.
    c.created_at = format!("2026-01-01T00:00:{second:02}");
    c.messages = messages
        .iter()
        .map(|(role, text)| ChatMessage::new(*role, Some(json!(text))))
        .collect();
    save_chat(&c).unwrap();
    c.id
}

fn search(query: &str, limit: usize) -> Vec<SearchEvent> {
    let mut events = Vec::new();
    search_chats(query, limit, &|| false, &mut |e| events.push(e));
    events
}

fn result_ids(events: &[SearchEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SearchEvent::Result { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

extern "C" fn collect_then_cancel(event: *const c_char, userdata: *mut c_void) -> bool {
    let events = unsafe { &mut *(userdata as *mut Vec<serde_json::Value>) };
    let text = unsafe { CStr::from_ptr(event) }.to_string_lossy().into_owned();
    events.push(serde_json::from_str(&text).unwrap());
    // Cancel after the first progress event.
    events.last().unwrap()["type"] != "progress"
}

#[test]
fn search_order_precheck_limit_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    set_config_dir(dir.path().to_str().unwrap());

    let old = make_chat("Old", 1, &[("user", "nginx setup")]);
    let titled = make_chat("Nginx notes", 2, &[("user", "hello")]);
    let new = make_chat("New", 3, &[("assistant", "more NGINX")]);
    make_chat("New Chat", 4, &[]);

    let events = search("nginx", 100);
    assert_eq!(result_ids(&events), [titled.clone(), new.clone(), old.clone()]);
    match &events[0] {
        SearchEvent::Result { title_match, hits, .. } => assert!(*title_match && hits.is_empty()),
        e => panic!("{e:?}"),
    }
    // Empty chats are never read, so two message candidates, not three.
    assert!(events.contains(&SearchEvent::Progress { scanned: 2, total: 2 }));
    assert_eq!(
        events.last().unwrap(),
        &SearchEvent::Done { results: 3, truncated: false, cancelled: false }
    );

    // A file as the Python edition writes it: non-ASCII as \u escapes.
    let python = make_chat("t", 5, &[("user", "placeholder")]);
    let path = dir.path().join("chats").join(format!("{python}.json"));
    let raw = std::fs::read_to_string(&path).unwrap().replace("placeholder", "Un CAF\\u00c9 cr\\u00e8me");
    std::fs::write(&path, raw).unwrap();
    assert_eq!(result_ids(&search("café", 100)), [python]);

    let limited = search("nginx", 2);
    assert_eq!(result_ids(&limited).len(), 2);
    assert_eq!(
        limited.last().unwrap(),
        &SearchEvent::Done { results: 2, truncated: true, cancelled: false }
    );

    assert_eq!(search("   ", 100), [SearchEvent::Done { results: 0, truncated: false, cancelled: false }]);

    // Through the FFI: returning false from the callback stops the scan.
    let mut seen: Vec<serde_json::Value> = Vec::new();
    let query = CString::new("zzz").unwrap();
    pengy_core::pengy_search_chats(
        query.as_ptr(),
        0,
        collect_then_cancel,
        &mut seen as *mut _ as *mut c_void,
    );
    let types: Vec<&str> = seen.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["progress", "done"]);
    assert_eq!(seen[1]["cancelled"], true);
}
