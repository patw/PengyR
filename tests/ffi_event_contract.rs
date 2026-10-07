//! The failure-event contract the Qt GUI depends on, driven through the real FFI.
//!
//! `pengy_llm_chat_run()` is what `ChatWorker` calls, and it forwards every
//! `LlmEvent` to a C callback. A failed turn must therefore arrive as *exactly
//! one* `error` event. If `LlmEvent::Error` were not terminal in that loop, the
//! channel would close and the `None` arm would follow it with a final response
//! reading "Chat ended unexpectedly" -- which the GUI appends as an assistant
//! message, i.e. a second, bogus answer arriving right after the real error.
//!
//! The CLI and Web frontends are covered by their own suites; this file exists
//! because the FFI loop is the one consumer with no other test harness.

use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// Serve one fixed status + body to every request; returns the base URL.
fn spawn_status_stub(status: u16, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let reason = match status {
                401 => "Unauthorized",
                403 => "Forbidden",
                _ => "Internal Server Error",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    base
}

extern "C" fn record_event(json: *const c_char, userdata: *mut c_void) {
    let text = unsafe { CStr::from_ptr(json) }
        .to_string_lossy()
        .into_owned();
    let sink = unsafe { &*(userdata as *const Mutex<Vec<String>>) };
    sink.lock().unwrap().push(text);
}

/// Run one turn through the FFI and return the events the frontend received.
fn run_turn_through_ffi(base_url: &str) -> Vec<String> {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let sink = Arc::as_ptr(&events) as *mut c_void;

    let base = CString::new(base_url).unwrap();
    let key = CString::new("").unwrap(); // a fresh install has no key
    let model = CString::new("stub-model").unwrap();
    let messages = CString::new(r#"[{"role":"user","content":"hi"}]"#).unwrap();
    let tc = CString::new("none").unwrap();
    let effort = CString::new("").unwrap();

    let ok = pengy_core::pengy_llm_chat_run(
        base.as_ptr(),
        key.as_ptr(),
        model.as_ptr(),
        messages.as_ptr(),
        tc.as_ptr(),
        effort.as_ptr(),
        false,
        std::ptr::null_mut(), // confirm state: nothing to confirm
        std::ptr::null_mut(), // sudo state
        std::ptr::null_mut(), // question state
        Some(record_event),
        sink,
        std::ptr::null_mut(), // fresh per-run tool context
    );
    assert!(ok, "a failed turn must still end the run normally");

    let out = events.lock().unwrap().clone();
    out
}

#[test]
fn length_completion_reaches_gui_as_one_terminal_error() {
    let base = spawn_status_stub(200,
        r#"{"choices":[{"message":{"role":"assistant","content":""},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":48,"total_tokens":58}}"#);
    let events = run_turn_through_ffi(&base);
    assert_eq!(events.len(), 1, "no bogus final_response after truncation: {events:?}");
    let event: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["kind"], "truncated");
    assert!(event["message"].as_str().unwrap().contains("before an answer was produced"));
}

#[test]
fn rejected_credentials_reach_the_gui_as_one_error_event() {
    let base = spawn_status_stub(
        401,
        r#"{"error":{"message":"You did not provide an API key. You need to provide your API key in an Authorization header using Bearer auth."}}"#,
    );
    let events = run_turn_through_ffi(&base);

    assert_eq!(
        events.len(),
        1,
        "expected exactly one event, got: {events:#?}"
    );
    let event: serde_json::Value = serde_json::from_str(&events[0]).expect("valid JSON event");
    assert_eq!(event["type"], "error");
    assert_eq!(event["kind"], "credentials");
    let message = event["message"].as_str().unwrap();
    assert!(
        message.contains("No API credentials are configured"),
        "{message}"
    );
    // The endpoint's own advice is replaced, not appended to.
    assert!(!message.contains("Authorization header"), "{message}");
    // ...and the GUI is not handed a "Chat ended unexpectedly" final response
    // to append as an assistant message after the error.
    assert!(!events[0].contains("final_response"), "{}", events[0]);
}

#[test]
fn server_error_reaches_the_gui_as_one_error_event() {
    let base = spawn_status_stub(500, r#"{"error":{"message":"boom"}}"#);
    let events = run_turn_through_ffi(&base);

    assert_eq!(events.len(), 1, "{events:#?}");
    let event: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["kind"], "error");
    assert!(event["message"].as_str().unwrap().contains("boom"));
    assert!(!event.to_string().contains("final_response"));
}

/// A stub that accepts the connection, waits `delay`, then answers 200 with a
/// normal completion.  Used to prove the FFI honours the *configured* LLM
/// timeout: with `llm_timeout = 1` the request times out first, whereas the old
/// hardcoded 300 would instead wait out the delay and return the completion.
fn spawn_slow_stub(delay: std::time::Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow stub");
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            std::thread::sleep(delay);
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"late"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    base
}

/// The desktop app has no per-call timeout argument: `ChatWorker` reads it from
/// settings.json, and `pengy_llm_chat_run` is the only path it has.  This pins
/// the regression that the FFI used to hardcode 300 and ignore the setting
/// entirely (the GUI's Settings dialog still wrote `llm_timeout`, so the control
/// looked live but did nothing).
#[test]
fn ffi_honours_configured_llm_timeout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), r#"{"llm_timeout": 1}"#).unwrap();
    // PENGY_CONFIG_DIR is resolved per call (not a OnceLock), so this points the
    // run at the scratch config.  Every other test in this binary passes its own
    // base_url and uses an immediate stub, so a 1s timeout cannot affect them.
    std::env::set_var("PENGY_CONFIG_DIR", dir.path());

    let base = spawn_slow_stub(std::time::Duration::from_secs(3));
    let started = std::time::Instant::now();
    let events = run_turn_through_ffi(&base);
    let elapsed = started.elapsed();

    std::env::remove_var("PENGY_CONFIG_DIR");

    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "llm_timeout=1 must abort before the 3s stub answers; took {elapsed:?} \
         (a hardcoded 300s timeout would instead wait for the completion)"
    );
    assert_eq!(events.len(), 1, "{events:#?}");
    let event: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(event["type"], "error");
    let message = event["message"].as_str().unwrap();
    assert!(message.contains("timed out"), "{message}");
    assert!(message.contains(&base), "the error must name the endpoint: {message}");
}
