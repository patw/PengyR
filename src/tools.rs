//! Tool definitions and execution for Pengy.
//!
//! Defines 14 OpenAI function-calling tools and their implementations.

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
pub static USER_AGENT: Mutex<String> = Mutex::new(String::new());

/// A blocking callback that prompts the user for a sudo password.
/// Returns the password, or `None` if the user cancels.
pub type SudoProvider = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// Per-run tool state: sudo provider, cached sudo password, and the set of
/// active subprocess groups.
///
/// Each concurrent run (e.g. one per GUI tab) gets its own context so a sudo
/// prompt is routed to the right run and pressing Stop on one run kills only
/// that run's subprocesses — never another tab's.  Shared as `Arc<ToolContext>`
/// so it can be cloned into `spawn_blocking` closures and across the FFI.
pub struct ToolContext {
    sudo_provider: Mutex<Option<SudoProvider>>,
    cached_sudo_password: Mutex<Option<String>>,
    active_process_groups: Mutex<HashSet<u32>>,
    /// Set by `pengy_llm_cancel` so the LLM loop aborts at the next yield point.
    pub cancelled: Arc<AtomicBool>,
}

impl ToolContext {
    pub fn new() -> Self {
        Self {
            sudo_provider: Mutex::new(None),
            cached_sudo_password: Mutex::new(None),
            active_process_groups: Mutex::new(HashSet::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Install (or clear) the sudo provider; clears any cached password.
    pub fn set_sudo_provider(&self, provider: Option<SudoProvider>) {
        *self.sudo_provider.lock().unwrap() = provider;
        *self.cached_sudo_password.lock().unwrap() = None;
    }

    pub fn clear_sudo(&self) {
        *self.cached_sudo_password.lock().unwrap() = None;
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

pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        td("read_file", "Read the contents of a file",
            &[("path", "string", "The file path to read")],
            &["path"]),
        td("write_file", "Write content to a file",
            &[("path", "string", "The file path to write to"),
              ("content", "string", "The content to write to the file")],
            &["path", "content"]),
        td("replace_in_file", "Perform an exact string replacement in an existing file. The old_str must match exactly one occurrence — if zero or multiple matches are found, the edit is rejected.",
            &[("path", "string", "The file path to edit"),
              ("old_str", "string", "The exact text to find and replace. Must match exactly one location."),
              ("new_str", "string", "The text to replace it with. Use empty string to delete.")],
            &["path", "old_str", "new_str"]),
        apply_changes_definition(),
        td("run_bash", "Run a bash command in the terminal",
            &[("command", "string", "The bash command to execute")],
            &["command"]),
        td("web_search", "Search the web using native Rust metasearch backends (Brave, DuckDuckGo, Mojeek, Yahoo, Google, Startpage, Yandex)",
            &[("query", "string", "The search query"),
              ("max_results", "integer", "Maximum number of results to return (default: 5)")],
            &["query"]),
        td("download_file", "Download a file from a URL to the user's Downloads directory",
            &[("url", "string", "The URL of the file to download"),
              ("filename", "string", "Optional filename to save as")],
            &["url"]),
        td("fetch_url", "Fetch the text content of a URL into the context window",
            &[("url", "string", "The URL to fetch")],
            &["url"]),
        td("run_python", "Execute Python code",
            &[("code", "string", "The Python code to execute")],
            &["code"]),
        td("directory_tree", "Show a visual tree of the directory structure. Skips common noise directories like .git, node_modules, __pycache__ by default.",
            &[("path", "string", "The directory path to show the tree for"),
              ("max_depth", "integer", "Maximum depth to recurse (default: 3)"),
              ("show_hidden", "boolean", "Whether to show hidden files/directories (default: false)")],
            &["path"]),
        td("read_multiple_files", "Read multiple files at once, returning each with a clear header.",
            &[("paths", "array", "List of file paths to read")],
            &["paths"]),
        td("search_content", "Search for a regex pattern in files under a directory. Returns matching lines with file path, line number, and optional surrounding context.",
            &[("pattern", "string", "The regex pattern to search for"),
              ("path", "string", "The directory or file to search in"),
              ("file_glob", "string", "Optional glob to filter files"),
              ("context_lines", "integer", "Number of lines of context (default: 0)"),
              ("max_results", "integer", "Maximum number of matches to return (default: 50)")],
            &["pattern", "path"]),
        td("glob", "Find files matching a glob pattern. Returns sorted file paths with sizes. Use ** for recursive search. Prefer this over run_bash('find ...') or run_bash('ls ...').",
            &[("pattern", "string", "The glob pattern to match against file paths"),
              ("path", "string", "The directory to search in (default: current working directory)")],
            &["pattern"]),
        todowrite_definition(),
        ask_user_question_definition(),
    ]
}

fn apply_changes_definition() -> ToolDef {
    ToolDef { tool_type: "function".into(), function: FunctionDef {
        name: "apply_changes".into(),
        description: "Apply bounded transactional exact-text edits across files. Validate all operations in memory first; if any operation fails, no files are changed. Use dry_run to preview the unified diff.".into(),
        parameters: ParametersDef {
            param_type: "object".into(),
            properties: serde_json::json!({
                "changes": {
                    "type": "array",
                    "description": "Files and exact-text operations to apply",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File path to edit"},
                            "operations": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "kind": {"type": "string", "enum": ["replace", "insert_after", "delete"]},
                                        "old": {"type": "string", "description": "Exact text to match for replace/delete"},
                                        "anchor": {"type": "string", "description": "Exact text after which to insert"},
                                        "new": {"type": "string", "description": "Replacement text"},
                                        "text": {"type": "string", "description": "Text to insert"},
                                        "expected_matches": {"type": "integer", "description": "Expected exact match count; defaults to 1"}
                                    },
                                    "required": ["kind"]
                                }
                            }
                        },
                        "required": ["path", "operations"]
                    }
                },
                "dry_run": {"type": "boolean", "description": "Validate and return a diff without writing files (default: false)"},
                "postconditions": {
                    "type": "array",
                    "description": "Optional content checks evaluated before writing",
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
        description: "Create and update a structured task list for tracking progress during complex multi-step operations. Send the COMPLETE list every time — do not send incremental updates. Exactly one task must be in_progress at any time. Mark tasks completed immediately after finishing them. Use imperative forms for content (e.g. 'Run tests', 'Add JWT middleware').".into(),
        parameters: ParametersDef {
            param_type: "object".into(),
            properties: serde_json::json!({
                "todos": {
                    "type": "array",
                    "description": "The complete list of tasks with their current statuses",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Imperative task description, e.g. 'Run the tests'"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Current task status — exactly one task must be in_progress"}
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
        description: "Ask the user one or more multiple-choice questions to clarify requirements or resolve ambiguity.".into(),
        parameters: ParametersDef {
            param_type: "object".into(),
            properties: serde_json::json!({
                "questions": {
                    "type": "array",
                    "description": "One or more questions, each with a header, question text, and list of options",
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {"type": "string", "description": "Short label for the question group (e.g. 'Theme', 'Output Format')"},
                            "question": {"type": "string", "description": "The question text to display to the user"},
                            "options": {
                                "type": "array",
                                "description": "List of answer choices for this question",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "description": "Short answer label (e.g. 'Dark', 'JSON')"},
                                        "description": {"type": "string", "description": "Brief explanation of what this option means"}
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

async fn execute_tool_inner(
    name: &str,
    arguments: &serde_json::Value,
    ctx: &Arc<ToolContext>,
) -> String {
    match name {
        "read_file" => read_file(a(arguments, "path", "")).await,
        "write_file" => write_file(a(arguments, "path", ""), a(arguments, "content", "")).await,
        "replace_in_file" => {
            replace_in_file(
                a(arguments, "path", ""),
                a(arguments, "old_str", ""),
                a(arguments, "new_str", ""),
            ).await
        }
        "apply_changes" => apply_changes(arguments).await,
        "run_bash" => run_bash(a(arguments, "command", ""), ctx.clone()).await,
        "web_search" => {
            web_search(a(arguments, "query", ""), aus(arguments, "max_results", 5)).await
        }
        "download_file" => {
            download_file(a(arguments, "url", ""), aopt(arguments, "filename")).await
        }
        "fetch_url" => fetch_url(a(arguments, "url", "")).await,
        "run_python" => run_python(a(arguments, "code", ""), ctx.clone()).await,
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
        "ask_user_question" => "ask_user_question must be handled by the harness — it should never reach execute_tool directly.".to_string(),
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
            let count = current.matches(needle).count();
            if count != expected {
                errors.push(format!(
                    "{raw} operation {oi}: matches {count} locations; expected {expected}"
                ));
                continue;
            }
            let repl = if kind == "insert_after" {
                format!("{needle}{replacement}")
            } else {
                replacement.to_string()
            };
            current = current.replacen(needle, &repl, expected);
        }
        result_bytes += original.len() + current.len();
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
                if content.contains(x) {
                    errors.push(format!(
                        "postcondition {i}: {raw} still contains forbidden text"
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
    for (path, (old, new)) in &prepared {
        if old != new {
            diff.push_str(&format!("--- {}\n+++ {}\n", path.display(), path.display()));
            diff.push_str(&format!(
                "@@ changed content: {} -> {} bytes @@\n",
                old.len(),
                new.len()
            ));
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
    let mut temps = Vec::new();
    for (path, (_, new)) in &prepared {
        let tmp = path.with_file_name(format!(
            ".{}.pengy-tmp-{}",
            path.file_name().unwrap().to_string_lossy(),
            std::process::id()
        ));
        if let Err(e) = std::fs::write(&tmp, new) {
            for t in temps {
                let _ = std::fs::remove_file(t);
            }
            return format!("Error: write failed; no changes applied: {e}");
        }
        temps.push(tmp);
    }
    for (i, (path, _)) in prepared.iter().enumerate() {
        if let Err(e) = std::fs::rename(&temps[i], path) {
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
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
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

/// If `text` exceeds the configured `TOOL_OUTPUT_MAX_CHARS`, keep head+tail
/// and snip the middle.  0 = no limit.
fn snip_tool_output(text: String) -> String {
    let limit = *TOOL_OUTPUT_MAX_CHARS.lock().unwrap();
    if limit == 0 || text.len() <= limit {
        return text;
    }
    let head_chars = (limit / 5).max(500);
    let tail_chars = limit - head_chars;

    // Find char-boundary cut points
    let head = truncate_on_char_boundary(&text, head_chars);
    let tail_start = text.len().saturating_sub(tail_chars);
    // Back up to nearest char boundary
    let tail_start = {
        let mut pos = tail_start;
        while pos > 0 && !text.is_char_boundary(pos) {
            pos -= 1;
        }
        pos
    };
    let tail = &text[tail_start..];

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

async fn read_file(path: String) -> String {
    let p = expand_home(&path);
    match std::fs::read_to_string(&p) {
        Ok(c) => snip_tool_output(c),
        Err(e) => {
            if !p.exists() {
                format!("Error: File not found: {path}")
            } else if !p.is_file() {
                format!("Error: Not a file: {path}")
            } else {
                format!("Error reading file: {e}")
            }
        }
    }
}

async fn write_file(path: String, content: String) -> String {
    let p = expand_home(&path);
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("Error creating directory: {e}");
        }
    }
    match std::fs::write(&p, &content) {
        Ok(_) => format!("Successfully wrote to {path}"),
        Err(e) => format!("Error writing file: {e}"),
    }
}

async fn replace_in_file(path: String, old_str: String, new_str: String) -> String {
    let p = expand_home(&path);
    if old_str.is_empty() {
        return "Error: old_str is empty. You must provide the exact text to replace.".into();
    }
    let content = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(_) => {
            return if !p.exists() {
                format!("Error: File not found: {path}")
            } else {
                format!("Error: Not a file: {path}")
            };
        }
    };
    let count = content.matches(&old_str).count();
    if count == 0 {
        return format!(
            "Error: old_str not found in {path}.\n\n\
             Tip: read the file first to get the exact text."
        );
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
             Make old_str longer or more specific."
        );
    }
    let new_content = content.replacen(&old_str, &new_str, 1);
    if let Err(e) = std::fs::write(&p, &new_content) {
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

/// Rewrite the first word-boundary `sudo` that isn't already followed by
/// `-S` into `sudo -S`. Mirrors the reference's
/// `re.sub(r'\bsudo\b(?!\s+-S)', 'sudo -S', command, count=1)`.
static SUDO_WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bsudo\b").unwrap());
static SUDO_DASH_S_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+-S").unwrap());
static SUDO_PROMPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[sudo[^]]*\].*\n?").unwrap());

fn rewrite_first_sudo(command: &str) -> String {
    for m in SUDO_WORD_RE.find_iter(command) {
        if !SUDO_DASH_S_RE.is_match(&command[m.end()..]) {
            return format!("{}sudo -S{}", &command[..m.start()], &command[m.end()..]);
        }
    }
    command.to_string()
}

async fn run_bash(command: String, ctx: Arc<ToolContext>) -> String {
    let timeout = timeout_secs();

    let password_needed = SUDO_WORD_RE.is_match(&command);
    if password_needed {
        let need_pw = { ctx.cached_sudo_password.lock().unwrap().is_none() };
        if need_pw {
            let provider = ctx.sudo_provider.lock().unwrap().take();
            let pw = match provider {
                Some(cb) => {
                    let result = cb();
                    *ctx.sudo_provider.lock().unwrap() = Some(cb);
                    result
                }
                None => {
                    return "Error: sudo detected but no password provider is configured.".into()
                }
            };
            match pw {
                Some(p) => {
                    *ctx.cached_sudo_password.lock().unwrap() = Some(p);
                }
                None => return "Cancelled: sudo password not provided.".into(),
            }
        }
    }

    // Ensure privileged commands read password from stdin. Rewrite only the
    // first `sudo` that isn't already followed by `-S` (word-boundary match,
    // not a substring match, so `sudoku`/`pseudo-tty` are left untouched;
    // and only one occurrence, since the piped password is consumed once —
    // rewriting every `sudo` would leave later invocations blocked on an
    // interactive prompt). The `regex` crate has no lookahead support, so
    // this is done as a manual scan instead of a single regex substitution.
    let command = rewrite_first_sudo(&command);

    let (stdout_path, stderr_path, stdout_file, stderr_file) = match create_output_files("bash") {
        Ok(files) => files,
        Err(e) => return format!("Error creating output files: {e}"),
    };

    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c").arg(&command);
    cmd.stdout(Stdio::from(stdout_file));
    cmd.stderr(Stdio::from(stderr_file));
    cmd.stdin(Stdio::piped());
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

    if password_needed {
        let pw_guard = ctx.cached_sudo_password.lock().unwrap();
        if let Some(ref pw) = *pw_guard {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = writeln!(stdin, "{pw}");
            }
        }
    }

    // Use tokio::task::spawn_blocking to avoid blocking the async runtime.
    // Output is redirected to files instead of piped: a child that writes more
    // than an OS pipe buffer can otherwise block forever before it exits.
    let ctx_blocking = ctx.clone();
    let result = tokio::task::spawn_blocking(move || {
        let wait_result = if timeout > 0 {
            match wait_timeout_status(&mut child, Duration::from_secs(timeout)) {
                Ok(Some(status)) => Ok(status),
                Ok(None) => {
                    // Timed out — kill the process group
                    terminate_process_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(format!("Error: Command timed out after {timeout} seconds"))
                }
                Err(e) => Err(format!("Error running command: {e}")),
            }
        } else {
            child
                .wait()
                .map_err(|e| format!("Error running command: {e}"))
        };
        ctx_blocking.unregister_process(pid);

        let mut out = read_and_remove(&stdout_path);
        let err = read_and_remove(&stderr_path);
        wait_result.map(|status| {
            let err = SUDO_PROMPT_RE.replace_all(&err, "").to_string();
            if !err.is_empty() {
                out.push('\n');
                out.push_str(&err);
            }
            if !status.success() {
                out.push_str(&format!("\n[Exit code: {}]", status.code().unwrap_or(-1)));
            }
            if out.is_empty() {
                "(No output)".into()
            } else {
                snip_tool_output(out)
            }
        })
    })
    .await;

    match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => e,
        Err(join_err) => format!("Error: Task panicked: {join_err}"),
    }
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
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    text
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

async fn download_file(url: String, filename: Option<String>) -> String {
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

    let mut downloads = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    downloads.push("Downloads");
    let _ = std::fs::create_dir_all(&downloads);

    let fname = filename
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            parsed
                .path_segments()
                .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
                .unwrap_or("download")
                .to_string()
        });
    let dest = downloads.join(&fname);

    let mut builder = reqwest::Client::builder().user_agent(ua());
    let timeout = timeout_secs();
    if timeout > 0 {
        builder = builder.timeout(Duration::from_secs(timeout));
    }
    let client = builder.build().unwrap_or_default();

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error downloading file: {e}"),
    };
    if !resp.status().is_success() {
        return format!("Error downloading file: HTTP {}", resp.status());
    }

    let max_size: usize = 100 * 1024 * 1024;
    let mut total: usize = 0;
    let mut out = match std::fs::File::create(&dest) {
        Ok(f) => f,
        Err(e) => return format!("Error writing file: {e}"),
    };

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                return format!("Error downloading: {e}");
            }
        };
        total += chunk.len();
        if total > max_size {
            let _ = std::fs::remove_file(&dest);
            return format!("Error: Download exceeds maximum size of {max_size} bytes.");
        }
        if let Err(e) = out.write_all(&chunk) {
            let _ = std::fs::remove_file(&dest);
            return format!("Error writing file: {e}");
        }
    }

    format!("Downloaded to {} ({total} bytes)", dest.display())
}

async fn fetch_url(url_str: String) -> String {
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

    if text.len() > 250_000 {
        format!(
            "{}\n\n[... truncated at 250,000 characters ...]",
            truncate_on_char_boundary(&text, 250_000)
        )
    } else {
        text
    }
}

async fn run_python(code: String, ctx: Arc<ToolContext>) -> String {
    let timeout = timeout_secs();
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
    cmd.arg(&tmp)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
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
                Ok(Some(status)) => Ok(status),
                Ok(None) => {
                    terminate_process_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(format!(
                        "Error: Python execution timed out after {timeout} seconds"
                    ))
                }
                Err(e) => Err(format!("Error running Python: {e}")),
            }
        } else {
            child
                .wait()
                .map_err(|e| format!("Error running Python: {e}"))
        };
        ctx_blocking.unregister_process(pid);

        let mut s = read_and_remove(&stdout_path);
        let err = read_and_remove(&stderr_path);
        wait_result.map(|status| {
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
    const MAX_PER_FILE: usize = 250_000;
    const MAX_TOTAL: usize = 1_250_000; // 5× the global tool output limit

    if paths.is_empty() {
        return "Error: no paths provided.".into();
    }
    if paths.len() > MAX_FILES {
        return format!(
            "Error: too many files ({}). Maximum is {MAX_FILES}.",
            paths.len()
        );
    }

    let mut parts: Vec<String> = Vec::new();
    let mut total_chars = 0;
    let mut errors = 0;

    for raw_path in &paths {
        let p = expand_home(raw_path);
        let sep = "=".repeat(60);
        let header = format!("{sep}\n📄 {raw_path}");

        if !p.exists() {
            parts.push(format!("{header}\n  ❌ File not found."));
            errors += 1;
            continue;
        }
        if !p.is_file() {
            parts.push(format!("{header}\n  ❌ Not a file."));
            errors += 1;
            continue;
        }

        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(e) => {
                parts.push(format!("{header}\n  ❌ Error reading file: {e}"));
                errors += 1;
                continue;
            }
        };

        let content = if content.len() > MAX_PER_FILE {
            let truncated = truncate_on_char_boundary(&content, MAX_PER_FILE);
            let fsize = p.metadata().map(|m| m.len()).unwrap_or(0);
            format!(
                "{truncated}\n\n[... truncated at {MAX_PER_FILE} characters \
                 (full file is {fsize} bytes) ...]"
            )
        } else {
            content
        };

        let block = format!("{header}\n{content}");
        if total_chars + block.len() > MAX_TOTAL {
            let remaining = MAX_TOTAL - total_chars;
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

    if errors == paths.len() {
        parts.join("\n\n")
    } else {
        parts.join("\n\n")
    }
}

async fn search_content(
    pattern: String,
    path: String,
    file_glob: Option<String>,
    context_lines: usize,
    max_results: usize,
) -> String {
    let root = expand_home(&path);
    if !root.exists() {
        return format!("Error: Path not found: {path}");
    }

    let compiled = match Regex::new(&pattern) {
        Ok(r) => r,
        Err(_) => match Regex::new(&regex::escape(&pattern)) {
            Ok(r) => r,
            Err(e) => return format!("Error: Invalid regex pattern: {e}"),
        },
    };

    let context_lines = context_lines.min(10);
    let max_results = max_results.clamp(1, 200);

    let mut results: Vec<String> = Vec::new();
    let mut files_searched = 0;
    let mut files_skipped = 0;
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

    let walker = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_entry(|e| !should_skip_dir(e));

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
            if !matches_glob(fname, glob) {
                continue;
            }
        }
        if !is_likely_text(entry.path()) {
            files_skipped += 1;
            continue;
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
        let mut summary = format!("No matches found for '{pattern}' in {path}");
        if files_searched > 0 {
            summary.push_str(&format!(" (searched {files_searched} files"));
            if files_skipped > 0 {
                summary.push_str(&format!(
                    ", skipped {files_skipped} binary/non-matching files"
                ));
            }
            summary.push(')');
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
    format!("{summary}\n{}\n{out}", "─".repeat(60))
}

fn search_one_file(
    filepath: &Path,
    compiled: &Regex,
    context_lines: usize,
    results: &mut Vec<String>,
    max_results: usize,
    root: Option<&Path>,
) -> bool {
    let content = match std::fs::read_to_string(filepath) {
        Ok(c) => c,
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

fn should_skip_dir(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|name| {
            name.starts_with('.') || name.ends_with(".egg-info") || ALWAYS_SKIP_DIRS.contains(name)
        })
        .unwrap_or(false)
}

static ALWAYS_SKIP_FILES: Lazy<HashSet<&str>> =
    Lazy::new(|| [".DS_Store", "Thumbs.db"].iter().copied().collect());

fn is_likely_text(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        let lower = ext.to_lowercase();
        let text_exts = [
            "py",
            "pyi",
            "pyx",
            "c",
            "cpp",
            "cc",
            "cxx",
            "h",
            "hpp",
            "hxx",
            "rs",
            "go",
            "java",
            "kt",
            "scala",
            "swift",
            "js",
            "jsx",
            "ts",
            "tsx",
            "mjs",
            "cjs",
            "rb",
            "rake",
            "php",
            "pl",
            "pm",
            "sh",
            "bash",
            "zsh",
            "fish",
            "html",
            "htm",
            "css",
            "scss",
            "sass",
            "less",
            "json",
            "yaml",
            "yml",
            "toml",
            "ini",
            "cfg",
            "conf",
            "xml",
            "svg",
            "rss",
            "md",
            "markdown",
            "rst",
            "txt",
            "tex",
            "sql",
            "r",
            "jl",
            "lua",
            "zig",
            "nim",
            "ex",
            "exs",
            "cmake",
            "make",
            "mk",
            "dockerfile",
            "env",
            "gitignore",
            "editorconfig",
        ];
        if text_exts.contains(&lower.as_str()) {
            return true;
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let text_files = [
            "makefile",
            "dockerfile",
            "license",
            "changelog",
            "authors",
            "todo",
        ];
        if text_files.contains(&name.to_lowercase().as_str()) {
            return true;
        }
    }
    false
}

static GLOB_BRACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*)\{([^}]+)\}(.*)$").unwrap());

// Cache compiled glob regexes — the glob is constant for an entire
// search_content call, so this avoids recompiling per file.
static GLOB_CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Regex>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

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

    fn test_tool_timeout_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_TOOL_TIMEOUT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // ── rewrite_first_sudo ──────────────────────────────────────────

    #[test]
    fn sudo_rewrite_plain_command() {
        assert_eq!(rewrite_first_sudo("sudo apt update"), "sudo -S apt update");
    }

    #[test]
    fn sudo_rewrite_already_dash_s_unchanged() {
        assert_eq!(
            rewrite_first_sudo("sudo -S apt update"),
            "sudo -S apt update"
        );
    }

    #[test]
    fn sudo_rewrite_sudoku_unchanged() {
        assert_eq!(rewrite_first_sudo("echo sudoku"), "echo sudoku");
    }

    #[test]
    fn sudo_rewrite_pseudo_tty_unchanged() {
        assert_eq!(
            rewrite_first_sudo("ls /dev/pseudo-tty"),
            "ls /dev/pseudo-tty"
        );
    }

    #[test]
    fn sudo_rewrite_only_first_of_two() {
        assert_eq!(
            rewrite_first_sudo("sudo apt update && sudo apt upgrade"),
            "sudo -S apt update && sudo apt upgrade"
        );
    }

    // ── is_readonly_tool ───────────────────────────────────────────

    #[test]
    fn readonly_tools_classified_correctly() {
        assert!(is_readonly_tool("read_file"));
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
    fn tool_definitions_has_fifteen_tools() {
        assert_eq!(tool_definitions().len(), 15);
    }

    #[test]
    fn tool_definitions_json_has_fifteen_tools() {
        let json = tool_definitions_json();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 15);
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
        assert_eq!(names.len(), 15);
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
        assert_eq!(parsed.as_array().unwrap().len(), 15);
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
        let result = run_bash("sleep 5".into(), Arc::new(ToolContext::new())).await;
        *TOOL_TIMEOUT.lock().unwrap() = old;
        assert!(result.contains("Command timed out after 1 seconds"));
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
        ctx_a.set_sudo_provider(Some(Box::new(|| Some("pw-a".into()))));
        ctx_b.set_sudo_provider(Some(Box::new(|| Some("pw-b".into()))));
        let call = |c: &ToolContext| c.sudo_provider.lock().unwrap().as_ref().unwrap()();
        assert_eq!(call(&ctx_a), Some("pw-a".to_string()));
        assert_eq!(call(&ctx_b), Some("pw-b".to_string()));
    }

    #[test]
    fn cached_sudo_password_not_shared() {
        let ctx_a = ToolContext::new();
        let ctx_b = ToolContext::new();
        *ctx_a.cached_sudo_password.lock().unwrap() = Some("secret".into());
        assert!(ctx_b.cached_sudo_password.lock().unwrap().is_none());
        ctx_a.clear_sudo();
        assert!(ctx_a.cached_sudo_password.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn run_bash_refuses_sudo_without_provider() {
        let _guard = test_tool_timeout_guard();
        // A context with no provider must refuse sudo regardless of any other.
        let ctx = Arc::new(ToolContext::new());
        let result = run_bash("sudo true".into(), ctx).await;
        assert!(result.contains("no password provider"));
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
        let result = read_file(path.to_str().unwrap().to_string()).await;
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn read_file_not_found() {
        let result = read_file("/tmp/pengy_nonexistent_file_12345.txt".into()).await;
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
        )
        .await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn replace_in_file_multiple_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replace.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();
        let result = replace_in_file(path.to_str().unwrap().into(), "aaa".into(), "x".into()).await;
        assert!(result.contains("matches 2 locations"));
    }

    #[tokio::test]
    async fn replace_in_file_empty_old_str() {
        let result = replace_in_file("/tmp/x".into(), "".into(), "y".into()).await;
        assert!(result.contains("old_str is empty"));
    }

    #[tokio::test]
    async fn replace_in_file_not_found() {
        let result = replace_in_file(
            "/tmp/pengy_nonexistent_12345.txt".into(),
            "x".into(),
            "y".into(),
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
        )
        .await;
        assert!(result.contains("not found"));
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
        let content = "a".repeat(249_999) + "🐧extra";
        std::fs::write(&path, &content).unwrap();
        let result = read_multiple_files(vec![path.to_str().unwrap().into()]).await;
        assert!(result.contains("[... truncated at 250000 characters"));
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(!result.contains("🐧extra"));
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

        let result = glob_tool(
            "*.py".into(),
            Some(dir.path().to_str().unwrap().into()),
        ).await;
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
        let td = defs.iter().find(|t| t.function.name == "ask_user_question").unwrap();
        let questions = &td.function.parameters.properties["questions"];
        assert_eq!(questions["type"], "array");
        let items = &questions["items"];
        assert_eq!(items["type"], "object", "questions items must be objects, not strings!");
        let required: Vec<String> = items["required"]
            .as_array().unwrap().iter()
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
            .as_array().unwrap().iter()
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
        let td = defs.iter().find(|t| t.function.name == "todowrite").unwrap();
        let todos = &td.function.parameters.properties["todos"];
        assert_eq!(todos["type"], "array");
        let items = &todos["items"];
        assert_eq!(items["type"], "object");
        let required: Vec<String> = items["required"]
            .as_array().unwrap().iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(required, vec!["content", "status"]);
        let props = &items["properties"];
        assert_eq!(props["content"]["type"], "string");
        assert_eq!(props["status"]["type"], "string");
        let status_enum: Vec<String> = props["status"]["enum"]
            .as_array().unwrap().iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(status_enum, vec!["pending", "in_progress", "completed"]);
    }

    // ── Schema content: apply_changes ─────────────────────────────

    #[test]
    fn apply_changes_schema_has_full_operation_properties() {
        let defs = tool_definitions();
        let ac = defs.iter().find(|t| t.function.name == "apply_changes").unwrap();
        let params = &ac.function.parameters.properties;

        // changes array
        let changes = &params["changes"];
        assert_eq!(changes["type"], "array");
        let change_items = &changes["items"];
        assert_eq!(change_items["type"], "object");
        let change_required: Vec<String> = change_items["required"]
            .as_array().unwrap().iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(change_required, vec!["path", "operations"]);

        // operations within each change
        let operations = &change_items["properties"]["operations"];
        assert_eq!(operations["type"], "array");
        let op_items = &operations["items"];
        assert_eq!(op_items["type"], "object");
        let op_required: Vec<String> = op_items["required"]
            .as_array().unwrap().iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(op_required.contains(&"kind".to_string()));
        let op_props = &op_items["properties"];
        let kind_enum: Vec<String> = op_props["kind"]["enum"]
            .as_array().unwrap().iter()
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
        assert!(!params["dry_run"]["description"].as_str().unwrap_or("").is_empty());

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
            let file_pattern = current
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            // Use the original last component from the pattern (preserves wildcards)
            let original_file_part = pattern
                .rsplit('/')
                .next()
                .unwrap_or(file_pattern);
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
        "build",
        "dist",
        "target",
    ]
    .iter()
    .cloned()
    .collect();

    // Split pattern into prefix and glob parts for simple matching
    let has_recursive = pattern.contains("**");
    let glob_suffix = if has_recursive {
        pattern
            .rsplit("**")
            .next()
            .unwrap_or(&pattern)
            .trim_start_matches('/')
    } else {
        &pattern
    };

    let mut matches: Vec<(String, bool, u64)> = Vec::new();
    let max_depth = if has_recursive { usize::MAX } else { 1 };

    let walker = walkdir::WalkDir::new(&search_dir)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            } // never skip the root
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with('.') && !pattern.starts_with('.') {
                    return false;
                }
                if skip_dirs.contains(name) {
                    return false;
                }
            }
            true
        });

    for entry in walker.flatten() {
        if entry.file_type().is_dir() {
            continue;
        }
        let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        let rel_str = rel.to_string_lossy();

        // Simple glob matching: check suffix pattern against the path
        if !simple_glob_match(&rel_str, glob_suffix) {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        matches.push((rel_str.to_string(), false, size));
    }

    if matches.is_empty() {
        return format!(
            "No files matching '{}' in {}",
            pattern,
            search_dir.display()
        );
    }

    matches.sort_by(|a, b| a.0.cmp(&b.0));

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
        lines.push(format!("{path_str}  ({} B)", size));
    }

    let result = lines.join("\n");
    snip_tool_output(result)
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
            "Error: {in_progress_count} tasks marked in_progress. Exactly one task must be in_progress at a time."
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
