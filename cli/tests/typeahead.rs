//! Type-ahead while a turn runs, driven through a real PTY.
//!
//! Two behaviours that used to be wrong and are easy to break again:
//!
//! 1. What you type mid-turn is *queued*, not dropped. (rustyline used to keep
//!    exactly one buffered line and discard the rest -- v1.16.0 measured 1 of 3.)
//! 2. A prompt raised mid-turn never consumes it, and Enter alone at a tool
//!    confirmation does not mean "execute".
//!
//! Both need a terminal: the capture path is a no-op when stdin is not a tty, so
//! `Command` with piped stdio (what signals.rs uses) cannot exercise it. The child
//! gets a pty as its controlling terminal, and the test writes keystrokes to the
//! master side.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// A stub endpoint: every request is answered with the next canned reply after
/// that reply's delay. Records the last user message of each request, in order.
struct Stub {
    base_url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

fn spawn_stub(plan: Vec<(u64, String)>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let plan = Arc::new(Mutex::new(plan.into_iter().collect::<std::collections::VecDeque<_>>()));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let record = record.clone();
            let plan = plan.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body_start = loop {
                    let Ok(n) = stream.read(&mut chunk) else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break end + 4;
                        }
                    }
                };

                // Last "content" string in the request is the last user message.
                let body = String::from_utf8_lossy(&buf[body_start..]).to_string();
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                    if let Some(last) = json["messages"]
                        .as_array()
                        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
                    {
                        let text = last["content"].as_str().unwrap_or("").to_string();
                        record.lock().unwrap().push(text);
                    }
                }

                let (delay_ms, content) = plan
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((50, "done".to_string()));
                std::thread::sleep(Duration::from_millis(delay_ms));
                // "TOOL:<command>" asks for a tool call, so a test can reach the
                // confirmation prompt; anything else is a plain reply.
                let (message, finish) = match content.strip_prefix("TOOL:") {
                    Some(command) => (
                        serde_json::json!({
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "run_bash",
                                    "arguments": serde_json::json!({"command": command}).to_string()
                                }
                            }]
                        }),
                        "tool_calls",
                    ),
                    None => (
                        serde_json::json!({"role": "assistant", "content": content}),
                        "stop",
                    ),
                };
                let payload = serde_json::json!({
                    "id": "chatcmpl-stub",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "stub",
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": finish
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });

    Stub {
        base_url: format!("http://127.0.0.1:{port}"),
        seen,
    }
}

/// A pty the child can use as its controlling terminal.
fn open_pty() -> (OwnedFd, OwnedFd) {
    let mut master = 0;
    let mut slave = 0;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

struct Session {
    child: Child,
    master: OwnedFd,
    out: Arc<Mutex<String>>,
    _config: TempDir,
}

impl Session {
    fn start(base_url: &str, tool_confirmation: &str) -> Session {
        let config = TempDir::new().expect("temp config");
        std::fs::write(
            config.path().join("settings.json"),
            format!(
                r#"{{"base_url": "{base_url}", "api_key": "test", "model": "stub-model", "tool_confirmation": "{tool_confirmation}"}}"#
            ),
        )
        .expect("write settings");

        let (master, slave) = open_pty();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pengy-cli"));
        cmd.arg("--config-dir").arg(config.path());
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()));
        cmd.stdout(Stdio::from(slave.try_clone().unwrap()));
        cmd.stderr(Stdio::from(slave));
        unsafe {
            cmd.pre_exec(|| {
                // Own session, then adopt the pty so the line discipline delivers
                // signals (and Ctrl-C would reach us like a real terminal).
                libc::setsid();
                libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn pengy-cli");

        // Drain the pty in the background: the child blocks writing if nobody reads.
        let master_read = master.try_clone().unwrap();
        let out = Arc::new(Mutex::new(String::new()));
        let sink = out.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let fd = std::mem::ManuallyDrop::new(master_read);
            loop {
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut pfd, 1, 100) };
                if rc < 0 {
                    break;
                }
                if rc == 0 {
                    continue;
                }
                let n = unsafe {
                    libc::read(
                        fd.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n as usize]));
            }
        });

        Session {
            child,
            master,
            out,
            _config: config,
        }
    }

    fn type_text(&self, text: &str) {
        let mut f = std::fs::File::from(self.master.try_clone().unwrap());
        let _ = f.write_all(text.as_bytes());
        let _ = f.flush();
    }

    fn output(&self) -> String {
        self.out.lock().unwrap().clone()
    }

    fn wait_for_output(&self, needle: &str, within: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < within {
            if self.output().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Three lines typed during one turn are all sent, in order.
#[test]
fn three_queued_lines_all_send_in_order() {
    let stub = spawn_stub(vec![
        (2_500, "R1".to_string()),
        (50, "R2".to_string()),
        (50, "R3".to_string()),
        (50, "R4".to_string()),
    ]);
    let session = Session::start(&stub.base_url, "all");

    assert!(
        session.wait_for_output("You", Duration::from_secs(10)),
        "the prompt never appeared: {}",
        session.output()
    );
    session.type_text("first\r");
    std::thread::sleep(Duration::from_millis(700));
    session.type_text("QA\r");
    std::thread::sleep(Duration::from_millis(150));
    session.type_text("QB\r");
    std::thread::sleep(Duration::from_millis(150));
    session.type_text("QC\r");

    // Three queued messages are announced while the turn is still running.
    assert!(
        session.wait_for_output("queued 3", Duration::from_secs(6)),
        "type-ahead was never announced: {}",
        session.output()
    );

    // ...and all three are sent, in order, one turn each.
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let seen = stub.seen.lock().unwrap().clone();
        if seen.len() >= 4 {
            assert_eq!(
                &seen[..4],
                &[
                    "first".to_string(),
                    "QA".to_string(),
                    "QB".to_string(),
                    "QC".to_string()
                ],
                "queued messages were dropped or reordered"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of 4 messages arrived: {:?}",
            seen.len(),
            seen
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A tool confirmation does not consume type-ahead, and Enter alone at the prompt
/// asks again instead of executing.
#[test]
fn confirmation_does_not_eat_typeahead_or_execute_on_enter() {
    // The reply must be slow enough that the type-ahead lands *during* the turn,
    // before the confirmation prompt appears.
    let stub = spawn_stub(vec![
        (1_500, "TOOL:echo hi".to_string()),
        (50, "done".to_string()),
        (50, "done2".to_string()),
    ]);
    let session = Session::start(&stub.base_url, "none");

    assert!(
        session.wait_for_output("You", Duration::from_secs(10)),
        "prompt never appeared: {}",
        session.output()
    );
    session.type_text("first\r");
    std::thread::sleep(Duration::from_millis(500));
    // Queued while the turn runs -- right before the confirmation appears.
    session.type_text("my queued follow-up\r");

    assert!(
        session.wait_for_output("[1/2/3/4]", Duration::from_secs(10)),
        "no tool confirmation: {}",
        session.output()
    );
    // The queued text must not have been taken as the answer.
    let out = session.output();
    assert!(
        out.contains("queued 1"),
        "type-ahead was not queued: {out}"
    );

    // Enter alone: re-ask, do not execute.
    session.type_text("\r");
    assert!(
        session.wait_for_output("Press 1 to execute", Duration::from_secs(5)),
        "empty Enter was accepted as an answer: {}",
        session.output()
    );

    // Decline, and the queued message goes out as the next turn.
    session.type_text("3\r");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let seen = stub.seen.lock().unwrap().clone();
        if seen.iter().any(|m| m == "my queued follow-up") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the queued message never went out after the turn: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
