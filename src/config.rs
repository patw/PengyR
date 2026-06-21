//! Configuration management for Pengy.
//!
//! Loads/saves `~/.config/pengy/settings.json` with defaults merged on load.
//! On first run, writes defaults to disk so the file can be hand-edited.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::{fs, io};

const CONFIG_DIR: &str = "pengy";
const CONFIG_FILE: &str = "settings.json";

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

    /// Number of recent turns to keep when compacting context. 0 = keep all.
    #[serde(default)]
    pub context_keep_turns: usize,

    /// UI scale percentage (75–200).
    #[serde(default = "default_ui_scale")]
    pub ui_scale: u32,

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
            context_keep_turns: 0,
            ui_scale: default_ui_scale(),
            user_agent: default_user_agent(),
            tool_timeout: default_tool_timeout(),
        }
    }
}

/// Return the path to the config file.
fn config_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(CONFIG_DIR);
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
                        if let Some(s) = v.as_str() { config.base_url = s.to_string(); }
                    }
                    if let Some(v) = obj.get("api_key") {
                        if let Some(s) = v.as_str() { config.api_key = s.to_string(); }
                    }
                    if let Some(v) = obj.get("model") {
                        if let Some(s) = v.as_str() { config.model = s.to_string(); }
                    }
                    if let Some(v) = obj.get("system_message") {
                        if let Some(s) = v.as_str() { config.system_message = s.to_string(); }
                    }
                    if let Some(v) = obj.get("tool_confirmation") {
                        if let Some(s) = v.as_str() { config.tool_confirmation = s.to_string(); }
                    }
                    if let Some(v) = obj.get("context_keep_turns") {
                        if let Some(n) = v.as_u64() { config.context_keep_turns = n as usize; }
                    }
                    if let Some(v) = obj.get("ui_scale") {
                        if let Some(n) = v.as_u64() { config.ui_scale = n as u32; }
                    }
                    if let Some(v) = obj.get("user_agent") {
                        if let Some(s) = v.as_str() { config.user_agent = s.to_string(); }
                    }
                    if let Some(v) = obj.get("tool_timeout") {
                        if let Some(n) = v.as_u64() { config.tool_timeout = n; }
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
