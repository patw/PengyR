//! Pengy core library — C FFI for Qt6 GUI
//!
//! The C++ Qt6 GUI creates a QThread that calls pengy_llm_chat().
//! Events are reported via callback. Tool confirmations block
//! on a condition variable that the Qt main thread signals.

mod config;
mod chat_manager;
mod tools;
mod llm_client;

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

// ── Global tokio runtime ──────────────────────────────────────────

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

// ── Helper: C string → Rust string ────────────────────────────────

unsafe fn cstr(s: *const c_char) -> String {
    if s.is_null() { String::new() } else { CStr::from_ptr(s).to_string_lossy().into_owned() }
}

fn to_c(s: &str) -> *mut c_char {
    CString::new(s).unwrap_or_default().into_raw()
}

// ── Config ────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn pengy_config_load() -> *mut c_char {
    to_c(&serde_json::to_string(&config::load_config()).unwrap_or_default())
}

#[no_mangle]
pub extern "C" fn pengy_config_save(json: *const c_char) -> bool {
    let s = unsafe { cstr(json) };
    serde_json::from_str::<config::Config>(&s)
        .map(|c| config::save_config(&c).is_ok())
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn pengy_config_render(template: *const c_char) -> *mut c_char {
    to_c(&config::render_system_message(&unsafe { cstr(template) }))
}

// ── Chats ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn pengy_chats_load() -> *mut c_char {
    to_c(&serde_json::to_string(&chat_manager::load_chats()).unwrap_or_default())
}

#[no_mangle]
pub extern "C" fn pengy_chat_create(title: *const c_char) -> *mut c_char {
    let t = unsafe { cstr(title) };
    match chat_manager::create_chat(if t.is_empty() { "New Chat" } else { &t }) {
        Ok(c) => to_c(&serde_json::to_string(&c).unwrap_or_default()),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn pengy_chat_delete(id: *const c_char) -> bool {
    chat_manager::delete_chat(&unsafe { cstr(id) }).is_ok()
}

#[no_mangle]
pub extern "C" fn pengy_chat_save(json: *const c_char) -> bool {
    serde_json::from_str::<chat_manager::Chat>(&unsafe { cstr(json) })
        .map(|c| chat_manager::save_chat(&c).is_ok())
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn pengy_chat_get(id: *const c_char) -> *mut c_char {
    match chat_manager::get_chat(&unsafe { cstr(id) }) {
        Some(c) => to_c(&serde_json::to_string(&c).unwrap_or_default()),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn pengy_clean_messages(json: *const c_char) -> *mut c_char {
    let msgs: Vec<chat_manager::ChatMessage> =
        serde_json::from_str(&unsafe { cstr(json) }).unwrap_or_default();
    to_c(&serde_json::to_string(&chat_manager::clean_dangling_tool_calls(&msgs)).unwrap_or_default())
}

// ── Tools ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn pengy_tool_is_readonly(name: *const c_char) -> bool {
    tools::is_readonly_tool(&unsafe { cstr(name) })
}

#[no_mangle]
pub extern "C" fn pengy_tool_set_user_agent(ua: *const c_char) {
    *tools::USER_AGENT.lock().unwrap() = unsafe { cstr(ua) };
}

#[no_mangle]
pub extern "C" fn pengy_tool_set_timeout(secs: u64) {
    *tools::TOOL_TIMEOUT.lock().unwrap() = secs;
}

// ── LLM Chat ──────────────────────────────────────────────────────
//
// Called from a QThread. Blocks until the conversation ends.
// Events are sent via `on_event` callback.
// Tool confirmations use a shared CondVar: the thread sets
// `confirm_state` to Pending(1) and waits; the Qt main thread
// sets it to Confirmed(2, yolo) or Declined(0).

pub type EventFn = extern "C" fn(event_json: *const c_char, userdata: *mut c_void);

/// Shared confirmation state between QThread and Qt main thread.
#[repr(C)]
pub struct ConfirmState {
    /// 0 = idle, 1 = pending, 2 = confirmed, 3 = declined
    pub status: i32,
    pub yolo_turn: bool,
}

#[no_mangle]
pub extern "C" fn pengy_llm_chat_run(
    base_url: *const c_char,
    api_key: *const c_char,
    model: *const c_char,
    messages_json: *const c_char,
    tool_confirmation: *const c_char,
    confirm_state: *mut ConfirmState,
    on_event: Option<EventFn>,
    userdata: *mut c_void,
) -> bool {
    let bu = unsafe { cstr(base_url) };
    let ak = unsafe { cstr(api_key) };
    let md = unsafe { cstr(model) };
    let ms = unsafe { cstr(messages_json) };
    let tc_str = unsafe { cstr(tool_confirmation) };

    let messages: Vec<chat_manager::ChatMessage> =
        serde_json::from_str(&ms).unwrap_or_default();
    let tc_mode = llm_client::ToolConfirmation::from_str(&tc_str);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (confirm_tx, confirm_rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let cancel2 = cancel.clone();
    let _handle = rt().spawn(async move {
        llm_client::chat(
            &bu, &ak, &md, messages, tc_mode,
            event_tx, confirm_rx, cancel2,
        ).await;
    });

    loop {
        match event_rx.blocking_recv() {
            Some(event) => {
                // Send event to C++ callback
                if let Some(ref cb) = on_event {
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    let cjson = CString::new(json).unwrap();
                    cb(cjson.as_ptr(), userdata);
                }

                match &event {
                    llm_client::LlmEvent::ToolRequest { name, .. } => {
                        // Check if we need user confirmation
                        let needs_confirm = tc_mode != llm_client::ToolConfirmation::All
                            && !(tc_mode == llm_client::ToolConfirmation::Safe
                                && tools::is_readonly_tool(name));

                        if needs_confirm && !confirm_state.is_null() {
                            // Signal the Qt thread: "we need confirmation"
                            unsafe {
                                (*confirm_state).status = 1; // pending
                            }
                            // Busy-wait for Qt to respond (runs on QThread, not main thread)
                            loop {
                                let status = unsafe { (*confirm_state).status };
                                if status == 2 || status == 3 {
                                    break;
                                }
                                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                                    let _ = confirm_tx.send(llm_client::Confirmation {
                                        tool_call_id: String::new(),
                                        confirmed: false,
                                        yolo_turn: false,
                                    });
                                    return false;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                            let (confirmed, yolo) = unsafe {
                                let s = &*confirm_state;
                                (s.status == 2, s.yolo_turn)
                            };
                            unsafe { (*confirm_state).status = 0; }
                            let _ = confirm_tx.send(llm_client::Confirmation {
                                tool_call_id: String::new(),
                                confirmed,
                                yolo_turn: yolo,
                            });
                        }
                        // else: auto-confirmed or safe — the generator handles it
                    }
                    llm_client::LlmEvent::FinalResponse { .. } => {
                        return true;
                    }
                    _ => {}
                }
            }
            None => return false,
        }
    }
}

#[no_mangle]
pub extern "C" fn pengy_llm_cancel(cancel_flag: *mut bool) {
    if !cancel_flag.is_null() {
        unsafe { *cancel_flag = true; }
    }
    tools::kill_active_process();
}

// ── Memory ────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn pengy_free(s: *mut c_char) {
    if !s.is_null() { unsafe { drop(CString::from_raw(s)); } }
}
