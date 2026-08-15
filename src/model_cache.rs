//! Persistent cache of the fetched model list.
//!
//! Stores the last successful `/models` result in `models_cache.json` inside
//! the config directory, keyed by the endpoint's base URL.  The cache is
//! allowed to go stale — its purpose is to keep the model dropdown populated
//! between fetches; a stale list is fine, an empty one is not.  The file
//! format matches the Python edition so all three implementations share it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::{fs, io};

const CACHE_FILE: &str = "models_cache.json";
const MAX_MODELS: usize = 500;

/// One cached model list, keyed by the endpoint it was fetched from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCache {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    pub models: Vec<String>,
}

fn cache_path() -> PathBuf {
    let mut p = crate::config::pengy_config_dir();
    p.push(CACHE_FILE);
    p
}

fn normalize(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

fn matches(cache: &ModelCache, base_url: &str) -> bool {
    normalize(&cache.url) == normalize(base_url)
}

fn backup_corrupt(path: &Path) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup = path.with_file_name(format!(
        "{}.corrupt-{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown"),
        ts
    ));
    let _ = fs::rename(path, &backup);
}

fn sanitize(models: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = models.into_iter().filter(|m| !m.is_empty()).collect();
    out.sort();
    out.dedup();
    out.truncate(MAX_MODELS);
    out
}

fn load_from(path: &Path) -> Option<ModelCache> {
    let text = fs::read_to_string(path).ok()?;
    let cache: ModelCache = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(_) => {
            backup_corrupt(path);
            return None;
        }
    };
    Some(ModelCache {
        url: cache.url,
        fetched_at: cache.fetched_at,
        models: sanitize(cache.models),
    })
}

fn save_to(path: &Path, base_url: &str, models: &[String]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let cache = ModelCache {
        url: base_url.trim().trim_end_matches('/').to_string(),
        fetched_at: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        ),
        models: sanitize(models.to_vec()),
    };

    let json = serde_json::to_string(&cache)?;
    let mut tmp = path.to_path_buf();
    tmp.set_extension("tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load the cache, returning `None` if missing or corrupt.
pub fn load_model_cache() -> Option<ModelCache> {
    load_from(&cache_path())
}

/// Persist *models* as the cached list for *base_url* (atomic write).
pub fn save_model_cache(base_url: &str, models: &[String]) -> io::Result<()> {
    save_to(&cache_path(), base_url, models)
}

/// Return the cached model list for *base_url*, or an empty vec on no match.
///
/// A cached list for a *different* endpoint is never returned — offering the
/// wrong endpoint's models would be worse than an empty dropdown.
pub fn cached_models_for(base_url: &str) -> Vec<String> {
    match load_model_cache() {
        Some(cache) if matches(&cache, base_url) => cache.models,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        (dir, path)
    }

    #[test]
    fn roundtrip() {
        let (_dir, path) = tmp_path();
        let models = vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()];
        save_to(&path, "https://api.openai.com/v1", &models).unwrap();

        let cache = load_from(&path).unwrap();
        assert_eq!(cache.url, "https://api.openai.com/v1");
        assert_eq!(cache.models, models);
        assert!(cache.fetched_at.is_some());
        assert!(matches(&cache, "https://api.openai.com/v1"));
    }

    #[test]
    fn url_keying_normalizes() {
        let (_dir, path) = tmp_path();
        save_to(&path, "https://api.openai.com/v1/", &["gpt-4o".into()]).unwrap();
        let cache = load_from(&path).unwrap();
        assert!(matches(&cache, "HTTPS://api.openai.com/v1"));
        assert!(matches(&cache, "https://api.openai.com/v1//"));
        assert!(!matches(&cache, "http://localhost:8080/v1"));
    }

    #[test]
    fn sorts_and_caps() {
        let (_dir, path) = tmp_path();
        let mut models: Vec<String> = (0..1000).map(|i| format!("m{i:04}")).collect();
        models.reverse();
        save_to(&path, "http://x", &models).unwrap();

        let got = load_from(&path).unwrap().models;
        assert_eq!(got.len(), MAX_MODELS);
        assert_eq!(got[0], "m0000");
    }

    #[test]
    fn corrupt_file_is_quarantined() {
        let (dir, path) = tmp_path();
        fs::write(&path, "{not json").unwrap();

        assert!(load_from(&path).is_none());
        assert!(!path.exists());
        let mut found = false;
        for e in fs::read_dir(dir.path()).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            if name.starts_with("models_cache.json.corrupt-") {
                found = true;
            }
        }
        assert!(found);
    }

    #[test]
    fn missing_file_returns_none() {
        let (_dir, path) = tmp_path();
        assert!(load_from(&path).is_none());
    }
}
