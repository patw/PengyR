//! Tool definitions and execution for Pengy.
//!
//! Defines 16 OpenAI function-calling tools (17 on Windows, which adds
//! run_powershell) and their implementations.

use futures_util::StreamExt;
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Global state ────────────────────────────────────────────────────

pub static TOOL_TIMEOUT: Mutex<u64> = Mutex::new(300);
pub static TOOL_OUTPUT_MAX_CHARS: Mutex<usize> = Mutex::new(250000);
pub static DOWNLOAD_MAX_MB: Mutex<u64> = Mutex::new(100);
pub static USER_AGENT: Mutex<String> = Mutex::new(String::new());

/// download_file uses a stall timeout (no bytes for this long) instead of the
/// tool timeout, so a large transfer isn't killed mid-stream.
const DOWNLOAD_STALL_TIMEOUT: u64 = 120;

/// Preprocessing limits `read_image` applies before base64-encoding.
pub static IMAGE_MAX_DIMENSION: Mutex<u32> = Mutex::new(4096);
pub static IMAGE_MAX_MB: Mutex<f64> = Mutex::new(4.5);
pub static IMAGE_QUALITY: Mutex<u8> = Mutex::new(85);

/// An image queued by `read_image`, waiting to be attached to the conversation.
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub path: String,
    pub mime: String,
    pub b64: String,
}

/// A blocking callback that prompts the user for a sudo password.
/// Called with the target host (`None` for the local machine) so the prompt
/// can name it.  Returns the password, or `None` if the user cancels.
pub type SudoProvider = Box<dyn Fn(Option<&str>) -> Option<String> + Send + Sync>;

/// Per-run tool state: sudo provider, cached sudo passwords (keyed by host,
/// `None` = local), and the set of active subprocess groups.
///
/// Each concurrent run (e.g. one per GUI tab) gets its own context so a sudo
/// prompt is routed to the right run and pressing Stop on one run kills only
/// that run's subprocesses — never another tab's.  Shared as `Arc<ToolContext>`
/// so it can be cloned into `spawn_blocking` closures and across the FFI.
///
/// A password is never offered to a host it wasn't entered for.
pub struct ToolContext {
    sudo_provider: Mutex<Option<SudoProvider>>,
    cached_sudo_passwords: Mutex<HashMap<Option<String>, String>>,
    active_process_groups: Mutex<HashSet<u32>>,
    pending_images: Mutex<Vec<PendingImage>>,
    /// Set by `pengy_llm_cancel` so the LLM loop aborts at the next yield point.
    pub recovery: Mutex<crate::context_recovery::Options>,
    pub cancelled: Arc<AtomicBool>,
}

impl ToolContext {
    pub fn new() -> Self {
        Self {
            sudo_provider: Mutex::new(None),
            cached_sudo_passwords: Mutex::new(HashMap::new()),
            active_process_groups: Mutex::new(HashSet::new()),
            pending_images: Mutex::new(Vec::new()),
            recovery: Mutex::new(crate::context_recovery::Options::default()),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Install (or clear) the sudo provider; clears any cached passwords.
    pub fn set_sudo_provider(&self, provider: Option<SudoProvider>) {
        *self.sudo_provider.lock().unwrap() = provider;
        self.clear_sudo();
    }

    pub fn clear_sudo(&self) {
        self.cached_sudo_passwords.lock().unwrap().clear();
    }

    fn cached_sudo_password(&self, host: Option<&str>) -> Option<String> {
        self.cached_sudo_passwords
            .lock()
            .unwrap()
            .get(&host.map(String::from))
            .cloned()
    }

    /// Discard *host*'s cached password (after a failed sudo authentication).
    pub fn forget_sudo_password(&self, host: Option<&str>) {
        self.cached_sudo_passwords
            .lock()
            .unwrap()
            .remove(&host.map(String::from));
    }

    /// Queue an image for attachment to the conversation.
    ///
    /// `execute_tool` returns a `String` and OpenAI-compatible APIs only accept
    /// string content in a `role: "tool"` message, so `read_image` can't hand
    /// the picture back through its return value.  It parks the encoded image
    /// here and the LLM loop drains the queue once every tool result for the
    /// turn is in place.
    pub fn add_pending_image(&self, path: String, mime: String, b64: String) {
        self.pending_images
            .lock()
            .unwrap()
            .push(PendingImage { path, mime, b64 });
    }

    /// Return queued images and clear the queue.
    pub fn take_pending_images(&self) -> Vec<PendingImage> {
        std::mem::take(&mut *self.pending_images.lock().unwrap())
    }

    fn register_process(&self, pid: u32) {
        self.active_process_groups.lock().unwrap().insert(pid);
    }

    fn unregister_process(&self, pid: u32) {
        self.active_process_groups.lock().unwrap().remove(&pid);
    }

    /// Kill every subprocess group registered in this context.
    pub fn kill_all(&self) {
        let pids: Vec<u32> = self
            .active_process_groups
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect();
        for pid in pids {
            terminate_process_group(pid);
        }
        self.active_process_groups.lock().unwrap().clear();
    }
}

impl Default for ToolContext {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tool schema definitions ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: ParametersDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParametersDef {
    #[serde(rename = "type")]
    pub param_type: String,
    pub properties: serde_json::Value,
    pub required: Vec<String>,
}

use std::sync::LazyLock;

// Cached serde_json::Value of the tool definitions — built once, reused on
// every API call. Avoids constructing 11 ToolDef structs + serializing them
// to JSON on every request. Cloning a Value::Array is much cheaper than the
// struct construction + serialization it replaces.
static TOOLS_JSON: LazyLock<serde_json::Value> =
    LazyLock::new(|| serde_json::to_value(tool_definitions()).unwrap());

/// Pre-serialized tool definitions as a JSON value (cached).
/// Use this in the LLM loop instead of `tool_definitions()` to avoid
/// per-request allocation.
pub fn tool_definitions_json() -> serde_json::Value {
    TOOLS_JSON.clone()
}

/// The tool list the model sees on this platform (see `platform_tools`).
pub fn tool_definitions() -> Vec<ToolDef> {
    platform_tools(
        base_tool_definitions(),
        cfg!(windows),
        powershell_label(POWERSHELL.as_deref()),
        windows_is_admin(),
    )
}

/// The POSIX tool list; Windows adapts it in `platform_tools`.
fn base_tool_definitions() -> Vec<ToolDef> {
    vec![
        td("read_file", "Read a text file. Long files are truncated with a header saying how to continue using offset/limit. Use read_image for images.",
            &[("path", "string", "File path"),
              ("offset", "integer", "First line to read (1-based)"),
              ("limit", "integer", "Maximum number of lines")],
            &["path"]),
        td("read_image", "Look at an image file (PNG, JPEG, GIF, WebP, BMP, TIFF); it is attached so you can see it.",
            &[("path", "string", "Image file path")],
            &["path"]),
        td("write_file", "Create or overwrite a file. Parent directories are created.",
            &[("path", "string", "File path"),
              ("content", "string", "Full file content")],
            &["path", "content"]),
        td("replace_in_file", "Replace exact text in a file. old_str must match exactly one place (include surrounding lines to make it unique) unless replace_all=true.",
            &[("path", "string", "File path"),
              ("old_str", "string", "Exact text to replace, including whitespace"),
              ("new_str", "string", "Replacement text (empty to delete)"),
              ("replace_all", "boolean", "Replace every occurrence")],
            &["path", "old_str", "new_str"]),
        apply_changes_definition(),
        td("run_bash", "Run a bash command and return its output. Non-interactive: stdin is closed. For root, write sudo in the command and set elevated=true. To run on another machine set host (don't call ssh yourself). Killed after the tool timeout.",
            &[("command", "string", "Bash command"),
              ("cwd", "string", "Working directory"),
              ("host", "string", "ssh destination (key login must already work)"),
              ("elevated", "boolean", "true when the command uses sudo")],
            &["command"]),
        td("web_search", "Search the web; returns titles, URLs and snippets.",
            &[("query", "string", "Search query"),
              ("max_results", "integer", "Number of results (default 5)")],
            &["query"]),
        td("download_file", "Download a URL to a file and return the saved path.",
            &[("url", "string", "URL to download"),
              ("filename", "string", "File name (default: from the URL)"),
              ("dir", "string", "Directory to save into (default ~/Downloads)"),
              ("max_size_mb", "integer", "Size limit in MB (default: the configured limit; 0 = none)")],
            &["url"]),
        td("fetch_url", "Fetch a URL and return the body as text (HTML is converted to text).",
            &[("url", "string", "URL to fetch"),
              ("max_chars", "integer", "Maximum characters returned (0 = no limit)")],
            &["url"]),
        td("run_python", "Run Python code in a fresh process; only printed output comes back. Killed after the tool timeout.",
            &[("code", "string", "Python source"),
              ("cwd", "string", "Working directory")],
            &["code"]),
        td("directory_tree", "Show a directory tree; use it to get oriented in an unfamiliar project. Skips .git, node_modules and similar.",
            &[("path", "string", "Directory"),
              ("max_depth", "integer", "Depth limit (default 3)"),
              ("show_hidden", "boolean", "Include dot-files")],
            &["path"]),
        td("read_multiple_files", "Read several text files in one call.",
            &[("paths", "array", "File paths")],
            &["paths"]),
        td("search_content", "Search file contents under a directory or in a file. Literal text unless regex=true. Skips binary files and .git, node_modules and similar.",
            &[("pattern", "string", "Text (or regex) to find"),
              ("regex", "boolean", "Treat pattern as a regular expression"),
              ("path", "string", "Directory or file to search"),
              ("file_glob", "string", "Only files matching this glob, e.g. *.py or src/**/*.ts"),
              ("context_lines", "integer", "Lines of context around each match"),
              ("max_results", "integer", "Maximum matches (default 50)"),
              ("ignore_case", "boolean", "Case-insensitive match")],
            &["pattern", "path"]),
        td("glob", "Find files by glob pattern; ** matches any depth. Skips hidden and build/vendor directories; at most 200 results.",
            &[("pattern", "string", "Glob, e.g. src/**/*.py"),
              ("path", "string", "Directory to search (default: current)")],
            &["pattern"]),
        todowrite_definition(),
        ask_user_question_definition(),
    ]
}

fn apply_changes_definition() -> ToolDef {
    ToolDef { tool_type: "function".into(), function: FunctionDef {
        name: "apply_changes".into(),
        description: "Apply several exact-text edits across existing files, all or nothing. dry_run=true returns the unified diff without writing.".into(),
        parameters: ParametersDef {
            param_type: "object".into(),
            properties: serde_json::json!({
                "changes": {
                    "type": "array",
                    "description": "Per file: path and operations. An operation is replace (old, new), delete (old) or insert_after (anchor, text); old/anchor must match expected_matches times (default 1)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File path"},
                            "operations": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "kind": {"type": "string", "enum": ["replace", "insert_after", "delete"]},
                                        "old": {"type": "string", "description": "Exact text for replace/delete"},
                                        "anchor": {"type": "string", "description": "Exact text to insert after"},
                                        "new": {"type": "string", "description": "Replacement text"},
                                        "text": {"type": "string", "description": "Text to insert"},
                                        "expected_matches": {"type": "integer", "description": "Required match count (default 1)"}
                                    },
                                    "required": ["kind"]
                                }
                            }
                        },
                        "required": ["path", "operations"]
                    }
                },
                "dry_run": {"type": "boolean", "description": "Preview only; nothing is written"},
                "postconditions": {
                    "type": "array",
                    "description": "Optional {path, contains, does_not_contain} plain-substring checks on a file's whole edited text, comments and strings included; a failed check cancels every edit",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "contains": {"type": "string"},
                            "does_not_contain": {"type": "string"}
                        },
                        "required": ["path"]
                    }
                }
            }),
            required: vec!["changes".into()],
        }
    }}
}

fn todowrite_definition() -> ToolDef {
    ToolDef { tool_type: "function".into(), function: FunctionDef {
        name: "todowrite".into(),
        description: "Keep a task list for multi-step work. Send the whole list each time; at most one task in_progress.".into(),
        parameters: ParametersDef {
            param_type: "object".into(),
            properties: serde_json::json!({
                "todos": {
                    "type": "array",
                    "description": "All tasks with their status",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Task, imperative (e.g. 'Run the tests')"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"], "description": "pending, in_progress or completed"}
                        },
                        "required": ["content", "status"]
                    }
                }
            }),
            required: vec!["todos".into()],
        }
    }}
}

fn ask_user_question_definition() -> ToolDef {
    ToolDef { tool_type: "function".into(), function: FunctionDef {
        name: "ask_user_question".into(),
        description: "Ask the user multiple-choice questions. Use it instead of guessing when the request is ambiguous, or when what you found contradicts what the user said.".into(),
        parameters: ParametersDef {
            param_type: "object".into(),
            properties: serde_json::json!({
                "questions": {
                    "type": "array",
                    "description": "Questions, each with a header, question text and options",
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {"type": "string", "description": "Short label, e.g. 'Auth'"},
                            "question": {"type": "string", "description": "The question"},
                            "options": {
                                "type": "array",
                                "description": "The choices",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "description": "Choice label"},
                                        "description": {"type": "string", "description": "One-line explanation of the choice"}
                                    },
                                    "required": ["label", "description"]
                                }
                            }
                        },
                        "required": ["header", "question", "options"]
                    }
                }
            }),
            required: vec!["questions".into()],
        }
    }}
}

fn td(name: &str, desc: &str, props: &[(&str, &str, &str)], required: &[&str]) -> ToolDef {
    let mut properties = serde_json::Map::new();
    for (pname, ptype, pdesc) in props {
        let schema = if *ptype == "array" {
            serde_json::json!({"type": ptype, "items": {"type": "string"}, "description": pdesc})
        } else {
            serde_json::json!({"type": ptype, "description": pdesc})
        };
        properties.insert(pname.to_string(), schema);
    }
    ToolDef {
        tool_type: "function".into(),
        function: FunctionDef {
            name: name.into(),
            description: desc.into(),
            parameters: ParametersDef {
                param_type: "object".into(),
                properties: serde_json::Value::Object(properties),
                required: required.iter().map(|s| s.to_string()).collect(),
            },
        },
    }
}

// ── Platform-specific tool surface ──────────────────────────────────
//
// One local-shell tool per platform, named for the shell it really runs: the
// tool name is the strongest hint a model gets about which syntax to write.
// POSIX keeps run_bash exactly as defined above.  Windows gets run_powershell
// for local commands, and run_bash survives only for remote hosts (host=),
// since an ssh target really does run a POSIX shell.  No shell translation
// either way.  Keep the wording identical to the Python and C++ editions.

/// PowerShell used by run_powershell, resolved once (Windows only).
static POWERSHELL: LazyLock<Option<PathBuf>> =
    LazyLock::new(|| if cfg!(windows) { find_powershell() } else { None });

/// PowerShell 7 (pwsh) is preferred when installed, but a clean Windows 11
/// ships only Windows PowerShell 5.1, so that is the supported floor.  The
/// absolute System32 path covers a PATH that has lost the v1.0 directory.
/// Never falls back to cmd.exe.
fn find_powershell() -> Option<PathBuf> {
    for name in ["pwsh", "powershell"] {
        if let Some(found) = find_in_path(name, std::env::var_os("PATH")) {
            // find_in_path accepts a bare name whose .exe exists; spawn the
            // real file.
            if !found.is_file() {
                return Some(found.with_extension("exe"));
            }
            return Some(found);
        }
    }
    let builtin = system_root()
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    builtin.is_file().then_some(builtin)
}

fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

/// Human/model-facing name for the resolved PowerShell.
fn powershell_label(path: Option<&Path>) -> &'static str {
    let full = path.map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    let name = full.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    if name == "pwsh" || name == "pwsh.exe" {
        "PowerShell 7"
    } else {
        "Windows PowerShell 5.1"
    }
}

/// True when this process holds an elevated (Administrator) token.
#[cfg(windows)]
fn windows_is_admin() -> bool {
    #[link(name = "shell32")]
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
fn windows_is_admin() -> bool {
    false
}

fn run_powershell_definition(label: &str, is_admin: bool) -> ToolDef {
    let privilege = if is_admin {
        "Pengy is running as Administrator, so commands already have full administrative rights (HKLM registry, services, scheduled tasks, firewall, Windows features); no elevation step is needed."
    } else {
        "Pengy is NOT running as Administrator. Commands that need admin rights (writing HKLM, managing services, scheduled tasks that run as SYSTEM or with highest privileges, firewall rules, Windows features, machine-wide installs) fail with access denied. Do not try to self-elevate with Start-Process -Verb RunAs or sudo: the elevated process cannot be captured here. Instead tell the user the step needs admin rights: they can restart Pengy with Run as administrator, or run the command themselves."
    };
    let dialect = if label == "Windows PowerShell 5.1" {
        " This is Windows PowerShell 5.1, not PowerShell 7: there are no && / || chain operators and no ternary operator, and curl/wget are aliases for Invoke-WebRequest (use curl.exe for real curl)."
    } else {
        ""
    };
    let desc = format!(
        "Run a PowerShell script on this Windows machine with {label}. Use PowerShell syntax and cmdlets; this is not bash. The script may span multiple lines. It is non-interactive: stdin is closed, so anything that prompts (Read-Host, Get-Credential, confirmation prompts, an editor) fails rather than waits; pass -Force, -Confirm:$false or other non-interactive flags. Set cwd to run in a specific directory. Default table formatting is cut to a narrow width, so for wide or detailed objects pipe to Format-List, ConvertTo-Json, or Out-String -Width 4096. The exit code is the last native command's exit code, or 1 if the script throws or fails to parse; non-terminating errors are shown but do not change it. {privilege}{dialect} To run on a remote Linux or macOS machine, use run_bash with host. Commands are killed once the configured tool timeout elapses."
    );
    td(
        "run_powershell",
        &desc,
        &[("command", "string", "The PowerShell script to execute"),
          ("cwd", "string", "Optional working directory to run the script in")],
        &["command"],
    )
}

/// Windows variant of run_bash: same parameters, host required.
fn remote_only_run_bash(mut def: ToolDef) -> ToolDef {
    def.function.description = "Run a bash command on a remote Linux or macOS host over ssh. On this Windows machine run_bash is only for remote hosts: host is required, and local commands go through run_powershell. Use the ssh destination the user uses (an ~/.ssh/config alias, host, or user@host); key-based login must already work. The command is non-interactive: stdin is closed, so anything that prompts or waits for input fails rather than waits; pass non-interactive flags instead. cwd, if given, is a path on the remote host. To run something as root there, include an explicit `sudo ...` in the command AND set elevated=true; Pengy then prompts for that host's sudo password. elevated=true does NOT elevate on its own: a command with elevated=true but no `sudo` is rejected. Do not wrap the command in ssh yourself. Commands are killed once the configured tool timeout elapses.".into();
    def.function.parameters.required = vec!["command".into(), "host".into()];
    def
}

/// Return the tool list the model sees on this platform.
fn platform_tools(base: Vec<ToolDef>, windows: bool, powershell_label: &str, is_admin: bool) -> Vec<ToolDef> {
    if !windows {
        return base;
    }
    let mut tools = Vec::with_capacity(base.len() + 1);
    for def in base {
        if def.function.name == "run_bash" {
            tools.push(run_powershell_definition(powershell_label, is_admin));
            tools.push(remote_only_run_bash(def));
            continue;
        }
        tools.push(def);
    }
    tools
}

/// On Windows run_bash is remote-only; a local call gets this pointer.
fn windows_local_run_bash_error(windows: bool, host: Option<&str>) -> Option<String> {
    if windows && host.is_none() {
        Some("Error: on Windows, run_bash only runs on remote hosts (set host). Use run_powershell for commands on this machine.".into())
    } else {
        None
    }
}

pub fn is_readonly_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "read_multiple_files"
            | "directory_tree"
            | "search_content"
            | "web_search"
            | "fetch_url"
            | "glob"
            | "todowrite"
            | "read_image"
    )
}

// ── Tool execution dispatcher ───────────────────────────────────────

pub async fn execute_tool(
    name: &str,
    arguments: &serde_json::Value,
    ctx: &Arc<ToolContext>,
) -> String {
    let timeout = timeout_secs();
    if timeout == 0 {
        return execute_tool_inner(name, arguments, ctx).await;
    }

    let outer = Duration::from_secs(timeout.saturating_add(30));
    match tokio::time::timeout(outer, execute_tool_inner(name, arguments, ctx)).await {
        Ok(result) => result,
        Err(_) => format!(
            "Tool timed out (outer safety net after {}s)",
            outer.as_secs()
        ),
    }
}

/// Each tool's JSON-schema `parameters`, keyed by name, for argument validation.
static TOOL_PARAMETERS: LazyLock<HashMap<String, serde_json::Value>> = LazyLock::new(|| {
    TOOLS_JSON
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some((f.get("name")?.as_str()?.to_string(), f.get("parameters")?.clone()))
        })
        .collect()
});

/// Schema-check ask_user_question, which the harness handles instead of
/// execute_tool, so a malformed call reaches the model as an error rather than
/// reaching the user as an empty or broken prompt.
pub fn validate_question_arguments(arguments: &serde_json::Value) -> Result<serde_json::Value, String> {
    let args = crate::tool_args::normalize_arguments(
        "ask_user_question",
        arguments,
        TOOL_PARAMETERS.get("ask_user_question"),
    )?;
    if args["questions"].as_array().is_none_or(|q| q.is_empty()) {
        return Err("Error: invalid arguments for ask_user_question; nothing was run.\n\
                    - questions: must contain at least one question"
            .to_string());
    }
    Ok(args)
}

async fn execute_tool_inner(
    name: &str,
    arguments: &serde_json::Value,
    ctx: &Arc<ToolContext>,
) -> String {
    if name == "ask_user_question" {
        return "ask_user_question must be handled by the harness — it should never reach execute_tool directly.".to_string();
    }
    let normalized =
        match crate::tool_args::normalize_arguments(name, arguments, TOOL_PARAMETERS.get(name)) {
            Ok(v) => v,
            Err(e) => return e,
        };
    let arguments = &normalized;
    match name {
        "read_file" => {
            read_file(
                a(arguments, "path", ""),
                aopt_usize(arguments, "offset"),
                aopt_usize(arguments, "limit"),
            )
            .await
        }
        "read_image" => read_image(a(arguments, "path", ""), ctx).await,
        "write_file" => write_file(a(arguments, "path", ""), a(arguments, "content", "")).await,
        "replace_in_file" => {
            replace_in_file(
                a(arguments, "path", ""),
                a(arguments, "old_str", ""),
                a(arguments, "new_str", ""),
                abool(arguments, "replace_all", false),
            ).await
        }
        "apply_changes" => apply_changes(arguments).await,
        "run_powershell" => {
            run_powershell(a(arguments, "command", ""), aopt(arguments, "cwd"), ctx.clone()).await
        }
        "run_bash" => {
            let host = aopt(arguments, "host").filter(|h| !h.is_empty());
            if let Some(e) = windows_local_run_bash_error(cfg!(windows), host.as_deref()) {
                return e;
            }
            run_bash(
                a(arguments, "command", ""),
                aopt(arguments, "cwd"),
                abool(arguments, "elevated", false),
                host,
                ctx.clone(),
            )
            .await
        }
        "web_search" => {
            web_search(a(arguments, "query", ""), aus(arguments, "max_results", 5)).await
        }
        "download_file" => {
            download_file(
                a(arguments, "url", ""),
                aopt(arguments, "filename"),
                aopt(arguments, "dir"),
                aopt_i64(arguments, "max_size_mb"),
            )
            .await
        }
        "fetch_url" => fetch_url(a(arguments, "url", ""), aopt_usize(arguments, "max_chars")).await,
        "run_python" => run_python(a(arguments, "code", ""), aopt(arguments, "cwd"), ctx.clone()).await,
        "directory_tree" => {
            directory_tree(
                a(arguments, "path", ""),
                aus(arguments, "max_depth", 3),
                abool(arguments, "show_hidden", false),
            )
            .await
        }
        "read_multiple_files" => {
            let paths: Vec<String> = arguments
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            read_multiple_files(paths).await
        }
        "search_content" => {
            search_content(
                a(arguments, "pattern", ""),
                a(arguments, "path", ""),
                aopt(arguments, "file_glob"),
                aus(arguments, "context_lines", 0),
                aus(arguments, "max_results", 50),
                abool(arguments, "regex", false),
                abool(arguments, "ignore_case", false),
            )
            .await
        }
        "glob" => glob_tool(a(arguments, "pattern", ""), aopt(arguments, "path")).await,
        "todowrite" => {
            let todos: Vec<serde_json::Value> = arguments
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|arr| arr.clone())
                .unwrap_or_default();
            todowrite(todos).await
        }
        _ => format!("Unknown tool: {name}"),
    }
}

async fn apply_changes(args: &serde_json::Value) -> String {
    const MAX_FILES: usize = 20;
    const MAX_OPS: usize = 100;
    const MAX_BLOCK: usize = 256_000;
    const MAX_RESULT: usize = 1_000_000;
    let changes = match args.get("changes").and_then(|v| v.as_array()) {
        Some(v) if !v.is_empty() => v,
        _ => return "Error: changes must be a non-empty list.".into(),
    };
    if changes.len() > MAX_FILES {
        return format!(
            "Error: too many files ({}). Maximum is {MAX_FILES}.",
            changes.len()
        );
    }
    let mut prepared: HashMap<PathBuf, (String, String)> = HashMap::new();
    // Files in the order given, so the diff and the commit order are stable.
    let mut order: Vec<PathBuf> = Vec::new();
    let mut errors = Vec::new();
    let mut ops = 0usize;
    let mut result_bytes = 0usize;
    for (fi, change) in changes.iter().enumerate() {
        let obj = match change.as_object() {
            Some(x) => x,
            None => {
                errors.push(format!("file {fi}: must be an object"));
                continue;
            }
        };
        let raw = obj.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let path = expand_home(raw)
            .canonicalize()
            .unwrap_or_else(|_| expand_home(raw));
        if prepared.contains_key(&path) {
            errors.push(format!("{raw}: duplicate path"));
            continue;
        }
        if !path.exists() {
            errors.push(format!("{raw}: file not found"));
            continue;
        }
        if !path.is_file() {
            errors.push(format!("{raw}: not a file"));
            continue;
        }
        let original = match std::fs::read_to_string(&path) {
            Ok(x) => x,
            Err(_) => {
                errors.push(format!("{raw}: binary or non-UTF-8 file"));
                continue;
            }
        };
        let mut current = original.clone();
        let arr = match obj.get("operations").and_then(|v| v.as_array()) {
            Some(x) if !x.is_empty() => x,
            _ => {
                errors.push(format!("{raw}: operations must be non-empty"));
                continue;
            }
        };
        ops += arr.len();
        if ops > MAX_OPS {
            errors.push(format!("too many operations; maximum is {MAX_OPS}"));
            break;
        }
        for (oi, op) in arr.iter().enumerate() {
            let o = match op.as_object() {
                Some(x) => x,
                None => {
                    errors.push(format!("{raw} operation {oi}: must be an object"));
                    continue;
                }
            };
            let kind = o.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let expected = o
                .get("expected_matches")
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as usize;
            if expected == 0 {
                errors.push(format!(
                    "{raw} operation {oi}: expected_matches must be positive"
                ));
                continue;
            }
            let (needle, replacement) = match kind {
                "replace" => (
                    o.get("old").and_then(|v| v.as_str()).unwrap_or(""),
                    o.get("new").and_then(|v| v.as_str()).unwrap_or(""),
                ),
                "delete" => (o.get("old").and_then(|v| v.as_str()).unwrap_or(""), ""),
                "insert_after" => (
                    o.get("anchor").and_then(|v| v.as_str()).unwrap_or(""),
                    o.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                ),
                _ => {
                    errors.push(format!("{raw} operation {oi}: unknown kind {kind:?}"));
                    continue;
                }
            };
            if needle.is_empty() {
                errors.push(format!(
                    "{raw} operation {oi}: match text must be non-empty"
                ));
                continue;
            }
            if needle.len() > MAX_BLOCK || replacement.len() > MAX_BLOCK {
                errors.push(format!(
                    "{raw} operation {oi}: text block exceeds {MAX_BLOCK} bytes"
                ));
                continue;
            }
            let (needle, replacement) = match_newlines(&current, needle, replacement);
            let count = current.matches(needle.as_str()).count();
            if count != expected {
                errors.push(format!(
                    "{raw} operation {oi}: matches {count} locations; expected {expected}"
                ));
                continue;
            }
            let repl = if kind == "insert_after" {
                format!("{needle}{replacement}")
            } else {
                replacement
            };
            current = current.replacen(needle.as_str(), &repl, expected);
        }
        result_bytes += original.len() + current.len();
        order.push(path.clone());
        prepared.insert(path, (original, current));
    }
    if result_bytes > MAX_RESULT {
        errors.push(format!("result exceeds {MAX_RESULT} bytes"));
    }
    if let Some(conditions) = args.get("postconditions").and_then(|v| v.as_array()) {
        for (i, c) in conditions.iter().enumerate() {
            let o = match c.as_object() {
                Some(x) => x,
                None => {
                    errors.push(format!("postcondition {i}: must be object"));
                    continue;
                }
            };
            let raw = o.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let path = expand_home(raw)
                .canonicalize()
                .unwrap_or_else(|_| expand_home(raw));
            let content = prepared
                .get(&path)
                .map(|(_, x)| x.clone())
                .or_else(|| std::fs::read_to_string(&path).ok())
                .unwrap_or_default();
            if let Some(x) = o.get("contains").and_then(|v| v.as_str()) {
                if !content.contains(x) {
                    errors.push(format!(
                        "postcondition {i}: {raw} does not contain expected text"
                    ));
                }
            }
            if let Some(x) = o.get("does_not_contain").and_then(|v| v.as_str()) {
                if let Some(at) = forbidden_text_location(&content, x) {
                    errors.push(format!(
                        "postcondition {i}: {raw} still contains forbidden text {at}"
                    ));
                }
            }
        }
    }
    if !errors.is_empty() {
        return format!(
            "Error: no changes applied.\n{}",
            errors
                .iter()
                .map(|x| format!("- {x}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let dry = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut diff = String::new();
    for path in &order {
        let (old, new) = &prepared[path];
        if old != new {
            diff.push_str(&unified_diff(&path.display().to_string(), old, new));
        }
    }
    if dry {
        return format!(
            "Dry run: no changes applied.\nFiles: {}\n\n{}",
            prepared.len(),
            diff
        )
        .trim_end()
        .into();
    }
    // Stage every file before replacing any, so a write failure (disk full,
    // permissions) leaves all targets untouched; only the renames remain after.
    let mut staged: Vec<StagedWrite> = order
        .iter()
        .filter(|p| prepared[*p].0 != prepared[*p].1)
        .map(|p| StagedWrite::new(p, prepared[p].1.as_bytes().to_vec()))
        .collect();
    for i in 0..staged.len() {
        if let Err(e) = staged[i].stage() {
            for s in staged.iter_mut() {
                s.discard();
            }
            return format!("Error: write failed; no changes applied: {e}");
        }
    }
    for i in 0..staged.len() {
        if let Err(e) = staged[i].commit() {
            return format!(
                "Error: rename failed after validation; changes may be partially applied: {e}"
            );
        }
    }
    format!("Applied changes to {} file(s).\n\n{}", prepared.len(), diff)
        .trim_end()
        .into()
}

fn terminate_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(format!("-{pid}"))
            .output();
    }
    // taskkill /T walks the parent-PID tree (Windows has no process group to
    // signal).  Invoked by absolute path so a taskkill.exe earlier on PATH
    // can't stand in for it.
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new(system_root().join("System32").join("taskkill.exe"));
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_console(&mut cmd);
        let _ = cmd.status();
    }
}

/// Start a tool subprocess without a console window on Windows.
///
/// The GUI is a windowed app, so every powershell/ssh/python/taskkill child
/// would otherwise flash a console.  CREATE_NO_WINDOW still gives the child a
/// (hidden) console, so console APIs such as [Console]::OutputEncoding work.
pub(crate) fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

// ── Argument helpers ────────────────────────────────────────────────

/// Truncate `s` to at most `max_bytes`, backing up to the nearest char boundary.
fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// First `max_bytes` of `s`, backed up to the last line break.
///
/// Cutting on a raw index leaves a broken half-line at the seam, which on
/// source code is a fragment the model may try to reason about or "fix".
/// Falls back to the char-boundary cut when one line is longer than the budget.
fn cut_at_line_end(s: &str, max_bytes: usize) -> &str {
    let head = truncate_on_char_boundary(s, max_bytes);
    if head.len() == s.len() {
        return head;
    }
    match head.rfind('\n') {
        Some(pos) if pos > 0 => &head[..pos],
        _ => head,
    }
}

/// Last `max_bytes` of `s`, advanced to the start of the next whole line.
fn cut_at_line_start(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    match s[start..].find('\n') {
        Some(pos) => &s[start + pos + 1..],
        None => &s[start..],
    }
}

/// Head truncation for *file* content, cut on a line boundary.
///
/// Returns `(text, lines_kept, truncated)`.  Files truncate from the head
/// rather than being snipped in the middle: the head is where imports and
/// declarations live, and the caller can report which lines survived so the
/// model can page through the rest with offset/limit.
fn truncate_head_lines(text: &str, limit: Option<usize>) -> (&str, usize, bool) {
    let budget = limit.unwrap_or_else(|| *TOOL_OUTPUT_MAX_CHARS.lock().unwrap());
    if budget == 0 || text.len() <= budget {
        return (text, text.matches('\n').count() + 1, false);
    }
    let kept = cut_at_line_end(text, budget);
    (kept, kept.matches('\n').count() + 1, true)
}

const BINARY_SAMPLE_CHARS: usize = 4096;
const BINARY_NONPRINTABLE_RATIO: f64 = 0.25;

fn is_nonprintable(c: char) -> bool {
    !matches!(c, '\n' | '\r' | '\t') && c.is_control()
}

/// Heuristically detect binary blobs that decoded as text without erroring.
///
/// A hard-invalid byte sequence is now handled upstream by decoding with
/// `String::from_utf8_lossy` (U+FFFD replacement) rather than the strict
/// `fs::read_to_string` this used to go through -- but that still leaves the
/// case that never raised in the first place: bytes that happen to form
/// valid text (a UTF-16 file decoded as UTF-8 leaves a NUL between every
/// ASCII byte; a core dump or compiled binary can have long printable-looking
/// runs) but aren't meaningful for the model to read and can blow out the
/// context window.
///
/// Returns `(is_binary, nonprintable_ratio)` computed over a leading sample.
fn looks_binary(text: &str) -> (bool, f64) {
    if text.is_empty() {
        return (false, 0.0);
    }
    let sample: Vec<char> = text.chars().take(BINARY_SAMPLE_CHARS).collect();
    let nonprintable = sample.iter().filter(|c| is_nonprintable(**c)).count();
    let ratio = nonprintable as f64 / sample.len() as f64;
    let is_binary = sample.contains(&'\0') || ratio > BINARY_NONPRINTABLE_RATIO;
    (is_binary, ratio)
}

/// Tail-biased truncation for *command* output (run_bash, run_python).
///
/// Keeps the head (~20%) and tail (~80%) and snips the middle: a command echo
/// sits at the start and the error that matters usually sits at the end, so the
/// middle of a build log is the disposable part.  File reads use
/// `truncate_head_lines` instead — a gap in the middle of a source file is not
/// disposable, and unlike a log it can be paged around.  0 = no limit.
///
/// Runs the binary guard first: output that looks like a binary blob rather
/// than text is blocked outright rather than truncated, since truncating a
/// binary dump still floods the context with useless bytes.
fn snip_tool_output(text: String) -> String {
    let (is_binary, ratio) = looks_binary(&text);
    if is_binary {
        return format!(
            "[Binary output blocked: {} chars, ~{:.0}% non-printable/control characters. \
             Refusing to load this into context. If you need the data, redirect it to a file \
             and use read_file/search_content on it, or use download_file for URLs.]",
            text.chars().count(),
            ratio * 100.0
        );
    }

    let limit = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
    if limit == 0 || text.len() <= limit {
        return text;
    }
    let head_chars = (limit / 5).max(500);
    let tail_chars = limit - head_chars;

    // Cut on line boundaries so neither seam leaves a broken half-line.
    let head = cut_at_line_end(&text, head_chars);
    let tail = cut_at_line_start(&text, tail_chars);

    let snipped = text.len() - head.len() - tail.len();
    format!(
        "{head}\n\n[... snipped {snipped} chars from middle — set tool_output_max_chars \
         to change this limit (current: {limit}) ...]\n\n{tail}"
    )
}

fn a(args: &serde_json::Value, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| default.to_string())
}

fn aopt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// Optional integer argument — `None` when the model omitted the key, which
/// `read_file` needs in order to tell "whole file" from an explicit range.
fn aopt_usize(args: &serde_json::Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

/// Optional signed integer argument — `None` when the model omitted the key.
/// download_file's max_size_mb needs this to distinguish 0/negative (no limit).
fn aopt_i64(args: &serde_json::Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

fn aus(args: &serde_json::Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(default)
}

fn abool(args: &serde_json::Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn expand_home(path: &str) -> PathBuf {
    if path.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            if path == "~" {
                return home;
            }
            if path.starts_with("~/") {
                return home.join(&path[2..]);
            }
        }
    }
    PathBuf::from(path)
}

fn ua() -> String {
    USER_AGENT.lock().unwrap().clone()
}

fn timeout_secs() -> u64 {
    *TOOL_TIMEOUT.lock().unwrap()
}

// ── Tool implementations ────────────────────────────────────────────

/// Read file contents, optionally just the line range `offset..offset+limit`.
///
/// A ranged read is reported as `[Lines A-B of N]` so the model knows where it
/// is in the file and can ask for the next page; a whole-file read that had to
/// be snipped says how many lines exist so paging is discoverable.
async fn read_file(path: String, offset: Option<usize>, limit: Option<usize>) -> String {
    let p = expand_home(&path);
    let text = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => {
            return if !p.exists() {
                format!("Error: File not found: {path}")
            } else if !p.is_file() {
                format!("Error: Not a file: {path}")
            } else {
                format!("Error reading file: {e}")
            };
        }
    };

    // Hard-invalid UTF-8 already failed read_to_string above; this catches the
    // other half — bytes that decode as valid UTF-8 but aren't text (a UTF-16
    // file leaves a NUL between every ASCII byte).
    if looks_binary(&text).0 {
        return format!("Error: File appears to be binary (not text): {path}");
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    let start = offset.unwrap_or(1).max(1);
    if start > total {
        return format!(
            "Error: offset {start} is past the end of {path}, which has {total} lines."
        );
    }
    let count = limit.map(|l| l.max(1)).unwrap_or(total - start + 1);
    let end = total.min(start + count - 1);

    let body = lines[start - 1..end].join("\n");
    let (body, kept, truncated) = truncate_head_lines(&body, None);
    let shown_end = start + kept - 1;

    // A plain whole-file read that fit stays bare — no header to parse.
    if !truncated && offset.is_none() && limit.is_none() {
        return body.to_string();
    }

    let mut header = format!("[Lines {start}-{shown_end} of {total} in {path}");
    if truncated {
        header.push_str(&format!(
            " — output limit reached, pass offset={} to continue",
            shown_end + 1
        ));
    }
    format!("{header}]\n{body}")
}

/// Extensions `read_image` will attempt, so a text file gets a useful error
/// instead of a decoder trace.
const IMAGE_SUFFIXES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];

/// Queue an image for attachment to the conversation.
///
/// Returns a text summary for the tool result; the picture itself reaches the
/// caller via `ToolContext::take_pending_images` — see `add_pending_image` for
/// why it can't ride in the return value.
async fn read_image(path: String, ctx: &Arc<ToolContext>) -> String {
    let p = expand_home(&path);
    if !p.exists() {
        return format!("Error: File not found: {path}");
    }
    if !p.is_file() {
        return format!("Error: Not a file: {path}");
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !IMAGE_SUFFIXES.contains(&ext.as_str()) {
        return format!(
            "Error: {path} is not a recognized image file. Supported extensions: {}. \
             Use read_file for text.",
            IMAGE_SUFFIXES.join(", ")
        );
    }

    let original_size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    let (dim, mb, quality) = {
        (
            *IMAGE_MAX_DIMENSION.lock().unwrap(),
            *IMAGE_MAX_MB.lock().unwrap(),
            *IMAGE_QUALITY.lock().unwrap(),
        )
    };

    let (width, height) = match image::image_dimensions(&p) {
        Ok(d) => d,
        Err(e) => return format!("Error: {path} could not be decoded as an image: {e}"),
    };

    match crate::image_utils::preprocess(&p, dim, mb, quality) {
        Ok(result) => {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&result.bytes);
            let encoded_len = result.bytes.len() as u64;
            let mime = result.mime.clone();
            ctx.add_pending_image(p.to_string_lossy().to_string(), result.mime, b64);

            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            let mut summary = format!(
                "Loaded {name} — {width}×{height}, {}",
                format_size(original_size)
            );
            if encoded_len != original_size {
                summary.push_str(&format!(
                    " → {mime}, {} after preprocessing",
                    format_size(encoded_len)
                ));
            }
            summary.push_str(". The image is attached below; look at it directly.");
            summary
        }
        Err(e) => format!("Error reading image: {e}"),
    }
}

/// Follow a chain of symlinks at `path` (not its ancestors) to the file a
/// write should actually replace, so the link itself survives the rename.
fn resolve_write_target(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::read_link(&p) {
                Ok(link) if link.is_absolute() => p = link,
                Ok(link) => p = p.parent().unwrap_or(Path::new(".")).join(link),
                Err(_) => break,
            },
            _ => break,
        }
    }
    p
}

static STAGE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A file write staged to a temp file beside its target, then renamed over it.
///
/// Every edit tool writes through this so a failed write (disk full, quota,
/// signal) can never truncate or delete the file being edited: the target is
/// only ever replaced by an atomic rename of a complete, synced temp file.
/// The rename keeps the target's permission bits (and owner, where allowed)
/// and follows symlinks so the link itself survives and its target is edited.
/// If the directory doesn't allow creating a temp file but the target itself
/// is writable, it falls back to a checked in-place write.  Keep identical to
/// `_StagedWrite` (Python) and `StagedWrite` (C++).
pub(crate) struct StagedWrite {
    target: PathBuf,
    data: Vec<u8>,
    temp: Option<PathBuf>,
    in_place: bool,
}

impl StagedWrite {
    pub(crate) fn new(path: &Path, data: Vec<u8>) -> Self {
        StagedWrite { target: resolve_write_target(path), data, temp: None, in_place: false }
    }

    pub(crate) fn stage(&mut self) -> std::io::Result<()> {
        if self.target.is_dir() {
            return Err(std::io::Error::other(format!("{} is a directory", self.target.display())));
        }
        if let Some(parent) = self.target.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let meta = std::fs::metadata(&self.target).ok();
        // A rename only needs a writable directory; still refuse a target the
        // user made read-only, as a plain open() for writing would.
        if meta.is_some() {
            if let Err(e) = std::fs::OpenOptions::new().write(true).open(&self.target) {
                return Err(e);
            }
        }
        let name = self.target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let temp = self.target.with_file_name(format!(
            ".{name}.pengy-tmp-{}-{}",
            std::process::id(),
            STAGE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o666);
        }
        let mut file = match opts.open(&temp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && meta.is_some() => {
                // Target is writable (checked above) but its directory isn't.
                self.in_place = true;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        self.temp = Some(temp.clone());
        let written = file.write_all(&self.data).and_then(|_| file.sync_all());
        drop(file);
        if let Err(e) = written {
            self.discard();
            return Err(e);
        }
        if let Some(m) = meta {
            if let Err(e) = std::fs::set_permissions(&temp, m.permissions()) {
                self.discard();
                return Err(e);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let _ = std::os::unix::fs::chown(&temp, Some(m.uid()), Some(m.gid()));
            }
        }
        Ok(())
    }

    pub(crate) fn commit(&mut self) -> std::io::Result<()> {
        if self.in_place {
            let mut f = std::fs::OpenOptions::new().write(true).open(&self.target)?;
            f.write_all(&self.data)?;
            f.set_len(self.data.len() as u64)?;
            return f.sync_all();
        }
        if let Some(temp) = &self.temp {
            std::fs::rename(temp, &self.target)?;
            self.temp = None;
        }
        Ok(())
    }

    pub(crate) fn discard(&mut self) {
        if let Some(temp) = self.temp.take() {
            let _ = std::fs::remove_file(temp);
        }
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        self.discard();
    }
}

/// Stage and commit a single text write (see `StagedWrite`).
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let mut staged = StagedWrite::new(path, text.as_bytes().to_vec());
    staged.stage()?;
    staged.commit()
}

/// Adapt LF-typed `old`/`new` to a CRLF file.
///
/// Models type edits with LF even for CRLF files, so an exact-text edit typed
/// with `\n` would never match.  When `old` has no match as typed, has no CR of
/// its own, and the file uses CRLF, retry with CRLF line endings and write
/// `new` the same way, so the file keeps its line-ending style.  Mirrors
/// `_match_newlines` (Python) and `matchNewlines` (C++).
fn match_newlines(content: &str, old: &str, new: &str) -> (String, String) {
    if content.contains(old) || !old.contains('\n') || old.contains('\r') || !content.contains("\r\n") {
        return (old.to_string(), new.to_string());
    }
    let crlf_old = old.replace('\n', "\r\n");
    if !content.contains(&crlf_old) {
        return (old.to_string(), new.to_string());
    }
    let new = if new.contains('\r') { new.to_string() } else { new.replace('\n', "\r\n") };
    (crlf_old, new)
}

/// Largest LCS table (lines x lines) `match_lines` builds for one region.
const LCS_MAX_CELLS: usize = 4_000_000;

/// Matching `(i, j)` line pairs between `a` and `b`, in order.  A region small
/// enough for the LCS table gets an exact LCS.  A larger one is split on lines
/// that occur exactly once on each side (kept in order by a longest increasing
/// subsequence, as patience diff does) and its gaps are matched the same way,
/// so two edits far apart in a big file still diff as two small hunks instead
/// of one replace spanning everything between them.  Only a large region with
/// no such line is left unmatched.  Same algorithm as the C++ edition.
fn match_lines(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut regions = vec![(0, a.len(), 0, b.len())];
    while let Some((mut a1, mut a2, mut b1, mut b2)) = regions.pop() {
        while a1 < a2 && b1 < b2 && a[a1] == b[b1] {
            pairs.push((a1, b1));
            a1 += 1;
            b1 += 1;
        }
        while a1 < a2 && b1 < b2 && a[a2 - 1] == b[b2 - 1] {
            a2 -= 1;
            b2 -= 1;
            pairs.push((a2, b2));
        }
        let (n, m) = (a2 - a1, b2 - b1);
        if n == 0 || m == 0 {
            continue;
        }
        if n * m <= LCS_MAX_CELLS {
            lcs_pairs(&a[a1..a2], &b[b1..b2], a1, b1, &mut pairs);
            continue;
        }
        let (mut pa, mut pb) = (a1, b1);
        for (i, j) in unique_anchors(a, b, a1, a2, b1, b2) {
            pairs.push((i, j));
            regions.push((pa, i, pb, j));
            pa = i + 1;
            pb = j + 1;
        }
        if pa > a1 {
            regions.push((pa, a2, pb, b2));
        }
    }
    pairs.sort_unstable();
    pairs
}

/// Exact LCS of `am` and `bm`, pushed as pairs offset by `ao`/`bo`.
fn lcs_pairs(am: &[&str], bm: &[&str], ao: usize, bo: usize, pairs: &mut Vec<(usize, usize)>) {
    let (n, m) = (am.len(), bm.len());
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * (m + 1) + j] = if am[i] == bm[j] {
                dp[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if am[i] == bm[j] {
            pairs.push((ao + i, bo + j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
}

/// Lines occurring exactly once in `a[a1..a2]` and once in `b[b1..b2]`, as
/// `(i, j)` pairs: the longest run of them in the same order on both sides.
fn unique_anchors(a: &[&str], b: &[&str], a1: usize, a2: usize, b1: usize, b2: usize) -> Vec<(usize, usize)> {
    // line -> (count in a, index in a, count in b, index in b)
    let mut seen: HashMap<&str, (u32, usize, u32, usize)> = HashMap::new();
    for (i, line) in a.iter().enumerate().take(a2).skip(a1) {
        let e = seen.entry(*line).or_insert((0, i, 0, 0));
        e.0 += 1;
    }
    for (j, line) in b.iter().enumerate().take(b2).skip(b1) {
        if let Some(e) = seen.get_mut(*line) {
            e.2 += 1;
            e.3 = j;
        }
    }
    let mut cands: Vec<(usize, usize)> = seen
        .values()
        .filter(|e| e.0 == 1 && e.2 == 1)
        .map(|e| (e.1, e.3))
        .collect();
    cands.sort_unstable();
    // Longest increasing subsequence of the b indices (patience sorting).
    let mut tails: Vec<usize> = Vec::new(); // index into cands
    let mut prev: Vec<Option<usize>> = vec![None; cands.len()];
    for k in 0..cands.len() {
        let pos = tails.partition_point(|&t| cands[t].1 < cands[k].1);
        prev[k] = if pos > 0 { Some(tails[pos - 1]) } else { None };
        if pos == tails.len() {
            tails.push(k);
        } else {
            tails[pos] = k;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut k = tails.last().copied();
    while let Some(x) = k {
        out.push(cands[x]);
        k = prev[x];
    }
    out.reverse();
    out
}

/// Line-level opcodes (`tag`, a-range, b-range) turning `a` into `b`: common
/// prefix/suffix trimmed, then the middle matched by `match_lines`.  Same
/// algorithm as the C++ edition.
fn diff_opcodes(a: &[&str], b: &[&str]) -> Vec<(char, usize, usize, usize, usize)> {
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    // Matching (i, j) pairs in the middle, in order.
    let pairs = match_lines(am, bm);
    let mut ops = Vec::new();
    if pre > 0 {
        ops.push(('=', 0, pre, 0, pre));
    }
    let (mut ai, mut bj) = (0usize, 0usize);
    let mut k = 0;
    while k <= pairs.len() {
        let (ni, nj) = if k < pairs.len() { pairs[k] } else { (am.len(), bm.len()) };
        if ai < ni || bj < nj {
            let tag = if ai < ni && bj < nj { 'r' } else if ai < ni { 'd' } else { 'i' };
            ops.push((tag, pre + ai, pre + ni, pre + bj, pre + nj));
        }
        if k < pairs.len() {
            let mut run = 1;
            while k + run < pairs.len() && pairs[k + run] == (ni + run, nj + run) {
                run += 1;
            }
            ops.push(('=', pre + ni, pre + ni + run, pre + nj, pre + nj + run));
            ai = ni + run;
            bj = nj + run;
            k += run;
        } else {
            k += 1;
        }
    }
    if suf > 0 {
        ops.push(('=', a.len() - suf, a.len(), b.len() - suf, b.len()));
    }
    // Merge adjacent equal runs (prefix/middle/suffix boundaries).
    let mut merged: Vec<(char, usize, usize, usize, usize)> = Vec::new();
    for op in ops {
        if let Some(last) = merged.last_mut() {
            if last.0 == '=' && op.0 == '=' && last.2 == op.1 {
                last.2 = op.2;
                last.4 = op.4;
                continue;
            }
        }
        merged.push(op);
    }
    merged
}

/// Where `needle` first occurs in `content`, for a failed `does_not_contain`:
/// "at line N: <that line>", plus "(+K more)" for further matches.  The check
/// is a plain substring test, so the match is often in a comment or string the
/// caller didn't mean; showing the line makes that a one-look fix instead of a
/// blind retry.  A line longer than 160 characters is cut to a window around
/// the match.  Same text as the Python and C++ editions.
fn forbidden_text_location(content: &str, needle: &str) -> Option<String> {
    let pos = content.find(needle)?;
    let more = if needle.is_empty() { 0 } else { content[pos + needle.len()..].matches(needle).count() };
    let line_no = content[..pos].matches('\n').count() + 1;
    let start = content[..pos].rfind('\n').map_or(0, |i| i + 1);
    let end = content[pos..].find('\n').map_or(content.len(), |i| pos + i);
    let raw = &content[start..end];
    let line = raw.trim();
    let lead = raw.len() - raw.trim_start().len();
    let col_bytes = (pos - start).saturating_sub(lead).min(line.len());
    let chars: Vec<char> = line.chars().collect();
    let excerpt = if chars.len() > 160 {
        let col = line[..col_bytes].chars().count();
        let from = col.saturating_sub(60).min(chars.len() - 160);
        let to = (from + 160).min(chars.len());
        format!(
            "{}{}{}",
            if from > 0 { "..." } else { "" },
            chars[from..to].iter().collect::<String>(),
            if to < chars.len() { "..." } else { "" }
        )
    } else {
        line.to_string()
    };
    let mut at = format!("at line {line_no}: {excerpt}");
    if more > 0 {
        at.push_str(&format!(" (+{more} more)"));
    }
    Some(at)
}

fn unified_range(start: usize, stop: usize) -> String {
    let len = stop - start;
    match len {
        1 => format!("{}", start + 1),
        0 => format!("{start},0"),
        _ => format!("{},{len}", start + 1),
    }
}

/// A unified diff (3 lines of context) of `old` → `new`, in difflib's format.
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    const CTX: usize = 3;
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let mut codes = diff_opcodes(&a, &b);
    if codes.is_empty() {
        return String::new();
    }
    if codes[0].0 == '=' {
        let c = codes[0];
        codes[0] = ('=', c.1.max(c.2.saturating_sub(CTX)), c.2, c.3.max(c.4.saturating_sub(CTX)), c.4);
    }
    let last = codes.len() - 1;
    if codes[last].0 == '=' {
        let c = codes[last];
        codes[last] = ('=', c.1, c.2.min(c.1 + CTX), c.3, c.4.min(c.3 + CTX));
    }
    let mut groups: Vec<Vec<(char, usize, usize, usize, usize)>> = Vec::new();
    let mut group = Vec::new();
    for c in codes {
        let (tag, mut i1, i2, mut j1, j2) = c;
        if tag == '=' && i2 - i1 > 2 * CTX {
            group.push(('=', i1, i2.min(i1 + CTX), j1, j2.min(j1 + CTX)));
            groups.push(std::mem::take(&mut group));
            i1 = i1.max(i2.saturating_sub(CTX));
            j1 = j1.max(j2.saturating_sub(CTX));
        }
        group.push((tag, i1, i2, j1, j2));
    }
    if !group.is_empty() && !(group.len() == 1 && group[0].0 == '=') {
        groups.push(group);
    }
    if groups.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {path}\n+++ {path}\n");
    let line = |prefix: char, text: &str, out: &mut String| {
        out.push(prefix);
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    };
    for g in groups {
        let (first, lastc) = (g[0], g[g.len() - 1]);
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            unified_range(first.1, lastc.2),
            unified_range(first.3, lastc.4)
        ));
        for (tag, i1, i2, j1, j2) in g {
            if tag == '=' {
                for t in &a[i1..i2] {
                    line(' ', t, &mut out);
                }
                continue;
            }
            for t in &a[i1..i2] {
                line('-', t, &mut out);
            }
            for t in &b[j1..j2] {
                line('+', t, &mut out);
            }
        }
    }
    out
}

async fn write_file(path: String, content: String) -> String {
    match write_atomic(&expand_home(&path), &content) {
        Ok(_) => format!("Successfully wrote to {path}"),
        Err(e) => format!("Error writing file: {e}"),
    }
}

async fn replace_in_file(path: String, old_str: String, new_str: String, replace_all: bool) -> String {
    let p = expand_home(&path);
    if old_str.is_empty() {
        return "Error: old_str is empty. You must provide the exact text to replace.".into();
    }
    if !p.exists() {
        return format!("Error: File not found: {path}");
    }
    if !p.is_file() {
        return format!("Error: Not a file: {path}");
    }
    let content = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(_) => return format!("Error: {path} is a binary file (not UTF-8)."),
    };
    let (old_str, new_str) = match_newlines(&content, &old_str, &new_str);
    let count = content.matches(&old_str).count();
    if count == 0 {
        return format!(
            "Error: old_str not found in {path}.\n\n\
             Tip: read the file first to get the exact text."
        );
    }
    if replace_all {
        if let Err(e) = write_atomic(&p, &content.replace(&old_str, &new_str)) {
            return format!("Error writing file: {e}");
        }
        return format!("✅ Replaced {count} occurrence(s) in {path}.");
    }
    if count > 1 {
        let mut found_lines = Vec::new();
        let mut pos = 0;
        for _ in 0..count {
            if let Some(idx) = content[pos..].find(&old_str) {
                let line_num = content[..pos + idx].chars().filter(|&c| c == '\n').count() + 1;
                found_lines.push(line_num);
                pos += idx + 1;
            }
        }
        return format!(
            "Error: old_str matches {count} locations in {path}.\n\n\
             Matches found on lines: {found_lines:?}\n\n\
             Make old_str longer or more specific, or set replace_all=true to replace every occurrence."
        );
    }
    let new_content = content.replacen(&old_str, &new_str, 1);
    if let Err(e) = write_atomic(&p, &new_content) {
        return format!("Error writing file: {e}");
    }
    let old_line = content[..content.find(&old_str).unwrap()]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1;
    let old_lines = old_str.chars().filter(|&c| c == '\n').count() + 1;
    let new_lines = new_str.chars().filter(|&c| c == '\n').count() + 1;
    format!(
        "✅ Successfully replaced in {path}:\n   Lines {old_line}–{} → \
         {old_lines} line(s) replaced with {new_lines} line(s)",
        old_line + old_lines - 1
    )
}

static SUDO_DASH_S_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+-S\b").unwrap());
static SUDO_DASH_A_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+-A\b").unwrap());
static SUDO_PROMPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[sudo[^]]*\].*\n?").unwrap());

/// Return spans for unquoted `sudo` command words; data mentions stay inert.
fn sudo_invocation_spans(command: &str) -> Vec<(usize, usize)> {
    let b = command.as_bytes();
    let mut i = 0;
    let mut at_command_start = true;
    let mut out = Vec::new();
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            if b[i] == b'\n' {
                at_command_start = true;
            }
            i += 1;
            continue;
        }
        if b[i] == b'#'
            && (i == 0 || b[i - 1].is_ascii_whitespace() || b";|&()\n".contains(&b[i - 1]))
        {
            i = command[i..]
                .find('\n')
                .map(|n| i + n + 1)
                .unwrap_or(b.len());
            at_command_start = true;
            continue;
        }
        if b";|&()".contains(&b[i]) {
            at_command_start = true;
            i += 1;
            continue;
        }
        let start = i;
        let mut quoted = false;
        while i < b.len() && !b[i].is_ascii_whitespace() && !b";|&()".contains(&b[i]) {
            if b[i] == b'\\' {
                i = (i + 2).min(b.len());
            } else if b[i] == b'\'' || b[i] == b'\"' {
                quoted = true;
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        let word = &command[start..i];
        if at_command_start && !quoted && matches!(word, "sudo" | "/usr/bin/sudo" | "/bin/sudo") {
            out.push((start, i));
        }
        at_command_start = false;
    }
    out
}

/// Rewrite only parsed sudo commands to use askpass.
fn rewrite_sudo_for_askpass(command: &str) -> String {
    let mut out = String::with_capacity(command.len() + 16);
    let mut last = 0;
    for (start, end) in sudo_invocation_spans(command) {
        out.push_str(&command[last..start]);
        out.push_str(&command[start..end]);
        let rest = &command[end..];
        if let Some(m) = SUDO_DASH_S_RE.find(rest) {
            out.push_str(" -A");
            last = end + m.end();
        } else if SUDO_DASH_A_RE.is_match(rest) {
            last = end;
        } else {
            out.push_str(" -A");
            last = end;
        }
    }
    out.push_str(&command[last..]);
    out
}

/// Temporary `SUDO_ASKPASS` helper script.
///
/// The password is written to a private 0600 file that only the askpass script
/// reads. It is deliberately *not* placed in the child process's environment:
/// a `PENGY_SUDO_PASSWORD` env var would be visible to every child of the
/// command (`printenv`/`env`, a build script, a package post-install) and could
/// leak back into tool output and the chat log. Directory, script and password
/// file are all single-use and removed when the command exits.
struct AskpassHelper {
    dir: PathBuf,
    path: PathBuf,
    pw_path: PathBuf,
}

impl AskpassHelper {
    fn new(password: &str) -> Result<Self, std::io::Error> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("pengy-askpass-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&dir)?;
        let pw_path = dir.join("pw");
        std::fs::write(&pw_path, password)?;
        let path = dir.join("askpass.sh");
        std::fs::write(&path, format!("#!/bin/sh\ncat '{}'\n", pw_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&pw_path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self { dir, path, pw_path })
    }
}

impl Drop for AskpassHelper {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.pw_path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn resolve_cwd(cwd: &Option<String>) -> Result<Option<PathBuf>, String> {
    match cwd {
        None => Ok(None),
        Some(s) => {
            let p = expand_home(s);
            if !p.is_dir() {
                Err(format!("Error: cwd not found or not a directory: {s}"))
            } else {
                Ok(Some(p))
            }
        }
    }
}

// Remote-execution wrapper for run_bash(host=...).  Sent over ssh's stdin to
// `sh -s`, so the only thing on the ssh command line is `sh -s` (independent of
// the remote login shell) and the password never touches an argv.  The whole
// script is one `{ ... }` compound command so the shell reads all of it before
// running any of it; after that, stdin holds only the still-open channel.
//
//   - The askpass helper mirrors AskpassHelper: a 0600 password file in a 0700
//     mktemp dir, read by a 0700 script.  `pw` is never exported and is unset
//     before the command starts, so `env` inside the command can't leak it.
//     The dir must be executable (sudo execs the helper), hence the candidate
//     list — $XDG_RUNTIME_DIR is tmpfs, /tmp is often noexec.
//   - Stop: the Pengy side holds ssh's stdin open for the whole run.  Killing
//     the local ssh closes the channel; the watcher's `cat` sees EOF and
//     SIGTERMs the command's process group (setsid).  `sudo` relays SIGTERM to
//     its root child.  No pty, so there is no SIGHUP to rely on.
//   - `exec 3<&0`: background jobs get /dev/null as stdin when job control is
//     off, so the watcher must read the channel through a saved fd.
//   - `trap ... PIPE`: after the client disconnects, dash writes a "Terminated"
//     notice to the dead channel; without a handler it dies of SIGPIPE before
//     the EXIT trap removes the password dir.  Handler traps (unlike ignored
//     signals) reset to default in the child, so the command's own SIGPIPE
//     semantics are unchanged.
//
// Keep byte-identical with the Python and C++ editions.
const REMOTE_WRAPPER: &str = r#"{
umask 077
use_sudo=__USE_SUDO__
pw=__PASSWORD__
cmd=__COMMAND__
cwd=__CWD__
d=
if [ "$use_sudo" = 1 ]; then
  for base in "${XDG_RUNTIME_DIR:-}" "${HOME:-}/.cache" /tmp; do
    [ -n "$base" ] && [ -d "$base" ] && [ -w "$base" ] || continue
    t=$(mktemp -d "$base/pengy-askpass.XXXXXX" 2>/dev/null) || continue
    printf '#!/bin/sh\ncat "%s/pw"\n' "$t" > "$t/askpass"
    chmod 700 "$t/askpass"
    : > "$t/pw"
    if "$t/askpass" >/dev/null 2>&1; then d=$t; break; fi
    rm -rf "$t"
  done
  if [ -z "$d" ]; then
    unset pw
    echo "pengy: no writable, executable private directory for SUDO_ASKPASS on the remote host" >&2
    exit 125
  fi
  trap 'rm -rf "$d"' EXIT
  printf '%s\n' "$pw" > "$d/pw"
fi
unset pw
trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; trap 'exit 141' PIPE
if [ -n "$cwd" ]; then cd "$cwd" || exit 126; fi
if command -v bash >/dev/null 2>&1; then run=bash; else run=sh; fi
exec 3<&0
if [ -n "$d" ]; then
  SUDO_ASKPASS="$d/askpass" setsid "$run" -c "$cmd" </dev/null 3<&- &
else
  setsid "$run" -c "$cmd" </dev/null 3<&- &
fi
child=$!
( cat >/dev/null; kill -TERM -"$child" 2>/dev/null ) <&3 >/dev/null 2>&1 &
watcher=$!
exec 3<&-
wait "$child"; rc=$?
kill "$watcher" 2>/dev/null
exit "$rc"
}
"#;

/// ssh destinations: hostnames, ~/.ssh/config aliases, user@host, IPv6
/// literals.  No leading '-' (ssh option injection such as -oProxyCommand=...),
/// no whitespace/quotes/shell metacharacters/slashes.
static HOST_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._@:%-]+$").unwrap());

/// Failed-authentication messages from classic sudo and sudo-rs.  On a match
/// the cached password for that host is discarded so the next elevated call
/// prompts again instead of replaying a bad password until the account locks.
static SUDO_AUTH_FAILURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)Sorry, try again\.|incorrect password attempt|Authentication failed, try again\.|incorrect authentication attempt",
    )
    .unwrap()
});

/// stderr shapes that mean ssh itself failed (exit 255 is ambiguous: a remote
/// command can exit 255 too).
static SSH_FAILURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^ssh: |Permission denied \(|Host key verification failed").unwrap()
});

/// Return an error message if *host* is not a safe ssh destination.
fn validate_host(host: &str) -> Option<String> {
    if host.is_empty() || host.starts_with('-') || !HOST_RE.is_match(host) {
        return Some(format!(
            "Error: invalid host {host:?}. Use an ssh destination such as \
             `web1`, `user@web1.example.com`, or an ~/.ssh/config alias."
        ));
    }
    None
}

/// POSIX single-quote *s* for a shell (same rules as Python's `shlex.quote`).
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return s.into();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

static REMOTE_PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"__(USE_SUDO|PASSWORD|COMMAND|CWD)__").unwrap());

/// Fill REMOTE_WRAPPER's placeholders with POSIX single-quoted literals.
///
/// One pass over the template: substituting placeholders one after another
/// would also rewrite placeholder text inside an already-inserted value (a
/// password containing `__COMMAND__`), breaking out of its quoting.
fn build_remote_script(command: &str, password: Option<&str>, cwd: Option<&str>) -> String {
    REMOTE_PLACEHOLDER_RE
        .replace_all(REMOTE_WRAPPER, |caps: &regex::Captures| match &caps[1] {
            "USE_SUDO" => if password.is_some() { "1" } else { "0" }.to_string(),
            "PASSWORD" => shell_quote(password.unwrap_or("")),
            "COMMAND" => shell_quote(command),
            _ => shell_quote(cwd.unwrap_or("")),
        })
        .into_owned()
}

/// ssh arguments for a remote run.  -T: no pty (separate stdout/stderr, no
/// echo, and the stdin-EOF watcher works).  BatchMode: never block on a
/// login/passphrase/host-key prompt.  `--` before the host as a second guard
/// against option injection.
fn remote_ssh_args(host: &str) -> Vec<String> {
    [
        "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15",
        "-o", "ServerAliveInterval=15", "--", host, "sh", "-s",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Locate an executable named *name* on a PATH-style list.
fn find_in_path(name: &str, path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(candidate)
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                candidate.is_file() || candidate.with_extension("exe").is_file()
            }
        })
}

/// Make a write to a dead pipe return EPIPE instead of killing the process.
///
/// Rust binaries (CLI, Web) already ignore SIGPIPE, but the Qt GUI links this
/// crate into a C++ `main`, where writing the rest of the script to an ssh that
/// has already exited would take the whole app down.  Only the default action
/// is replaced — an installed handler is left alone.
fn ignore_default_sigpipe() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            let old = libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            if old != libc::SIG_DFL {
                libc::signal(libc::SIGPIPE, old);
            }
        });
    }
}

/// Forget *host*'s cached password after a failed sudo authentication.
fn sudo_auth_failure_note(ctx: &ToolContext, host: Option<&str>, stderr: &str) -> String {
    if !SUDO_AUTH_FAILURE_RE.is_match(stderr) {
        return String::new();
    }
    ctx.forget_sudo_password(host);
    let place = host.map(|h| format!(" on {h}")).unwrap_or_default();
    format!("\n[sudo authentication failed{place}; the cached password was discarded]")
}

/// Return the cached password for *host*, prompting once if needed.
async fn sudo_password_for(ctx: &Arc<ToolContext>, host: Option<&str>) -> Result<String, String> {
    if let Some(pw) = ctx.cached_sudo_password(host) {
        return Ok(pw);
    }
    let provider = ctx.sudo_provider.lock().unwrap().take();
    let cb = match provider {
        Some(cb) => cb,
        None => return Err("Error: sudo detected but no password provider is configured.".into()),
    };
    // Providers block until the user answers (the web one on a Condvar). Doing
    // that on a Tokio worker strands whatever task the provider just woke —
    // e.g. the SSE stream that must deliver `sudo_request` — in that worker's
    // non-stealable LIFO slot, so the prompt never shows.
    let host_owned = host.map(String::from);
    let pw = match tokio::task::spawn_blocking(move || {
        let result = cb(host_owned.as_deref());
        (cb, result)
    })
    .await
    {
        Ok((cb, result)) => {
            *ctx.sudo_provider.lock().unwrap() = Some(cb);
            result
        }
        Err(_) => return Err("Error: sudo password provider failed.".into()),
    };
    match pw {
        Some(p) => {
            ctx.cached_sudo_passwords
                .lock()
                .unwrap()
                .insert(host.map(String::from), p.clone());
            Ok(p)
        }
        None => Err("Cancelled: sudo password not provided.".into()),
    }
}

/// Run a bash command, locally or on *host* over ssh.
async fn run_bash(
    command: String,
    cwd: Option<String>,
    elevated: bool,
    host: Option<String>,
    ctx: Arc<ToolContext>,
) -> String {
    let local_cwd = match &host {
        Some(h) => {
            if let Some(e) = validate_host(h) {
                return e;
            }
            None // a remote path; the wrapper cds into it
        }
        None => match resolve_cwd(&cwd) {
            Ok(c) => c,
            Err(e) => return e,
        },
    };

    let password_needed = !sudo_invocation_spans(&command).is_empty();
    if password_needed && !elevated {
        return "Elevation required: this command invokes sudo. Retry run_bash with elevated=true to request sudo access.".into();
    }
    if elevated && !password_needed {
        // Fail loudly instead of silently running unprivileged. A caller that
        // asked for elevation must actually contain a `sudo` invocation, so the
        // privilege escalation is explicit and shows up in the sudo/audit log.
        return "Error: elevated=true was set, but the command does not invoke sudo. \
                Add an explicit `sudo ...` to the command (so the elevation is an \
                auditable sudo call), or omit elevated=true if no root is needed."
            .into();
    }

    // Route privileged commands through SUDO_ASKPASS. The `regex` crate has no
    // lookahead support, so the rewrite is a manual scan.  Only parsed command
    // words get -A — never a mention in data, a comment, or a quoted string.
    let mut password = None;
    let command = if password_needed {
        match sudo_password_for(&ctx, host.as_deref()).await {
            Ok(p) => password = Some(p),
            Err(e) => return e,
        }
        rewrite_sudo_for_askpass(&command)
    } else {
        command
    };

    match host {
        Some(h) => {
            let remote_cwd = cwd.filter(|c| !c.is_empty());
            run_bash_remote(command, remote_cwd, password, h, ctx).await
        }
        None => run_bash_local(command, local_cwd, password, ctx).await,
    }
}

async fn run_bash_local(
    command: String,
    run_cwd: Option<PathBuf>,
    password: Option<String>,
    ctx: Arc<ToolContext>,
) -> String {
    let timeout = timeout_secs();
    let used_sudo = password.is_some();
    let askpass = match password {
        Some(ref pw) => match AskpassHelper::new(pw) {
            Ok(helper) => Some(helper),
            Err(e) => return format!("Error preparing sudo askpass helper: {e}"),
        },
        None => None,
    };

    let (stdout_path, stderr_path, stdout_file, stderr_file) = match create_output_files("bash") {
        Ok(files) => files,
        Err(e) => return format!("Error creating output files: {e}"),
    };

    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c").arg(&command);
    cmd.stdout(Stdio::from(stdout_file));
    cmd.stderr(Stdio::from(stderr_file));
    // The command never inherits our stdin: the password goes via askpass, and
    // a child reading the terminal would hang the GUI/CLI.
    cmd.stdin(Stdio::null());
    if let Some(dir) = &run_cwd {
        cmd.current_dir(dir);
    }
    if let Some(ref helper) = askpass {
        cmd.env("SUDO_ASKPASS", &helper.path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            remove_output_files(&stdout_path, &stderr_path);
            return format!("Error running command: {e}");
        }
    };

    let pid = child.id();
    ctx.register_process(pid);

    // Use tokio::task::spawn_blocking to avoid blocking the async runtime.
    // Output is redirected to files instead of piped: a child that writes more
    // than an OS pipe buffer can otherwise block forever before it exits.
    let ctx_blocking = ctx.clone();
    let result = tokio::task::spawn_blocking(move || {
        let wait_result = wait_child(&mut child, pid, timeout);
        ctx_blocking.unregister_process(pid);

        let out = read_and_remove(&stdout_path);
        let err = read_and_remove(&stderr_path);
        wait_result.map(|status| {
            let Some(status) = status else {
                return timed_out_output(format!("Error: Command timed out after {timeout} seconds"), out, &err);
            };
            let mut out = join_command_output(out, &err, status);
            if used_sudo {
                out.push_str(&sudo_auth_failure_note(&ctx_blocking, None, &err));
            }
            finish_command_output(out)
        })
    })
    .await;
    drop(askpass);

    match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => e,
        Err(join_err) => format!("Error: Task panicked: {join_err}"),
    }
}

/// Run *command* on *host* through REMOTE_WRAPPER over ssh.
async fn run_bash_remote(
    command: String,
    cwd: Option<String>,
    password: Option<String>,
    host: String,
    ctx: Arc<ToolContext>,
) -> String {
    let timeout = timeout_secs();
    let ssh = match find_in_path("ssh", std::env::var_os("PATH")) {
        Some(p) => p,
        None => {
            return "Error: run_bash host= requires the `ssh` client, which was not found on PATH."
                .into()
        }
    };
    ignore_default_sigpipe();
    let used_sudo = password.is_some();
    let script = build_remote_script(&command, password.as_deref(), cwd.as_deref());
    drop(password);

    let (stdout_path, stderr_path, stdout_file, stderr_file) = match create_output_files("bash") {
        Ok(files) => files,
        Err(e) => return format!("Error creating output files: {e}"),
    };

    let mut cmd = std::process::Command::new(&ssh);
    cmd.args(remote_ssh_args(&host));
    hide_console(&mut cmd);
    // stdin stays open for the whole run: its EOF is what tells the remote
    // watcher to kill the command, so it is closed only once ssh has exited.
    cmd.stdin(Stdio::piped());
    // Output goes to files, and we wait on ssh itself rather than on output
    // EOF: if some other process inherited the output (a ProxyCommand helper),
    // EOF never comes while the channel stays open — a deadlock on Stop.
    cmd.stdout(Stdio::from(stdout_file));
    cmd.stderr(Stdio::from(stderr_file));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            remove_output_files(&stdout_path, &stderr_path);
            return format!("Error running command on {host}: {e}");
        }
    };
    let pid = child.id();
    ctx.register_process(pid);

    // Feed the script from a thread: a large command can exceed the pipe
    // buffer.  The writer hands stdin back when done so it stays open until
    // ssh exits; if ssh dies first the write just fails with EPIPE.
    let (stdin_tx, stdin_rx) = std::sync::mpsc::channel();
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = stdin.write_all(script.as_bytes());
            let _ = stdin.flush();
            let _ = stdin_tx.send(stdin);
        });
    }

    let ctx_blocking = ctx.clone();
    let result = tokio::task::spawn_blocking(move || {
        let wait_result = wait_child(&mut child, pid, timeout);
        ctx_blocking.unregister_process(pid);
        // Closing stdin ends the remote channel's input: the wrapper's watcher
        // then kills the remote command if it is still running.  A writer
        // still stuck after ssh exited keeps (and later drops) it itself.
        if let Ok(stdin) = stdin_rx.recv_timeout(Duration::from_secs(5)) {
            drop(stdin);
        }

        let out = read_and_remove(&stdout_path);
        let err = read_and_remove(&stderr_path);
        wait_result.map(|status| {
            let Some(status) = status else {
                return timed_out_output(format!("Error: Command timed out after {timeout} seconds"), out, &err);
            };
            let mut out = join_command_output(out, &err, status);
            if status.code() == Some(255) && SSH_FAILURE_RE.is_match(&err) {
                out.push_str(&format!(
                    "\n[ssh to {host} failed. run_bash host= requires key-based \
                     login and an existing known_hosts entry]"
                ));
            }
            if used_sudo {
                out.push_str(&sudo_auth_failure_note(&ctx_blocking, Some(&host), &err));
            }
            finish_command_output(out)
        })
    })
    .await;

    match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => e,
        Err(join_err) => format!("Error: Task panicked: {join_err}"),
    }
}

// Fixed prelude for run_powershell.  The model's script is written to a UTF-8
// temp file and compiled with [ScriptBlock]::Create rather than run via -File
// (blocked by the default Restricted execution policy on Windows client SKUs,
// and -ExecutionPolicy Bypass is a pattern EDR flags) or -EncodedCommand (a
// classic malware IOC that corporate EDR blocks outright).  Reading the file
// with an explicit UTF-8 encoding sidesteps 5.1's ANSI default for BOM-less
// scripts, and nothing model-authored ever crosses the command line, so there
// is no quoting to get wrong.
//   - ProgressPreference: progress records otherwise leak onto redirected
//     output (as CLIXML on 5.1).
//   - PSStyle.OutputRendering (7.2+): pwsh emits ANSI colour even into a pipe.
//   - OutputEncoding: UTF-8 both ways so non-ASCII output survives; the
//     Console setter can throw without a console, hence the try.
//   - A parse error surfaces from Create() and must exit non-zero, otherwise
//     the run reports success.
//   - Exit code: the last native exit code (LASTEXITCODE), or 1 when the
//     script throws; `exit N` inside the script ends the process directly.
// Must stay free of double quotes: it is one argv element, and Windows argv
// quoting of embedded quotes is the fragile part.
// Keep byte-identical with the Python and C++ editions.
const POWERSHELL_PRELUDE: &str = concat!(
    "$ProgressPreference = 'SilentlyContinue'; ",
    "if ($PSStyle) { $PSStyle.OutputRendering = 'PlainText' }; ",
    "try { [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false) } catch {}; ",
    "$OutputEncoding = [Text.UTF8Encoding]::new($false); ",
    "try { $__pengy = [ScriptBlock]::Create([IO.File]::ReadAllText('{path}', [Text.Encoding]::UTF8)) } ",
    "catch { $e = $_.Exception; if ($e.InnerException) { $e = $e.InnerException }; ",
    "[Console]::Error.WriteLine($e.Message); exit 1 }; ",
    "$global:LASTEXITCODE = 0; ",
    "& $__pengy; ",
    "exit $LASTEXITCODE",
);

fn powershell_prelude(script_path: &str) -> String {
    // PowerShell single-quoted strings escape ' by doubling it.
    POWERSHELL_PRELUDE.replace("{path}", &script_path.replace('\'', "''"))
}

/// Run a PowerShell script locally (Windows' run_powershell tool).
async fn run_powershell(command: String, cwd: Option<String>, ctx: Arc<ToolContext>) -> String {
    run_powershell_with(POWERSHELL.clone(), command, cwd, ctx).await
}

async fn run_powershell_with(
    exe: Option<PathBuf>,
    command: String,
    cwd: Option<String>,
    ctx: Arc<ToolContext>,
) -> String {
    let exe = match exe {
        Some(p) => p,
        None => {
            return "Error: PowerShell was not found (looked for pwsh and powershell on PATH and Windows PowerShell under System32).".into()
        }
    };
    let run_cwd = match resolve_cwd(&cwd) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let timeout = timeout_secs();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let script = std::env::temp_dir().join(format!("pengy-{}-{}.ps1", std::process::id(), nanos));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&script)
        .and_then(|mut f| f.write_all(command.as_bytes()));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&script);
        return format!("Error writing temp file: {e}");
    }

    let (stdout_path, stderr_path, stdout_file, stderr_file) = match create_output_files("powershell") {
        Ok(files) => files,
        Err(e) => {
            let _ = std::fs::remove_file(&script);
            return format!("Error creating output files: {e}");
        }
    };

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(powershell_prelude(&script.to_string_lossy()))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    if let Some(dir) = &run_cwd {
        cmd.current_dir(dir);
    }
    // POSIX pwsh (tests): own group so a timeout's group kill can't reach us.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    hide_console(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&script);
            remove_output_files(&stdout_path, &stderr_path);
            return format!("Error running PowerShell: {e}");
        }
    };
    let pid = child.id();
    ctx.register_process(pid);

    let ctx_blocking = ctx.clone();
    let result = tokio::task::spawn_blocking(move || {
        let wait_result = wait_child(&mut child, pid, timeout);
        ctx_blocking.unregister_process(pid);
        let out = read_and_remove(&stdout_path);
        let err = read_and_remove(&stderr_path);
        wait_result.map(|status| match status {
            Some(status) => finish_command_output(join_command_output(out, &err, status)),
            None => timed_out_output(format!("Error: Command timed out after {timeout} seconds"), out, &err),
        })
    })
    .await;
    let _ = std::fs::remove_file(&script);

    match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => e,
        Err(join_err) => format!("Error: Task panicked: {join_err}"),
    }
}

/// Wait for a command's process group leader, killing the group on timeout.
/// `Ok(None)` means it timed out and was killed.
fn wait_child(
    child: &mut std::process::Child,
    pid: u32,
    timeout: u64,
) -> Result<Option<std::process::ExitStatus>, String> {
    if timeout > 0 {
        match wait_timeout_status(child, Duration::from_secs(timeout)) {
            Ok(Some(status)) => Ok(Some(status)),
            Ok(None) => {
                // Timed out — kill the process group
                terminate_process_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                Ok(None)
            }
            Err(e) => Err(format!("Error running command: {e}")),
        }
    } else {
        child
            .wait()
            .map(Some)
            .map_err(|e| format!("Error running command: {e}"))
    }
}

/// Append stderr (minus sudo prompt lines) and a non-zero exit code to stdout.
fn join_command_output(mut out: String, err: &str, status: std::process::ExitStatus) -> String {
    let err = SUDO_PROMPT_RE.replace_all(err, "");
    if !err.is_empty() {
        out.push('\n');
        out.push_str(&err);
    }
    if !status.success() {
        out.push_str(&format!("\n[Exit code: {}]", status.code().unwrap_or(-1)));
    }
    out
}

fn finish_command_output(out: String) -> String {
    if out.is_empty() {
        "(No output)".into()
    } else {
        snip_tool_output(out)
    }
}

/// A timeout's error plus whatever the command printed before it was killed:
/// the last lines of a hung watcher or build are what explain the hang, and
/// dropping them left the model with nothing to go on.  Same text as the
/// Python and C++ editions.
fn timed_out_output(error: String, out: String, err: &str) -> String {
    let err = SUDO_PROMPT_RE.replace_all(err, "");
    let mut partial = out;
    if !err.is_empty() {
        if !partial.is_empty() {
            partial.push('\n');
        }
        partial.push_str(&err);
    }
    if partial.trim().is_empty() {
        return error;
    }
    format!("{error}. Output before it was killed:\n{}", snip_tool_output(partial))
}

/// Wait for a child process with a timeout, without blocking the async runtime.
fn wait_timeout_status(
    child: &mut std::process::Child,
    dur: Duration,
) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(Some(status)),
            None => {
                if start.elapsed() >= dur {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn temp_output_paths(prefix: &str) -> (PathBuf, PathBuf) {
    let mut out = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    out.push(format!(
        "pengy_{prefix}_{}_{}.out",
        std::process::id(),
        nanos
    ));

    let mut err = std::env::temp_dir();
    err.push(format!(
        "pengy_{prefix}_{}_{}.err",
        std::process::id(),
        nanos
    ));
    (out, err)
}

fn create_output_files(
    prefix: &str,
) -> Result<(PathBuf, PathBuf, std::fs::File, std::fs::File), std::io::Error> {
    let (stdout_path, stderr_path) = temp_output_paths(prefix);
    let stdout_file = std::fs::File::create(&stdout_path)?;
    let stderr_file = std::fs::File::create(&stderr_path)?;
    Ok((stdout_path, stderr_path, stdout_file, stderr_file))
}

fn read_and_remove(path: &Path) -> String {
    // Decode leniently (U+FFFD for invalid sequences) rather than
    // `fs::read_to_string`'s strict decoding: that silently turned any
    // invalid-UTF-8 output (e.g. a command emitting raw binary bytes) into
    // an empty string via `unwrap_or_default()`. Lossy decoding lets the
    // content reach `snip_tool_output`'s binary guard as text to classify,
    // instead of vanishing without a trace.
    let bytes = std::fs::read(path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn remove_output_files(stdout_path: &Path, stderr_path: &Path) {
    let _ = std::fs::remove_file(stdout_path);
    let _ = std::fs::remove_file(stderr_path);
}

// ── Rate limiter for web searches ──

static LAST_SEARCH_TIME: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Clone, Debug)]
struct WebSearchHit {
    title: String,
    href: String,
    body: String,
    engine: &'static str,
}

async fn web_search(query: String, max_results: usize) -> String {
    // Rate-limit between searches so repeated tool calls do not hammer public search endpoints.
    let wait_ms = {
        let last = LAST_SEARCH_TIME.lock().unwrap();
        last.map(|prev| {
            let elapsed = prev.elapsed();
            if elapsed < Duration::from_millis(800) {
                (Duration::from_millis(800) - elapsed).as_millis() as u64
            } else {
                0
            }
        })
        .unwrap_or(0)
    };
    if wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }
    *LAST_SEARCH_TIME.lock().unwrap() = Some(Instant::now());

    let max_results = max_results.clamp(1, 25);
    let q = query.trim().to_string();
    if q.is_empty() {
        return "Error: query is empty.".into();
    }

    // Native ddgs-inspired metasearch.  DuckDuckGo/Mojeek alone are often not enough;
    // Brave/Yahoo/Google/Startpage/Yandex provide the coverage that the previous Python
    // ddgs fallback was giving us.
    let search_fut = async {
        tokio::join!(
            search_brave(&q, max_results),
            search_ddg_native(&q, max_results),
            search_mojeek_native(&q, max_results),
            search_yahoo(&q, max_results),
            search_google(&q, max_results),
            search_startpage(&q, max_results),
            search_yandex(&q, max_results),
        )
    };

    let (brave, ddg, mojeek, yahoo, google, startpage, yandex) =
        match tokio::time::timeout(Duration::from_secs(12), search_fut).await {
            Ok(results) => results,
            Err(_) => return format!("Web search timed out for query: {q}"),
        };

    let backend_results = vec![
        ("Brave", brave),
        ("DuckDuckGo", ddg),
        ("Mojeek", mojeek),
        ("Yahoo", yahoo),
        ("Google", google),
        ("Startpage", startpage),
        ("Yandex", yandex),
    ];

    let mut failures = Vec::new();
    let mut hits = Vec::new();
    for (name, result) in backend_results {
        match result {
            Ok(mut h) => hits.append(&mut h),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }

    let hits = rank_and_dedupe_hits(hits, &q);
    if !hits.is_empty() {
        return format_hits(&hits, max_results);
    }

    if failures.is_empty() {
        format!("No results found for query: {q}")
    } else {
        format!(
            "Web search failed for query: {q}\n\nBackends tried:\n- {}",
            failures.join("\n- ")
        )
    }
}

fn format_hits(hits: &[WebSearchHit], max_results: usize) -> String {
    let mut lines = Vec::new();
    for (i, hit) in hits.iter().take(max_results).enumerate() {
        lines.push(format!("{}. {}", i + 1, hit.title));
        if !hit.href.is_empty() {
            lines.push(format!("   URL: {}", hit.href));
        }
        if !hit.body.is_empty() {
            lines.push(format!("   {}", hit.body));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim().to_string()
}

fn rank_and_dedupe_hits(hits: Vec<WebSearchHit>, query: &str) -> Vec<WebSearchHit> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for mut hit in hits {
        hit.title = normalize_search_text(&hit.title);
        hit.body = normalize_search_text(&hit.body);
        hit.href = normalize_search_url(&hit.href);
        if hit.title.is_empty() || hit.href.is_empty() || !hit.href.starts_with("http") {
            continue;
        }
        let key = canonical_search_url_key(&hit.href);
        if seen.insert(key) {
            deduped.push(hit);
        }
    }

    let tokens = query_tokens(query);
    let score = |hit: &WebSearchHit| -> i32 {
        let href_l = hit.href.to_lowercase();
        let title_l = hit.title.to_lowercase();
        let body_l = hit.body.to_lowercase();
        let mut s = 0;
        if href_l.contains("wikipedia.org") {
            s += 100;
        }
        if matches!(hit.engine, "brave" | "google" | "yahoo" | "startpage") {
            s += 5;
        }
        let title_hits = tokens.iter().filter(|t| title_l.contains(*t)).count() as i32;
        let body_hits = tokens.iter().filter(|t| body_l.contains(*t)).count() as i32;
        if title_hits > 0 && body_hits > 0 {
            s += 40;
        } else if title_hits > 0 {
            s += 25;
        } else if body_hits > 0 {
            s += 10;
        }
        s + title_hits * 3 + body_hits
    };

    deduped.sort_by(|a, b| score(b).cmp(&score(a)));
    deduped
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() >= 3)
        .collect()
}

fn canonical_search_url_key(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_lowercase();
    for marker in ["?utm_", "&utm_", "?fbclid=", "&fbclid="] {
        if let Some(i) = u.find(marker) {
            u.truncate(i);
        }
    }
    u
}

fn normalize_search_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_search_url(s: &str) -> String {
    urldecode(s.trim()).replace(' ', "+")
}

fn collect_text(el: scraper::ElementRef<'_>, selector: &scraper::Selector) -> String {
    el.select(selector)
        .next()
        .map(|x| normalize_search_text(&x.text().collect::<Vec<_>>().join(" ")))
        .unwrap_or_default()
}

fn collect_attr(el: scraper::ElementRef<'_>, selector: &scraper::Selector, attr: &str) -> String {
    el.select(selector)
        .next()
        .and_then(|x| x.value().attr(attr))
        .unwrap_or("")
        .to_string()
}

fn reqwest_search_client() -> Result<reqwest::Client, String> {
    let ua = ua();
    let user_agent = if ua.is_empty() {
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36"
    } else {
        &ua
    };
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))
}

async fn search_brave(query: &str, max_results: usize) -> Result<Vec<WebSearchHit>, String> {
    let client = reqwest_search_client()?;
    let resp = client
        .get("https://search.brave.com/search")
        .query(&[("q", query), ("source", "web")])
        .header(
            reqwest::header::COOKIE,
            "useLocation=0; safesearch=off; us=us",
        )
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("returned HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse("div[data-type='web']").unwrap();
    let title_sel = scraper::Selector::parse("div.title, .sitename-container").unwrap();
    let href_sel = scraper::Selector::parse("a[href]").unwrap();
    let body_sel = scraper::Selector::parse(".snippet .content, .snippet").unwrap();

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if hits.len() >= max_results {
            break;
        }
        let title = collect_text(item, &title_sel);
        let href = collect_attr(item, &href_sel, "href");
        let body = collect_text(item, &body_sel);
        if !title.is_empty() && href.starts_with("http") {
            hits.push(WebSearchHit {
                title,
                href,
                body,
                engine: "brave",
            });
        }
    }
    if hits.is_empty() {
        Err("No results found.".into())
    } else {
        Ok(hits)
    }
}

async fn search_ddg_native(query: &str, max_results: usize) -> Result<Vec<WebSearchHit>, String> {
    let client = primp::Client::builder()
        .impersonate(primp::Impersonate::ChromeV146)
        .impersonate_os(primp::ImpersonateOS::Linux)
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query), ("b", ""), ("l", "us-en")])
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("returned HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    if html.len() < 5000 {
        return Err("returned a silent block page".into());
    }
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse("div.result, div.web-result").unwrap();
    let title_sel = scraper::Selector::parse("a.result__a").unwrap();
    let body_sel = scraper::Selector::parse("div.result__snippet").unwrap();

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if hits.len() >= max_results {
            break;
        }
        let title = collect_text(item, &title_sel);
        let mut href = collect_attr(item, &title_sel, "href");
        if let Some(p) = href.find("uddg=") {
            href = urldecode(&href[p + 5..]);
        }
        let body = collect_text(item, &body_sel);
        if !title.is_empty() && !href.contains("duckduckgo.com/y.js") {
            hits.push(WebSearchHit {
                title,
                href,
                body,
                engine: "duckduckgo",
            });
        }
    }
    if hits.is_empty() {
        Err("No results found.".into())
    } else {
        Ok(hits)
    }
}

async fn search_mojeek_native(
    query: &str,
    max_results: usize,
) -> Result<Vec<WebSearchHit>, String> {
    let client = reqwest_search_client()?;
    let resp = client
        .get("https://www.mojeek.com/search")
        .query(&[("q", query)])
        .header(reqwest::header::COOKIE, "arc=us; lb=en")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("returned HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse("ul.results-standard > li, ul.results > li").unwrap();
    let title_sel = scraper::Selector::parse("h2 a.title, h2 a[href]").unwrap();
    let body_sel = scraper::Selector::parse("p.s").unwrap();

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if hits.len() >= max_results {
            break;
        }
        let title = collect_text(item, &title_sel);
        let href = collect_attr(item, &title_sel, "href");
        let body = collect_text(item, &body_sel);
        if !title.is_empty() {
            hits.push(WebSearchHit {
                title,
                href,
                body,
                engine: "mojeek",
            });
        }
    }
    if hits.is_empty() {
        Err("No results found.".into())
    } else {
        Ok(hits)
    }
}

async fn search_yahoo(query: &str, max_results: usize) -> Result<Vec<WebSearchHit>, String> {
    let client = reqwest_search_client()?;
    let token_a = uuid::Uuid::new_v4().simple().to_string();
    let token_b = uuid::Uuid::new_v4().simple().to_string();
    let url = format!("https://search.yahoo.com/search;_ylt={token_a};_ylu={token_b}");
    let resp = client
        .get(&url)
        .query(&[("p", query)])
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("returned HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse("div[class*='relsrch']").unwrap();
    let title_sel = scraper::Selector::parse("div[class*='Title'] h3, h3.title, h3").unwrap();
    let href_sel =
        scraper::Selector::parse("div[class*='Title'] a[href], h3 a[href], a[href]").unwrap();
    let body_sel = scraper::Selector::parse("div[class*='Text'], p").unwrap();

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if hits.len() >= max_results {
            break;
        }
        let title = collect_text(item, &title_sel);
        let mut href = collect_attr(item, &href_sel, "href");
        href = extract_yahoo_url(&href);
        let body = collect_text(item, &body_sel);
        if !title.is_empty() && href.starts_with("http") {
            hits.push(WebSearchHit {
                title,
                href,
                body,
                engine: "yahoo",
            });
        }
    }
    if hits.is_empty() {
        Err("No results found.".into())
    } else {
        Ok(hits)
    }
}

fn extract_yahoo_url(raw: &str) -> String {
    if let Some(start) = raw.find("/RU=") {
        let rest = &raw[start + 4..];
        let end = rest
            .find("/RK=")
            .or_else(|| rest.find("/RS="))
            .unwrap_or(rest.len());
        return urldecode(&rest[..end]);
    }
    raw.to_string()
}

fn google_mobile_ua() -> String {
    // ddgs uses old-ish Android Chrome UAs plus the NST^WV suffix; keep it deterministic but similar.
    "Mozilla/5.0 (Linux; Android 8.0; Pixel 2 Build/OPD3.170816.012) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/56.0.2924.1880 Mobile Safari/537.36NST^WV".into()
}

async fn search_google(query: &str, max_results: usize) -> Result<Vec<WebSearchHit>, String> {
    let client = reqwest::Client::builder()
        .user_agent(google_mobile_ua())
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;
    let resp = client
        .get("https://www.google.com/search")
        .query(&[
            ("q", query),
            ("filter", "1"),
            ("start", "0"),
            ("hl", "en-US"),
            ("lr", "lang_en"),
            ("cr", "countryUS"),
        ])
        .header(reqwest::header::COOKIE, "CONSENT=YES+")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("returned HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse("div[data-hveid]").unwrap();
    let h3_sel = scraper::Selector::parse("h3").unwrap();
    let href_sel = scraper::Selector::parse("a[href]").unwrap();

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if hits.len() >= max_results {
            break;
        }
        let title = collect_text(item, &h3_sel);
        if title.is_empty() {
            continue;
        }
        let mut href = String::new();
        for a in item.select(&href_sel) {
            let h = a.value().attr("href").unwrap_or("");
            if h.starts_with("/url?q=") || h.starts_with("http") {
                href = h.to_string();
                break;
            }
        }
        if href.starts_with("/url?q=") {
            href = href
                .split("?q=")
                .nth(1)
                .unwrap_or("")
                .split('&')
                .next()
                .unwrap_or("")
                .to_string();
            href = urldecode(&href);
        }
        let all_text = normalize_search_text(&item.text().collect::<Vec<_>>().join(" "));
        let body = normalize_search_text(all_text.replacen(&title, "", 1).as_str());
        if href.starts_with("http") {
            hits.push(WebSearchHit {
                title,
                href,
                body,
                engine: "google",
            });
        }
    }
    if hits.is_empty() {
        Err("No results found.".into())
    } else {
        Ok(hits)
    }
}

async fn search_startpage(query: &str, max_results: usize) -> Result<Vec<WebSearchHit>, String> {
    let client = reqwest_search_client()?;
    let home = client
        .get("https://www.startpage.com/")
        .send()
        .await
        .map_err(|e| format!("home request failed: {e}"))?;
    if !home.status().is_success() {
        return Err(format!("home returned HTTP {}", home.status().as_u16()));
    }
    let home_html = home
        .text()
        .await
        .map_err(|e| format!("home read failed: {e}"))?;
    let sc = {
        let home_doc = scraper::Html::parse_document(&home_html);
        let sc_sel =
            scraper::Selector::parse("form#search input[name='sc'], input[name='sc']").unwrap();
        home_doc
            .select(&sc_sel)
            .next()
            .and_then(|x| x.value().attr("value"))
            .unwrap_or("")
            .to_string()
    };

    let form = [
        ("query", query),
        ("cat", "web"),
        ("t", "device"),
        ("sc", sc.as_str()),
        ("lui", "english"),
        ("language", "english"),
        ("abp", "1"),
        ("abd", "0"),
        ("abe", "0"),
        ("qsr", "en_US"),
        ("qadf", "none"),
        ("segment", "organic"),
    ];
    let resp = client
        .post("https://www.startpage.com/sp/search")
        .header(reqwest::header::REFERER, "https://www.startpage.com/")
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("returned HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse("div.result").unwrap();
    let title_sel = scraper::Selector::parse("h2, h3").unwrap();
    let href_sel = scraper::Selector::parse("a[href]").unwrap();
    let body_sel = scraper::Selector::parse("p").unwrap();

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if hits.len() >= max_results {
            break;
        }
        let title = collect_text(item, &title_sel);
        let href = collect_attr(item, &href_sel, "href");
        let body = collect_text(item, &body_sel);
        if !title.is_empty() && href.starts_with("http") {
            hits.push(WebSearchHit {
                title,
                href,
                body,
                engine: "startpage",
            });
        }
    }
    if hits.is_empty() {
        Err("No results found.".into())
    } else {
        Ok(hits)
    }
}

async fn search_yandex(query: &str, max_results: usize) -> Result<Vec<WebSearchHit>, String> {
    let client = reqwest_search_client()?;
    let searchid = format!(
        "{}",
        (Instant::now().elapsed().as_nanos() % 9_000_000) + 1_000_000
    );
    let resp = client
        .get("https://yandex.com/search/site/")
        .query(&[
            ("text", query),
            ("web", "1"),
            ("searchid", searchid.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("returned HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse("li.serp-item, li[class*='serp-item']").unwrap();
    let title_sel = scraper::Selector::parse("h3").unwrap();
    let href_sel = scraper::Selector::parse("h3 a[href], a[href]").unwrap();
    let body_sel = scraper::Selector::parse("div[class*='text']").unwrap();

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if hits.len() >= max_results {
            break;
        }
        let title = collect_text(item, &title_sel);
        let href = collect_attr(item, &href_sel, "href");
        let body = collect_text(item, &body_sel);
        if !title.is_empty() && href.starts_with("http") {
            hits.push(WebSearchHit {
                title,
                href,
                body,
                engine: "yandex",
            });
        }
    }
    if hits.is_empty() {
        Err("No results found.".into())
    } else {
        Ok(hits)
    }
}

#[allow(dead_code)]
fn urlencoding(s: &str) -> String {
    let mut result = String::new();
    for byte in s.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(*byte as char);
            }
            b' ' => result.push('+'),
            _ => result.push_str(&format!("%{:02X}", byte)),
        }
    }
    result
}

fn urldecode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

/// Reduce `raw` to a bare filename inside ~/Downloads.
///
/// The model chooses this name and may be acting on instructions from a fetched
/// page, so a path component here must never escape the download directory —
/// `../../.bashrc` has to land as `.bashrc`.  `PathBuf::join` would otherwise
/// let an absolute path replace the destination entirely.  Backslashes are
/// folded too so a Windows-style path can't slip through on POSIX.
fn safe_download_name(raw: &str) -> String {
    let name = raw.replace('\\', "/");
    let name = name.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        return "download".to_string();
    }
    name.to_string()
}

async fn download_file(
    url: String,
    filename: Option<String>,
    dir: Option<String>,
    max_size_mb: Option<i64>,
) -> String {
    let parsed = match url::Url::parse(&url) {
        Ok(u) => u,
        Err(e) => return format!("Error: Invalid URL: {e}"),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return format!(
            "Error: Only http/https URLs are supported (got '{}').",
            parsed.scheme()
        );
    }

    let target_dir = match dir {
        Some(d) => expand_home(&d),
        None => {
            let mut d = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            d.push("Downloads");
            d
        }
    };
    if let Err(e) = std::fs::create_dir_all(&target_dir) {
        return format!("Error creating directory: {e}");
    }
    if !target_dir.is_dir() {
        return format!("Error: dir is not a directory: {}", target_dir.display());
    }

    let fname = safe_download_name(&filename.filter(|s| !s.trim().is_empty()).unwrap_or_else(
        || {
            parsed
                .path_segments()
                .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
                .unwrap_or("download")
                .to_string()
        },
    ));
    let dest = target_dir.join(&fname);

    let limit_mb = max_size_mb.unwrap_or_else(|| *DOWNLOAD_MAX_MB.lock().unwrap() as i64);
    let limit_bytes: usize = if limit_mb <= 0 {
        0
    } else {
        (limit_mb as usize) * 1024 * 1024
    };

    let client = reqwest::Client::builder()
        .user_agent(ua())
        .build()
        .unwrap_or_default();

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error downloading file: {e}"),
    };
    if !resp.status().is_success() {
        return format!("Error downloading file: HTTP {}", resp.status());
    }

    // Stream into a hidden temp file renamed over `dest` only once complete,
    // so a failed download never touches an existing file of the same name.
    let temp = dest.with_file_name(format!(".{fname}.pengy-dl-{}", std::process::id()));
    let fail = |msg: String| {
        let _ = std::fs::remove_file(&temp);
        msg
    };
    let mut total: usize = 0;
    let mut out = match std::fs::File::create(&temp) {
        Ok(f) => f,
        Err(e) => return format!("Error writing file: {e}"),
    };

    let mut stream = resp.bytes_stream();
    loop {
        let chunk =
            match tokio::time::timeout(Duration::from_secs(DOWNLOAD_STALL_TIMEOUT), stream.next())
                .await
            {
                Err(_) => {
                    return fail(format!(
                        "Error downloading: no data for {DOWNLOAD_STALL_TIMEOUT} seconds"
                    ));
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => return fail(format!("Error downloading: {e}")),
                Ok(Some(Ok(c))) => c,
            };
        total += chunk.len();
        if limit_bytes > 0 && total > limit_bytes {
            return fail(format!(
                "Error: Download exceeds maximum size of {}.",
                format_size(limit_bytes as u64)
            ));
        }
        if let Err(e) = out.write_all(&chunk) {
            return fail(format!("Error writing file: {e}"));
        }
    }
    if let Err(e) = out.sync_all().and_then(|_| std::fs::rename(&temp, &dest)) {
        return fail(format!("Error writing file: {e}"));
    }

    format!("Downloaded to {} ({total} bytes)", dest.display())
}

async fn fetch_url(url_str: String, max_chars: Option<usize>) -> String {
    let parsed = match url::Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => return format!("Error: Invalid URL: {e}"),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return format!(
            "Error: Only http/https URLs are supported (got '{}').",
            parsed.scheme()
        );
    }

    let mut builder = reqwest::Client::builder().user_agent(ua());
    let timeout = timeout_secs();
    if timeout > 0 {
        builder = builder.timeout(Duration::from_secs(timeout));
    }
    let client = builder.build().unwrap_or_default();

    let resp = match client.get(&url_str).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error fetching URL: {e}"),
    };
    if !resp.status().is_success() {
        return format!("Error fetching URL: HTTP {}", resp.status());
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return format!("Error reading response: {e}"),
    };
    let raw = if raw.len() > 2 * 1024 * 1024 {
        raw.slice(0..2 * 1024 * 1024)
    } else {
        raw
    };

    let text = String::from_utf8_lossy(&raw).to_string();
    // Single lowercase allocation instead of two (saves up to 4MB for a 2MB page)
    let text_lower = text.to_ascii_lowercase();
    let is_html = content_type.contains("html")
        || text_lower.contains("<html")
        || text_lower.contains("<!doctype");

    let text = if is_html {
        let document = scraper::Html::parse_document(&text);
        let body_text = if let Ok(body_sel) = scraper::Selector::parse("body") {
            document
                .select(&body_sel)
                .next()
                .map(|body| body.text().collect::<Vec<_>>().join(""))
                .unwrap_or_else(|| document.root_element().text().collect::<String>())
        } else {
            document.root_element().text().collect::<String>()
        };
        static NEWLINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
        NEWLINE_RE
            .replace_all(&body_text, "\n\n")
            .trim()
            .to_string()
    } else {
        text
    };

    let budget = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
    let limit = max_chars.unwrap_or(budget);
    if looks_binary(&text).0 {
        return "Error fetching URL: response appears to be binary data, not text".to_string();
    }
    if limit > 0 && text.len() > limit {
        format!(
            "{}\n\n[... truncated at {} characters — pass max_chars to adjust ...]",
            truncate_on_char_boundary(&text, limit),
            limit
        )
    } else {
        text
    }
}

async fn run_python(code: String, cwd: Option<String>, ctx: Arc<ToolContext>) -> String {
    let timeout = timeout_secs();
    let run_cwd = match resolve_cwd(&cwd) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut tmp = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    tmp.push(format!("pengy_py_{}_{}.py", std::process::id(), nanos));
    if let Err(e) = std::fs::write(&tmp, &code) {
        return format!("Error writing temp file: {e}");
    }

    let (stdout_path, stderr_path, stdout_file, stderr_file) = match create_output_files("python") {
        Ok(files) => files,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return format!("Error creating output files: {e}");
        }
    };

    let mut cmd = std::process::Command::new(python_interpreter());
    hide_console(&mut cmd);
    cmd.arg(&tmp)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    if let Some(dir) = &run_cwd {
        cmd.current_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            remove_output_files(&stdout_path, &stderr_path);
            return format!("Error running Python: {e}");
        }
    };
    let pid = child.id();
    ctx.register_process(pid);

    let ctx_blocking = ctx.clone();
    let result = tokio::task::spawn_blocking(move || {
        let wait_result = if timeout > 0 {
            match wait_timeout_status(&mut child, Duration::from_secs(timeout)) {
                Ok(Some(status)) => Ok(Some(status)),
                Ok(None) => {
                    terminate_process_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    Ok(None)
                }
                Err(e) => Err(format!("Error running Python: {e}")),
            }
        } else {
            child
                .wait()
                .map(Some)
                .map_err(|e| format!("Error running Python: {e}"))
        };
        ctx_blocking.unregister_process(pid);

        let mut s = read_and_remove(&stdout_path);
        let err = read_and_remove(&stderr_path);
        wait_result.map(|status| {
            let Some(status) = status else {
                return timed_out_output(
                    format!("Error: Python execution timed out after {timeout} seconds"),
                    s,
                    &err,
                );
            };
            if !err.is_empty() {
                s.push('\n');
                s.push_str(&err);
            }
            if !status.success() {
                s.push_str(&format!("\n[Exit code: {}]", status.code().unwrap_or(-1)));
            }
            if s.is_empty() {
                "(No output)".into()
            } else {
                snip_tool_output(s)
            }
        })
    })
    .await;

    let _ = std::fs::remove_file(&tmp);

    match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => e,
        Err(join_err) => format!("Error: Task panicked: {join_err}"),
    }
}

fn python_interpreter() -> PathBuf {
    if let Ok(path) = std::env::var("PENGY_PYTHON") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        if !venv.trim().is_empty() {
            let mut p = PathBuf::from(venv);
            #[cfg(windows)]
            p.push("Scripts\\python.exe");
            #[cfg(not(windows))]
            p.push("bin/python");
            return p;
        }
    }
    PathBuf::from("python3")
}

async fn directory_tree(path: String, max_depth: usize, show_hidden: bool) -> String {
    let root = expand_home(&path);
    if !root.exists() {
        return format!("Error: Directory not found: {path}");
    }
    if !root.is_dir() {
        return format!("Error: Not a directory: {path}");
    }

    let mut lines = vec![format!("{}/", root.display())];
    let mut file_count = 0;
    build_tree(
        &root,
        "",
        1,
        max_depth,
        show_hidden,
        &mut lines,
        &mut file_count,
        500,
    );

    if lines.len() == 1 {
        lines.push("(empty directory)".into());
    }
    let result = lines.join("\n");
    snip_tool_output(result)
}

fn build_tree(
    dir: &Path,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    show_hidden: bool,
    lines: &mut Vec<String>,
    file_count: &mut usize,
    max_entries: usize,
) {
    if depth > max_depth || *file_count >= max_entries {
        return;
    }
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(iter) => iter.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(e) => {
            lines.push(format!("{prefix}[Error: {e}]"));
            return;
        }
    };
    entries.retain(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !show_hidden && name.starts_with('.') {
            return false;
        }
        !ALWAYS_SKIP_DIRS.contains(name) && !name.ends_with(".egg-info")
    });
    entries.sort_by(|a, b| {
        let ad = a.is_dir();
        let bd = b.is_dir();
        if ad != bd {
            bd.cmp(&ad)
        } else {
            a.file_name().cmp(&b.file_name())
        }
    });

    for (i, entry) in entries.iter().enumerate() {
        if *file_count >= max_entries {
            lines.push(format!(
                "{prefix}... (truncated, {max_entries} entries reached)"
            ));
            return;
        }
        let is_last = i == entries.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("?");

        if entry.is_dir() {
            lines.push(format!("{prefix}{connector}{name}/"));
            *file_count += 1;
            if depth < max_depth {
                let ext = if is_last { "    " } else { "│   " };
                build_tree(
                    entry,
                    &format!("{prefix}{ext}"),
                    depth + 1,
                    max_depth,
                    show_hidden,
                    lines,
                    file_count,
                    max_entries,
                );
            }
        } else {
            let size = std::fs::metadata(entry).map(|m| m.len()).unwrap_or(0);
            lines.push(format!(
                "{prefix}{connector}{name}  ({})",
                format_size(size)
            ));
            *file_count += 1;
        }
    }
}

use once_cell::sync::Lazy;
static ALWAYS_SKIP_DIRS: Lazy<HashSet<&str>> = Lazy::new(|| {
    [
        "node_modules",
        ".git",
        ".svn",
        ".hg",
        "__pycache__",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        ".tox",
        ".eggs",
        ".DS_Store",
    ]
    .iter()
    .copied()
    .collect()
});

fn format_size(size: u64) -> String {
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else if size < 1024 * 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", size as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

async fn read_multiple_files(paths: Vec<String>) -> String {
    const MAX_FILES: usize = 20;

    if paths.is_empty() {
        return "Error: no paths provided.".into();
    }
    if paths.len() > MAX_FILES {
        return format!(
            "Error: too many files ({}). Maximum is {MAX_FILES}.",
            paths.len()
        );
    }

    // Derive per-file and total budgets from the tool output limit so the
    // single "max tool output" setting governs how much context a batch can
    // consume.  0 means "no limit".
    let budget = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
    let per_file = budget;
    let total = if budget > 0 { budget * 5 } else { 0 };

    let mut parts: Vec<String> = Vec::new();
    let mut total_chars = 0;

    for raw_path in &paths {
        let p = expand_home(raw_path);
        let sep = "=".repeat(60);
        let header = format!("{sep}\n📄 {raw_path}");

        if !p.exists() {
            parts.push(format!("{header}\n  ❌ File not found."));
            continue;
        }
        if !p.is_file() {
            parts.push(format!("{header}\n  ❌ Not a file."));
            continue;
        }

        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(e) => {
                parts.push(format!("{header}\n  ❌ Error reading file: {e}"));
                continue;
            }
        };

        // Same binary guard as read_file: valid-UTF-8 bytes that aren't text
        // (e.g. a UTF-16 file's NUL-interleaved ASCII) must not flood context.
        if looks_binary(&content).0 {
            parts.push(format!("{header}\n  ❌ Binary file (not text)."));
            continue;
        }

        // Same head-truncation and line-range reporting as read_file, so the
        // model can follow up with read_file(offset=...) on whichever file was cut.
        let total_lines = content.matches('\n').count() + 1;
        let (kept_text, kept, truncated) = truncate_head_lines(&content, Some(per_file));
        let content = if truncated {
            format!(
                "{kept_text}\n\n[... showed lines 1-{kept} of {total_lines} — \
                 read_file with offset={} to continue ...]",
                kept + 1
            )
        } else {
            content
        };

        let block = format!("{header}\n{content}");
        if total > 0 && total_chars + block.len() > total {
            let remaining = total - total_chars;
            if remaining > 200 {
                let short_block = format!(
                    "{header}\n{}...",
                    truncate_on_char_boundary(&content, remaining.saturating_sub(header.len() + 4))
                );
                parts.push(short_block);
            } else {
                parts.push(format!(
                    "\n[... output limit reached; {} files skipped ...]",
                    paths.len().saturating_sub(parts.len())
                ));
                break;
            }
        } else {
            parts.push(block);
        }
        total_chars += parts.last().map(|s| s.len()).unwrap_or(0);
    }

    parts.join("\n\n")
}

async fn search_content(
    pattern: String,
    path: String,
    file_glob: Option<String>,
    context_lines: usize,
    max_results: usize,
    regex: bool,
    ignore_case: bool,
) -> String {
    let root = expand_home(&path);
    if !root.exists() {
        return format!("Error: Path not found: {path}");
    }

    // Literal by default so metacharacters in code symbols (".", "(", "[", "*",
    // ...) don't silently become regex syntax; regex=true opts into regex.
    let source = if regex { pattern.clone() } else { regex::escape(&pattern) };
    let compiled = match regex::RegexBuilder::new(&source).case_insensitive(ignore_case).build() {
        Ok(r) => r,
        Err(e) => return format!("Error: Invalid regex pattern: {e}"),
    };

    let context_lines = context_lines.min(10);
    let max_results = max_results.clamp(1, 200);

    let mut results: Vec<String> = Vec::new();
    let mut files_searched = 0;
    let mut skipped_binary = 0;
    let mut skipped_large = 0;
    let mut truncated = false;

    if root.is_file() {
        search_one_file(
            &root,
            &compiled,
            context_lines,
            &mut results,
            max_results,
            None,
        );
        if results.is_empty() {
            return format!("No matches found for '{pattern}' in {path}");
        }
        return results.join("\n\n");
    }

    // The root itself is never filtered: searching "." or ~/.config or a dir
    // named "build" must search it, not prune it before the walk starts.
    let walker = walkdir::WalkDir::new(&root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !should_skip_dir(e));

    for entry in walker {
        if truncated {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let fname = entry.file_name().to_str().unwrap_or("");
        if ALWAYS_SKIP_FILES.contains(fname) {
            continue;
        }
        if let Some(ref glob) = file_glob {
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if !matches_file_glob(fname, &rel, glob) {
                continue;
            }
        }
        match search_skip_reason(entry.path()) {
            Some("large") => {
                skipped_large += 1;
                continue;
            }
            Some(_) => {
                skipped_binary += 1;
                continue;
            }
            None => {}
        }
        files_searched += 1;
        if search_one_file(
            entry.path(),
            &compiled,
            context_lines,
            &mut results,
            max_results,
            Some(&root),
        ) {
            truncated = true;
        }
    }

    if results.is_empty() {
        let mut summary =
            format!("No matches found for '{pattern}' in {path} (searched {files_searched} files");
        if skipped_binary > 0 {
            summary.push_str(&format!("; skipped {skipped_binary} binary"));
        }
        if skipped_large > 0 {
            summary.push_str(&format!("; skipped {skipped_large} over 10 MB"));
        }
        summary.push(')');
        if let Some(glob) = file_glob.as_deref().filter(|g| !g.is_empty()) {
            if files_searched == 0 && skipped_binary == 0 && skipped_large == 0 {
                summary.push_str(&format!(". No file matched file_glob '{glob}'."));
            }
        }
        return summary;
    }

    let out = results.join("\n\n");
    let mut summary = format!(
        "Found {} match(es) for '{pattern}' across {files_searched} file(s)",
        results.len()
    );
    if truncated {
        summary.push_str(" (results truncated)");
    }
    snip_tool_output(format!("{summary}\n{}\n{out}", "─".repeat(60)))
}

fn search_one_file(
    filepath: &Path,
    compiled: &Regex,
    context_lines: usize,
    results: &mut Vec<String>,
    max_results: usize,
    root: Option<&Path>,
) -> bool {
    // Lossy like Python's errors="replace": a stray invalid byte must not hide
    // a whole file from the search.
    let content = match std::fs::read(filepath) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return false,
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut matched_lines: HashSet<usize> = HashSet::new();
    for (i, line) in lines.iter().enumerate() {
        if compiled.is_match(line) {
            matched_lines.insert(i);
        }
    }
    if matched_lines.is_empty() {
        return false;
    }
    let display = match root {
        Some(r) => filepath
            .strip_prefix(r)
            .unwrap_or(filepath)
            .display()
            .to_string(),
        None => filepath.display().to_string(),
    };
    let regions = group_regions(&matched_lines, context_lines, lines.len());
    for (start, end) in regions {
        if results.len() >= max_results {
            return true;
        }
        let mut block = vec![format!("📄 {display}:")];
        for ln in start..end {
            let marker = if matched_lines.contains(&ln) {
                " ▸"
            } else {
                "  "
            };
            block.push(format!("{marker}{:5} │ {}", ln + 1, lines[ln]));
        }
        results.push(block.join("\n"));
    }
    results.len() >= max_results
}

fn group_regions(
    matched: &HashSet<usize>,
    context: usize,
    total_lines: usize,
) -> Vec<(usize, usize)> {
    let mut sorted: Vec<usize> = matched.iter().copied().collect();
    sorted.sort_unstable();
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for &line in &sorted {
        let start = line.saturating_sub(context);
        let end = (line + context + 1).min(total_lines);
        if let Some(last) = regions.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        regions.push((start, end));
    }
    regions
}

/// Hidden directories are searched (.github, .config and .vscode hold real
/// content and a model asking "which files mention X" expects them); only the
/// known-noise list is pruned.  Mirrors `_should_skip_search_dir`.
fn should_skip_dir(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .map(|name| {
                name.ends_with(".egg-info")
                    || ALWAYS_SKIP_DIRS.contains(name)
                    || SEARCH_EXTRA_SKIP_DIRS.contains(&name)
            })
            .unwrap_or(false)
}

const SEARCH_EXTRA_SKIP_DIRS: &[&str] = &[".venv", ".cache"];

static ALWAYS_SKIP_FILES: Lazy<HashSet<&str>> =
    Lazy::new(|| [".DS_Store", "Thumbs.db"].iter().copied().collect());

// search_content decides text vs binary by content (a NUL byte in the first
// 8 KB), not by an extension allowlist: an allowlist silently hid whole
// languages (.cs, .vue, .tf, .ps1), logs and extensionless scripts, and the
// model got "No matches" with no hint that files were never searched.  The
// extensions below are only a fast path to skip known binaries unread.
const SEARCH_BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tif", "tiff", "pdf", "zip", "gz", "tgz",
    "bz2", "xz", "zst", "7z", "rar", "tar", "jar", "war", "class", "pyc", "pyo", "o", "obj", "a",
    "lib", "so", "dylib", "dll", "exe", "bin", "wasm", "whl", "deb", "rpm", "woff", "woff2", "ttf",
    "otf", "eot", "mp3", "mp4", "wav", "flac", "ogg", "mkv", "mov", "avi", "webm", "sqlite", "db",
    "iso", "dmg",
];
const SEARCH_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// `Some("binary" | "large")` if `path` shouldn't be searched, else `None`.
fn search_skip_reason(path: &Path) -> Option<&'static str> {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        if SEARCH_BINARY_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
            return Some("binary");
        }
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return Some("binary");
    };
    if meta.len() > SEARCH_MAX_FILE_BYTES {
        return Some("large");
    }
    let mut buf = [0u8; 8192];
    let n = match std::fs::File::open(path).and_then(|mut f| std::io::Read::read(&mut f, &mut buf)) {
        Ok(n) => n,
        Err(_) => return Some("binary"),
    };
    if buf[..n].contains(&0) {
        return Some("binary");
    }
    None
}

static GLOB_BRACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*)\{([^}]+)\}(.*)$").unwrap());

// Cache compiled glob regexes — the glob is constant for an entire
// search_content call, so this avoids recompiling per file.
static GLOB_CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Regex>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// A `file_glob` without `/` matches the file name (`*.py`); one with `/`
/// matches the path relative to the search root (`src/*.py`, `**/test_*.py`),
/// with a leading `**/` also matching at the root.  Mirrors `_matches_glob`.
fn matches_file_glob(name: &str, rel: &str, glob: &str) -> bool {
    if !glob.contains('/') {
        return matches_glob(name, glob);
    }
    let patterns: Vec<String> = match GLOB_BRACE_RE.captures(glob) {
        Some(caps) => {
            let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let suffix = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            caps.get(2)
                .map(|m| m.as_str())
                .unwrap_or("")
                .split(',')
                .map(|c| format!("{prefix}{}{suffix}", c.trim()))
                .collect()
        }
        None => vec![glob.to_string()],
    };
    patterns.iter().any(|p| {
        if !p.contains('/') {
            return simple_glob_match(name, p);
        }
        simple_glob_match(rel, p) || p.strip_prefix("**/").is_some_and(|rest| simple_glob_match(rel, rest))
    })
}

fn matches_glob(name: &str, glob: &str) -> bool {
    // Handle brace expansion like *.{js,ts}
    if let Some(caps) = GLOB_BRACE_RE.captures(glob) {
        let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let choices = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let suffix = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        for choice in choices.split(',') {
            let pattern = format!("{prefix}{choice}{suffix}");
            if simple_glob_match(name, &pattern) {
                return true;
            }
        }
        return false;
    }
    simple_glob_match(name, glob)
}

fn simple_glob_match(name: &str, pattern: &str) -> bool {
    // Check the cache first — avoids recompiling the same glob pattern
    // for every file in a search_content directory walk.
    {
        let cache = GLOB_CACHE.lock().unwrap();
        if let Some(re) = cache.get(pattern) {
            return re.is_match(name);
        }
    }
    let escaped = pattern
        .replace('.', "\\.")
        .replace('*', ".*")
        .replace('?', ".");
    let re = match Regex::new(&format!("^{escaped}$")) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let result = re.is_match(name);
    GLOB_CACHE.lock().unwrap().insert(pattern.to_string(), re);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::collections::HashSet;
    use std::sync::Mutex as TestMutex;

    static TEST_TOOL_TIMEOUT_LOCK: Lazy<TestMutex<()>> = Lazy::new(|| TestMutex::new(()));

    pub(crate) fn test_tool_timeout_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_TOOL_TIMEOUT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    static TEST_TOOL_OUTPUT_LOCK: Lazy<TestMutex<()>> = Lazy::new(|| TestMutex::new(()));

    fn test_tool_output_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_TOOL_OUTPUT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // ── rewrite_sudo_for_askpass ────────────────────────────────────

    #[test]
    fn sudo_rewrite_plain_command() {
        assert_eq!(
            rewrite_sudo_for_askpass("sudo apt update"),
            "sudo -A apt update"
        );
    }

    #[test]
    fn sudo_rewrite_dash_s_becomes_askpass() {
        // stdin no longer carries the password, so -S must be replaced.
        assert_eq!(
            rewrite_sudo_for_askpass("sudo -S apt update"),
            "sudo -A apt update"
        );
    }

    #[test]
    fn sudo_rewrite_already_dash_a_unchanged() {
        assert_eq!(
            rewrite_sudo_for_askpass("sudo -A apt update"),
            "sudo -A apt update"
        );
    }

    #[test]
    fn sudo_rewrite_sudoku_unchanged() {
        assert_eq!(rewrite_sudo_for_askpass("echo sudoku"), "echo sudoku");
    }

    #[test]
    fn sudo_rewrite_pseudo_tty_unchanged() {
        assert_eq!(
            rewrite_sudo_for_askpass("ls /dev/pseudo-tty"),
            "ls /dev/pseudo-tty"
        );
    }

    #[test]
    fn sudo_rewrite_every_occurrence() {
        // askpass works for each invocation, unlike a single piped password.
        assert_eq!(
            rewrite_sudo_for_askpass("sudo apt update && sudo apt upgrade"),
            "sudo -A apt update && sudo -A apt upgrade"
        );
    }

    #[test]
    fn sudo_rewrite_in_pipeline() {
        assert_eq!(
            rewrite_sudo_for_askpass("echo hi | sudo tee /etc/f"),
            "echo hi | sudo -A tee /etc/f"
        );
    }

    #[test]
    fn sudo_mentions_are_not_invocations_or_rewritten() {
        for command in ["echo sudo", "echo 'sudo apt update'", "# sudo\necho ok"] {
            assert!(sudo_invocation_spans(command).is_empty(), "{command}");
            assert_eq!(rewrite_sudo_for_askpass(command), command);
        }
    }

    #[tokio::test]
    async fn sudo_requires_explicit_elevation_without_prompting() {
        let ctx = Arc::new(ToolContext::new());
        ctx.set_sudo_provider(Some(Box::new(|_| panic!("must not prompt"))));
        let result = run_bash("sudo true".into(), None, false, None, ctx).await;
        assert!(result.contains("Elevation required"));
    }

    /// The web frontend's sudo provider publishes a `sudo_request` to its SSE
    /// stream, then blocks until the browser answers.  Called straight from
    /// this async fn, that block parks a Tokio worker thread — and the SSE
    /// task the publish just woke sits in that same worker's non-stealable
    /// LIFO slot, so the prompt never reaches the browser (reconnecting the
    /// stream "fixed" it by replaying the log on another worker).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sudo_provider_block_does_not_starve_woken_tasks() {
        let (notify_tx, mut notify_rx) = tokio::sync::watch::channel(0usize);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (woke_tx, woke_rx) = std::sync::mpsc::channel();
        let woke_rx = std::sync::Mutex::new(woke_rx);
        tokio::spawn(async move {
            let _ = ready_tx.send(());
            if notify_rx.changed().await.is_ok() {
                let _ = woke_tx.send(());
            }
        });

        let saw_wake = Arc::new(AtomicBool::new(false));
        let saw_wake_provider = saw_wake.clone();
        tokio::spawn(async move {
            ready_rx.await.unwrap();
            tokio::task::yield_now().await;
            let ctx = Arc::new(ToolContext::new());
            ctx.set_sudo_provider(Some(Box::new(move |_| {
                notify_tx.send_replace(1);
                if woke_rx.lock().unwrap().recv_timeout(Duration::from_secs(3)).is_ok() {
                    saw_wake_provider.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                None
            })));
            run_bash("sudo true".into(), None, true, None, ctx).await
        })
        .await
        .unwrap();

        assert!(
            saw_wake.load(std::sync::atomic::Ordering::SeqCst),
            "task woken by the sudo provider never ran while it blocked"
        );
    }

    /// offset/limit page through a file; the header states where you are so the
    /// model can request the next page.  Mirrors Python's
    /// test_read_file_offset_and_limit — keep in sync.
    #[tokio::test]
    async fn read_file_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.txt");
        let body: String = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).unwrap();
        let p = path.to_str().unwrap().to_string();

        let ranged = read_file(p.clone(), Some(5), Some(3)).await;
        let mut it = ranged.lines();
        assert_eq!(it.next().unwrap(), format!("[Lines 5-7 of 20 in {p}]"));
        assert_eq!(it.collect::<Vec<_>>(), vec!["line 5", "line 6", "line 7"]);

        // limit alone starts at line 1; offset alone runs to the end.
        let head = read_file(p.clone(), None, Some(2)).await;
        assert_eq!(
            head.lines().skip(1).collect::<Vec<_>>(),
            vec!["line 1", "line 2"]
        );
        let tail = read_file(p.clone(), Some(19), None).await;
        assert_eq!(
            tail.lines().skip(1).collect::<Vec<_>>(),
            vec!["line 19", "line 20"]
        );

        // A limit past the end clamps instead of erroring.
        let clamped = read_file(p.clone(), Some(19), Some(100)).await;
        assert!(clamped.starts_with(&format!("[Lines 19-20 of 20 in {p}]")));

        // No offset/limit keeps the plain whole-file behaviour (no header).
        assert!(read_file(p, None, None).await.starts_with("line 1"));
    }

    #[tokio::test]
    async fn read_file_offset_past_end_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.txt");
        std::fs::write(&path, "a\nb\nc").unwrap();
        let result = read_file(path.to_str().unwrap().to_string(), Some(99), None).await;
        assert!(result.contains("Error"), "{result}");
        assert!(result.contains("3 lines"), "{result}");
    }

    // ── is_readonly_tool ───────────────────────────────────────────

    #[test]
    fn readonly_tools_classified_correctly() {
        assert!(is_readonly_tool("read_file"));
        assert!(is_readonly_tool("read_image"));
        assert!(is_readonly_tool("read_multiple_files"));
        assert!(is_readonly_tool("directory_tree"));
        assert!(is_readonly_tool("search_content"));
        assert!(is_readonly_tool("web_search"));
        assert!(is_readonly_tool("fetch_url"));
    }

    #[test]
    fn write_tools_not_readonly() {
        assert!(!is_readonly_tool("write_file"));
        assert!(!is_readonly_tool("replace_in_file"));
        assert!(!is_readonly_tool("run_bash"));
        assert!(!is_readonly_tool("run_python"));
        assert!(!is_readonly_tool("download_file"));
    }

    #[test]
    fn unknown_tool_not_readonly() {
        assert!(!is_readonly_tool("nonexistent_tool"));
        assert!(!is_readonly_tool(""));
    }

    // ── tool_definitions ───────────────────────────────────────────

    #[test]
    fn tool_definitions_has_sixteen_tools() {
        // Windows adds run_powershell.
        assert_eq!(tool_definitions().len(), 16 + usize::from(cfg!(windows)));
    }

    #[test]
    fn tool_definitions_json_has_sixteen_tools() {
        let json = tool_definitions_json();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 16 + usize::from(cfg!(windows)));
    }

    #[test]
    fn tool_definitions_json_matches_struct() {
        let from_struct: serde_json::Value = serde_json::to_value(tool_definitions()).unwrap();
        let from_cache = tool_definitions_json();
        assert_eq!(from_struct, from_cache);
    }

    #[test]
    fn tool_definitions_all_have_function_type() {
        for td in tool_definitions() {
            assert_eq!(td.tool_type, "function");
        }
    }

    #[test]
    fn tool_definitions_names_are_unique() {
        let names: HashSet<String> = tool_definitions()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert_eq!(names.len(), 16);
    }

    #[test]
    fn tool_definitions_all_have_required_fields() {
        for td in tool_definitions() {
            assert!(!td.function.name.is_empty());
            assert!(!td.function.description.is_empty());
            assert!(!td.function.parameters.required.is_empty());
            assert_eq!(td.function.parameters.param_type, "object");
        }
    }

    #[test]
    fn tool_definitions_serializes_to_valid_json() {
        let defs = tool_definitions();
        let json = serde_json::to_string(&defs).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 16);
    }

    #[test]
    fn read_multiple_files_schema_includes_string_items() {
        let defs = tool_definitions();
        let read_multi = defs
            .iter()
            .find(|t| t.function.name == "read_multiple_files")
            .expect("read_multiple_files tool definition");
        let paths = &read_multi.function.parameters.properties["paths"];
        assert_eq!(paths["type"], "array");
        assert_eq!(paths["items"]["type"], "string");
    }

    #[tokio::test]
    async fn run_python_uses_configured_timeout() {
        let _guard = test_tool_timeout_guard();
        let old = *TOOL_TIMEOUT.lock().unwrap();
        *TOOL_TIMEOUT.lock().unwrap() = 1;
        let result = run_python(
            "import time; time.sleep(5)".into(),
            None,
            Arc::new(ToolContext::new()),
        )
        .await;
        *TOOL_TIMEOUT.lock().unwrap() = old;
        assert!(result.contains("Python execution timed out after 1 seconds"));
    }

    #[tokio::test]
    async fn run_bash_uses_configured_timeout() {
        let _guard = test_tool_timeout_guard();
        let old = *TOOL_TIMEOUT.lock().unwrap();
        *TOOL_TIMEOUT.lock().unwrap() = 1;
        let result = run_bash("sleep 5".into(), None, false, None, Arc::new(ToolContext::new())).await;
        *TOOL_TIMEOUT.lock().unwrap() = old;
        assert!(result.contains("Command timed out after 1 seconds"));
    }

    /// A timeout keeps what the command printed before it was killed: the
    /// last lines of a hung watcher are what explain the hang.
    #[tokio::test]
    async fn timeouts_return_output_printed_before_the_kill() {
        let _guard = test_tool_timeout_guard();
        let old = *TOOL_TIMEOUT.lock().unwrap();
        *TOOL_TIMEOUT.lock().unwrap() = 1;
        let ctx = Arc::new(ToolContext::new());
        let bash = run_bash("echo started; echo oops >&2; sleep 5".into(), None, false, None, ctx.clone()).await;
        let python = run_python(
            "import sys, time; print('working', flush=True); time.sleep(5)".into(),
            None,
            ctx.clone(),
        )
        .await;
        let silent = run_bash("sleep 5".into(), None, false, None, ctx).await;
        *TOOL_TIMEOUT.lock().unwrap() = old;
        assert_eq!(bash, "Error: Command timed out after 1 seconds. Output before it was killed:\nstarted\n\noops\n");
        assert_eq!(python, "Error: Python execution timed out after 1 seconds. Output before it was killed:\nworking\n");
        assert_eq!(silent, "Error: Command timed out after 1 seconds");
    }

    #[tokio::test]
    async fn run_bash_respects_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_bash(
            "touch marker.txt && ls".into(),
            Some(dir.path().to_str().unwrap().to_string()),
            false,
            None,
            Arc::new(ToolContext::new()),
        )
        .await;
        assert!(result.contains("marker.txt"));
        assert!(dir.path().join("marker.txt").exists());
    }

    #[tokio::test]
    async fn run_bash_blocks_binary_output() {
        let result = run_bash(
            "head -c 4000 /dev/urandom".into(),
            None,
            false,
            None,
            Arc::new(ToolContext::new()),
        )
        .await;
        assert!(result.starts_with("[Binary output blocked:"), "{result}");
    }

    /// A command whose output isn't valid UTF-8 must not silently vanish
    /// (read_and_remove used to decode strictly and swallow it via
    /// `unwrap_or_default()`) -- it decodes leniently and goes through the
    /// same binary guard as everything else.
    #[tokio::test]
    async fn run_bash_survives_invalid_utf8() {
        let result = run_bash(
            r"printf '\xff\xfe\xfd\xfc'".into(),
            None,
            false,
            None,
            Arc::new(ToolContext::new()),
        )
        .await;
        assert_ne!(result, "(No output)");
    }

    #[tokio::test]
    async fn run_bash_invalid_cwd() {
        let result = run_bash(
            "pwd".into(),
            Some("/nonexistent_dir_xyz".into()),
            false,
            None,
            Arc::new(ToolContext::new()),
        )
        .await;
        assert!(result.contains("cwd not found"));
    }

    #[tokio::test]
    async fn run_python_respects_cwd() {
        let dir = tempfile::tempdir().unwrap();
        run_python(
            "open('marker.txt','w').write('x')".into(),
            Some(dir.path().to_str().unwrap().to_string()),
            Arc::new(ToolContext::new()),
        )
        .await;
        assert!(dir.path().join("marker.txt").exists());
    }

    #[tokio::test]
    async fn execute_tool_outer_safety_timeout_fires() {
        let _guard = test_tool_timeout_guard();
        let old = *TOOL_TIMEOUT.lock().unwrap();
        *TOOL_TIMEOUT.lock().unwrap() = 1;
        let args = serde_json::json!({"code": "import time; time.sleep(60)"});
        let ctx = Arc::new(ToolContext::new());
        let result = tokio::time::timeout(
            Duration::from_secs(35),
            execute_tool("run_python", &args, &ctx),
        )
        .await
        .expect("outer safety net should finish before test timeout");
        *TOOL_TIMEOUT.lock().unwrap() = old;
        assert!(
            result.contains("Python execution timed out")
                || result.contains("Tool timed out (outer safety net")
        );
    }

    #[test]
    fn kill_all_only_affects_own_context() {
        let _guard = test_tool_timeout_guard();
        let spawn_sleep = || {
            let mut command = std::process::Command::new("bash");
            command
                .arg("-c")
                .arg("sleep 30")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .stdin(Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            command.spawn().unwrap()
        };

        let ctx_a = Arc::new(ToolContext::new());
        let ctx_b = Arc::new(ToolContext::new());

        let mut child = spawn_sleep();
        let pid = child.id();
        ctx_b.register_process(pid);

        // Killing ctx_a must not touch ctx_b's process.
        ctx_a.kill_all();
        assert!(matches!(child.try_wait(), Ok(None)));

        // ctx_b owns it, so this kills it.
        ctx_b.kill_all();
        let _ = child.kill();
        let _ = child.wait();
        assert!(!ctx_b.active_process_groups.lock().unwrap().contains(&pid));
    }

    #[test]
    fn tool_context_sudo_provider_is_per_context() {
        let ctx_a = ToolContext::new();
        let ctx_b = ToolContext::new();
        ctx_a.set_sudo_provider(Some(Box::new(|_| Some("pw-a".into()))));
        ctx_b.set_sudo_provider(Some(Box::new(|_| Some("pw-b".into()))));
        let call = |c: &ToolContext| c.sudo_provider.lock().unwrap().as_ref().unwrap()(None);
        assert_eq!(call(&ctx_a), Some("pw-a".to_string()));
        assert_eq!(call(&ctx_b), Some("pw-b".to_string()));
    }

    #[test]
    fn cached_sudo_password_not_shared() {
        let ctx_a = ToolContext::new();
        let ctx_b = ToolContext::new();
        ctx_a
            .cached_sudo_passwords
            .lock()
            .unwrap()
            .insert(None, "secret".into());
        assert!(ctx_b.cached_sudo_passwords.lock().unwrap().is_empty());
        ctx_a.clear_sudo();
        assert!(ctx_a.cached_sudo_passwords.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_bash_refuses_sudo_without_provider() {
        let _guard = test_tool_timeout_guard();
        // A context with no provider must refuse sudo regardless of any other.
        let ctx = Arc::new(ToolContext::new());
        let result = run_bash("sudo true".into(), None, true, None, ctx).await;
        assert!(result.contains("no password provider"));
    }

    #[tokio::test]
    async fn run_bash_rejects_elevated_without_sudo() {
        let _guard = test_tool_timeout_guard();
        // Regression: elevated=true on a command with no `sudo` used to be a
        // silent no-op (ran unprivileged, no prompt, no error). It must fail
        // loudly, and must NOT run the command.
        let ctx = Arc::new(ToolContext::new());
        let result = run_bash("echo should-not-run".into(), None, true, None, ctx).await;
        assert!(result.contains("elevated=true"), "{result:?}");
        assert!(result.contains("does not invoke sudo"), "{result:?}");
        assert!(!result.contains("should-not-run"), "command ran: {result:?}");
    }

    #[tokio::test]
    async fn run_bash_rejects_elevated_with_only_quoted_sudo() {
        let _guard = test_tool_timeout_guard();
        // A quoted/comment mention of sudo is data, not an invocation: it must
        // not satisfy elevated=true.
        let ctx = Arc::new(ToolContext::new());
        let result = run_bash("echo 'sudo apt update'".into(), None, true, None, ctx).await;
        assert!(result.contains("does not invoke sudo"), "{result:?}");
        assert!(!result.contains("apt update"), "command ran: {result:?}");
    }

    #[tokio::test]
    async fn run_bash_plain_command_without_elevated_still_runs() {
        let _guard = test_tool_timeout_guard();
        // Regression guard: ordinary (non-elevated) commands are unaffected.
        let ctx = Arc::new(ToolContext::new());
        let result = run_bash("echo hello-plain".into(), None, false, None, ctx).await;
        assert!(result.contains("hello-plain"), "{result:?}");
    }

    #[tokio::test]
    async fn run_bash_sudo_authenticates_via_askpass() {
        let _guard = test_tool_timeout_guard();
        // Stub sudo that only succeeds when given -A plus a working askpass.
        let dir = tempfile::Builder::new()
            .prefix("pengy-sudo-test-")
            .tempdir()
            .unwrap();
        let sudo = dir.path().join("sudo");
        std::fs::write(
            &sudo,
            "#!/bin/bash\n\
             if [ \"$1\" != \"-A\" ]; then echo \"sudo: no tty present\" >&2; exit 1; fi\n\
             shift\n\
             pw=\"$(\"$SUDO_ASKPASS\")\"\n\
             if [ -n \"${PENGY_SUDO_PASSWORD:-}\" ]; then echo \"LEAK:${PENGY_SUDO_PASSWORD}\"; fi\n\
             echo \"pw=$pw\"\n\
             exec \"$@\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", dir.path().display()));

        let ctx = Arc::new(ToolContext::new());
        ctx.set_sudo_provider(Some(Box::new(|_| Some("s3cret".to_string()))));

        // Shapes the old stdin-piped `sudo -S` broke on.
        for command in [
            "sudo echo hi",
            "echo a; cat > /dev/null; sudo echo hi",
            "sudo echo one; sudo echo two",
            "sudo echo hi < /dev/null",
            "sudo -S echo hi",
        ] {
            let result = run_bash(command.into(), None, true, None, ctx.clone()).await;
            assert!(result.contains("pw=s3cret"), "{command:?} -> {result:?}");
            assert!(
                !result.contains("no tty present"),
                "{command:?} -> {result:?}"
            );
            assert!(
                !result.contains("LEAK:"),
                "password leaked into env: {command:?} -> {result:?}"
            );
        }

        std::env::set_var("PATH", old_path);
    }

    #[test]
    fn askpass_helper_keeps_password_out_of_env_and_script() {
        let helper = AskpassHelper::new("s3cret").unwrap();
        let script = std::fs::read_to_string(&helper.path).unwrap();
        // The script must not embed the password itself.
        assert!(!script.contains("s3cret"), "{script:?}");
        // The password lives in a separate file the script cats.
        let pw_file = script
            .split("cat '")
            .nth(1)
            .unwrap()
            .split('\'')
            .next()
            .unwrap();
        assert_eq!(std::fs::read_to_string(pw_file).unwrap(), "s3cret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&helper.path), 0o700);
            assert_eq!(mode(std::path::Path::new(pw_file)), 0o600);
        }
        drop(helper);
    }

    #[test]
    fn python_interpreter_prefers_pengy_python_env() {
        let _guard = test_tool_timeout_guard();
        let old = std::env::var("PENGY_PYTHON").ok();
        std::env::set_var("PENGY_PYTHON", "/tmp/pengy-python-test");
        assert_eq!(
            python_interpreter(),
            PathBuf::from("/tmp/pengy-python-test")
        );
        if let Some(v) = old {
            std::env::set_var("PENGY_PYTHON", v);
        } else {
            std::env::remove_var("PENGY_PYTHON");
        }
    }

    // ── expand_home ────────────────────────────────────────────────

    #[test]
    fn expand_home_tilde_slash() {
        let result = expand_home("~/Documents/test.txt");
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("Documents/test.txt"));
    }

    #[test]
    fn expand_home_tilde_only() {
        let result = expand_home("~");
        assert_eq!(result, dirs::home_dir().unwrap());
    }

    #[test]
    fn expand_home_absolute_path_unchanged() {
        let result = expand_home("/tmp/test.txt");
        assert_eq!(result, PathBuf::from("/tmp/test.txt"));
    }

    #[test]
    fn expand_home_relative_path_unchanged() {
        let result = expand_home("relative/path.txt");
        assert_eq!(result, PathBuf::from("relative/path.txt"));
    }

    // ── format_size ────────────────────────────────────────────────

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn format_size_gigabytes() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    // ── urlencoding ────────────────────────────────────────────────

    #[test]
    fn urlencoding_plain_text() {
        assert_eq!(urlencoding("hello"), "hello");
    }

    #[test]
    fn urlencoding_spaces_become_plus() {
        assert_eq!(urlencoding("hello world"), "hello+world");
    }

    #[test]
    fn urlencoding_special_chars() {
        assert_eq!(urlencoding("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn urlencoding_preserves_unreserved() {
        assert_eq!(urlencoding("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn urlencoding_empty() {
        assert_eq!(urlencoding(""), "");
    }

    // ── matches_glob / simple_glob_match ───────────────────────────

    #[test]
    fn glob_star_extension() {
        assert!(matches_glob("test.rs", "*.rs"));
        assert!(!matches_glob("test.py", "*.rs"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(simple_glob_match("a.rs", "?.rs"));
        assert!(!simple_glob_match("ab.rs", "?.rs"));
    }

    #[test]
    fn glob_brace_expansion() {
        assert!(matches_glob("test.js", "*.{js,ts}"));
        assert!(matches_glob("test.ts", "*.{js,ts}"));
        assert!(!matches_glob("test.py", "*.{js,ts}"));
    }

    #[test]
    fn glob_exact_match() {
        assert!(matches_glob("Makefile", "Makefile"));
        assert!(!matches_glob("makefile", "Makefile"));
    }

    // ── group_regions ──────────────────────────────────────────────

    #[test]
    fn group_regions_single_match_no_context() {
        let matched: HashSet<usize> = [5].into_iter().collect();
        let regions = group_regions(&matched, 0, 20);
        assert_eq!(regions, vec![(5, 6)]);
    }

    #[test]
    fn group_regions_single_match_with_context() {
        let matched: HashSet<usize> = [5].into_iter().collect();
        let regions = group_regions(&matched, 2, 20);
        assert_eq!(regions, vec![(3, 8)]);
    }

    #[test]
    fn group_regions_overlapping_merge() {
        let matched: HashSet<usize> = [5, 7].into_iter().collect();
        let regions = group_regions(&matched, 2, 20);
        assert_eq!(regions, vec![(3, 10)]);
    }

    #[test]
    fn group_regions_non_overlapping_separate() {
        let matched: HashSet<usize> = [2, 15].into_iter().collect();
        let regions = group_regions(&matched, 1, 20);
        assert_eq!(regions, vec![(1, 4), (14, 17)]);
    }

    #[test]
    fn group_regions_clamps_to_bounds() {
        let matched: HashSet<usize> = [0, 19].into_iter().collect();
        let regions = group_regions(&matched, 2, 20);
        assert_eq!(regions[0].0, 0);
        assert_eq!(regions.last().unwrap().1, 20);
    }

    #[test]
    fn group_regions_empty() {
        let matched: HashSet<usize> = HashSet::new();
        let regions = group_regions(&matched, 2, 20);
        assert!(regions.is_empty());
    }

    // ── argument helpers ───────────────────────────────────────────

    #[test]
    fn arg_helper_extracts_string() {
        let args = serde_json::json!({"path": "/tmp/test"});
        assert_eq!(a(&args, "path", ""), "/tmp/test");
    }

    #[test]
    fn arg_helper_default_on_missing() {
        let args = serde_json::json!({});
        assert_eq!(a(&args, "path", "/default"), "/default");
    }

    #[test]
    fn arg_helper_optional() {
        let args = serde_json::json!({"name": "test"});
        assert_eq!(aopt(&args, "name"), Some("test".into()));
        assert_eq!(aopt(&args, "missing"), None);
    }

    #[test]
    fn arg_helper_usize() {
        let args = serde_json::json!({"count": 42});
        assert_eq!(aus(&args, "count", 0), 42);
        assert_eq!(aus(&args, "missing", 10), 10);
    }

    #[test]
    fn arg_helper_bool() {
        let args = serde_json::json!({"flag": true});
        assert!(abool(&args, "flag", false));
        assert!(!abool(&args, "missing", false));
    }

    // ── tool execution (filesystem-based) ──────────────────────────

    #[tokio::test]
    async fn read_file_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let result = read_file(path.to_str().unwrap().to_string(), None, None).await;
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn read_file_not_found() {
        let result = read_file("/tmp/pengy_nonexistent_file_12345.txt".into(), None, None).await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn write_file_creates_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.txt");
        let result = write_file(path.to_str().unwrap().to_string(), "content".into()).await;
        assert!(result.contains("Successfully"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/file.txt");
        let result = write_file(path.to_str().unwrap().to_string(), "nested".into()).await;
        assert!(result.contains("Successfully"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nested");
    }

    #[tokio::test]
    async fn replace_in_file_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replace.txt");
        std::fs::write(&path, "hello world foo bar").unwrap();
        let result = replace_in_file(
            path.to_str().unwrap().into(),
            "world".into(),
            "universe".into(),
            false,
        )
        .await;
        assert!(result.contains("Successfully"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hello universe foo bar"
        );
    }

    #[tokio::test]
    async fn replace_in_file_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replace.txt");
        std::fs::write(&path, "hello world").unwrap();
        let result = replace_in_file(
            path.to_str().unwrap().into(),
            "nonexistent".into(),
            "x".into(),
            false,
        )
        .await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn replace_in_file_multiple_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replace.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();
        let result = replace_in_file(path.to_str().unwrap().into(), "aaa".into(), "x".into(), false).await;
        assert!(result.contains("matches 2 locations"));
    }

    #[tokio::test]
    async fn replace_in_file_empty_old_str() {
        let result = replace_in_file("/tmp/x".into(), "".into(), "y".into(), false).await;
        assert!(result.contains("old_str is empty"));
    }

    #[tokio::test]
    async fn replace_in_file_not_found() {
        let result = replace_in_file(
            "/tmp/pengy_nonexistent_12345.txt".into(),
            "x".into(),
            "y".into(),
            false,
        )
        .await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn directory_tree_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("subdir/nested.txt"), "nested").unwrap();
        let result = directory_tree(dir.path().to_str().unwrap().into(), 3, false).await;
        assert!(result.contains("subdir/"));
        assert!(result.contains("file.txt"));
        assert!(result.contains("nested.txt"));
    }

    #[tokio::test]
    async fn directory_tree_not_found() {
        let result = directory_tree("/tmp/pengy_nonexistent_dir_12345".into(), 3, false).await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn directory_tree_hides_hidden_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden"), "secret").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "public").unwrap();
        let result = directory_tree(dir.path().to_str().unwrap().into(), 3, false).await;
        assert!(!result.contains(".hidden"));
        assert!(result.contains("visible.txt"));
    }

    #[tokio::test]
    async fn directory_tree_shows_hidden_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden"), "secret").unwrap();
        let result = directory_tree(dir.path().to_str().unwrap().into(), 3, true).await;
        assert!(result.contains(".hidden"));
    }

    #[tokio::test]
    async fn execute_tool_unknown_tool() {
        let args = serde_json::json!({});
        let ctx = Arc::new(ToolContext::new());
        let result = execute_tool("nonexistent_tool", &args, &ctx).await;
        assert!(result.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn execute_tool_dispatches_read_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dispatch_test.txt");
        std::fs::write(&path, "dispatch content").unwrap();
        let args = serde_json::json!({"path": path.to_str().unwrap()});
        let ctx = Arc::new(ToolContext::new());
        let result = execute_tool("read_file", &args, &ctx).await;
        assert_eq!(result, "dispatch content");
    }

    #[tokio::test]
    async fn search_content_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        let result = search_content(
            "println".into(),
            file_path.to_str().unwrap().into(),
            None,
            0,
            50,
            false,
            false,
        )
        .await;
        assert!(result.contains("println"));
    }

    #[tokio::test]
    async fn search_content_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn main() {}").unwrap();
        let result = search_content(
            "nonexistent_pattern".into(),
            dir.path().to_str().unwrap().into(),
            None,
            0,
            50,
            false,
            false,
        )
        .await;
        assert!(result.contains("No matches"));
    }

    #[tokio::test]
    async fn search_content_path_not_found() {
        let result = search_content(
            "test".into(),
            "/tmp/pengy_nonexistent_12345".into(),
            None,
            0,
            50,
            false,
            false,
        )
        .await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn search_content_literal_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("search.rs");
        std::fs::write(&file_path, "value = 'a.b'\nvalue2 = 'axb'\n").unwrap();

        // Default: literal match — "a.b" must not match "axb".
        let literal = search_content(
            "a.b".into(),
            file_path.to_str().unwrap().into(),
            None,
            0,
            50,
            false,
            false,
        )
        .await;
        assert!(literal.contains("a.b"));
        assert!(!literal.contains("axb"));

        // regex=true: '.' is a wildcard, so it should match "axb" too.
        let regex = search_content(
            "a.b".into(),
            file_path.to_str().unwrap().into(),
            None,
            0,
            50,
            true,
            false,
        )
        .await;
        assert!(regex.contains("axb"));
    }

    #[tokio::test]
    async fn read_multiple_files_basic() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("a.txt");
        let p2 = dir.path().join("b.txt");
        std::fs::write(&p1, "content a").unwrap();
        std::fs::write(&p2, "content b").unwrap();
        let result = read_multiple_files(vec![
            p1.to_str().unwrap().into(),
            p2.to_str().unwrap().into(),
        ])
        .await;
        assert!(result.contains("content a"));
        assert!(result.contains("content b"));
    }

    #[tokio::test]
    async fn read_multiple_files_empty_paths() {
        let result = read_multiple_files(vec![]).await;
        assert!(result.contains("no paths"));
    }

    #[tokio::test]
    async fn read_multiple_files_too_many() {
        let paths: Vec<String> = (0..25).map(|i| format!("/tmp/file_{i}.txt")).collect();
        let result = read_multiple_files(paths).await;
        assert!(result.contains("too many"));
    }

    #[tokio::test]
    async fn read_multiple_files_limits_follow_output_limit() {
        let _guard = test_tool_output_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.txt");
        let body: String = (1..=2000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&p, &body).unwrap();

        let old = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = 1000;
        let result = read_multiple_files(vec![p.to_str().unwrap().into()]).await;
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = old;

        assert!(result.contains("showed lines 1-"));
        assert!(result.contains("read_file with offset="));
    }

    fn spawn_http_server(body: &str) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body_bytes = body.as_bytes().to_vec();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_bytes.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body_bytes);
            }
        });
        (format!("http://{addr}/"), handle)
    }

    #[tokio::test]
    async fn fetch_url_truncates_to_output_limit() {
        let _guard = test_tool_output_guard();
        let (url, handle) = spawn_http_server(&"x".repeat(5000));
        let old = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = 1000;
        let result = fetch_url(url, None).await;
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = old;
        handle.join().unwrap();
        assert!(result.contains("truncated at 1000"));
        assert!(result.len() < 5000);
    }

    #[tokio::test]
    async fn fetch_url_max_chars_override() {
        let _guard = test_tool_output_guard();
        let (url, handle) = spawn_http_server(&"x".repeat(5000));
        let old = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = 1000;
        let result = fetch_url(url, Some(0)).await;
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = old;
        handle.join().unwrap();
        assert!(!result.contains("truncated"));
        assert!(result.len() >= 5000);
    }

    #[tokio::test]
    async fn download_file_dir_and_filename() {
        let (url, handle) = spawn_http_server("hello download");
        let dir = tempfile::tempdir().unwrap();
        let result = download_file(
            url,
            Some("report.txt".into()),
            Some(dir.path().to_str().unwrap().to_string()),
            None,
        )
        .await;
        handle.join().unwrap();
        assert!(result.contains("Downloaded to"));
        let contents = std::fs::read_to_string(dir.path().join("report.txt")).unwrap();
        assert_eq!(contents, "hello download");
    }

    #[tokio::test]
    async fn download_file_max_size_exceeded() {
        let body = "x".repeat(1024 * 1024 + 100);
        let (url, handle) = spawn_http_server(&body);
        let dir = tempfile::tempdir().unwrap();
        let result = download_file(
            url,
            Some("big.bin".into()),
            Some(dir.path().to_str().unwrap().to_string()),
            Some(1),
        )
        .await;
        handle.join().unwrap();
        assert!(result.contains("exceeds maximum size"));
        assert!(!dir.path().join("big.bin").exists());
    }

    #[tokio::test]
    async fn download_file_max_size_unlimited() {
        let body = "x".repeat(1024 * 1024 + 100);
        let (url, handle) = spawn_http_server(&body);
        let dir = tempfile::tempdir().unwrap();
        let result = download_file(
            url,
            Some("big.bin".into()),
            Some(dir.path().to_str().unwrap().to_string()),
            Some(0),
        )
        .await;
        handle.join().unwrap();
        assert!(result.contains("Downloaded to"));
        assert!(std::fs::metadata(dir.path().join("big.bin")).unwrap().len() > 1024 * 1024);
    }

    // ── UTF-8 boundary truncation ───────────────────────────────────

    #[test]
    fn truncate_on_char_boundary_ascii_within_limit() {
        let s = "hello";
        assert_eq!(truncate_on_char_boundary(s, 10), "hello");
    }

    #[test]
    fn truncate_on_char_boundary_ascii_exact() {
        let s = "hello";
        assert_eq!(truncate_on_char_boundary(s, 5), "hello");
    }

    #[test]
    fn truncate_on_char_boundary_ascii_truncates() {
        let s = "hello world";
        assert_eq!(truncate_on_char_boundary(s, 5), "hello");
    }

    #[test]
    fn truncate_on_char_boundary_multibyte_at_boundary() {
        // 🐧 is 4 bytes; placing it so it straddles byte 3000
        let base = "a".repeat(2999);
        let s = format!("{base}🐧tail");
        // byte 3000 lands inside the 4-byte penguin — must back up to 2999
        let result = truncate_on_char_boundary(&s, 3000);
        assert_eq!(result.len(), 2999);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_on_char_boundary_multibyte_exactly_fits() {
        // 🐧 is 4 bytes starting at offset 2999 — doesn't fit in 3000
        // but fits at 3003 (2999 ascii + 4 bytes penguin)
        let base = "a".repeat(2999);
        let s = format!("{base}🐧tail");
        let result = truncate_on_char_boundary(&s, 3003);
        assert_eq!(result, format!("{base}🐧"));
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn read_multiple_files_truncates_on_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unicode.txt");
        // Write a file where a multibyte character straddles the current
        // 250_000-byte per-file limit. The reader must back up to a UTF-8
        // boundary before appending its truncation marker.
        // The whole file is one line, so there is no line break to back up to —
        // the char-boundary fallback must still keep the output valid UTF-8.
        let content = "a".repeat(249_999) + "🐧extra";
        std::fs::write(&path, &content).unwrap();
        let result = read_multiple_files(vec![path.to_str().unwrap().into()]).await;
        assert!(result.contains("[... showed lines 1-"), "{result:.200}");
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(!result.contains("🐧extra"));
    }

    /// Files truncate from the head, not the middle: the head holds imports and
    /// declarations, and unlike a log the rest can be paged to.  Mirrors
    /// Python's test_read_file_truncates_from_head_with_continuation.
    #[tokio::test]
    async fn read_file_truncates_from_head_with_continuation() {
        let _guard = test_tool_output_guard();
        let _guard = test_tool_timeout_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let body: String = (1..=5000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).unwrap();
        let p = path.to_str().unwrap().to_string();

        let prev = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = 2000;
        let result = read_file(p.clone(), None, None).await;
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = prev;

        let mut lines = result.lines();
        let header = lines.next().unwrap().to_string();
        let body_lines: Vec<&str> = lines.collect();

        // No middle gap — content runs contiguously from line 1.
        assert!(!result.contains("snipped"), "{header}");
        assert_eq!(body_lines[0], "line 1");
        assert!(header.contains("of 5000 in"), "{header}");
        assert!(header.contains("output limit reached"), "{header}");

        // The stated continuation offset is the next unseen line.
        let last_shown: usize = body_lines
            .last()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let offset: usize = header
            .split("offset=")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(offset, last_shown + 1);
    }

    /// run_bash output keeps head+tail: the command echo is at the start and
    /// the error that matters is usually at the end.
    #[test]
    fn command_output_stays_tail_biased() {
        let _guard = test_tool_output_guard();
        let _guard = test_tool_timeout_guard();
        let text: String = (1..=5000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let prev = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = 2000;
        let out = snip_tool_output(text);
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = prev;

        assert!(out.starts_with("line 1"));
        assert!(out.trim_end().ends_with("line 5000"));
        assert!(out.contains("snipped"));
    }

    /// Text with embedded NULs (e.g. a UTF-16 file decoded as UTF-8) is
    /// blocked outright rather than truncated into context.
    #[test]
    fn binary_guard_blocks_null_bytes() {
        let text = "a\0b\0c\0".repeat(100);
        let out = snip_tool_output(text);
        assert!(out.starts_with("[Binary output blocked:"));
        assert!(out.contains("non-printable"));
    }

    /// Output that is mostly control/non-printable characters, even without
    /// NULs, is still classified as binary.
    #[test]
    fn binary_guard_blocks_high_control_char_ratio() {
        let text: String = (0..500).map(|i| ((i % 31) + 1) as u8 as char).collect();
        let out = snip_tool_output(text);
        assert!(out.starts_with("[Binary output blocked:"));
    }

    /// Ordinary command output must not trip the binary guard.
    #[test]
    fn binary_guard_leaves_normal_text_alone() {
        let text = "def foo():\n\treturn 'hello world!' # comment\n".repeat(50);
        let out = snip_tool_output(text.clone());
        assert_eq!(out, text);
    }

    /// UTF-16 text decodes as *valid* UTF-8 (ASCII interleaved with NULs), so
    /// the strict decode alone misses it — read_file needs the guard too.
    #[tokio::test]
    async fn read_file_blocks_binary_that_decodes_as_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("utf16.txt");
        std::fs::write(
            &p,
            "hello"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let result = read_file(p.to_str().unwrap().into(), None, None).await;
        assert!(result.to_lowercase().contains("binary"), "{result:?}");
        assert!(!result.contains("hello"));
    }

    #[tokio::test]
    async fn read_multiple_files_blocks_binary_that_decodes_as_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("utf16.txt");
        std::fs::write(
            &bin,
            "hello"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let good = dir.path().join("good.txt");
        std::fs::write(&good, "plain text").unwrap();
        let result = read_multiple_files(vec![
            bin.to_str().unwrap().into(),
            good.to_str().unwrap().into(),
        ])
        .await;
        assert!(result.to_lowercase().contains("binary"), "{result:?}");
        assert!(result.contains("plain text"));
    }

    #[tokio::test]
    async fn fetch_url_blocks_binary_body() {
        let (url, handle) = spawn_http_server(&"a\0b\0c\0".repeat(200));
        let result = fetch_url(url, None).await;
        handle.join().unwrap();
        assert!(result.to_lowercase().contains("binary"), "{result:?}");
        assert!(!result.contains("a\0"));
    }

    /// Character-index cuts left a broken half-line at each seam, which on
    /// source code is a fragment the model may try to "fix".
    #[test]
    fn truncation_never_splits_a_line() {
        let _guard = test_tool_output_guard();
        let _guard = test_tool_timeout_guard();
        let text: String = (1..=5000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let prev = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = 2000;
        let out = snip_tool_output(text.clone());
        let (kept, _, _) = truncate_head_lines(&text, None);
        let kept = kept.to_string();
        *TOOL_OUTPUT_MAX_CHARS.lock().unwrap() = prev;

        let (head, rest) = out.split_once("[... snipped").unwrap();
        let tail = rest.split_once("]").unwrap().1;
        let whole = |frag: &str| frag.split_whitespace().count() == 2 && frag.starts_with("line ");
        for frag in head.trim().lines().chain(tail.trim().lines()) {
            assert!(whole(frag), "broken fragment at seam: {frag:?}");
        }
        assert!(whole(kept.lines().last().unwrap()));
    }

    /// A single line longer than the budget has no line break to back up to —
    /// it must still be cut rather than blowing the limit.
    #[test]
    fn truncation_handles_one_giant_line() {
        let text = "x".repeat(5000);
        let (kept, _, truncated) = truncate_head_lines(&text, Some(500));
        assert!(truncated);
        assert_eq!(kept.len(), 500);
    }

    // ── glob_tool ──────────────────────────────────────────────────

    #[tokio::test]
    async fn glob_tool_finds_py_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x").unwrap();
        std::fs::write(dir.path().join("b.rs"), "y").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.py"), "z").unwrap();

        let result = glob_tool("**/*.py".into(), Some(dir.path().to_str().unwrap().into())).await;
        assert!(result.contains("a.py"));
        assert!(result.contains("sub/c.py"));
        assert!(!result.contains("b.rs"));
    }

    #[tokio::test]
    async fn glob_tool_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let result = glob_tool("*.xyz".into(), Some(dir.path().to_str().unwrap().into())).await;
        assert!(result.contains("No files matching"));
    }

    #[tokio::test]
    async fn glob_tool_skips_hidden_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden.py"), "x").unwrap();
        std::fs::write(dir.path().join("visible.py"), "y").unwrap();

        let result = glob_tool("*.py".into(), Some(dir.path().to_str().unwrap().into())).await;
        assert!(result.contains("visible.py"));
        assert!(!result.contains(".hidden.py"));
    }

    /// ".env" is in the skip set as a virtualenv *directory* name; matching it
    /// against files made the common .env *file* unfindable.
    #[tokio::test]
    async fn glob_tool_finds_dotenv_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        let args = serde_json::json!({"pattern": ".env", "path": dir.path().to_str().unwrap()});
        let ctx = std::sync::Arc::new(ToolContext::new());
        let result = execute_tool("glob", &args, &ctx).await;
        assert!(result.contains(".env"), "{result}");
        assert!(!result.contains("No files matching"), "{result}");
    }

    /// The virtualenv case the skip entry exists for must keep working.
    #[tokio::test]
    async fn glob_tool_still_skips_dotenv_directory_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".env")).unwrap();
        std::fs::write(dir.path().join(".env/pyvenv.py"), "x").unwrap();
        std::fs::write(dir.path().join("real.py"), "y").unwrap();
        let args = serde_json::json!({"pattern": "**/*.py", "path": dir.path().to_str().unwrap()});
        let ctx = std::sync::Arc::new(ToolContext::new());
        let result = execute_tool("glob", &args, &ctx).await;
        assert!(result.contains("real.py"), "{result}");
        assert!(!result.contains("pyvenv.py"), "{result}");
    }

    /// A pattern whose final component starts with "." wants hidden entries,
    /// even when the pattern as a whole starts with "*".
    #[tokio::test]
    async fn glob_tool_finds_hidden_when_pattern_asks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.config"), "x").unwrap();
        let args =
            serde_json::json!({"pattern": "**/.config", "path": dir.path().to_str().unwrap()});
        let ctx = std::sync::Arc::new(ToolContext::new());
        let result = execute_tool("glob", &args, &ctx).await;
        assert!(result.contains(".config"), "{result}");
    }

    /// A recursive pattern ending in a literal (no wildcard) must still match;
    /// comparing the whole relative path made these unmatchable.
    #[tokio::test]
    async fn glob_tool_recursive_literal_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/Makefile"), "x").unwrap();
        let args =
            serde_json::json!({"pattern": "**/Makefile", "path": dir.path().to_str().unwrap()});
        let ctx = std::sync::Arc::new(ToolContext::new());
        let result = execute_tool("glob", &args, &ctx).await;
        assert!(result.contains("Makefile"), "{result}");
    }

    /// The model picks the download name and may be acting on a fetched page's
    /// instructions, so a path component must never leave ~/Downloads.
    #[test]
    fn download_filename_cannot_escape_downloads() {
        assert_eq!(safe_download_name("../../.bashrc"), ".bashrc");
        assert_eq!(safe_download_name("/etc/passwd"), "passwd");
        assert_eq!(safe_download_name("a/b/c.txt"), "c.txt");
        assert_eq!(safe_download_name("..\\..\\evil.exe"), "evil.exe");
        assert_eq!(safe_download_name(".."), "download");
        assert_eq!(safe_download_name("."), "download");
        assert_eq!(safe_download_name(""), "download");
        assert_eq!(safe_download_name("subdir/"), "download");
        assert_eq!(safe_download_name("report.pdf"), "report.pdf");
    }

    #[tokio::test]
    async fn glob_tool_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/foo.js"), "x").unwrap();
        std::fs::write(dir.path().join("src.js"), "y").unwrap();

        let result = glob_tool("**/*.js".into(), Some(dir.path().to_str().unwrap().into())).await;
        assert!(result.contains("src.js"));
        assert!(!result.contains("node_modules"));
    }

    #[tokio::test]
    async fn glob_tool_dir_prefix_in_pattern() {
        // When pattern includes a directory path, extract it as the search dir
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x").unwrap();
        std::fs::write(dir.path().join("b.rs"), "y").unwrap();

        let pattern = format!("{}/*.py", dir.path().to_str().unwrap());
        let result = glob_tool(pattern, None).await;
        assert!(result.contains("a.py"));
        assert!(!result.contains("b.rs"));
    }

    #[tokio::test]
    async fn glob_tool_exact_file_in_pattern() {
        // Pattern is a path to a specific file — should find it via dir extraction
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("target.rs"), "content").unwrap();

        let pattern = format!("{}/target.rs", dir.path().to_str().unwrap());
        let result = glob_tool(pattern, None).await;
        assert!(result.contains("target.rs"));
    }

    #[tokio::test]
    async fn glob_tool_dir_prefix_with_recursive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.py"), "x").unwrap();
        std::fs::write(dir.path().join("sub/b.py"), "y").unwrap();

        let pattern = format!("{}/**/*.py", dir.path().to_str().unwrap());
        let result = glob_tool(pattern, None).await;
        assert!(result.contains("a.py"));
        assert!(result.contains("sub/b.py"));
    }

    #[tokio::test]
    async fn glob_tool_explicit_path_takes_precedence() {
        // When path is explicitly provided, use it (don't extract from pattern)
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x").unwrap();

        let result = glob_tool("*.py".into(), Some(dir.path().to_str().unwrap().into())).await;
        assert!(result.contains("a.py"));
    }

    // ── todowrite ─────────────────────────────────────────────────

    #[tokio::test]
    async fn todowrite_echoes_back_valid_todos() {
        let todos = vec![
            serde_json::json!({"content": "Find auth code", "status": "in_progress"}),
            serde_json::json!({"content": "Add JWT", "status": "pending"}),
            serde_json::json!({"content": "Write tests", "status": "pending"}),
        ];
        let result = todowrite(todos).await;
        assert!(result.contains("[→] Find auth code"));
        assert!(result.contains("[ ] Add JWT"));
        assert!(result.contains("[ ] Write tests"));
    }

    #[tokio::test]
    async fn todowrite_rejects_multiple_in_progress() {
        let todos = vec![
            serde_json::json!({"content": "Task A", "status": "in_progress"}),
            serde_json::json!({"content": "Task B", "status": "in_progress"}),
        ];
        let result = todowrite(todos).await;
        assert!(result.contains("Error"));
        assert!(result.contains("in_progress"));
    }

    #[tokio::test]
    async fn todowrite_rejects_invalid_status() {
        let todos = vec![serde_json::json!({"content": "Task A", "status": "done"})];
        let result = todowrite(todos).await;
        assert!(result.contains("invalid status"));
    }

    #[tokio::test]
    async fn todowrite_rejects_empty_content() {
        let todos = vec![serde_json::json!({"content": "", "status": "pending"})];
        let result = todowrite(todos).await;
        assert!(result.contains("content is empty"));
    }

    #[tokio::test]
    async fn todowrite_rejects_empty_list() {
        let result = todowrite(vec![]).await;
        assert!(result.contains("empty"));
    }

    #[tokio::test]
    async fn todowrite_all_pending_is_valid() {
        let todos = vec![
            serde_json::json!({"content": "Task A", "status": "pending"}),
            serde_json::json!({"content": "Task B", "status": "pending"}),
        ];
        let result = todowrite(todos).await;
        assert!(!result.contains("Error"));
    }

    #[tokio::test]
    async fn todowrite_allows_all_completed() {
        let todos = vec![
            serde_json::json!({"content": "Task A", "status": "completed"}),
            serde_json::json!({"content": "Task B", "status": "completed"}),
        ];
        let result = todowrite(todos).await;
        assert!(result.contains("[✓]"));
    }

    // ── ask_user_question ─────────────────────────────────────────

    #[test]
    fn ask_user_question_definition_exists() {
        let defs = tool_definitions();
        assert!(defs.iter().any(|t| t.function.name == "ask_user_question"));
    }

    #[test]
    fn ask_user_question_is_not_readonly() {
        assert!(!is_readonly_tool("ask_user_question"));
    }

    #[tokio::test]
    async fn ask_user_question_execute_returns_harness_message() {
        let args = serde_json::json!({"questions": []});
        let ctx = Arc::new(ToolContext::new());
        let result = execute_tool("ask_user_question", &args, &ctx).await;
        assert!(result.contains("harness"));
    }

    #[test]
    fn ask_user_question_schema_items_are_objects_not_strings() {
        let defs = tool_definitions();
        let td = defs
            .iter()
            .find(|t| t.function.name == "ask_user_question")
            .unwrap();
        let questions = &td.function.parameters.properties["questions"];
        assert_eq!(questions["type"], "array");
        let items = &questions["items"];
        assert_eq!(
            items["type"], "object",
            "questions items must be objects, not strings!"
        );
        let required: Vec<String> = items["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(required, vec!["header", "question", "options"]);
        let props = &items["properties"];
        assert_eq!(props["header"]["type"], "string");
        assert_eq!(props["question"]["type"], "string");
        // options array
        let options = &props["options"];
        assert_eq!(options["type"], "array");
        let opt_items = &options["items"];
        assert_eq!(opt_items["type"], "object");
        let opt_required: Vec<String> = opt_items["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(opt_required, vec!["label", "description"]);
        let opt_props = &opt_items["properties"];
        assert_eq!(opt_props["label"]["type"], "string");
        assert_eq!(opt_props["description"]["type"], "string");
    }

    // ── Schema content: todowrite ─────────────────────────────────

    #[test]
    fn todowrite_schema_items_are_objects_not_strings() {
        let defs = tool_definitions();
        let td = defs
            .iter()
            .find(|t| t.function.name == "todowrite")
            .unwrap();
        let todos = &td.function.parameters.properties["todos"];
        assert_eq!(todos["type"], "array");
        let items = &todos["items"];
        assert_eq!(items["type"], "object");
        let required: Vec<String> = items["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(required, vec!["content", "status"]);
        let props = &items["properties"];
        assert_eq!(props["content"]["type"], "string");
        assert_eq!(props["status"]["type"], "string");
        let status_enum: Vec<String> = props["status"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(status_enum, vec!["pending", "in_progress", "completed"]);
    }

    // ── Schema content: apply_changes ─────────────────────────────

    #[test]
    fn apply_changes_schema_has_full_operation_properties() {
        let defs = tool_definitions();
        let ac = defs
            .iter()
            .find(|t| t.function.name == "apply_changes")
            .unwrap();
        let params = &ac.function.parameters.properties;

        // changes array
        let changes = &params["changes"];
        assert_eq!(changes["type"], "array");
        let change_items = &changes["items"];
        assert_eq!(change_items["type"], "object");
        let change_required: Vec<String> = change_items["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(change_required, vec!["path", "operations"]);

        // operations within each change
        let operations = &change_items["properties"]["operations"];
        assert_eq!(operations["type"], "array");
        let op_items = &operations["items"];
        assert_eq!(op_items["type"], "object");
        let op_required: Vec<String> = op_items["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(op_required.contains(&"kind".to_string()));
        let op_props = &op_items["properties"];
        let kind_enum: Vec<String> = op_props["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(kind_enum, vec!["replace", "insert_after", "delete"]);
        assert_eq!(op_props["old"]["type"], "string");
        assert_eq!(op_props["new"]["type"], "string");
        assert_eq!(op_props["anchor"]["type"], "string");
        assert_eq!(op_props["text"]["type"], "string");
        assert_eq!(op_props["expected_matches"]["type"], "integer");

        // dry_run
        assert_eq!(params["dry_run"]["type"], "boolean");
        assert!(!params["dry_run"]["description"]
            .as_str()
            .unwrap_or("")
            .is_empty());

        // postconditions
        let post = &params["postconditions"];
        assert_eq!(post["type"], "array");
        let post_items = &post["items"];
        assert_eq!(post_items["type"], "object");
        let post_props = &post_items["properties"];
        assert!(post_props.get("contains").is_some());
        assert!(post_props.get("does_not_contain").is_some());
        assert_eq!(post_props["contains"]["type"], "string");
        assert_eq!(post_props["does_not_contain"]["type"], "string");
    }
}
// ---------------------------------------------------------------------------
// glob — file pattern matching
// ---------------------------------------------------------------------------

/// When a user passes a pattern like `~/src/*.rs` without an explicit `path`,
/// walk up from the full pattern until we find an existing directory, then use
/// that as the search path and the last component as the filename pattern.
fn extract_dir_from_glob_pattern(pattern: &str) -> Option<(String, String)> {
    let expanded = expand_home(pattern);
    let path = std::path::Path::new(&expanded);

    // Walk up from the full path until we find an existing part
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        if parent.exists() && parent.is_dir() {
            let dir_str = parent.to_string_lossy().to_string();
            let file_pattern = current.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Use the original last component from the pattern (preserves wildcards)
            let original_file_part = pattern.rsplit('/').next().unwrap_or(file_pattern);
            // Preserve `**/` prefix if the original pattern had it before the
            // last component — the caller needs it for recursive matching.
            let has_recursive = pattern.contains("**/");
            let final_pattern = if has_recursive {
                format!("**/{}", original_file_part)
            } else {
                original_file_part.to_string()
            };
            return Some((dir_str, final_pattern));
        }
        current = parent.to_path_buf();
    }
    None
}

async fn glob_tool(pattern: String, path: Option<String>) -> String {
    // If no explicit path was given and the pattern contains '/', extract the
    // longest existing directory prefix from the pattern so that users can
    // pass e.g. "~/src/*.rs" as the pattern without a separate path argument.
    let (pattern, path) = if path.is_none() && pattern.contains('/') {
        extract_dir_from_glob_pattern(&pattern)
            .map(|(d, p)| (p, Some(d)))
            .unwrap_or((pattern, None))
    } else {
        (pattern, path)
    };

    let root = if let Some(ref p) = path {
        let expanded = expand_home(p);
        match expanded.canonicalize() {
            Ok(r) => r,
            Err(e) => return format!("Error resolving path: {e}"),
        }
    } else {
        match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => return format!("Error getting current directory: {e}"),
        }
    };

    let search_dir = if root.is_dir() {
        root.clone()
    } else {
        root.parent()
            .map(|p: &std::path::Path| p.to_path_buf())
            .unwrap_or(root.clone())
    };

    let skip_dirs: std::collections::HashSet<&str> = [
        ".git",
        ".svn",
        ".hg",
        "__pycache__",
        "node_modules",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        ".tox",
        ".eggs",
        ".venv",
        "venv",
        ".env",
        "build",
        "dist",
        "target",
    ]
    .iter()
    .cloned()
    .collect();

    // A pattern whose final component starts with "." is asking for hidden
    // entries.  Testing the whole pattern would miss "**/.config" or "src/.env",
    // since those start with "*" and "s".
    let segments: Vec<String> = pattern
        .replace('\\', "/")
        .trim_end_matches('/')
        .split('/')
        .map(String::from)
        .collect();
    let wants_hidden = segments.last().is_some_and(|s| s.starts_with('.'));
    let wants_hidden_dirs = segments[..segments.len().saturating_sub(1)]
        .iter()
        .any(|s| s.starts_with('.') && s != "." && s != "..");

    // Match the whole relative path with real glob semantics: "*" and "?"
    // stay within one path segment, "**/" spans any number of directories.
    // (Matching only the text after "**" against the file name ignored any
    // prefix, so "src/**/*.py" also returned files outside src/.)
    let has_recursive = pattern.contains("**");
    let matcher = match glob_to_regex(&pattern.replace('\\', "/")) {
        Some(r) => r,
        None => return format!("Error with glob pattern '{pattern}'"),
    };
    let max_depth = if has_recursive {
        usize::MAX
    } else {
        pattern.trim_matches('/').split('/').count()
    };

    let mut matches: Vec<(String, bool, u64)> = Vec::new();
    let walker = walkdir::WalkDir::new(&search_dir)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            } // never skip the root
            if let Some(name) = e.file_name().to_str() {
                // Hidden directories are walked only when the pattern names
                // one (".github/**/*.yml"); hidden files only when the last
                // segment asks for them ("**/.env").
                let hidden_ok = if e.file_type().is_dir() { wants_hidden_dirs } else { wants_hidden };
                if name.starts_with('.') && !hidden_ok {
                    return false;
                }
                // Only directories are pruned by the skip set: ".env" and
                // "target" are in there as *directory* names, and matching them
                // against files made a plain ".env" file unfindable.
                if e.file_type().is_dir() && skip_dirs.contains(name) {
                    return false;
                }
            }
            true
        });

    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(&search_dir).unwrap_or(entry.path());
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !matcher.is_match(&rel_str) {
            continue;
        }
        let is_dir = entry.file_type().is_dir();
        if is_dir && entry.file_name().to_string_lossy().starts_with('.') && !wants_hidden {
            continue;
        }
        let shown = entry
            .path()
            .strip_prefix(&root)
            .unwrap_or(rel)
            .to_string_lossy()
            .to_string();
        let size = if is_dir { 0 } else { entry.metadata().map(|m| m.len()).unwrap_or(0) };
        matches.push((shown, is_dir, size));
    }

    if matches.is_empty() {
        return format!(
            "No files matching '{}' in {}",
            pattern,
            search_dir.display()
        );
    }

    // Directories first, then by path.
    matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let max_results = 200;
    let mut lines: Vec<String> = Vec::new();
    for (i, (path_str, _is_dir, size)) in matches.iter().enumerate() {
        if i >= max_results {
            lines.push(format!(
                "... and {} more (truncated at {max_results})",
                matches.len() - max_results
            ));
            break;
        }
        if *_is_dir {
            lines.push(format!("{path_str}/"));
        } else {
            lines.push(format!("{path_str}  ({})", format_size(*size)));
        }
    }

    let result = lines.join("\n");
    snip_tool_output(result)
}

/// Translate a glob into an anchored regex over a `/`-separated relative path:
/// `**/` = any number of directories (including none), `**` = anything,
/// `*` = anything within one segment, `?` = one character, `{a,b}` = either.
fn glob_to_regex(glob: &str) -> Option<Regex> {
    let mut re = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    let mut in_brace = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            if chars.get(i + 2) == Some(&'/') {
                re.push_str("(?:.*/)?");
                i += 3;
            } else {
                re.push_str(".*");
                i += 2;
            }
            continue;
        }
        match c {
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '{' => {
                in_brace = true;
                re.push_str("(?:");
            }
            '}' if in_brace => {
                in_brace = false;
                re.push(')');
            }
            ',' if in_brace => re.push('|'),
            _ => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).ok()
}

// ---------------------------------------------------------------------------
// todowrite — task list management
// ---------------------------------------------------------------------------

async fn todowrite(todos: Vec<serde_json::Value>) -> String {
    if todos.is_empty() {
        return "Error: todos list is empty. Provide at least one task.".to_string();
    }

    let mut in_progress_count = 0;
    let mut errors: Vec<String> = Vec::new();

    for (i, t) in todos.iter().enumerate() {
        let obj = match t.as_object() {
            Some(o) => o,
            None => {
                errors.push(format!("Item {i}: not an object"));
                continue;
            }
        };
        let content_text = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let status = obj.get("status").and_then(|v| v.as_str()).unwrap_or("");

        if content_text.is_empty() {
            errors.push(format!("Item {i}: content is empty"));
        }
        match status {
            "pending" | "in_progress" | "completed" => {}
            _ => errors.push(format!(
                "Item {i}: invalid status '{status}' — must be pending, in_progress, or completed"
            )),
        }
        if status == "in_progress" {
            in_progress_count += 1;
        }
    }

    if !errors.is_empty() {
        return format!(
            "Error validating todos:\n{}",
            errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    if in_progress_count > 1 {
        return format!(
            "Error: {in_progress_count} tasks marked in_progress. At most one task may be in_progress at a time."
        );
    }

    let icons: std::collections::HashMap<&str, &str> = [
        ("pending", "[ ]"),
        ("in_progress", "[→]"),
        ("completed", "[✓]"),
    ]
    .iter()
    .cloned()
    .collect();

    let lines: Vec<String> = todos
        .iter()
        .map(|t| {
            let obj = t.as_object().unwrap();
            let content_text = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let status = obj.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let icon = icons.get(status).unwrap_or(&"[?]");
            format!("{icon} {content_text}")
        })
        .collect();

    lines.join("\n")
}

/// run_bash(host=...) — remote execution and remote sudo escalation.
///
/// A stub `ssh` on PATH stands in for the real client: it records its argv and
/// runs the `sh -s` wrapper locally in a separate session.  That mirrors the
/// real process topology — killing the local "ssh" (Stop/timeout) does not
/// signal the "remote" shell directly; only the closed stdin channel reaches
/// it — so the wrapper's cleanup and kill paths are exercised honestly.
/// Mirrors Python's TestRemoteRunBash — keep in sync.
#[cfg(all(test, unix))]
mod remote_tests {
    use super::tests::test_tool_timeout_guard;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Remote {
        dir: tempfile::TempDir,
        argv_log: PathBuf,
        runtime: PathBuf,
        old_path: String,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Remote {
        fn drop(&mut self) {
            std::env::set_var("PATH", &self.old_path);
        }
    }

    fn write_exec(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn remote() -> Remote {
        let guard = test_tool_timeout_guard();
        let dir = tempfile::Builder::new()
            .prefix("pengy-remote-test-")
            .tempdir()
            .unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let argv_log = dir.path().join("ssh-argv");
        let runtime = dir.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        write_exec(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" > '{}'\n\
                 export XDG_RUNTIME_DIR='{}'\n\
                 while [ \"$1\" != \"--\" ]; do shift; done\n\
                 shift 2\n\
                 exec 3<&0\n\
                 setsid \"$@\" <&3 3<&- &\n\
                 exec 3<&-\n\
                 wait \"$!\"\n",
                argv_log.display(),
                runtime.display()
            ),
        );
        // Stub sudo: succeeds only via -A with a working askpass; a password
        // of "wrong" reproduces classic sudo's failed-auth output.
        write_exec(
            &bin.join("sudo"),
            "#!/bin/bash\n\
             if [ \"$1\" != \"-A\" ]; then echo \"sudo: a terminal is required\" >&2; exit 1; fi\n\
             shift\n\
             pw=\"$(\"$SUDO_ASKPASS\")\"\n\
             if [ \"$pw\" = wrong ]; then echo \"Sorry, try again.\" >&2; \
             echo \"sudo: 3 incorrect password attempts\" >&2; exit 1; fi\n\
             echo \"pw=$pw\"\n\
             exec \"$@\"\n",
        );
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", bin.display()));
        Remote {
            dir,
            argv_log,
            runtime,
            old_path,
            _guard: guard,
        }
    }

    fn runtime_entries(r: &Remote) -> usize {
        std::fs::read_dir(&r.runtime).unwrap().count()
    }

    /// A context whose provider records each prompt's host and answers from
    /// `passwords` (default "s3cret").
    fn ctx_with(
        passwords: &[(Option<&str>, &str)],
    ) -> (Arc<ToolContext>, Arc<Mutex<Vec<Option<String>>>>) {
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let map: HashMap<Option<String>, String> = passwords
            .iter()
            .map(|(h, p)| (h.map(String::from), p.to_string()))
            .collect();
        let ctx = Arc::new(ToolContext::new());
        let log = prompts.clone();
        ctx.set_sudo_provider(Some(Box::new(move |host| {
            log.lock().unwrap().push(host.map(String::from));
            Some(
                map.get(&host.map(String::from))
                    .cloned()
                    .unwrap_or_else(|| "s3cret".into()),
            )
        })));
        (ctx, prompts)
    }

    async fn run(ctx: &Arc<ToolContext>, args: serde_json::Value) -> String {
        execute_tool("run_bash", &args, ctx).await
    }

    #[tokio::test]
    async fn host_validation_rejects_injection() {
        let r = remote();
        let (ctx, prompts) = ctx_with(&[]);
        for host in ["-oProxyCommand=touch /tmp/x", "a b", "a;b", "`id`", "$(id)", "a/b", "'q'"] {
            let result = run(
                &ctx,
                serde_json::json!({"command": "sudo true", "elevated": true, "host": host}),
            )
            .await;
            assert!(result.starts_with("Error: invalid host"), "{host} -> {result}");
        }
        assert!(prompts.lock().unwrap().is_empty());
        assert!(!r.argv_log.exists());
    }

    #[test]
    fn host_validation_accepts_ssh_destinations() {
        for host in ["web1", "pat@web1.lan", "web-1.example.com", "::1", "fe80::1%eth0"] {
            assert!(validate_host(host).is_none(), "{host}");
        }
    }

    #[tokio::test]
    async fn ssh_argv() {
        let r = remote();
        let ctx = Arc::new(ToolContext::new());
        run(&ctx, serde_json::json!({"command": "true", "host": "web1"})).await;
        let argv = std::fs::read_to_string(&r.argv_log).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(&argv[argv.len() - 4..], ["--", "web1", "sh", "-s"]);
        assert!(argv.contains(&"-T") && argv.contains(&"BatchMode=yes"));
    }

    #[tokio::test]
    async fn unelevated_remote_command() {
        let r = remote();
        let ctx = Arc::new(ToolContext::new());
        let cwd = r.dir.path().to_str().unwrap();
        let result = run(
            &ctx,
            serde_json::json!({
                "command": "pwd; echo \"askpass=${SUDO_ASKPASS:-none}\"; cat; echo stdin-closed; exit 7",
                "cwd": cwd, "host": "web1",
            }),
        )
        .await;
        assert!(result.contains(cwd), "{result}");
        assert!(result.contains("askpass=none"), "{result}");
        assert!(result.contains("stdin-closed"), "{result}");
        assert!(result.contains("[Exit code: 7]"), "{result}");
        assert_eq!(runtime_entries(&r), 0);
    }

    #[tokio::test]
    async fn remote_cwd_failure() {
        let _r = remote();
        let ctx = Arc::new(ToolContext::new());
        let result = run(
            &ctx,
            serde_json::json!({"command": "pwd", "cwd": "/nonexistent_dir_xyz", "host": "web1"}),
        )
        .await;
        assert!(result.contains("[Exit code: 126]"), "{result}");
    }

    #[tokio::test]
    async fn elevation_rules_apply_to_remote() {
        let _r = remote();
        let (ctx, prompts) = ctx_with(&[]);
        assert!(run(&ctx, serde_json::json!({"command": "sudo true", "host": "web1"}))
            .await
            .contains("Elevation required"));
        assert!(run(
            &ctx,
            serde_json::json!({"command": "echo 'sudo true'", "elevated": true, "host": "web1"})
        )
        .await
        .contains("does not invoke sudo"));
        assert!(run(
            &ctx,
            serde_json::json!({"command": "ssh web1 sudo true", "elevated": true})
        )
        .await
        .contains("does not invoke sudo"));
        assert!(prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn remote_sudo_delivers_password_without_leaking() {
        let r = remote();
        let password = "p'a$s w\"d\\x";
        let (ctx, prompts) = ctx_with(&[(Some("web1"), password)]);
        for command in [
            "sudo echo hi",
            "echo a; cat > /dev/null; sudo echo hi",
            "sudo echo one; sudo echo two",
            "echo x | sudo cat",
            "sudo -S echo hi",
        ] {
            let result = run(
                &ctx,
                serde_json::json!({"command": command, "elevated": true, "host": "web1"}),
            )
            .await;
            assert!(result.contains(&format!("pw={password}")), "{command} -> {result}");
            assert!(!result.contains("terminal is required"), "{command} -> {result}");
        }
        let result = run(
            &ctx,
            serde_json::json!({"command": "sudo env", "elevated": true, "host": "web1"}),
        )
        .await;
        // Only the stub's pw= line, never the environment.
        assert_eq!(result.matches(password).count(), 1, "{result}");
        assert_eq!(*prompts.lock().unwrap(), vec![Some("web1".to_string())]);
        assert_eq!(runtime_entries(&r), 0);
        assert!(!std::fs::read_to_string(&r.argv_log).unwrap().contains(password));
    }

    #[tokio::test]
    async fn passwords_are_cached_per_host() {
        let _r = remote();
        let (ctx, prompts) = ctx_with(&[(Some("web1"), "pw-web1"), (Some("db2"), "pw-db2")]);
        let args = |h: &str| serde_json::json!({"command": "sudo true", "elevated": true, "host": h});
        let r1 = run(&ctx, args("web1")).await;
        let r2 = run(&ctx, args("db2")).await;
        let r3 = run(&ctx, args("web1")).await;
        assert!(r1.contains("pw=pw-web1") && r2.contains("pw=pw-db2") && r3.contains("pw=pw-web1"));
        assert_eq!(
            *prompts.lock().unwrap(),
            vec![Some("web1".to_string()), Some("db2".to_string())]
        );
        ctx.clear_sudo();
        assert!(ctx.cached_sudo_passwords.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn local_prompt_is_keyed_separately() {
        let _r = remote();
        let (ctx, prompts) = ctx_with(&[(None, "local-pw"), (Some("web1"), "remote-pw")]);
        assert!(run(&ctx, serde_json::json!({"command": "sudo true", "elevated": true}))
            .await
            .contains("pw=local-pw"));
        assert!(run(
            &ctx,
            serde_json::json!({"command": "sudo true", "elevated": true, "host": "web1"})
        )
        .await
        .contains("pw=remote-pw"));
        assert_eq!(*prompts.lock().unwrap(), vec![None, Some("web1".to_string())]);
    }

    async fn auth_failure_evicts_only_that_host(host: Option<&str>) {
        let _r = remote();
        let (ctx, prompts) = ctx_with(&[(host, "wrong")]);
        ctx.cached_sudo_passwords
            .lock()
            .unwrap()
            .insert(Some("other".into()), "fine".into());
        let mut args = serde_json::json!({"command": "sudo true", "elevated": true});
        if let Some(h) = host {
            args["host"] = h.into();
        }
        let result = run(&ctx, args.clone()).await;
        assert!(result.contains("sudo authentication failed"), "{result}");
        {
            let cache = ctx.cached_sudo_passwords.lock().unwrap();
            assert_eq!(cache.len(), 1);
            assert_eq!(cache.get(&Some("other".to_string())).map(String::as_str), Some("fine"));
        }
        run(&ctx, args).await;
        // Re-prompted, not replayed.
        let want = vec![host.map(String::from), host.map(String::from)];
        assert_eq!(*prompts.lock().unwrap(), want);
    }

    #[tokio::test]
    async fn local_auth_failure_evicts_only_local() {
        auth_failure_evicts_only_that_host(None).await;
    }

    #[tokio::test]
    async fn remote_auth_failure_evicts_only_that_host() {
        auth_failure_evicts_only_that_host(Some("web1")).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_kills_remote_command_and_cleans_up() {
        let r = remote();
        let (ctx, _prompts) = ctx_with(&[]);
        let marker = r.dir.path().join("remote-pid");
        let start = Instant::now();
        let task = {
            let ctx = ctx.clone();
            let command = format!("sudo sh -c 'echo $$ > {}; exec sleep 30'", marker.display());
            tokio::spawn(async move {
                execute_tool(
                    "run_bash",
                    &serde_json::json!({"command": command, "elevated": true, "host": "web1"}),
                    &ctx,
                )
                .await
            })
        };
        for _ in 0..100 {
            if std::fs::read_to_string(&marker).map(|s| !s.trim().is_empty()).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(runtime_entries(&r) > 0, "askpass dir should exist mid-run");
        ctx.kill_all();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("run_bash did not return after Stop")
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
        let mut alive = true;
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if alive {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("remote command survived Stop");
        }
        for _ in 0..100 {
            if runtime_entries(&r) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(runtime_entries(&r), 0, "askpass dir leaked after Stop");
    }

    #[test]
    fn missing_ssh_client() {
        let empty = tempfile::tempdir().unwrap();
        assert!(find_in_path("ssh", Some(empty.path().as_os_str().to_owned())).is_none());
        assert!(find_in_path("ssh", None).is_none());
    }

    #[tokio::test]
    async fn ssh_connection_failure_hint() {
        let r = remote();
        write_exec(
            &r.dir.path().join("bin").join("ssh"),
            "#!/bin/sh\necho 'user@web1: Permission denied (publickey).' >&2\nexit 255\n",
        );
        let ctx = Arc::new(ToolContext::new());
        let result = run(&ctx, serde_json::json!({"command": "true", "host": "web1"})).await;
        assert!(result.contains("ssh to web1 failed"), "{result}");
    }

    #[tokio::test]
    async fn large_command_does_not_deadlock() {
        let _r = remote();
        let ctx = Arc::new(ToolContext::new());
        let payload = "x".repeat(100_000);
        let result = run(
            &ctx,
            serde_json::json!({"command": format!("printf %s {payload} | wc -c"), "host": "web1"}),
        )
        .await;
        assert!(result.contains("100000"), "{result}");
    }

    #[test]
    fn wrapper_parses_under_sh() {
        let script = build_remote_script("echo 'hi'\nsudo -A true", Some("a'b\"c$d\\e\nf"), Some("/tmp"));
        let mut child = std::process::Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn placeholder_text_in_values_is_not_substituted() {
        let script = build_remote_script("echo __CWD__", Some("__COMMAND__ x"), Some("/a b"));
        assert!(script.contains("\npw='__COMMAND__ x'\n"), "{script}");
        assert!(script.contains("\ncmd='echo __CWD__'\n"), "{script}");
        assert!(script.contains("\ncwd='/a b'\n"), "{script}");
    }

    #[test]
    fn shell_quote_matches_shlex() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("/tmp/a-b_c.d"), "/tmp/a-b_c.d");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    /// The wrapper must stay byte-identical across the Python, Rust and C++
    /// editions; this is the SHA-256 of Python's `_REMOTE_WRAPPER`.
    #[test]
    fn wrapper_matches_other_editions() {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(REMOTE_WRAPPER.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "75d7e71ce303efd968769a96b9eb433eb9841ba446a363b718b33d54b6ac0fca");
    }

    #[tokio::test]
    async fn real_ssh_host() {
        let Ok(host) = std::env::var("PENGY_TEST_SSH_HOST") else {
            return; // set PENGY_TEST_SSH_HOST to a key-auth ssh host
        };
        let ctx = Arc::new(ToolContext::new());
        let result = run(
            &ctx,
            serde_json::json!({
                "command": "echo remote-ok; echo \"${SUDO_ASKPASS:-none}\"; yes | head -1",
                "host": host,
            }),
        )
        .await;
        assert!(result.contains("remote-ok") && result.contains("none"), "{result}");
    }
}


#[cfg(test)]
mod powershell_tests {
    use super::tests::test_tool_timeout_guard;
    use super::*;

    fn by_name(defs: &[ToolDef], name: &str) -> FunctionDef {
        defs.iter()
            .find(|d| d.function.name == name)
            .map(|d| d.function.clone())
            .unwrap_or_else(|| panic!("no {name}"))
    }

    #[test]
    fn posix_surface_unchanged() {
        let base = serde_json::to_value(base_tool_definitions()).unwrap();
        let posix = serde_json::to_value(platform_tools(base_tool_definitions(), false, "", false)).unwrap();
        assert_eq!(base, posix);
        assert!(!base_tool_definitions().iter().any(|d| d.function.name == "run_powershell"));
        assert_eq!(by_name(&base_tool_definitions(), "run_bash").parameters.required, vec!["command"]);
    }

    #[test]
    fn windows_surface() {
        let base = base_tool_definitions();
        let win = platform_tools(base_tool_definitions(), true, "PowerShell 7", false);
        let names: Vec<&str> = win.iter().map(|d| d.function.name.as_str()).collect();
        assert_eq!(names.iter().filter(|n| **n == "run_powershell").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "run_bash").count(), 1);
        let ps = names.iter().position(|n| *n == "run_powershell").unwrap();
        assert_eq!(names[ps + 1], "run_bash");
        assert_eq!(win.len(), base.len() + 1);
        let bash = by_name(&win, "run_bash");
        assert_eq!(bash.parameters.required, vec!["command", "host"]);
        assert!(bash.description.contains("remote"));
        // No other schema may send the model to run_bash for local work.
        for d in &win {
            if !["run_bash", "run_powershell"].contains(&d.function.name.as_str()) {
                assert!(!d.function.description.contains("run_bash"), "{}", d.function.name);
            }
        }
    }

    #[test]
    fn privilege_and_dialect_wording() {
        let admin = run_powershell_definition("PowerShell 7", true).function.description;
        let user = run_powershell_definition("Windows PowerShell 5.1", false).function.description;
        assert!(admin.contains("running as Administrator") && !admin.contains("NOT running"));
        assert!(user.contains("NOT running as Administrator"));
        assert!(user.contains("Start-Process -Verb RunAs"));
        assert!(user.contains("5.1") && user.contains("&&"));
        assert!(!admin.contains("&&"));
    }

    #[test]
    fn labels() {
        assert_eq!(powershell_label(Some(Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe"))), "PowerShell 7");
        assert_eq!(powershell_label(Some(Path::new("/usr/bin/pwsh"))), "PowerShell 7");
        assert_eq!(
            powershell_label(Some(Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"))),
            "Windows PowerShell 5.1"
        );
    }

    #[test]
    fn local_run_bash_rejected_on_windows() {
        let e = windows_local_run_bash_error(true, None).unwrap();
        assert!(e.starts_with("Error") && e.contains("run_powershell"));
        assert!(windows_local_run_bash_error(true, Some("web1")).is_none());
        assert!(windows_local_run_bash_error(false, None).is_none());
    }

    #[tokio::test]
    async fn missing_executable() {
        let r = run_powershell_with(None, "Get-Date".into(), None, Arc::new(ToolContext::new())).await;
        assert!(r.contains("PowerShell was not found"), "{r}");
    }

    #[test]
    fn prelude_quoting() {
        // One argv element on Windows: embedded double quotes are the fragile part.
        assert!(!POWERSHELL_PRELUDE.contains('"'));
        assert!(powershell_prelude(r"C:\Users\o'brien\x.ps1").contains(r"'C:\Users\o''brien\x.ps1'"));
    }

    // ── Live: drive the real prelude through pwsh when it is installed ──

    fn pwsh() -> Option<PathBuf> {
        find_in_path("pwsh", std::env::var_os("PATH"))
    }

    async fn run(command: &str, cwd: Option<String>) -> Option<String> {
        let exe = pwsh()?;
        Some(run_powershell_with(Some(exe), command.into(), cwd, Arc::new(ToolContext::new())).await)
    }

    #[tokio::test]
    async fn live_unicode_multiline_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("dir with späce");
        std::fs::create_dir(&d).unwrap();
        let Some(out) = run("\"héllo ✓\"\nif ($true) {\n  (Get-Location).Path\n}", Some(d.to_string_lossy().into())).await else {
            return;
        };
        assert!(out.contains("héllo ✓"), "{out}");
        assert!(out.contains(&*d.to_string_lossy()), "{out}");
        assert!(!out.contains("[Exit code"), "{out}");
    }

    #[tokio::test]
    async fn live_no_ansi_and_exit_codes() {
        let Some(out) = run("Write-Error 'bad'", None).await else { return };
        assert!(!out.contains("\x1b[") && out.contains("bad"), "{out:?}");
        let out = run("throw \"boom\"", None).await.unwrap();
        assert!(out.contains("boom") && out.contains("[Exit code: 1]"), "{out}");
        let out = run("if ($true) {", None).await.unwrap();
        assert!(out.contains("Missing closing") && out.contains("[Exit code: 1]"), "{out}");
        let out = run("exit 7", None).await.unwrap();
        assert!(out.contains("[Exit code: 7]"), "{out}");
    }

    #[tokio::test]
    async fn live_timeout_kills_and_cleans_up() {
        if pwsh().is_none() {
            return;
        }
        let _guard = test_tool_timeout_guard();
        let old = *TOOL_TIMEOUT.lock().unwrap();
        *TOOL_TIMEOUT.lock().unwrap() = 2;
        let scripts = || -> HashSet<PathBuf> {
            std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    let n = p.file_name().unwrap().to_string_lossy();
                    n.starts_with(&format!("pengy-{}-", std::process::id())) && n.ends_with(".ps1")
                })
                .collect()
        };
        let before = scripts();
        let out = run("Start-Sleep -Seconds 30", None).await.unwrap();
        *TOOL_TIMEOUT.lock().unwrap() = old;
        assert!(out.contains("timed out"), "{out}");
        assert!(scripts().is_subset(&before));
    }
}

/// Regression tests for tool sharp edges found by the cross-edition audit;
/// mirror `tests/test_tool_hardening.py` (Python) and the C++ `hardening*` tests.
#[cfg(test)]
mod hardening_tests {
    use super::*;
    use serde_json::json;

    async fn run(name: &str, args: serde_json::Value) -> String {
        execute_tool(name, &args, &Arc::new(ToolContext::default())).await
    }

    fn replace_op(path: &Path, old: &str, new: &str) -> serde_json::Value {
        json!({"path": path.to_str().unwrap(), "operations": [{"kind": "replace", "old": old, "new": new}]})
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn per_file_dry_run_is_rejected_and_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("t.txt");
        std::fs::write(&f, "alpha\nbeta\n").unwrap();
        let mut change = replace_op(&f, "beta", "BETA");
        change["dry_run"] = json!(true);
        let r = run("apply_changes", json!({"changes": [change]})).await;
        assert!(r.contains("dry_run is a top-level argument"), "{r}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn string_true_is_a_dry_run_with_real_hunks() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("t.txt");
        std::fs::write(&f, "alpha\nbeta\n").unwrap();
        let r = run("apply_changes", json!({"changes": [replace_op(&f, "beta", "BETA")], "dry_run": "true"})).await;
        assert!(r.starts_with("Dry run: no changes applied."), "{r}");
        assert!(r.contains("@@ -1,2 +1,2 @@\n alpha\n-beta\n+BETA"), "{r}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "alpha\nbeta\n");
    }

    #[test]
    fn unified_diff_matches_difflib_shape() {
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line3\n", "LINE3\n").replace("line18\n", "");
        let d = unified_diff("f", &old, &new);
        assert_eq!(
            d,
            "--- f\n+++ f\n@@ -1,6 +1,6 @@\n line1\n line2\n-line3\n+LINE3\n line4\n line5\n line6\n\
             @@ -15,6 +15,5 @@\n line15\n line16\n line17\n-line18\n line19\n line20\n"
        );
        assert_eq!(unified_diff("f", "a", "b"), "--- f\n+++ f\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(unified_diff("f", "", "x\n"), "--- f\n+++ f\n@@ -0,0 +1 @@\n+x\n");
    }

    /// `diff_opcodes` must describe `a` -> `b` exactly: ranges contiguous from
    /// 0 to the ends, and every '=' range really equal.
    fn assert_opcodes_rebuild(a: &[&str], b: &[&str]) {
        let (mut ai, mut bj) = (0, 0);
        for (tag, i1, i2, j1, j2) in diff_opcodes(a, b) {
            assert_eq!((i1, j1), (ai, bj));
            if tag == '=' {
                assert_eq!(a[i1..i2], b[j1..j2]);
            }
            ai = i2;
            bj = j2;
        }
        assert_eq!((ai, bj), (a.len(), b.len()));
    }

    /// Two edits ~2000 lines apart: the region between them is too big for
    /// the LCS table, which used to report it as one replace — a 4000-line
    /// "diff" for a 2-line edit.  It now splits on unique lines, in any size of
    /// file.
    #[test]
    fn unified_diff_keeps_distant_edits_as_separate_hunks() {
        for (n, x, y) in [(4024usize, 359usize, 2401usize), (3000, 359, 2459), (10000, 359, 2401)] {
            let mut lines: Vec<String> = (1..=n).map(|i| format!("filler = {i}\n")).collect();
            lines[x] = "name = \"bytes\"\n".into();
            lines[x + 1] = "version = \"1.12.0\"\n".into();
            lines[y] = "name = \"rayon\"\n".into();
            lines[y + 1] = "version = \"1.12.0\"\n".into();
            let old: String = lines.concat();
            let new = old.replace("version = \"1.12.0\"", "version = \"1.12.1\"");
            let d = unified_diff("f", &old, &new);
            let hunks: Vec<&str> = d.lines().filter(|l| l.starts_with("@@")).collect();
            assert_eq!(
                hunks,
                [format!("@@ -{},7 +{},7 @@", x - 1, x - 1), format!("@@ -{},7 +{},7 @@", y - 1, y - 1)],
                "n={n}"
            );
            assert_eq!(d.lines().count(), 20, "n={n}");
        }
    }

    #[test]
    fn diff_opcodes_stay_exact_on_large_regions() {
        // Unique lines with edits scattered through a region far over the cap.
        let a: Vec<String> = (0..5000).map(|i| format!("l{i}\n")).collect();
        let mut b = a.clone();
        b.remove(1000);
        b.insert(2500, "added\n".into());
        b[4000] = "changed\n".into();
        let (a, b): (Vec<&str>, Vec<&str>) = (a.iter().map(|x| x.as_str()).collect(), b.iter().map(|x| x.as_str()).collect());
        assert_opcodes_rebuild(&a, &b);
        let d = unified_diff("f", &a.concat(), &b.concat());
        assert_eq!(d.lines().filter(|l| l.starts_with("@@")).count(), 3, "{d}");
        // No line unique on both sides: still a correct (if coarse) diff.
        let a: Vec<&str> = std::iter::repeat_n("x\n", 2500).chain(["a\n"]).chain(std::iter::repeat_n("x\n", 2500)).collect();
        let b: Vec<&str> = std::iter::repeat_n("x\n", 2500).chain(["b\n"]).chain(std::iter::repeat_n("y\n", 2500)).collect();
        assert_opcodes_rebuild(&a, &b);
    }

    #[tokio::test]
    async fn failed_does_not_contain_shows_the_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("t.py");
        std::fs::write(&f, "import httpx\nimport pytest\n\ndef helper():\n    \"\"\"No longer builds an httpx.Request.\"\"\"\n    # httpx.Request was the old way\n").unwrap();
        let r = run(
            "apply_changes",
            json!({
                "changes": [replace_op(&f, "import httpx\nimport pytest", "import pytest")],
                "postconditions": [{"path": f.to_str().unwrap(), "does_not_contain": "httpx.Request"}]
            }),
        )
        .await;
        assert!(
            r.contains("still contains forbidden text at line 4: \"\"\"No longer builds an httpx.Request.\"\"\" (+1 more)"),
            "{r}"
        );
        assert!(std::fs::read_to_string(&f).unwrap().starts_with("import httpx\n"));
    }

    #[test]
    fn forbidden_text_location_windows_a_long_line() {
        let line = format!("{}NEEDLE{}", "a".repeat(300), "b".repeat(300));
        let at = forbidden_text_location(&format!("first\n  {line}\n"), "NEEDLE").unwrap();
        assert_eq!(at, format!("at line 2: ...{}NEEDLE{}...", "a".repeat(60), "b".repeat(94)));
        // Near the end the window still shows 160 characters.
        let at = forbidden_text_location(&format!("{}NEEDLE", "a".repeat(300)), "NEEDLE").unwrap();
        assert_eq!(at, format!("at line 1: ...{}NEEDLE", "a".repeat(154)));
        assert_eq!(forbidden_text_location("abc", "x"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_stage_writes_nothing_anywhere() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, "ORIGINAL\n").unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let b = locked.join("b.txt");
        std::fs::write(&b, "ORIGINAL\n").unwrap();
        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = run("apply_changes", json!({"changes": [replace_op(&a, "ORIGINAL", "NEW"), replace_op(&b, "ORIGINAL", "NEW")]})).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(r.starts_with("Error: write failed; no changes applied"), "{r}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ORIGINAL\n");
        assert_eq!(names(dir.path()), vec!["a.txt", "locked"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edits_keep_permissions_symlinks_and_crlf() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for (tool, args) in [
            ("write_file", json!({"content": "echo NEW\n"})),
            ("replace_in_file", json!({"old_str": "hi", "new_str": "NEW"})),
            ("apply_changes", json!(null)),
        ] {
            let f = dir.path().join(format!("{tool}.sh"));
            std::fs::write(&f, "echo hi\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o750)).unwrap();
            let link = dir.path().join(format!("{tool}.link"));
            std::os::unix::fs::symlink(&f, &link).unwrap();
            let args = if tool == "apply_changes" {
                json!({"changes": [replace_op(&link, "hi", "NEW")]})
            } else {
                let mut a = args.clone();
                a["path"] = json!(link.to_str().unwrap());
                a
            };
            let r = run(tool, args).await;
            assert!(!r.starts_with("Error"), "{tool}: {r}");
            assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "{tool}");
            assert!(std::fs::read_to_string(&f).unwrap().contains("NEW"), "{tool}");
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o750, "{tool}");
        }
        for tool in ["replace_in_file", "apply_changes"] {
            let f = dir.path().join(format!("{tool}.txt"));
            std::fs::write(&f, b"one\r\ntwo\r\nthree\r\n").unwrap();
            let args = if tool == "apply_changes" {
                json!({"changes": [replace_op(&f, "one\ntwo", "ONE\nTWO")]})
            } else {
                json!({"path": f.to_str().unwrap(), "old_str": "one\ntwo", "new_str": "ONE\nTWO"})
            };
            let r = run(tool, args).await;
            assert!(!r.starts_with("Error"), "{tool}: {r}");
            assert_eq!(std::fs::read(&f).unwrap(), b"ONE\r\nTWO\r\nthree\r\n", "{tool}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_only_file_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt;
        for tool in ["write_file", "replace_in_file", "apply_changes"] {
            let dir = tempfile::tempdir().unwrap();
            let f = dir.path().join("locked.txt");
            std::fs::write(&f, "ORIGINAL\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
            let p = f.to_str().unwrap();
            let args = match tool {
                "write_file" => json!({"path": p, "content": "NEW"}),
                "replace_in_file" => json!({"path": p, "old_str": "ORIGINAL", "new_str": "NEW"}),
                _ => json!({"changes": [replace_op(&f, "ORIGINAL", "NEW")]}),
            };
            let r = run(tool, args).await;
            assert!(r.contains("Permission denied") || r.contains("permission denied"), "{tool}: {r}");
            assert_eq!(std::fs::read_to_string(&f).unwrap(), "ORIGINAL\n", "{tool}");
            assert_eq!(names(dir.path()), vec!["locked.txt"], "{tool}");
        }
    }

    fn spawn_status_server(status: &'static str, body_len: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let header = format!("HTTP/1.1 {status}\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n");
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&vec![b'x'; body_len]);
            }
        });
        format!("http://{addr}/file")
    }

    #[tokio::test]
    async fn failed_download_keeps_existing_file() {
        for (url, extra) in [
            (spawn_status_server("404 Not Found", 0), json!(null)),
            (spawn_status_server("200 OK", 2 * 1024 * 1024), json!(1)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("keep.bin"), "PRECIOUS").unwrap();
            let mut args = json!({"url": url, "dir": dir.path().to_str().unwrap(), "filename": "keep.bin"});
            if !extra.is_null() {
                args["max_size_mb"] = extra;
            }
            let r = run("download_file", args).await;
            assert!(r.starts_with("Error"), "{r}");
            assert_eq!(std::fs::read_to_string(dir.path().join("keep.bin")).unwrap(), "PRECIOUS");
            assert_eq!(names(dir.path()), vec!["keep.bin"]);
        }
    }

    #[tokio::test]
    async fn search_covers_all_text_files_and_reports_skips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["Program.cs", "app.vue", "main.tf", "deploy", "server.log", "data.csv"] {
            std::fs::write(root.join(name), "needle\n").unwrap();
        }
        std::fs::write(root.join("blob.dat"), b"needle\x00\x01").unwrap();
        let r = run("search_content", json!({"pattern": "needle", "path": root.to_str().unwrap()})).await;
        for name in ["Program.cs", "app.vue", "main.tf", "deploy", "server.log", "data.csv"] {
            assert!(r.contains(name), "{name}: {r}");
        }
        assert!(!r.contains("blob.dat"), "{r}");
        let r = run("search_content", json!({"pattern": "absent", "path": root.to_str().unwrap()})).await;
        assert!(r.contains("(searched 6 files; skipped 1 binary)"), "{r}");
        let r = run("search_content", json!({"pattern": "needle", "path": root.to_str().unwrap(), "file_glob": "*.rs"})).await;
        assert!(r.contains("No file matched file_glob '*.rs'"), "{r}");
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "needle\n").unwrap();
        std::fs::write(root.join("b.py"), "needle\n").unwrap();
        let r = run("search_content", json!({"pattern": "needle", "path": root.to_str().unwrap(), "file_glob": "src/*.py"})).await;
        assert!(r.contains("src/a.py") && !r.contains("b.py:"), "{r}");
    }

    #[tokio::test]
    async fn search_hidden_dirs_but_not_noise_dirs() {
        let dir = tempfile::tempdir().unwrap();
        for rel in [".github/workflows/ci.yml", ".env", "app/auth.py", ".git/config", "node_modules/x/index.js", ".venv/lib/site.py"] {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "needle\n").unwrap();
        }
        let r = run("search_content", json!({"pattern": "needle", "path": dir.path().to_str().unwrap()})).await;
        for rel in [".github/workflows/ci.yml", ".env", "app/auth.py"] {
            assert!(r.contains(rel), "{rel}: {r}");
        }
        for rel in [".git/", "node_modules", ".venv"] {
            assert!(!r.contains(rel), "{rel}: {r}");
        }
    }

    #[tokio::test]
    async fn search_root_named_like_noise_is_searched() {
        let dir = tempfile::tempdir().unwrap();
        for name in [".config", "build", "target"] {
            let sub = dir.path().join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("f.txt"), "needle\n").unwrap();
            let r = run("search_content", json!({"pattern": "needle", "path": sub.to_str().unwrap()})).await;
            assert!(r.contains("Found 1 match"), "{name}: {r}");
        }
    }

    #[tokio::test]
    async fn replace_all_and_ignore_case() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("h.py");
        std::fs::write(&f, "client_v1.a()\nclient_v1.b()\n").unwrap();
        let p = f.to_str().unwrap();
        let r = run("replace_in_file", json!({"path": p, "old_str": "client_v1", "new_str": "client_v2"})).await;
        assert!(r.contains("set replace_all=true"), "{r}");
        let r = run("replace_in_file", json!({"path": p, "old_str": "client_v1", "new_str": "client_v2", "replace_all": true})).await;
        assert_eq!(r, format!("✅ Replaced 2 occurrence(s) in {p}."));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "client_v2.a()\nclient_v2.b()\n");
        std::fs::write(dir.path().join("a.txt"), "Needle\n").unwrap();
        let d = dir.path().to_str().unwrap();
        let r = run("search_content", json!({"pattern": "needle", "path": d, "file_glob": "*.txt"})).await;
        assert!(r.contains("No matches"), "{r}");
        let r = run("search_content", json!({"pattern": "needle", "path": d, "file_glob": "*.txt", "ignore_case": true})).await;
        assert!(r.contains("Found 1 match"), "{r}");
    }

    #[tokio::test]
    async fn glob_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join(".hidden/x.txt"), "x").unwrap();
        std::fs::write(root.join("y.txt"), "y").unwrap();
        std::fs::write(root.join("src/sub/a.py"), "a").unwrap();
        std::fs::write(root.join("b.py"), "b").unwrap();
        let p = root.to_str().unwrap();
        let r = run("glob", json!({"pattern": "**/*.txt", "path": p})).await;
        assert!(r.contains("y.txt") && !r.contains(".hidden"), "{r}");
        let r = run("glob", json!({"pattern": ".hidden/*.txt", "path": p})).await;
        assert!(r.contains("x.txt"), "{r}");
        let r = run("glob", json!({"pattern": "src/**/*.py", "path": p})).await;
        assert!(r.contains("src/sub/a.py") && !r.contains("b.py"), "{r}");
        let r = run("glob", json!({"pattern": "*.{py,txt}", "path": p})).await;
        assert!(r.contains("b.py") && r.contains("y.txt"), "{r}");
    }
}
