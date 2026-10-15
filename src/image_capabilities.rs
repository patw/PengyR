//! Persistent image-input rejections, shared on disk with Python and C++.
//! One atomic file per route avoids lost updates between tabs/processes.
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::PathBuf, sync::{LazyLock, Mutex}};

static UNSAVED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn route(base_url: &str, model: &str) -> (PathBuf, String) {
    let endpoint = base_url.trim().trim_end_matches('/').to_string();
    let identity = serde_json::to_vec(&[endpoint.as_str(), model]).unwrap();
    let digest = format!("{:x}", Sha256::digest(&identity));
    (crate::config::pengy_config_dir().join("image_rejections").join(format!("{digest}.json")), endpoint)
}

pub fn images_rejected(base_url: &str, model: &str) -> bool {
    let (path, endpoint) = route(base_url, model);
    let data = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    if let Some(data) = data {
        return data["v"] == 1 && data["endpoint"] == endpoint && data["model"] == model;
    }
    UNSAVED.lock().unwrap_or_else(|p| p.into_inner()).contains(&path)
}

pub fn remember_image_rejection(base_url: &str, model: &str) {
    let (path, endpoint) = route(base_url, model);
    let result = (|| -> std::io::Result<()> {
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let suffix = format!(".{}-{}.tmp", std::process::id(), rand::random::<u64>());
        let tmp = path.with_file_name(format!("{}{}", path.file_name().unwrap().to_string_lossy(), suffix));
        let bytes = serde_json::to_vec(&serde_json::json!({"v":1, "endpoint":endpoint, "model":model,
            "rejected_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()}))?;
        let write = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)] {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if write.is_err() { let _ = std::fs::remove_file(&tmp); }
        write?;
        Ok(())
    })();
    let mut unsaved = UNSAVED.lock().unwrap_or_else(|p| p.into_inner());
    if result.is_err() { unsaved.insert(path); } else { unsaved.remove(&path); }
}
