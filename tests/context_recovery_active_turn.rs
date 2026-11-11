//! Regression: a long single-turn job must recover from a full context window on
//! its own, without the user typing "try again".
//!
//! Measured 2026-11-11 on `pengy` (GUI) against Basalt: a job failed with
//! "Generation limit reached ... Pengy retried after 4 context reduction(s)
//! without success" (prompt 261,216 tokens, completion 920). Typing "try again"
//! recovered immediately, reporting `historical reasoning; 306008 fewer
//! characters` and then `tool call arguments; 371360 fewer characters`.
//!
//! Cause: every elision stage protects the ACTIVE turn — reasoning is only
//! stripped before the last user message, and tool-argument elision has the same
//! boundary — so inside one long agentic turn the ladder can only spend its four
//! attempts on tool-output previews/stubs, which cannot touch the reasoning and
//! argument payload. A new user message moves that boundary, which is why the
//! manual retry worked. `active_turn` is the same reduction applied without
//! waiting for the user (see `context_recovery::Recovery::deep_plan`).
use pengy_core::{
    chat_manager::{ChatMessage, FunctionCall, ToolCall},
    config, context_recovery, llm_client, tools,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc;

const MODEL: &str = "local";
/// The fake engine's window, in "tokens" (characters / 4). Anything larger is
/// answered exactly like Basalt answers an over-window request: 200 OK with
/// finish_reason=length and a prompt that sits at the window.
const WINDOW_TOKENS: usize = 150_000;

fn filler(chars: usize) -> String {
    "the quick brown fox jumps over the lazy dog. ".repeat(chars / 45 + 1)
}

/// ONE user turn followed by a long agentic stretch: every generated message
/// belongs to the active task, which the elision stages deliberately protect.
fn long_single_turn(rounds: usize) -> Vec<ChatMessage> {
    let mut out = vec![
        ChatMessage::new("system", Some(serde_json::json!("You are Pengy."))),
        ChatMessage::new("user", Some(serde_json::json!("build the whole thing, all of it"))),
    ];
    for i in 0..rounds {
        let mut assistant = ChatMessage::new("assistant", Some(serde_json::json!("")));
        assistant.reasoning = Some(serde_json::json!(filler(20_000)));
        assistant.tool_calls = vec![ToolCall {
            id: format!("call_{i}"),
            call_type: "function".into(),
            function: FunctionCall {
                name: "run_bash".into(),
                arguments: serde_json::json!({"command": filler(20_000)}).to_string(),
            },
        }];
        out.push(assistant);
        out.push(ChatMessage {
            role: "tool".into(),
            content: Some(serde_json::json!(filler(40_000))),
            attachments: vec![],
            tool_calls: vec![],
            tool_call_id: Some(format!("call_{i}")),
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        });
    }
    out
}

fn serve(listener: TcpListener, stop: Arc<AtomicBool>, seen: Arc<Mutex<Vec<usize>>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut sock) = stream else { continue };
            sock.set_read_timeout(Some(std::time::Duration::from_secs(20))).ok();
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let (start, length) = loop {
                match sock.read(&mut buf) {
                    Ok(0) => break (0, 0),
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                    Err(_) => break (0, 0),
                }
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok()))
                        .unwrap_or(0);
                    break (pos + 4, length);
                }
            };
            if start == 0 {
                continue;
            }
            while raw.len() < start + length {
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            let req: serde_json::Value =
                serde_json::from_slice(&raw[start..start + length.min(raw.len() - start)]).unwrap_or(serde_json::json!({}));
            let chars = req["messages"].to_string().len();
            let tokens = chars / 4;
            let over = req.get("tools").is_some() && tokens > WINDOW_TOKENS;
            seen.lock().unwrap().push(tokens);
            let body = if over {
                serde_json::json!({"choices":[{"finish_reason":"length","message":{"role":"assistant","content":""}}],"usage":{"prompt_tokens":261216,"completion_tokens":920,"total_tokens":262136}})
            } else {
                serde_json::json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"all done"}}],"usage":{"prompt_tokens":tokens,"completion_tokens":5,"total_tokens":tokens+5}})
            };
            let text = body.to_string();
            let _ = write!(sock, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", text.len(), text);
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn long_single_turn_recovers_without_a_new_user_message() {
    let dir = tempfile::tempdir().unwrap();
    config::set_config_dir(dir.path().to_str().unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::<usize>::new()));
    let server = serve(listener, stop.clone(), seen.clone());

    let ctx = Arc::new(tools::ToolContext::new());
    {
        let mut opts = context_recovery::Options::default();
        opts.chat_id = "long-job".into();
        opts.compact_at_percent = 75;
        opts.context_windows.insert(MODEL.into(), 262_144);
        *ctx.recovery.lock().unwrap() = opts;
    }

    let messages = long_single_turn(24);
    let tokens = serde_json::to_value(&messages).unwrap().to_string().len() / 4;
    assert!(tokens > WINDOW_TOKENS * 2, "fixture must start well over the window, got ~{tokens}");

    let (event_tx, mut rx) = mpsc::unbounded_channel();
    let (_confirm_tx, confirm_rx) = mpsc::unbounded_channel();
    llm_client::chat(
        &base, "k", MODEL, messages, llm_client::ToolConfirmation::None, "", false, 30, 4,
        event_tx, confirm_rx, Arc::new(AtomicBool::new(false)), ctx.clone(),
    )
    .await;

    let mut strategies = vec![];
    let mut final_answer = None;
    let mut error = None;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            llm_client::LlmEvent::ContextCompacted { message, .. } => {
                let text = message.unwrap_or_default();
                let strategy = text
                    .trim_start_matches("Context recovery — ")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .to_string();
                strategies.push(strategy);
            }
            llm_client::LlmEvent::FinalResponse { content, .. } => final_answer = Some(content),
            llm_client::LlmEvent::Error { message, .. } => error = Some(message),
            _ => {}
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect(base.trim_start_matches("http://"));
    let _ = server.join();

    assert_eq!(error, None, "the turn must not fail; reductions were {strategies:?}");
    assert_eq!(final_answer.as_deref(), Some("all done"));
    assert!(
        strategies.iter().any(|s| s.contains("active turn")),
        "the last-resort stage must be what saves the turn, got {strategies:?}"
    );
    assert!(
        seen.lock().unwrap().last().is_some_and(|t| *t < WINDOW_TOKENS),
        "the request that succeeded must fit the window, saw {:?}",
        seen.lock().unwrap()
    );
}
