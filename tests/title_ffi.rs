//! Edit Chat / Bulk Rename through the FFI the GUI uses: a real request to a
//! stub endpoint, the title written by `set_chat_title`, and `title_source`
//! deciding what a run visits.  Its own test binary, so it can own the
//! process-global config dir.

use std::ffi::{c_char, CStr, CString};
use std::io::{Read, Write};
use std::net::TcpListener;

use pengy_core::chat_manager::{create_chat, get_chat, save_chat, ChatMessage};
use pengy_core::config::{load_config, save_config, set_config_dir};

/// Answers every request with a completion whose content is `content`.
fn spawn_title_stub(status: u16, content: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 65536];
            let _ = sock.read(&mut buf);
            let body = serde_json::json!({
                "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                             "finish_reason": "stop"}]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    base
}

fn take(raw: *mut c_char) -> serde_json::Value {
    assert!(!raw.is_null());
    let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
    pengy_core::pengy_free(raw);
    serde_json::from_str(&text).unwrap()
}

fn step(id: &str, redo: bool) -> serde_json::Value {
    let id = CString::new(id).unwrap();
    take(pengy_core::pengy_title_bulk_step(id.as_ptr(), redo))
}

fn point_at(base_url: &str) {
    let mut config = load_config();
    config.base_url = base_url.into();
    config.model = "stub-model".into();
    save_config(&config).unwrap();
}

fn chat(title: &str, source: Option<&str>, with_messages: bool) -> String {
    let mut c = create_chat(title).unwrap();
    if with_messages {
        c.messages = vec![ChatMessage::new("user", Some(serde_json::json!("mount nfs at boot")))];
    }
    c.title_source = source.map(Into::into);
    save_chat(&c).unwrap();
    c.id
}

#[test]
fn bulk_step_and_targets_follow_title_source() {
    let dir = tempfile::tempdir().unwrap();
    set_config_dir(dir.path().to_str().unwrap());
    point_at(&spawn_title_stub(200, "\"NFS Mount at Boot.\""));

    let auto = chat("Auto", None, true);
    let mine = chat("Mine", Some("user"), true);
    let modeled = chat("Modeled", Some("model"), true);
    let empty = chat("New Chat", None, false);

    let targets = take(pengy_core::pengy_title_bulk_targets(false));
    assert_eq!(targets, serde_json::json!([auto]));
    let redo = take(pengy_core::pengy_title_bulk_targets(true));
    let mut redo: Vec<String> = serde_json::from_value(redo).unwrap();
    redo.sort();
    let mut want = vec![auto.clone(), modeled.clone()];
    want.sort();
    assert_eq!(redo, want, "redo includes model titles, never user ones or empty chats");

    let r = step(&auto, false);
    assert_eq!(r["status"], "renamed");
    let saved = get_chat(&auto).unwrap();
    assert_eq!(saved.title, "NFS Mount at Boot");
    assert_eq!(saved.title_source.as_deref(), Some("model"));
    // Resumable: the renamed chat drops out of the next run.
    assert_eq!(take(pengy_core::pengy_title_bulk_targets(false)), serde_json::json!([]));
    // Same title again is still written (and marked) but reported unchanged.
    assert_eq!(step(&auto, true)["status"], "unchanged");

    // The file's own marker protects a chat even if the queue included it.
    assert_eq!(step(&mine, true)["status"], "kept");
    assert_eq!(get_chat(&mine).unwrap().title, "Mine");
    assert_eq!(step(&modeled, false)["status"], "kept");
    assert_eq!(step(&empty, false)["status"], "skipped");
    assert_eq!(step("no-such-chat", false)["status"], "skipped");

    // Generate (Edit Chat) on a live copy returns a suggestion without saving.
    let live = serde_json::to_string(&get_chat(&modeled).unwrap()).unwrap();
    let live = CString::new(live).unwrap();
    let g = take(pengy_core::pengy_title_generate(live.as_ptr()));
    assert_eq!(g["title"], "NFS Mount at Boot");
    assert_eq!(get_chat(&modeled).unwrap().title, "Modeled");

    // An endpoint failure is "failed" (counts toward the abort), not "skipped".
    point_at(&spawn_title_stub(500, "boom"));
    let r = step(&modeled, true);
    assert_eq!(r["status"], "failed", "{r}");
    assert!(r["error"].as_str().unwrap().contains("HTTP 500"), "{r}");
    assert_eq!(get_chat(&modeled).unwrap().title, "Modeled");
}
