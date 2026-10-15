//! Cross-turn suppression uses the actual provider path, retaining originals.
//! Separate test binary isolates the process-global config override.
use pengy_core::{attachments, chat_manager::ChatMessage, config, image_capabilities, llm_client, tools};
use std::{io::{Read, Write}, net::TcpListener, sync::{Arc, Mutex, atomic::AtomicBool}};
use tokio::sync::mpsc;

async fn turn(base: &str, model: &str, messages: Vec<ChatMessage>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_confirm, confirm_rx) = mpsc::unbounded_channel();
    llm_client::chat(base, "k", model, messages, llm_client::ToolConfirmation::All, "", false,
        5, 4, tx, confirm_rx, Arc::new(AtomicBool::new(false)), Arc::new(tools::ToolContext::new())).await;
    let mut final_seen = false;
    while let Some(event) = rx.recv().await {
        match event {
            llm_client::LlmEvent::FinalResponse { .. } => final_seen = true,
            llm_client::LlmEvent::Error { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    assert!(final_seen);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_route_stays_text_only_across_turns_but_other_routes_can_see_images() {
    let dir = tempfile::tempdir().unwrap();
    config::set_config_dir(dir.path().to_str().unwrap());
    let image = dir.path().join("original.png");
    image::RgbImage::from_pixel(32, 32, image::Rgb([255, 0, 0])).save(&image).unwrap();
    let reference = attachments::import_image(&image, "original.png", 4096, 4.5, 85).unwrap();
    let mut original = ChatMessage::new("user", Some(serde_json::json!("look")));
    original.attachments.push(reference);
    let saved = serde_json::to_value(&original).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let capture = requests.clone();
    let server = std::thread::spawn(move || {
        for n in 0..6 {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut raw = vec![];
            let mut buf = [0; 4096];
            let (start, length) = loop {
                let count = sock.read(&mut buf).unwrap(); assert!(count > 0);
                raw.extend_from_slice(&buf[..count]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                    let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok()).unwrap();
                    break (pos + 4, length);
                }
            };
            while raw.len() < start + length {
                let count = sock.read(&mut buf).unwrap(); assert!(count > 0);
                raw.extend_from_slice(&buf[..count]);
            }
            capture.lock().unwrap().push(serde_json::from_slice(&raw[start..start+length]).unwrap());
            let (status, body) = if n == 0 {
                (400, serde_json::json!({"error":{"message":"This model does not support image inputs"}}))
            } else { (200, serde_json::json!({"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]})) };
            let body = body.to_string();
            write!(sock, "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    turn(&base, "text", vec![original.clone()]).await;
    turn(&(base.clone()+"/"), "text", vec![original.clone(), ChatMessage::new("user",Some(serde_json::json!("next")))]).await;
    turn(&base, "vision", vec![original.clone()]).await;
    turn(&(base.clone()+"/other"), "text", vec![original.clone()]).await;
    assert!(image_capabilities::images_rejected(&base, "text"));
    assert!(!image_capabilities::images_rejected(&base, "Text"));
    let cache = dir.path().join("image_rejections");
    for file in std::fs::read_dir(&cache).unwrap() { std::fs::remove_file(file.unwrap().path()).unwrap(); }
    assert!(!image_capabilities::images_rejected(&base, "text"));
    turn(&base, "text", vec![original.clone()]).await;
    server.join().unwrap();
    let requests = requests.lock().unwrap();
    let has_image = |r: &serde_json::Value| r["messages"].as_array().unwrap().iter().any(|m|
        m["content"].as_array().is_some_and(|parts| parts.iter().any(|p| p["type"]=="image_url")));
    assert!(has_image(&requests[0]));
    assert!(!has_image(&requests[1])); assert!(!has_image(&requests[2]));
    assert!(requests[2]["messages"][0]["content"].as_str().unwrap().contains("original.png"));
    assert_eq!(requests[2]["messages"][1]["content"], "next");
    for r in &requests[3..] { assert!(has_image(r)); }
    assert_eq!(serde_json::to_value(&original).unwrap(), saved);
    assert!(attachments::image_data_url(&original.attachments[0], 4096, 4.5, 85).is_some());
}
