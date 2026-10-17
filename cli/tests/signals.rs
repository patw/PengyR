//! SIGTERM / SIGINT / SIGHUP during a one-shot `pengy-cli` run.
//!
//! pengy-cli is mostly run by other programs as a subagent, and a parent that
//! gives up on it sends one of these.  Each must kill the running tool's
//! process group (its own session, so the signal never reaches it), repair and
//! save the chat, keep `--output json` stdout a valid document, and die by that
//! same signal, promptly -- never exit 0.  The Python and C++ editions carry
//! the same cases (`tests/test_cli_signals.py`, `cliSignal*` in tests.cpp).
//!
//! "Promptly" is asserted as under [`EXIT_WITHIN`]: the CLI takes a few tens of
//! milliseconds, but the bound must hold on a loaded CI machine too, and a
//! regression (waiting for the model, or for the 60s tool) still blows it.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const EXIT_WITHIN: Duration = Duration::from_secs(3);

/// Serves `response` to every request; `None` holds each request open.
/// Returns the base URL and a flag set once a request has arrived.
fn spawn_stub(response: Option<String>) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(AtomicBool::new(false));
    let flag = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let response = response.clone();
            let flag = flag.clone();
            std::thread::spawn(move || {
                // Read the whole request (headers + Content-Length body).
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
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
                            break;
                        }
                    }
                }
                flag.store(true, Ordering::SeqCst);
                match response {
                    None => std::thread::sleep(Duration::from_secs(30)),
                    Some(body) => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                    }
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), received)
}

fn tool_call_response(command: &str) -> String {
    serde_json::json!({
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "run_bash",
                        "arguments": serde_json::json!({"command": command}).to_string(),
                    },
                }],
            },
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
    .to_string()
}

/// True once `pid` no longer exists (a killed child is reaped by init).
fn gone(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn dangling_tool_calls(config_dir: &Path) -> Vec<String> {
    let mut dangling = Vec::new();
    let Ok(entries) = std::fs::read_dir(config_dir.join("chats")) else { return dangling };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == "index.json") {
            continue;
        }
        let chat: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let messages = chat["messages"].as_array().cloned().unwrap_or_default();
        let answered: Vec<&str> = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["tool_call_id"].as_str())
            .collect();
        for m in &messages {
            for call in m["tool_calls"].as_array().into_iter().flatten() {
                let id = call["id"].as_str().unwrap_or_default();
                if !answered.contains(&id) {
                    dangling.push(id.to_string());
                }
            }
        }
    }
    dangling
}

struct Outcome {
    signal: Option<i32>,
    took: Duration,
    stdout: String,
    stderr: String,
    tool_pid: Option<i32>,
    config: TempDir,
}

fn run_and_signal(sig: i32, in_tool: bool, output: &str) -> Outcome {
    let work = TempDir::new().unwrap();
    let pid_file = work.path().join("tool.pid");
    let response = in_tool
        .then(|| tool_call_response(&format!("echo $$ > '{}'; exec sleep 60", pid_file.display())));
    let (base_url, received) = spawn_stub(response);
    let config = TempDir::new().unwrap();
    std::fs::write(
        config.path().join("settings.json"),
        serde_json::json!({
            "base_url": base_url, "api_key": "test", "model": "stub-model",
            "tool_confirmation": "all",
        })
        .to_string(),
    )
    .unwrap();
    let home = TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pengy-cli"))
        .args(["--output", output, "run it"])
        .env("PENGY_CONFIG_DIR", config.path())
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pengy-cli");

    let started = Instant::now();
    let mut tool_pid = None;
    while started.elapsed() < Duration::from_secs(15) {
        if in_tool {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
            {
                tool_pid = Some(pid);
                break;
            }
        } else if received.load(Ordering::SeqCst) {
            break; // the request is in flight
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if in_tool {
        assert!(tool_pid.is_some(), "the tool never started");
    } else {
        assert!(received.load(Ordering::SeqCst), "the request never arrived");
    }

    let sent = Instant::now();
    unsafe { libc::kill(child.id() as i32, sig) };
    let mut waited = None;
    while sent.elapsed() < Duration::from_secs(10) {
        if let Ok(Some(status)) = child.try_wait() {
            waited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let took = sent.elapsed();
    if waited.is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    if let Some(pid) = tool_pid {
        if !gone(pid, Duration::from_secs(1)) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    Outcome {
        signal: waited.and_then(|s| s.signal()),
        took,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        tool_pid,
        config,
    }
}

fn assert_mid_tool(sig: i32, name: &str) {
    let out = run_and_signal(sig, true, "json");
    assert_eq!(out.signal, Some(sig), "stderr: {}", out.stderr);
    assert!(out.took < EXIT_WITHIN, "took {:?}", out.took);
    let pid = out.tool_pid.unwrap();
    assert!(gone(pid, Duration::from_secs(1)), "the tool's process outlived the CLI");
    assert!(dangling_tool_calls(out.config.path()).is_empty());
    let doc: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("valid JSON on stdout");
    assert_eq!(doc["error"]["type"], "interrupted");
    assert!(out.stderr.contains(&format!("Interrupted by {name}")), "{}", out.stderr);
}

#[test]
fn sigterm_mid_tool_kills_the_tool_and_dies_by_sigterm() {
    assert_mid_tool(libc::SIGTERM, "SIGTERM");
}

#[test]
fn sigint_mid_tool_kills_the_tool_and_dies_by_sigint() {
    assert_mid_tool(libc::SIGINT, "SIGINT");
}

#[test]
fn sighup_mid_tool_kills_the_tool_and_dies_by_sighup() {
    assert_mid_tool(libc::SIGHUP, "SIGHUP");
}

#[test]
fn sigterm_mid_request_dies_without_waiting_for_the_model() {
    let out = run_and_signal(libc::SIGTERM, false, "json");
    assert_eq!(out.signal, Some(libc::SIGTERM), "stderr: {}", out.stderr);
    assert!(out.took < EXIT_WITHIN, "took {:?}", out.took);
    let doc: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("valid JSON on stdout");
    assert_eq!(doc["error"]["type"], "interrupted");
}

#[test]
fn sigint_in_raw_mode_keeps_stdout_clean() {
    let out = run_and_signal(libc::SIGINT, false, "raw");
    assert_eq!(out.signal, Some(libc::SIGINT), "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "");
}
