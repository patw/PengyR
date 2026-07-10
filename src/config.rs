//! Configuration management for Pengy.
//!
//! Loads/saves `~/.config/pengy/settings.json` with defaults merged on load.
//! On first run, writes defaults to disk so the file can be hand-edited.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::{fs, io};

const CONFIG_DIR: &str = "pengy";
const CONFIG_FILE: &str = "settings.json";

/// Global override for config directory (set via --config-dir or
/// pengy_config_set_dir FFI).
static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Override the config directory path.
pub fn set_config_dir(path: &str) {
    let _ = CONFIG_DIR_OVERRIDE.set(PathBuf::from(path));
}

/// The application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_base_url")]
    pub base_url: String,

    #[serde(default)]
    pub api_key: String,

    #[serde(default = "default_model")]
    pub model: String,

    #[serde(default = "default_system_message")]
    pub system_message: String,

    /// "all" | "safe" | "none"
    #[serde(default = "default_tool_confirmation")]
    pub tool_confirmation: String,

    /// Optional reasoning effort. Empty string = provider default / omit.
    #[serde(default)]
    pub reasoning_effort: String,

    /// Preserve provider-returned reasoning fields in message history.
    #[serde(default)]
    pub preserve_reasoning: bool,

    /// Number of recent turns to keep when compacting context. 0 = keep all.
    #[serde(default)]
    pub context_keep_turns: usize,

    /// UI scale percentage (75–200).
    #[serde(default = "default_ui_scale")]
    pub ui_scale: u32,

    /// Theme mode: "system" | "light" | "dark".
    #[serde(default = "default_theme_mode")]
    pub theme_mode: String,

    /// Accent colour: default | blue | teal | green | orange | red | pink | purple.
    #[serde(default = "default_theme_accent")]
    pub theme_accent: String,

    /// User-Agent header sent in HTTP requests.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,

    /// Tool execution timeout in seconds. 0 means no timeout.
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout: u64,
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_model() -> String {
    "gpt-4o".into()
}
fn default_system_message() -> String {
    "You are a helpful assistant named Pengy. \
     The current date is {date} and the user is {username} on host {hostname} which is {osinfo}."
        .into()
}
fn default_tool_confirmation() -> String {
    "none".into()
}
fn default_ui_scale() -> u32 {
    100
}
fn default_theme_mode() -> String {
    "system".into()
}
fn default_theme_accent() -> String {
    "default".into()
}
fn default_user_agent() -> String {
    "PengyAgent/1.0".into()
}
fn default_tool_timeout() -> u64 {
    60
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            api_key: String::new(),
            model: default_model(),
            system_message: default_system_message(),
            tool_confirmation: default_tool_confirmation(),
            reasoning_effort: String::new(),
            preserve_reasoning: false,
            context_keep_turns: 0,
            ui_scale: default_ui_scale(),
            theme_mode: default_theme_mode(),
            theme_accent: default_theme_accent(),
            user_agent: default_user_agent(),
            tool_timeout: default_tool_timeout(),
        }
    }
}

/// Return the pengy config directory (`~/.config/pengy/`).
/// Uses `$HOME/.config/pengy` on all platforms so settings are shared with the
/// Python edition and consistent across macOS / Linux / Windows.
pub fn pengy_config_dir() -> PathBuf {
    if let Some(override_dir) = CONFIG_DIR_OVERRIDE.get() {
        return override_dir.clone();
    }
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".config");
    p.push(CONFIG_DIR);
    p
}

/// Return the path to the config file.
fn config_path() -> PathBuf {
    let mut p = pengy_config_dir();
    p.push(CONFIG_FILE);
    p
}

/// Load configuration from disk, merging with defaults.
/// On first run, writes the defaults so the file exists for hand-editing.
pub fn load_config() -> Config {
    let path = config_path();

    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(raw) => {
                let mut config = Config::default();
                // Merge saved values over defaults
                if let Some(obj) = raw.as_object() {
                    if let Some(v) = obj.get("base_url") {
                        if let Some(s) = v.as_str() {
                            config.base_url = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("api_key") {
                        if let Some(s) = v.as_str() {
                            config.api_key = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("model") {
                        if let Some(s) = v.as_str() {
                            config.model = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("system_message") {
                        if let Some(s) = v.as_str() {
                            config.system_message = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("tool_confirmation") {
                        if let Some(s) = v.as_str() {
                            config.tool_confirmation = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("reasoning_effort") {
                        if let Some(s) = v.as_str() {
                            config.reasoning_effort = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("preserve_reasoning") {
                        if let Some(b) = v.as_bool() {
                            config.preserve_reasoning = b;
                        }
                    }
                    if let Some(v) = obj.get("context_keep_turns") {
                        if let Some(n) = v.as_u64() {
                            config.context_keep_turns = n as usize;
                        }
                    }
                    if let Some(v) = obj.get("ui_scale") {
                        if let Some(n) = v.as_u64() {
                            config.ui_scale = n as u32;
                        }
                    }
                    if let Some(v) = obj.get("theme_mode") {
                        if let Some(s) = v.as_str() {
                            config.theme_mode = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("theme_accent") {
                        if let Some(s) = v.as_str() {
                            config.theme_accent = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("user_agent") {
                        if let Some(s) = v.as_str() {
                            config.user_agent = s.to_string();
                        }
                    }
                    if let Some(v) = obj.get("tool_timeout") {
                        if let Some(n) = v.as_u64() {
                            config.tool_timeout = n;
                        }
                    }
                }
                config
            }
            Err(_) => {
                // Corrupt file — backup and start fresh
                backup_corrupt_file(&path);
                let config = Config::default();
                save_config(&config).ok();
                config
            }
        },
        Err(_) => {
            // No file yet — write defaults
            let config = Config::default();
            save_config(&config).ok();
            config
        }
    }
}

/// Save configuration to disk atomically.
pub fn save_config(config: &Config) -> io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(config)?;

    // Atomic write: temp file + rename
    let mut tmp = path.clone();
    tmp.set_extension("tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Rename a corrupt file so data is recoverable.
fn backup_corrupt_file(path: &std::path::Path) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup = path.with_file_name(format!(
        "{}.corrupt-{}",
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown"),
        ts
    ));
    let _ = fs::rename(path, &backup);
}

/// Fill dynamic placeholders in the system message template.
pub fn render_system_message(template: &str) -> String {
    let today = chrono::Local::now().format("%B %d, %Y").to_string();
    let username = whoami();
    let hostname = hostname();
    let osinfo = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);

    template
        .replace("{date}", &today)
        .replace("{username}", &username)
        .replace("{hostname}", &hostname)
        .replace("{osinfo}", &osinfo)
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_has_expected_values() {
        let c = Config::default();
        assert_eq!(c.base_url, "https://api.openai.com/v1");
        assert_eq!(c.model, "gpt-4o");
        assert_eq!(c.tool_confirmation, "none");
        assert_eq!(c.reasoning_effort, "");
        assert!(!c.preserve_reasoning);
        assert_eq!(c.ui_scale, 100);
        assert_eq!(c.tool_timeout, 60);
        assert_eq!(c.reasoning_effort, "");
        assert!(!c.preserve_reasoning);
        assert_eq!(c.context_keep_turns, 0);
        assert!(c.api_key.is_empty());
    }

    #[test]
    fn config_serde_round_trip() {
        let c = Config {
            base_url: "http://localhost:8080/v1".into(),
            api_key: "sk-test".into(),
            model: "llama3".into(),
            system_message: "You are {username}".into(),
            tool_confirmation: "safe".into(),
            reasoning_effort: "high".into(),
            preserve_reasoning: true,
            context_keep_turns: 5,
            ui_scale: 150,
            theme_mode: "dark".into(),
            theme_accent: "purple".into(),
            user_agent: "TestAgent/1.0".into(),
            tool_timeout: 120,
        };
        let json = serde_json::to_string(&c).unwrap();
        let c2: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.base_url, c.base_url);
        assert_eq!(c2.api_key, c.api_key);
        assert_eq!(c2.model, c.model);
        assert_eq!(c2.tool_confirmation, c.tool_confirmation);
        assert_eq!(c2.reasoning_effort, c.reasoning_effort);
        assert_eq!(c2.preserve_reasoning, c.preserve_reasoning);
        assert_eq!(c2.context_keep_turns, c.context_keep_turns);
        assert_eq!(c2.ui_scale, c.ui_scale);
        assert_eq!(c2.theme_mode, c.theme_mode);
        assert_eq!(c2.theme_accent, c.theme_accent);
        assert_eq!(c2.tool_timeout, c.tool_timeout);
    }

    #[test]
    fn config_deserialize_with_missing_fields_uses_defaults() {
        let json = r#"{"api_key": "sk-test", "model": "custom-model"}"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.api_key, "sk-test");
        assert_eq!(c.model, "custom-model");
        assert_eq!(c.base_url, "https://api.openai.com/v1");
        assert_eq!(c.tool_confirmation, "none");
        assert_eq!(c.reasoning_effort, "");
        assert!(!c.preserve_reasoning);
        assert_eq!(c.ui_scale, 100);
        assert_eq!(c.tool_timeout, 60);
    }

    #[test]
    fn config_deserialize_empty_object_uses_all_defaults() {
        let c: Config = serde_json::from_str("{}").unwrap();
        let d = Config::default();
        assert_eq!(c.base_url, d.base_url);
        assert_eq!(c.model, d.model);
        assert_eq!(c.tool_confirmation, d.tool_confirmation);
        assert_eq!(c.reasoning_effort, d.reasoning_effort);
        assert_eq!(c.preserve_reasoning, d.preserve_reasoning);
        assert_eq!(c.ui_scale, d.ui_scale);
    }

    #[test]
    fn render_system_message_replaces_all_placeholders() {
        let template = "Date: {date}, User: {username}, Host: {hostname}, OS: {osinfo}";
        let rendered = render_system_message(template);
        assert!(!rendered.contains("{date}"));
        assert!(!rendered.contains("{username}"));
        assert!(!rendered.contains("{hostname}"));
        assert!(!rendered.contains("{osinfo}"));
    }

    #[test]
    fn render_system_message_no_placeholders_unchanged() {
        let template = "Hello, world!";
        assert_eq!(render_system_message(template), "Hello, world!");
    }

    #[test]
    fn render_system_message_empty_string() {
        assert_eq!(render_system_message(""), "");
    }

    #[test]
    fn render_system_message_contains_os_info() {
        let rendered = render_system_message("{osinfo}");
        assert!(rendered.contains(std::env::consts::OS));
        assert!(rendered.contains(std::env::consts::ARCH));
    }

    #[test]
    fn config_save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let c = Config {
            base_url: "http://test:1234/v1".into(),
            api_key: "sk-round-trip".into(),
            model: "test-model".into(),
            ..Config::default()
        };
        let json = serde_json::to_string_pretty(&c).unwrap();
        fs::write(&path, &json).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let c2: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(c2.base_url, "http://test:1234/v1");
        assert_eq!(c2.api_key, "sk-round-trip");
        assert_eq!(c2.model, "test-model");
    }
}
