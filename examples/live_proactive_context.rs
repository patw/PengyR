//! Operator-only live validation. Use tests/live_proactive_context.py; synthetic inputs only.
use pengy_core::{
    chat_manager::ChatMessage,
    config,
    context_recovery::Options,
    llm_client::{self, LlmEvent, ToolConfirmation},
    tools::ToolContext,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("explicit scratch directory required")?,
    );
    if !root.join("synthetic-messages.json").is_file() || !root.join("settings.json").is_file() {
        return Err("scratch synthetic fixture/settings required".into());
    }
    config::set_config_dir(root.to_str().ok_or("invalid path")?);
    let cfg = config::load_config();
    let manifest: Value = if root.join("live-manifest.json").is_file() {
        serde_json::from_slice(&std::fs::read(root.join("live-manifest.json"))?)?
    } else {
        json!({"model":"fast", "turns": if cfg.context_window==1048576 {2}else{3}})
    };
    if manifest["model"].as_str() != Some(cfg.model.as_str()) {
        return Err("model must match the explicitly prepared live manifest".into());
    }
    let mut messages: Vec<ChatMessage> =
        serde_json::from_slice(&std::fs::read(root.join("synthetic-messages.json"))?)?;
    let source = serde_json::to_value(&messages)?;
    let options = Options::configured(&cfg, "synthetic-live");
    if manifest["discovery_only"] == true {
        let discovered =
            pengy_core::context_windows::resolve(&cfg.base_url, &cfg.model, &cfg.api_key, &options)
                .await
                .ok_or("window discovery failed")?;
        assert_eq!(
            discovered.tokens,
            manifest["expected_window"]
                .as_u64()
                .ok_or("expected window required")?
        );
        std::fs::write(
            root.join("report.json"),
            serde_json::to_vec_pretty(
                &json!({"discovered_window":discovered.tokens,"input_only":discovered.input_only}),
            )?,
        )?;
        return Ok(());
    }
    let mut reports = vec![];
    for n in 0..manifest["turns"].as_u64().unwrap_or(3) as usize {
        let prompt = manifest["prompts"][n].as_str().unwrap_or("Return the permanent release codename, deployment path, and audit status in one short line. Do not call any tool.");
        messages.push(ChatMessage::new("user", Some(json!(prompt))));
        let before = serde_json::to_value(&messages)?;
        let ctx = Arc::new(ToolContext::new());
        *ctx.recovery.lock().unwrap() = options.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_confirm_tx, confirm_rx) = mpsc::unbounded_channel();
        let (base, key, model) = (cfg.base_url.clone(), cfg.api_key.clone(), cfg.model.clone());
        let owned = messages.clone();
        let stop = cancel.clone();
        let started = Instant::now();
        let worker = tokio::spawn(async move {
            llm_client::chat(
                &base,
                &key,
                &model,
                owned,
                ToolConfirmation::None,
                "none",
                false,
                300,
                4,
                tx,
                confirm_rx,
                stop,
                ctx,
            )
            .await;
        });
        let mut events = vec![];
        let mut answer = None;
        let mut tool_count = 0;
        let mut transcript = messages.clone();
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(330), rx.recv()).await?
        {
            match &event {
                LlmEvent::AssistantToolCalls { message } => {
                    let allowed = manifest["allowed_read_file"].as_str();
                    let safe = message.tool_calls.len() == 1
                        && message.tool_calls.iter().all(|tc| {
                            let args: Value =
                                serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
                            tc.function.name == "read_file"
                                && args["path"].as_str() == allowed
                                && allowed.is_some()
                        });
                    if !safe {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    transcript.push(message.clone());
                }
                LlmEvent::ToolRequest {
                    name,
                    args,
                    tool_call_id,
                    ..
                } => {
                    let safe = name == "read_file"
                        && args["path"].as_str() == manifest["allowed_read_file"].as_str()
                        && manifest["allowed_read_file"].is_string();
                    if !safe {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    _confirm_tx.send(llm_client::Confirmation {
                        tool_call_id: tool_call_id.clone(),
                        confirmed: safe,
                        yolo_turn: false,
                        answers: None,
                        unavailable: false,
                    })?;
                    tool_count += 1;
                }
                LlmEvent::QuestionRequest { .. } => {
                    cancel.store(true, Ordering::Relaxed);
                }
                LlmEvent::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => {
                    let expected = std::fs::read_to_string(
                        manifest["allowed_read_file"]
                            .as_str()
                            .ok_or("unexpected tool result")?,
                    )?;
                    assert!(content.ends_with(&expected), "full tool output was not preserved (only a ranged-read header may precede it)");
                    let mut m = ChatMessage::new("tool", Some(json!(content)));
                    m.tool_call_id = Some(tool_call_id.clone());
                    transcript.push(m);
                }
                LlmEvent::FinalResponse {
                    message: Some(m), ..
                } => answer = Some(m.clone()),
                _ => {}
            }
            let value = serde_json::to_value(&event)?;
            println!("{}", json!({"turn":n+1,"event":value}));
            events.push(value);
        }
        worker.await?;
        assert_eq!(
            serde_json::to_value(&messages)?,
            before,
            "input transcript changed"
        );
        std::fs::write(
            root.join(format!("events-turn-{}.json", n + 1)),
            serde_json::to_vec_pretty(&events)?,
        )?;
        let final_message =
            answer.ok_or("live turn failed or attempted a tool; see event report")?;
        let text = final_message
            .content
            .as_ref()
            .and_then(Value::as_str)
            .ok_or("missing answer")?;
        for token in ["HARBOR_17", "/tmp/harbor-17", "pending"] {
            assert!(
                text.to_lowercase().contains(&token.to_lowercase()),
                "lost permanent requirement: {text}"
            );
        }
        let compact = events
            .iter()
            .filter(|v| v["type"] == "context_compacted")
            .count();
        if (n == 0 && !manifest["allowed_read_file"].is_string()) || cfg.context_window == 1048576 {
            assert_eq!(compact, 0, "cold-start/1M negative control compacted");
        }
        let expect = manifest["expected_compactions"][n].as_u64();
        if let Some(expect) = expect {
            assert_eq!(
                compact,
                expect as usize,
                "unexpected proactive count on turn {}",
                n + 1
            );
        }
        if n == 1 && cfg.context_window == 262144 && expect.is_none() {
            assert_eq!(
                compact, 1,
                "expected one proactive summary before second turn"
            );
            assert!(events
                .iter()
                .any(|v| v["type"] == "context_compacted" && v["reason"] == "proactive"));
            assert!(root.join("context_recovery").is_dir());
        }
        if manifest["allowed_read_file"].is_string() && n == 0 {
            assert_eq!(tool_count, 1, "expected one safe local read only");
        }
        reports.push(json!({"turn":n+1,"seconds":started.elapsed().as_secs_f64(),"compactions":compact,"tools_executed":tool_count,"answer":text,"usage":events.last().unwrap()["usage"]}));
        transcript.push(final_message);
        messages = transcript;
        assert_eq!(
            serde_json::to_value(&messages[..source.as_array().unwrap().len()])?,
            source,
            "original history prefix changed"
        );
    }
    std::fs::write(
        root.join("transcript-after.json"),
        serde_json::to_vec(&messages)?,
    )?;
    std::fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(
            &json!({"window":cfg.context_window,"source_turns":200,"source_unchanged":true,"runs":reports}),
        )?,
    )?;
    Ok(())
}
