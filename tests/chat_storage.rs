//! Correctness tests for the split chat store.
//!
//! Chats live one per file in `<config>/chats/<id>.json`. `index.json` caches
//! the sidebar summary but is never authoritative -- these pin that it always
//! converges back to what the chat files say, and that the legacy `chats.json`
//! is read as a one-time seed and only trimmed by `delete_chat` (so a deleted
//! chat can't resurrect on a later import).
//!
//! `set_config_dir` writes a `OnceLock`, so the config dir can only be chosen
//! once per test binary. Everything therefore runs as one test with `reset()`
//! between phases rather than as separate `#[test]` fns.

use pengy_core::chat_manager::{
    create_chat, delete_chat, get_chat, load_chats, load_index, save_chat, save_chat_progress,
    save_chats, Chat, ChatMessage,
};
use pengy_core::config::set_config_dir;
use std::path::{Path, PathBuf};

fn reset(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir.join("chats"));
    let _ = std::fs::remove_file(dir.join("chats.json"));
    std::fs::create_dir_all(dir).unwrap();
}

fn mk(title: &str, msgs: Vec<ChatMessage>) -> Chat {
    let mut c = create_chat(title).unwrap();
    c.messages = msgs;
    save_chat(&c).unwrap();
    c
}

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage::new("user", Some(serde_json::Value::String(text.into())))
}

fn titles() -> Vec<String> {
    let mut t: Vec<String> = load_index().into_iter().map(|e| e.title).collect();
    t.sort();
    t
}

fn write_legacy(dir: &Path, chats: &[Chat]) {
    std::fs::write(
        dir.join("chats.json"),
        serde_json::to_string_pretty(chats).unwrap(),
    )
    .unwrap();
}

fn legacy_chat(id: &str, title: &str, msgs: Vec<ChatMessage>) -> Chat {
    let mut c = Chat::new(title);
    c.id = id.into();
    c.created_at = "2020-01-01T00:00:00".into();
    c.messages = msgs;
    c
}

#[test]
fn split_store_behaves() {
    let dir: PathBuf = std::env::temp_dir().join(format!("pengyr_store_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    set_config_dir(dir.to_str().unwrap());
    let chats_dir = dir.join("chats");

    // ── chats are separate files, and saves are isolated ────────────────
    reset(&dir);
    let mut a = mk("A", vec![]);
    let b = mk("B", vec![]);
    assert!(
        chats_dir.join(format!("{}.json", a.id)).exists(),
        "A has its own file"
    );
    assert!(
        chats_dir.join(format!("{}.json", b.id)).exists(),
        "B has its own file"
    );

    let b_file = chats_dir.join(format!("{}.json", b.id));
    let before = std::fs::read(&b_file).unwrap();
    a.messages.push(user_msg("hi"));
    save_chat(&a).unwrap();
    assert_eq!(
        std::fs::read(&b_file).unwrap(),
        before,
        "saving A must not rewrite B"
    );

    // ── index carries count and preview ─────────────────────────────────
    reset(&dir);
    mk(
        "A",
        vec![
            user_msg("first question"),
            ChatMessage::new(
                "assistant",
                Some(serde_json::Value::String("answer".into())),
            ),
        ],
    );
    let entry = load_index().remove(0);
    assert_eq!(entry.msg_count, 2, "index caches message count");
    assert_eq!(entry.preview, "first question", "index caches preview");

    // ── preview handles multipart (image) content ───────────────────────
    reset(&dir);
    mk(
        "A",
        vec![ChatMessage::new(
            "user",
            Some(serde_json::json!([
                {"type": "image_url", "image_url": {"url": "data:..."}},
                {"type": "text", "text": "describe this"},
            ])),
        )],
    );
    assert_eq!(
        load_index()[0].preview,
        "describe this",
        "multipart preview uses the text part"
    );

    // ── newest first ────────────────────────────────────────────────────
    reset(&dir);
    let mut older = mk("older", vec![]);
    let mut newer = mk("newer", vec![]);
    older.created_at = "2020-01-01T00:00:00".into();
    newer.created_at = "2030-01-01T00:00:00".into();
    save_chat(&older).unwrap();
    save_chat(&newer).unwrap();
    let got: Vec<String> = load_index().into_iter().map(|e| e.title).collect();
    assert_eq!(got, vec!["newer".to_string(), "older".to_string()]);

    // ── index is a cache: missing ───────────────────────────────────────
    reset(&dir);
    mk("A", vec![]);
    mk("B", vec![]);
    std::fs::remove_file(chats_dir.join("index.json")).unwrap();
    assert_eq!(titles(), vec!["A", "B"], "missing index is rebuilt");

    // ── index is a cache: corrupt ───────────────────────────────────────
    reset(&dir);
    mk("A", vec![]);
    std::fs::write(chats_dir.join("index.json"), "{ not json").unwrap();
    assert_eq!(titles(), vec!["A"], "corrupt index is rebuilt");
    let kept: Vec<_> = std::fs::read_dir(&chats_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("corrupt-"))
        .collect();
    assert!(!kept.is_empty(), "corrupt index is kept aside for recovery");

    // ── index is a cache: directory changed behind its back ─────────────
    reset(&dir);
    mk("A", vec![]);
    let ghost = legacy_chat("ghost", "GHOST", vec![]);
    std::fs::write(
        chats_dir.join("ghost.json"),
        serde_json::to_string_pretty(&ghost).unwrap(),
    )
    .unwrap();
    assert!(
        titles().contains(&"GHOST".to_string()),
        "added file is picked up"
    );
    std::fs::remove_file(chats_dir.join("ghost.json")).unwrap();
    assert!(
        !titles().contains(&"GHOST".to_string()),
        "removed file is dropped"
    );
    assert!(
        titles().contains(&"A".to_string()),
        "unrelated chat survives"
    );

    // ── delete removes the file ─────────────────────────────────────────
    reset(&dir);
    let gone = mk("A", vec![]);
    delete_chat(&gone.id).unwrap();
    assert!(!chats_dir.join(format!("{}.json", gone.id)).exists());
    assert!(load_index().is_empty());
    assert!(get_chat(&gone.id).is_none());

    // ── save_chats is additive, never destructive ───────────────────────
    reset(&dir);
    let keep = mk("KEEP", vec![]);
    let mut upd = mk("ALSO-KEEP", vec![]);
    upd.title = "UPDATED".into();
    save_chats(std::slice::from_ref(&upd)).unwrap();
    assert_eq!(
        titles(),
        vec!["KEEP", "UPDATED"],
        "save_chats must not delete"
    );
    assert!(get_chat(&keep.id).is_some());

    // ── legacy chats.json is migrated ───────────────────────────────────
    reset(&dir);
    write_legacy(&dir, &[legacy_chat("old-1", "OLD", vec![user_msg("q")])]);
    assert_eq!(titles(), vec!["OLD"], "legacy chat is visible");
    assert_eq!(get_chat("old-1").unwrap().messages.len(), 1);
    assert!(
        chats_dir.join("old-1.json").exists(),
        "legacy chat is split out"
    );
    assert_eq!(load_chats().len(), 1);

    // ── legacy chats.json is only trimmed by delete ─────────────────────
    reset(&dir);
    write_legacy(&dir, &[legacy_chat("old-1", "OLD", vec![])]);
    let legacy_bytes = std::fs::read(dir.join("chats.json")).unwrap();
    load_index();
    let mut c = get_chat("old-1").unwrap();
    c.title = "RENAMED".into();
    save_chat(&c).unwrap();
    load_chats();
    assert_eq!(
        std::fs::read(dir.join("chats.json")).unwrap(),
        legacy_bytes,
        "load/save/rename must not touch the legacy store"
    );
    delete_chat("old-1").unwrap();
    let remaining: Vec<Chat> =
        serde_json::from_slice(&std::fs::read(dir.join("chats.json")).unwrap()).unwrap();
    assert!(remaining.is_empty(), "delete must trim the deleted chat out");

    // ── deleted chat does not resurrect from legacy ─────────────────────
    reset(&dir);
    write_legacy(
        &dir,
        &[
            legacy_chat("old-1", "OLD", vec![]),
            legacy_chat("old-2", "KEEP", vec![]),
        ],
    );
    assert!(titles().contains(&"OLD".to_string()));
    delete_chat("old-1").unwrap();
    assert!(get_chat("old-1").is_none());
    // Simulate index loss: a rebuild re-runs the legacy import, but old-1
    // was trimmed out of chats.json, so it stays gone.
    std::fs::remove_file(chats_dir.join("index.json")).unwrap();
    let t = titles();
    assert!(
        !t.contains(&"OLD".to_string()),
        "deleted chat must not resurrect"
    );
    assert!(
        t.contains(&"KEEP".to_string()),
        "unrelated legacy chat survives"
    );
    assert!(get_chat("old-1").is_none());

    // ── another edition rewrote chats.json ──────────────────────────────
    reset(&dir);
    mk("MINE", vec![]);
    write_legacy(
        &dir,
        &[legacy_chat("other-1", "FROM-OTHER-EDITION", vec![])],
    );
    let t = titles();
    assert!(
        t.contains(&"FROM-OTHER-EDITION".to_string()),
        "other edition's chat imported"
    );
    assert!(
        t.contains(&"MINE".to_string()),
        "local chats survive the import"
    );

    // ── per-chat file wins over a stale legacy copy ─────────────────────
    let mut mine = get_chat("other-1").unwrap();
    mine.title = "CURRENT".into();
    save_chat(&mine).unwrap();
    write_legacy(&dir, &[legacy_chat("other-1", "LEGACY-STALE", vec![])]);
    load_index();
    assert_eq!(
        get_chat("other-1").unwrap().title,
        "CURRENT",
        "per-chat file must win over a stale legacy copy"
    );

    // ── save_chat_progress keeps out-of-band title edits ────────────────
    // A worker holds its own copy of the chat for a whole run; a rename that
    // lands mid-run must not be clobbered by the worker's next write.
    reset(&dir);
    let mut worker_copy = mk("Original", vec![user_msg("hi")]);
    let mut renamer_copy = get_chat(&worker_copy.id).unwrap();
    renamer_copy.title = "Renamed by user".into();
    save_chat(&renamer_copy).unwrap();

    worker_copy.messages.push(user_msg("mid-turn tool result"));
    save_chat_progress(&mut worker_copy).unwrap();

    let saved = get_chat(&worker_copy.id).unwrap();
    assert_eq!(saved.title, "Renamed by user", "rename must survive the run");
    assert_eq!(saved.messages.len(), 2, "worker's newer messages must land");

    let _ = std::fs::remove_dir_all(&dir);
}
