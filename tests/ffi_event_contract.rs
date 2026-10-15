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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pengy_core::llm_client::{
    MAX_EMPTY_RETRIES,
    MAX_INTERNAL_RETRIES, MAX_TIMEOUT_RETRIES, MAX_TRANSIENT_RETRIES, MAX_UNREACHABLE_RETRIES,
};

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
    run_turn_with_mode(base_url, "none")
}

/// ... with a specific confirmation mode.  "none" makes every tool call wait for
/// a confirmation this harness never sends, so a test exercising a tool round
/// must use "safe" (read-only tools auto-approve).
fn run_turn_with_mode(base_url: &str, tc_mode: &str) -> Vec<String> {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let sink = Arc::as_ptr(&events) as *mut c_void;

    let base = CString::new(base_url).unwrap();
    let key = CString::new("").unwrap(); // a fresh install has no key
    let model = CString::new("stub-model").unwrap();
    let messages = CString::new(r#"[{"role":"user","content":"hi"}]"#).unwrap();
    let tc = CString::new(tc_mode).unwrap();
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

    // A 500 now buys one retry, so the GUI sees the retry notice and then
    // exactly one error -- the contract this test pins is that the *failure*
    // arrives as a single terminal event, never as a final response.
    let notices: Vec<&String> = events.iter()
        .filter(|e| e.contains("\"retrying\""))
        .collect();
    assert_eq!(notices.len(), 1, "{events:#?}");
    let event: serde_json::Value =
        serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["kind"], "error");
    assert!(event["message"].as_str().unwrap().contains("boom"));
    assert!(
        !events.iter().any(|e| e.contains("final_response")),
        "a failed turn must not hand the GUI an assistant message: {events:?}"
    );
}

// ── Provider timeouts ────────────────────────────────────────────
//
// A provider that answers "I ran out of time" (408, or 504 from a proxy) is a
// busy endpoint, not a broken request, so the turn re-sends the request.  The
// budget is MAX_TIMEOUT_RETRIES per request; after that the ordinary timeout
// message is reported unchanged.

/// Answer `status` to the first `failures` requests, then a normal completion.
/// Returns the base URL and a counter of requests actually served.
fn spawn_flaky_stub(status: u16, failures: usize) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind flaky stub");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&served);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let n = {
                let mut guard = counter.lock().unwrap();
                *guard += 1;
                *guard
            };
            let (head, body) = if n <= failures {
                (format!("HTTP/1.1 {status} Timeout\r\n"),
                 r#"{"error":{"message":"Request timed out","type":"timeout"}}"#.to_string())
            } else {
                ("HTTP/1.1 200 OK\r\n".to_string(),
                 r#"{"choices":[{"message":{"role":"assistant","content":"recovered"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#.to_string())
            };
            let resp = format!(
                "{head}Content-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    (base, served)
}

fn timeout_notices(events: &[String]) -> Vec<serde_json::Value> {
    events.iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .filter(|e| e["type"] == "retrying" && e["reason"] == "timeout")
        .collect()
}

#[test]
fn provider_timeout_is_retried_and_the_turn_completes() {
    for status in [408u16, 504] {
        let (base, served) = spawn_flaky_stub(status, 1);
        let events = run_turn_through_ffi(&base);
        let notices = timeout_notices(&events);

        assert_eq!(notices.len(), 1, "{status}: {events:#?}");
        assert_eq!(notices[0]["status_code"], status, "{events:#?}");
        assert_eq!(notices[0]["attempt"], 1, "{events:#?}");
        assert_eq!(notices[0]["max_attempts"], MAX_TIMEOUT_RETRIES, "{events:#?}");
        let last: serde_json::Value =
            serde_json::from_str(events.last().unwrap()).unwrap();
        assert_eq!(last["type"], "final_response", "{events:#?}");
        assert!(last["content"].as_str().unwrap().contains("recovered"), "{events:#?}");
        assert_eq!(*served.lock().unwrap(), 2, "{status}: one retry expected");
    }
}

#[test]
fn every_retry_announcement_is_followed_by_a_launch_notice() {
    // A surface that has just shown "retrying in Xs" must hear when the pause
    // is over and the attempt is in flight, or it advertises a finished
    // backoff for the whole (llm_timeout-bounded) attempt -- the frozen
    // "retrying in 0.9s" hang. Each launch notice carries the same attempt
    // number its announcement promised, and follows it, never precedes it.
    let (base, served) = spawn_flaky_stub(408, 2);
    let events = run_turn_through_ffi(&base);
    let parsed: Vec<serde_json::Value> = events.iter()
        .map(|e| serde_json::from_str(e).unwrap())
        .collect();

    let seq: Vec<(String, u32)> = parsed.iter()
        .filter(|e| e["type"] == "retrying" || e["type"] == "request_started")
        .map(|e| (e["type"].as_str().unwrap().to_string(),
                 e["attempt"].as_u64().unwrap() as u32))
        .collect();
    assert_eq!(seq, vec![
        ("retrying".into(), 1), ("request_started".into(), 1),
        ("retrying".into(), 2), ("request_started".into(), 2),
    ], "{events:#?}");
    // The main-loop launches are turn-stage attempts.
    for e in &parsed {
        if e["type"] == "request_started" {
            assert_eq!(e["stage"], "turn", "{events:#?}");
        }
    }
    let last = parsed.last().unwrap();
    assert_eq!(last["type"], "final_response", "{events:#?}");
    assert!(last["content"].as_str().unwrap().contains("recovered"), "{events:#?}");
    assert_eq!(*served.lock().unwrap(), 3, "{events:#?}");
}

#[test]
fn provider_timeout_budget_is_spent_then_the_timeout_message() {
    let (base, served) = spawn_flaky_stub(408, 99);
    let events = run_turn_through_ffi(&base);
    let notices = timeout_notices(&events);

    assert_eq!(notices.len(), MAX_TIMEOUT_RETRIES as usize, "{events:#?}");
    assert_eq!(notices.last().unwrap()["attempt"], MAX_TIMEOUT_RETRIES, "{events:#?}");
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["type"], "error");
    assert!(last["message"].as_str().unwrap().contains("timed out"), "{events:#?}");
    // 1 request + the budget, and no silent extra attempt.
    assert_eq!(*served.lock().unwrap(), MAX_TIMEOUT_RETRIES as usize + 1);
}

#[test]
fn a_busy_endpoint_that_recovers_costs_the_user_nothing() {
    // The point of the feature: 429-style flakiness that used to end the turn.
    let (base, served) = spawn_flaky_stub(408, 2);
    let events = run_turn_through_ffi(&base);
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["type"], "final_response", "{events:#?}");
    assert_eq!(*served.lock().unwrap(), 3);
}

#[test]
fn overload_keeps_its_own_budget_and_label() {
    // Negative control: 429 must not be swept into the timeout budget/label.
    let base = spawn_status_stub(429, r#"{"error":{"message":"Too Many Requests"}}"#);
    let events = run_turn_through_ffi(&base);

    assert!(timeout_notices(&events).is_empty(), "{events:#?}");
    let overloaded: Vec<serde_json::Value> = events.iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .filter(|e| e["type"] == "retrying")
        .collect();
    assert_eq!(overloaded.len(), 5, "{events:#?}");
    assert!(overloaded.iter().all(|e| e["reason"].is_null()), "{events:#?}");
}

#[test]
fn a_bad_request_is_never_retried() {
    // Negative control: "the request was wrong" must fail fast, not after 3 waits.
    let base = spawn_status_stub(400, r#"{"error":{"message":"Invalid request"}}"#);
    let events = run_turn_through_ffi(&base);

    assert!(timeout_notices(&events).is_empty(), "{events:#?}");
    assert_eq!(events.len(), 1, "{events:#?}");
}

// ── 5xx and dropped routes ───────────────────────────────────────

#[test]
fn server_error_is_retried_and_the_turn_completes() {
    // A proxy/upstream restart answers 502/503 for seconds; a multi-hour task
    // must ride that out, not end on it.
    for status in [502u16, 503] {
        let (base, served) = spawn_flaky_stub(status, 1);
        let events = run_turn_through_ffi(&base);
        let notices: Vec<serde_json::Value> = events.iter()
            .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
            .filter(|e| e["type"] == "retrying" && e["reason"] == "server")
            .collect();

        assert_eq!(notices.len(), 1, "{status}: {events:#?}");
        assert_eq!(notices[0]["status_code"], status, "{events:#?}");
        assert_eq!(notices[0]["max_attempts"], MAX_TRANSIENT_RETRIES, "{events:#?}");
        let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
        assert_eq!(last["type"], "final_response", "{events:#?}");
        assert_eq!(*served.lock().unwrap(), 2, "{status}: one retry expected");
    }
}

#[test]
fn internal_server_error_gets_one_retry_only() {
    // A 500 is often the request's own fault, so it buys one retry, not a chain.
    let (base, served) = spawn_flaky_stub(500, 99);
    let events = run_turn_through_ffi(&base);
    let notices: Vec<serde_json::Value> = events.iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .filter(|e| e["type"] == "retrying" && e["reason"] == "server")
        .collect();

    assert_eq!(notices.len(), MAX_INTERNAL_RETRIES as usize, "{events:#?}");
    assert_eq!(notices[0]["max_attempts"], MAX_INTERNAL_RETRIES, "{events:#?}");
    assert_eq!(*served.lock().unwrap(), MAX_INTERNAL_RETRIES as usize + 1);
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["type"], "error");
}

/// A stub that serves forever, plus the switch that takes it away.
fn spawn_stub_with_stop(status: u16, body: &'static str) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}/v1");
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop2.load(Ordering::Relaxed) {
                break; // the listener drops here: the port is closed
            }
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    (base, stop)
}

fn wait_for_closed_port(addr: std::net::SocketAddr) {
    // Wake the serving loop (it breaks on the next connection) and wait until
    // the port really refuses, so the client sees a genuine dropped route.
    let _ = std::net::TcpStream::connect(addr);
    for _ in 0..200 {
        if std::net::TcpStream::connect(addr).is_err() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("port {addr} never closed");
}

#[test]
fn a_route_that_drops_mid_session_is_retried() {
    // The endpoint answered, then went away: a restart, an OOM kill, a
    // sleep/wake dropping the route.  That is worth seconds, not a verdict.
    let (base, stop) = spawn_stub_with_stop(200,
        r#"{"choices":[{"message":{"role":"assistant","content":"first"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#);
    let addr = base.trim_start_matches("http://").trim_end_matches("/v1").parse().unwrap();

    let first = run_turn_through_ffi(&base);
    let last: serde_json::Value = serde_json::from_str(first.last().unwrap()).unwrap();
    assert_eq!(last["type"], "final_response", "{first:#?}");

    stop.store(true, Ordering::Relaxed);
    wait_for_closed_port(addr);

    let events = run_turn_through_ffi(&base);
    let notices: Vec<serde_json::Value> = events.iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .filter(|e| e["type"] == "retrying" && e["reason"] == "unreachable")
        .collect();
    assert_eq!(notices.len(), MAX_UNREACHABLE_RETRIES as usize, "{events:#?}");
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["type"], "error");
    assert!(last["message"].as_str().unwrap().contains("Nothing answered"), "{events:#?}");
}

#[test]
fn first_contact_refusal_is_not_retried() {
    // Negative control: a wrong URL must produce the "is your server running?"
    // help immediately, not after three waits.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    drop(listener); // nothing is listening, and never was

    let events = run_turn_through_ffi(&base);
    assert!(timeout_notices(&events).is_empty(), "{events:#?}");
    assert_eq!(events.len(), 1, "{events:#?}");
    let event: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(event["type"], "error");
    assert!(event["message"].as_str().unwrap().contains("Nothing answered"), "{events:#?}");
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
///
/// Since timeout auto-retry landed, the turn no longer aborts on the first
/// timeout: it spends the timeout-retry budget and *then* reports the timeout.
/// The regression this test guards is unchanged -- a hardcoded 300s would sit
/// through the 3s stub and hand back its completion -- so what is pinned here is
/// that the configured 1s deadline is what ends every attempt, that the chain is
/// bounded by MAX_TIMEOUT_RETRIES, and that the stub's late answer never reaches
/// the GUI.
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

    let parsed: Vec<serde_json::Value> = events.iter()
        .map(|e| serde_json::from_str(e).unwrap())
        .collect();
    let timeouts: Vec<&serde_json::Value> = parsed.iter()
        .filter(|e| e["type"] == "retrying" && e["reason"] == "timeout")
        .collect();

    // Four attempts at 1s plus the backoff ladder: bounded, and far short of
    // the 300s the FFI used to hardcode (which would have waited for the
    // completion instead).
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "the timeout chain must stay bounded by the retry budget; took {elapsed:?} \
         (a hardcoded 300s timeout would instead wait for the completion)"
    );
    assert_eq!(timeouts.len(), MAX_TIMEOUT_RETRIES as usize, "{events:#?}");
    assert!(
        !events.iter().any(|e| e.contains("late")),
        "the late completion must not reach the GUI: {events:#?}"
    );

    let last = parsed.last().expect("the turn must end with an event");
    assert_eq!(last["type"], "error");
    let message = last["message"].as_str().unwrap();
    assert!(message.contains("timed out"), "{message}");
    assert!(message.contains(&base), "the error must name the endpoint: {message}");
}

// ── Degenerate 200 responses ─────────────────────────────────────
//
// An endpoint that answers 200 and produces nothing usable is a provider
// anomaly, not a completed answer.  Two shapes: no `choices` at all (the shape a
// proxy answers when it loses its upstream mid-generation), and a message with
// neither content nor tool calls (observed in the field: a reasoning model whose
// whole output allowance goes to reasoning tokens).

/// Serve each `(status, body)` in order, then repeat the last one forever.
fn spawn_scripted_stub(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted stub");
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let queue = Arc::new(Mutex::new(replies));
    let served = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&served);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let (status, body) = {
                let mut q = queue.lock().unwrap();
                let next = q.first().cloned().unwrap_or((200, "{}"));
                if q.len() > 1 { q.remove(0); }
                next
            };
            let mut guard = counter.lock().unwrap();
            *guard += 1;
            drop(guard);
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    (base, served)
}

const NO_CHOICES: &str = r#"{"choices":[],"usage":null}"#;
const NO_CHOICES_KEY: &str = r#"{"id":"cmpl-stub","object":"chat.completions"}"#;
const EMPTY_MESSAGE: &str = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null},"finish_reason":"stop"}],"usage":null}"#;

#[test]
fn no_choices_is_retried_then_reported_actionably() {
    for payload in [NO_CHOICES, NO_CHOICES_KEY] {
        let (base, served) = spawn_scripted_stub(vec![(200, payload)]);
        let events = run_turn_through_ffi(&base);
        let notices: Vec<serde_json::Value> = events.iter()
            .filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok())
            .filter(|e| e["type"] == "retrying" && e["reason"] == "empty")
            .collect();

        assert_eq!(notices.len(), MAX_EMPTY_RETRIES as usize, "{events:#?}");
        let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
        assert_eq!(last["type"], "error", "{events:#?}");
        assert_eq!(last["kind"], "empty", "{events:#?}");
        let message = last["message"].as_str().unwrap();
        assert!(message.contains("no choices"), "{message}");
        // The user gets a sentence they can act on, not a raw body dump.
        assert!(message.contains("proxy"), "{message}");
        assert!(!message.contains("index out of range"), "{message}");
        assert_eq!(*served.lock().unwrap(), MAX_EMPTY_RETRIES as usize + 1, "{events:#?}");
    }
}

#[test]
fn no_choices_that_recovers_costs_the_user_nothing() {
    let (base, served) = spawn_scripted_stub(vec![
        (200, NO_CHOICES),
        (200, r#"{"choices":[{"message":{"role":"assistant","content":"answered"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#),
    ]);
    let events = run_turn_through_ffi(&base);
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["type"], "final_response", "{events:#?}");
    assert!(last["content"].as_str().unwrap().contains("answered"), "{events:#?}");
    assert_eq!(*served.lock().unwrap(), 2);
}

#[test]
fn an_empty_reply_is_reported_and_never_delivered_as_an_answer() {
    let (base, served) = spawn_scripted_stub(vec![(200, EMPTY_MESSAGE)]);
    let events = run_turn_through_ffi(&base);

    // No blank assistant turn reaches the GUI, so nothing blank is persisted.
    assert!(
        !events.iter().any(|e| e.contains("final_response")),
        "an empty reply must not be delivered as an answer: {events:#?}"
    );
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["kind"], "empty", "{events:#?}");
    assert!(last["message"].as_str().unwrap().contains("empty reply"), "{events:#?}");
    // An empty answer is the endpoint's shape, not a blip: no full-cost chain.
    assert_eq!(*served.lock().unwrap(), 1, "{events:#?}");
}

#[test]
fn an_empty_reply_with_reasoning_names_the_budget() {
    // The field signature (BotTalk ff4f1c1f): reasoning tokens, no answer.
    let (base, _served) = spawn_scripted_stub(vec![(200,
        r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"thinking thinking thinking"},"finish_reason":"stop"}],"usage":{"prompt_tokens":40,"completion_tokens":1500,"total_tokens":1540}}"#)]);
    let events = run_turn_through_ffi(&base);
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    let message = last["message"].as_str().unwrap();
    assert_eq!(last["kind"], "empty", "{events:#?}");
    assert!(message.contains("reasoning"), "{message}");
    assert!(message.contains("output allowance"), "{message}");
    assert!(message.contains("1500"), "{message}");
}

#[test]
fn an_empty_message_with_tool_calls_is_still_a_normal_round() {
    // Negative control: content:"" plus tool calls is a normal round, not an anomaly.
    let (base, _served) = spawn_scripted_stub(vec![
        (200, r#"{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"t1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"does-not-exist.txt\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#),
        (200, r#"{"choices":[{"message":{"role":"assistant","content":"after the tool"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#),
    ]);
    let events = run_turn_with_mode(&base, "safe");
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["type"], "final_response", "{events:#?}");
    assert!(last["content"].as_str().unwrap().contains("after the tool"), "{events:#?}");
}

#[test]
fn a_truncated_empty_answer_keeps_the_generation_limit_path() {
    // Negative control: finish_reason=length is already handled, and stays there.
    let (base, _served) = spawn_scripted_stub(vec![
        (200, r#"{"choices":[{"message":{"role":"assistant","content":""},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":48,"total_tokens":58}}"#),
    ]);
    let events = run_turn_through_ffi(&base);
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["kind"], "truncated", "{events:#?}");
    assert!(last["message"].as_str().unwrap().contains("finish_reason=length"), "{events:#?}");
}
