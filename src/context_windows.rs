//! Opt-in, bounded window discovery; atomic per-route files avoid lost map updates.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub tokens: u64,
    pub input_only: bool,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    v: u32,
    endpoint: String,
    model: String,
    window: Option<Window>,
    source: String,
    expires: u64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn route(base: &str, model: &str) -> PathBuf {
    let key = serde_json::to_vec(&[base.trim().trim_end_matches('/'), model]).unwrap();
    crate::config::pengy_config_dir()
        .join("context_windows")
        .join(format!("{:x}.json", Sha256::digest(key)))
}
fn read(path: &Path, base: &str, model: &str) -> Option<Option<Window>> {
    let entry: Entry = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (entry.v == 1
        && entry.endpoint == base.trim().trim_end_matches('/')
        && entry.model == model
        && entry.expires > now()
        && entry.window.is_none_or(|w| w.tokens > 0))
    .then_some(entry.window)
}
fn write(
    path: &Path,
    base: &str,
    model: &str,
    window: Option<Window>,
    source: &str,
) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let entry = Entry {
        v: 1,
        endpoint: base.trim().trim_end_matches('/').into(),
        model: model.into(),
        window,
        source: source.into(),
        expires: now() + if window.is_some() { 86400 } else { 300 },
    };
    let result = (|| {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(&serde_json::to_vec(&entry)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}
fn number(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n > 0)
}
fn fields(v: &Value) -> Option<Window> {
    for key in ["context_window", "context_length"] {
        if let Some(n) = number(&v[key]) {
            return Some(Window {
                tokens: n,
                input_only: false,
            });
        }
    }
    if let Some(n) = number(&v["meta"]["n_ctx"]) {
        return Some(Window {
            tokens: n,
            input_only: false,
        });
    }
    number(&v["max_input_tokens"]).map(|n| Window {
        tokens: n,
        input_only: true,
    })
}
pub fn model_window(body: &Value, model: &str) -> Option<Window> {
    body["data"]
        .as_array()?
        .iter()
        .find(|v| v["id"].as_str() == Some(model))
        .and_then(fields)
}
fn bound_to_model(body: &Value, model: &str, single_model: bool) -> bool {
    body["model"]
        .as_str()
        .or_else(|| body["model_alias"].as_str())
        == Some(model)
        || single_model
}
fn siblings(base: &str) -> String {
    base.trim_end_matches('/')
        .strip_suffix("/v1")
        .unwrap_or(base.trim_end_matches('/'))
        .into()
}
/// Explicit override (including model zero) always wins. Never probe when off.
pub async fn resolve(
    base: &str,
    model: &str,
    key: &str,
    options: &crate::context_recovery::Options,
) -> Option<Window> {
    if let Some(&tokens) = options.context_windows.get(model) {
        return (tokens > 0).then_some(Window {
            tokens,
            input_only: false,
        });
    }
    if options.context_window > 0 {
        return Some(Window {
            tokens: options.context_window,
            input_only: false,
        });
    }
    if !options.enabled || !options.window_auto {
        return None;
    }
    let path = route(base, model);
    if let Some(cached) = read(&path, base, model) {
        return cached;
    }
    let discover = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(700))
            .build()
            .ok()?;
        let mut single_model = false;
        for (suffix, url) in [
            ("models", format!("{}/models", base.trim_end_matches('/'))),
            ("props", format!("{}/props", siblings(base))),
            ("health", format!("{}/health", siblings(base))),
        ] {
            let mut req = client.get(&url);
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
            let Ok(resp) = req.send().await else { continue };
            if !resp.status().is_success() {
                continue;
            }
            // Do not buffer an unbounded discovery body even if the endpoint misbehaves.
            use futures_util::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut bytes = Vec::new();
            let mut complete = true;
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    complete = false;
                    break;
                };
                if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                    complete = false;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            if !complete {
                continue;
            }
            let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let found = match suffix {
                "models" => {
                    single_model = body["data"]
                        .as_array()
                        .is_some_and(|a| a.len() == 1 && a[0]["id"].as_str() == Some(model));
                    model_window(&body, model)
                }
                "props" if bound_to_model(&body, model, single_model) => {
                    number(&body["default_generation_settings"]["n_ctx"]).map(|tokens| Window {
                        tokens,
                        input_only: false,
                    })
                }
                "health" if bound_to_model(&body, model, single_model) => {
                    number(&body["max_context"]).map(|tokens| Window {
                        tokens,
                        input_only: false,
                    })
                }
                _ => None,
            };
            if let Some(w) = found {
                return Some((w, suffix));
            }
        }
        None
    };
    let found = tokio::time::timeout(Duration::from_secs(2), discover)
        .await
        .ok()
        .flatten();
    let window = found.map(|(w, _)| w);
    let _ = write(
        &path,
        base,
        model,
        window,
        found.map(|(_, s)| s).unwrap_or("unknown"),
    );
    window
}
/// Only parse explicit maximum-context wording; never pick arbitrary first numbers.
pub fn error_window(message: &str) -> Option<Window> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(?:maximum|max) context (?:length|window|size)(?: is| of|:|=)?\s*([0-9][0-9,]*)\s*(?:tokens)?").unwrap()
    });
    static STRATA_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)prompt \([0-9,]+ tokens\) leaves no room to answer in the context \(([0-9,]+)\)",
        )
        .unwrap()
    });
    let cap = RE
        .captures(message)
        .or_else(|| STRATA_RE.captures(message))?;
    let tokens = cap[1]
        .replace(',', "")
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)?;
    Some(Window {
        tokens,
        input_only: false,
    })
}
pub fn remember_error(base: &str, model: &str, message: &str) -> Option<Window> {
    let w = error_window(message)?;
    let _ = write(&route(base, model), base, model, Some(w), "context_error");
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn current_model_shapes_and_route_prefixes() {
        let body = json!({"data":[{"id":"local","meta":{"n_ctx":262144}},{"id":"fast","context_window":1048576},{"id":"constrained","max_input_tokens":131072}]});
        assert_eq!(
            model_window(&body, "local"),
            Some(Window {
                tokens: 262144,
                input_only: false
            })
        );
        assert_eq!(model_window(&body, "fast").unwrap().tokens, 1048576);
        assert!(model_window(&body, "constrained").unwrap().input_only);
        assert!(model_window(&body, "other").is_none());
        assert_eq!(siblings("http://host/proxy/v1/"), "http://host/proxy");
        assert!(!bound_to_model(&json!({}), "local", false));
    }
    #[test]
    fn context_error_does_not_confuse_requested_and_output_tokens() {
        assert_eq!(
            error_window("requested 300000; maximum context length is 262,144 tokens")
                .unwrap()
                .tokens,
            262144
        );
        assert!(error_window("maximum output tokens: 393216").is_none());
        assert!(error_window("prompt 200000 exceeds context").is_none());
        assert_eq!(error_window("prompt (405835 tokens) leaves no room to answer in the context (262144); requests are never truncated").unwrap().tokens,262144);
    }
    #[test]
    fn cache_roundtrip_negative_corrupt_and_route_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window.json");
        let w = Window {
            tokens: 1048576,
            input_only: false,
        };
        write(&path, "http://host/v1/", "fast", Some(w), "models").unwrap();
        assert_eq!(read(&path, "http://host/v1", "fast"), Some(Some(w)));
        assert!(read(&path, "http://host/v1", "local").is_none());
        write(&path, "http://host/v1", "fast", None, "unknown").unwrap();
        assert_eq!(read(&path, "http://host/v1", "fast"), Some(None));
        std::fs::write(&path, "corrupt").unwrap();
        assert!(read(&path, "http://host/v1", "fast").is_none());
    }
}
