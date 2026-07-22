//! Correctness tests for the in-memory `chats.json` cache.
//!
//! The cache skips re-parsing `chats.json` when the file is unchanged, keyed by
//! (mtime, size). These tests pin the behaviour that matters: reads stay
//! correct, writes are visible, and an external writer (the CLI, or the
//! Python/C++ editions sharing ~/.config/pengy/) invalidates the cache.

use pengy_core::chat_manager::{
    create_chat, delete_chat, get_chat, load_chats, save_chat, Chat,
};
use pengy_core::config::set_config_dir;

fn setup() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pengyr_cachetest_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    set_config_dir(dir.to_str().unwrap());
    dir
}

#[test]
fn cache_stays_correct_across_reads_writes_and_external_edits() {
    let dir = setup();
    let path = dir.join("chats.json");

    let a = create_chat("A").unwrap();
    let b = create_chat("B").unwrap();
    assert_eq!(load_chats().len(), 2, "two chats after create");

    // Reads go through the cache and stay correct.
    assert_eq!(get_chat(&b.id).unwrap().title, "B");
    assert_eq!(get_chat(&a.id).unwrap().title, "A");

    // A write is visible through the cache...
    let mut a2 = a.clone();
    a2.title = "A-renamed".into();
    save_chat(&a2).unwrap();
    assert_eq!(get_chat(&a.id).unwrap().title, "A-renamed");

    // ...and actually landed on disk (read the file directly, bypassing cache).
    let on_disk: Vec<Chat> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        on_disk.iter().any(|c| c.title == "A-renamed"),
        "rename must be persisted to disk"
    );

    // An external writer must invalidate the cache.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut arr: Vec<Chat> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut ext = Chat::new("external");
    ext.id = "ext".into();
    arr.push(ext);
    std::fs::write(&path, serde_json::to_string_pretty(&arr).unwrap()).unwrap();

    assert_eq!(
        get_chat("ext").map(|c| c.title),
        Some("external".to_string()),
        "external write must invalidate the cache"
    );
    assert_eq!(load_chats().len(), 3);

    // Delete works through the cache too.
    delete_chat("ext").unwrap();
    assert_eq!(load_chats().len(), 2);
    assert!(get_chat("ext").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}
