//! Isolated integration binary so config overrides never affect other tests.
use pengy_core::{attachments, chat_manager::ChatMessage, config, llm_client, tools};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use tokio::sync::mpsc;

#[tokio::test(flavor = "multi_thread")]
async fn attachment_reference_image_recovery_preserves_history_and_retries_once() {
    let dir = tempfile::tempdir().unwrap();
    config::set_config_dir(dir.path().to_str().unwrap());
    let path = dir.path().join("sample.png");
    image::RgbImage::from_pixel(32, 32, image::Rgb([255, 0, 0]))
        .save(&path)
        .unwrap();
    let reference = attachments::import_image(&path, "sample.png", 4096, 4.5, 85).unwrap();
    let mut original = ChatMessage::new("user", Some(serde_json::json!("Inspect my attachment")));
    original.attachments.push(reference);
    let before = serde_json::to_value(&original).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = requests.clone();
    // Reject twice: the second response must surface, not strip/retry forever.
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let (start, length) = loop {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap();
                    break (pos + 4, length);
                }
            };
            while raw.len() < start + length {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&buf[..n]);
            }
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&raw[start..start + length]).unwrap());
            let body = serde_json::json!({"error":{"message":"Adapter cannot translate pictures",
                "source":"openai-proxy", "code":"unsupported_content_type", "content_type":"image_url"}}).to_string();
            write!(sock, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    let (event_tx, mut rx) = mpsc::unbounded_channel();
    let (_confirm_tx, confirm_rx) = mpsc::unbounded_channel();
    llm_client::chat(
        &base,
        "test-key",
        "test",
        vec![original.clone()],
        llm_client::ToolConfirmation::None,
        "",
        false,
        10,
        4,
        event_tx,
        confirm_rx,
        Arc::new(AtomicBool::new(false)),
        Arc::new(tools::ToolContext::new()),
    )
    .await;
    assert!(
        matches!(rx.recv().await.unwrap(), llm_client::LlmEvent::Error { message, .. }
        if message.contains("Adapter cannot translate pictures"))
    );
    server.join().unwrap();
    let req = requests.lock().unwrap();
    assert_eq!(req.len(), 2);
    assert!(req[0]["messages"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["type"] == "image_url"));
    assert_eq!(req[1]["messages"][0]["content"], "Inspect my attachment");
    assert!(req[1]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("proxy adapter"));
    assert!(req
        .iter()
        .all(|r| r["messages"][0].get("attachments").is_none()));
    assert_eq!(serde_json::to_value(original).unwrap(), before);
}
