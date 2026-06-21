//! Tool definitions and execution for Pengy.
//!
//! Defines 11 OpenAI function-calling tools and their implementations.

use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ── Global state ────────────────────────────────────────────────────

pub static SUDO_PASSWORD_PROVIDER: Mutex<
    Option<Box<dyn Fn() -> Option<String> + Send + Sync>>,
> = Mutex::new(None);

pub static CACHED_SUDO_PASSWORD: Mutex<Option<String>> = Mutex::new(None);
pub static TOOL_TIMEOUT: Mutex<u64> = Mutex::new(60);
pub static USER_AGENT: Mutex<String> = Mutex::new(String::new());

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
        td("run_bash", "Run a bash command in the terminal",
            &[("command", "string", "The bash command to execute")],
            &["command"]),
        td("web_search", "Search the web using DuckDuckGo (via ddgs multi-engine metasearch)",
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
    ]
}

fn td(
    name: &str,
    desc: &str,
    props: &[(&str, &str, &str)],
    required: &[&str],
) -> ToolDef {
    let mut properties = serde_json::Map::new();
    for (pname, ptype, pdesc) in props {
        properties.insert(
            pname.to_string(),
            serde_json::json!({"type": ptype, "description": pdesc}),
        );
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
    )
}

// ── Tool execution dispatcher ───────────────────────────────────────

pub async fn execute_tool(name: &str, arguments: &serde_json::Value) -> String {
    match name {
        "read_file" => read_file(a(arguments, "path", "")).await,
        "write_file" => write_file(a(arguments, "path", ""), a(arguments, "content", "")).await,
        "replace_in_file" => {
            replace_in_file(
                a(arguments, "path", ""),
                a(arguments, "old_str", ""),
                a(arguments, "new_str", ""),
            )
            .await
        }
        "run_bash" => run_bash(a(arguments, "command", "")).await,
        "web_search" => web_search(a(arguments, "query", ""), aus(arguments, "max_results", 5)).await,
        "download_file" => {
            download_file(a(arguments, "url", ""), aopt(arguments, "filename")).await
        }
        "fetch_url" => fetch_url(a(arguments, "url", "")).await,
        "run_python" => run_python(a(arguments, "code", "")).await,
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
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
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
        _ => format!("Unknown tool: {name}"),
    }
}

pub fn kill_active_process() {
    // Process killing is handled by the calling code via tokio's abort handle
    // This is a no-op in the Rust port since we use tokio tasks, not raw processes
    // (individual tool subprocesses handle their own cleanup)
}

// ── Argument helpers ────────────────────────────────────────────────

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
        Ok(c) => c,
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
                let line_num =
                    content[..pos + idx].chars().filter(|&c| c == '\n').count() + 1;
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

async fn run_bash(command: String) -> String {
    let timeout = timeout_secs();

    let password_needed = Regex::new(r"\bsudo\b").unwrap().is_match(&command);
    if password_needed {
        let need_pw = { CACHED_SUDO_PASSWORD.lock().unwrap().is_none() };
        if need_pw {
            let provider = SUDO_PASSWORD_PROVIDER.lock().unwrap().take();
            let pw = match provider {
                Some(cb) => {
                    let result = cb();
                    *SUDO_PASSWORD_PROVIDER.lock().unwrap() = Some(cb);
                    result
                }
                None => return "Error: sudo detected but no password provider is configured.".into(),
            };
            match pw {
                Some(p) => { *CACHED_SUDO_PASSWORD.lock().unwrap() = Some(p); }
                None => return "Cancelled: sudo password not provided.".into(),
            }
        }
    }

    // Ensure sudo reads password from stdin
    let command = if password_needed && !command.contains("sudo -S") {
        command.replacen("sudo", "sudo -S", 1)
    } else {
        command
    };

    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c").arg(&command);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("Error running command: {e}"),
    };

    let pid = child.id();

    if password_needed {
        let pw_guard = CACHED_SUDO_PASSWORD.lock().unwrap();
        if let Some(ref pw) = *pw_guard {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = writeln!(stdin, "{pw}");
            }
        }
    }

    // Use tokio::task::spawn_blocking to avoid blocking the async runtime
    let result = tokio::task::spawn_blocking(move || {
        if timeout > 0 {
            let dur = Duration::from_secs(timeout);
            match wait_timeout(&mut child, dur) {
                Ok(Some(output)) => Ok(output),
                Ok(None) => {
                    // Timed out — kill the process group
                    let _ = std::process::Command::new("kill")
                        .arg("-9")
                        .arg(format!("-{pid}"))
                        .output();
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(format!("Error: Command timed out after {timeout} seconds"))
                }
                Err(e) => Err(format!("Error running command: {e}")),
            }
        } else {
            match child.wait_with_output() {
                Ok(output) => Ok(output),
                Err(e) => Err(format!("Error running command: {e}")),
            }
        }
    })
    .await;

    match result {
        Ok(Ok(output)) => {
            let mut out = String::from_utf8_lossy(&output.stdout).to_string();
            let err = String::from_utf8_lossy(&output.stderr).to_string();
            let err = Regex::new(r"^\[sudo[^]]*\].*\n?")
                .unwrap()
                .replace_all(&err, "");
            if !err.is_empty() {
                out.push('\n');
                out.push_str(&err);
            }
            if !output.status.success() {
                out.push_str(&format!(
                    "\n[Exit code: {}]",
                    output.status.code().unwrap_or(-1)
                ));
            }
            if out.is_empty() {
                "(No output)".into()
            } else {
                out
            }
        }
        Ok(Err(e)) => e,
        Err(join_err) => format!("Error: Task panicked: {join_err}"),
    }
}

/// Wait for a child process with a timeout, without blocking the async runtime.
fn wait_timeout(
    child: &mut std::process::Child,
    dur: Duration,
) -> Result<Option<std::process::Output>, std::io::Error> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                // Process exited — collect output
                let stdout = child.stdout.take().map(|mut s| {
                    let mut buf = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut s, &mut buf);
                    buf
                }).unwrap_or_default();
                let stderr = child.stderr.take().map(|mut s| {
                    let mut buf = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut s, &mut buf);
                    buf
                }).unwrap_or_default();
                return Ok(Some(std::process::Output {
                    status,
                    stdout,
                    stderr,
                }));
            }
            None => {
                if start.elapsed() >= dur {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

// ── Rate limiter for web searches ──

static LAST_SEARCH_TIME: Mutex<Option<Instant>> = Mutex::new(None);

async fn web_search(query: String, max_results: usize) -> String {
    // Rate-limit between searches
    let wait_ms = {
        let last = LAST_SEARCH_TIME.lock().unwrap();
        last.map(|prev| {
            let elapsed = prev.elapsed();
            if elapsed < Duration::from_millis(800) {
                (Duration::from_millis(800) - elapsed).as_millis() as u64
            } else { 0 }
        }).unwrap_or(0)
    };
    if wait_ms > 0 { tokio::time::sleep(Duration::from_millis(wait_ms)).await; }
    *LAST_SEARCH_TIME.lock().unwrap() = Some(Instant::now());

    // ── Primary: Python ddgs (10 engines, TLS impersonation, battle-tested) ──
    let result = search_ddgs_python(&query, max_results).await;
    if !result.starts_with("ddgs:") && !result.starts_with("No results") {
        return result;
    }

    // ── Fallback: Mojeek via reqwest ──
    let result = search_mojeek(&query, max_results).await;
    if !result.starts_with("No results")
        && !result.starts_with("Mojeek returned HTTP")
        && !result.starts_with("Error searching Mojeek")
    {
        return result;
    }

    // ── Fallback: DDG via primp ──
    let client = match primp::Client::builder()
        .impersonate(primp::Impersonate::ChromeV146)
        .impersonate_os(primp::ImpersonateOS::Linux)
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("Error creating HTTP client: {e}"),
    };
    search_ddg(&client, &query, max_results).await
}

/// Python ddgs subprocess — identical to the proven Python Pengy approach.
async fn search_ddgs_python(query: &str, max_results: usize) -> String {
    let safe_query = query.replace('\'', "'\\''");
    let cmd = format!(
        "timeout 8 python3 -c 'import json,sys;from ddgs import DDGS;d=DDGS();r=list(d.text(sys.argv[1],max_results=int(sys.argv[2])));print(json.dumps(r))' '{}' {} 2>/dev/null",
        safe_query, max_results
    );

    let output = match std::process::Command::new("bash")
        .arg("-c").arg(&cmd)
        .stdout(Stdio::piped()).stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(e) => return format!("ddgs spawn error: {e}"),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let exit = output.status.code().unwrap_or(-1);
    if exit == 124 { return "Web search timed out after 8 seconds.".into(); }
    if stdout.is_empty() { return format!("ddgs: no output (exit {exit})"); }

    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(json) => {
            if let Some(arr) = json.as_array() {
                if arr.is_empty() { return "No results found.".into(); }
                let mut lines = Vec::new();
                for (i, r) in arr.iter().enumerate() {
                    if i >= max_results { break; }
                    let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let href = r.get("href").and_then(|v| v.as_str()).unwrap_or("");
                    let body = r.get("body").and_then(|v| v.as_str()).unwrap_or("");
                    if title.is_empty() { continue; }
                    lines.push(format!("{}. {title}", i + 1));
                    if !href.is_empty() { lines.push(format!("   URL: {href}")); }
                    if !body.is_empty() { lines.push(format!("   {body}")); }
                    lines.push(String::new());
                }
                return if lines.is_empty() { "No results found.".into() } else { lines.join("\n").trim().to_string() };
            }
            format!("ddgs: unexpected JSON: {}", &stdout[..200.min(stdout.len())])
        }
        Err(e) => format!("ddgs JSON error: {e} — raw: {}", &stdout[..200.min(stdout.len())]),
    }
}

/// Mojeek search — fast, server-rendered HTML, no rate limiting.
/// Structure: ul.results-standard > li.rN > h2 > a.title (title+href) + p.s (snippet)
async fn search_mojeek(query: &str, max_results: usize) -> String {
    let encoded = urlencoding(query);
    let url = format!("https://www.mojeek.com/search?q={encoded}");

    let ua = ua();
    let user_agent = if ua.is_empty() { "Pengy/2.0" } else { &ua };

    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_default();

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error searching Mojeek: {e}"),
    };

    if resp.status().as_u16() != 200 {
        return format!("Mojeek returned HTTP {}.", resp.status().as_u16());
    }

    let html = match resp.text().await {
        Ok(t) => t,
        Err(e) => return format!("Error reading search response: {e}"),
    };

    let document = scraper::Html::parse_document(&html);
    let mut results: Vec<String> = Vec::new();

    // Mojeek: <li class="r1">...<h2><a class="title" href="URL">TITLE</a></h2>...<p class="s">SNIPPET</p></li>
    let title_sel = scraper::Selector::parse("h2 a.title").unwrap();
    let snippet_sel = scraper::Selector::parse("p.s").unwrap();
    let li_sel = scraper::Selector::parse("ul.results-standard > li").unwrap();

    for li in document.select(&li_sel) {
        if results.len() >= max_results {
            break;
        }
        let title = li.select(&title_sel).next()
            .map(|a| a.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let href = li.select(&title_sel).next()
            .and_then(|a| a.value().attr("href"))
            .unwrap_or("")
            .to_string();
        let snippet = li.select(&snippet_sel).next()
            .map(|p| p.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        if title.is_empty() {
            continue;
        }
        let mut entry = format!("{}. {title}", results.len() + 1);
        if !href.is_empty() {
            entry.push_str(&format!("\n   URL: {href}"));
        }
        if !snippet.is_empty() {
            entry.push_str(&format!("\n   {snippet}"));
        }
        results.push(entry);
    }

    if results.is_empty() {
        "No results found.".into()
    } else {
        results.join("\n\n")
    }
}

/// DuckDuckGo HTML search via primp — browser-impersonated POST with form data.
/// Extracts results from the DDG HTML endpoint using correct CSS selectors.
async fn search_ddg(client: &primp::Client, query: &str, max_results: usize) -> String {
    let resp = match client
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query), ("b", ""), ("l", "us-en")])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return format!("Error searching DuckDuckGo: {e}"),
    };

    if resp.status().as_u16() != 200 {
        return format!("DuckDuckGo returned HTTP {}.", resp.status().as_u16());
    }

    let html = match resp.text().await {
        Ok(t) => t,
        Err(e) => return format!("Error reading search response: {e}"),
    };

    if html.len() < 5000 {
        return "DuckDuckGo returned a silent block page.".into();
    }

    let mut results: Vec<String> = Vec::new();
    let document = scraper::Html::parse_document(&html);

    let rs = scraper::Selector::parse("div.result").unwrap();
    let ts = scraper::Selector::parse("a.result__a").unwrap();
    let ss = scraper::Selector::parse("div.result__snippet").unwrap();

    for el in document.select(&rs) {
        if results.len() >= max_results {
            break;
        }
        let title = el.select(&ts).next()
            .map(|a| a.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let href = el.select(&ts).next()
            .and_then(|a| a.value().attr("href"))
            .map(|h| if let Some(p) = h.find("uddg=") { urldecode(&h[p + 5..]) } else { h.to_string() })
            .unwrap_or_default();
        let snippet = el.select(&ss).next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        if title.is_empty() { continue; }
        if href.contains("duckduckgo.com/y.js") { continue; }

        let mut entry = format!("{}. {title}", results.len() + 1);
        if !href.is_empty() { entry.push_str(&format!("\n   URL: {href}")); }
        if !snippet.is_empty() { entry.push_str(&format!("\n   {snippet}")); }
        results.push(entry);
    }

    if results.is_empty() { "No results found.".into() } else { results.join("\n\n") }
}

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

    let fname = filename.unwrap_or_else(|| {
        url.split('?')
            .next()
            .unwrap_or(&url)
            .rsplit('/')
            .next()
            .unwrap_or("download")
            .to_string()
    });
    let dest = downloads.join(&fname);

    let client = reqwest::Client::builder()
        .user_agent(ua())
        .build()
        .unwrap_or_default();

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error downloading file: {e}"),
    };

    let max_size: usize = 100 * 1024 * 1024;
    // Download to bytes with a size cap
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return format!("Error downloading: {e}"),
    };
    if bytes.len() > max_size {
        return format!("Error: Download exceeds maximum size of {max_size} bytes.");
    }
    let total = bytes.len();

    match std::fs::write(&dest, &bytes) {
        Ok(_) => format!("Downloaded to {} ({total} bytes)", dest.display()),
        Err(e) => format!("Error writing file: {e}"),
    }
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

    let client = reqwest::Client::builder()
        .user_agent(ua())
        .build()
        .unwrap_or_default();

    let resp = match client.get(&url_str).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error fetching URL: {e}"),
    };

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
    let is_html =
        text.to_lowercase().contains("<html") || text.to_lowercase().contains("<!doctype");

    let text = if is_html {
        let document = scraper::Html::parse_document(&text);
        let body_text = document.root_element().text().collect::<String>();
        Regex::new(r"\n{3,}")
            .unwrap()
            .replace_all(&body_text, "\n\n")
            .to_string()
    } else {
        text
    };

    if text.len() > 50_000 {
        format!(
            "{}...\n\n[... truncated at 50,000 characters ...]",
            &text[..50_000]
        )
    } else {
        text
    }
}

async fn run_python(code: String) -> String {
    let _timeout = timeout_secs();
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("pengy_py_{}.py", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, &code) {
        return format!("Error writing temp file: {e}");
    }

    let output = std::process::Command::new("python3")
        .arg(&tmp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let _ = std::fs::remove_file(&tmp);

    match output {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).to_string();
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            if !err.is_empty() {
                s.push('\n');
                s.push_str(&err);
            }
            if !out.status.success() {
                s.push_str(&format!(
                    "\n[Exit code: {}]",
                    out.status.code().unwrap_or(-1)
                ));
            }
            if s.is_empty() {
                "(No output)".into()
            } else {
                s
            }
        }
        Err(e) => format!("Error running Python: {e}"),
    }
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
    if result.len() > 40_000 {
        format!(
            "{}...\n\n[... truncated at 40,000 characters ...]",
            &result[..40_000]
        )
    } else {
        result
    }
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
        "node_modules", ".git", ".svn", ".hg", "__pycache__", ".mypy_cache",
        ".pytest_cache", ".ruff_cache", ".tox", ".eggs", ".DS_Store",
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
    const MAX_PER_FILE: usize = 50_000;
    const MAX_TOTAL: usize = 120_000;

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
            let truncated = &content[..MAX_PER_FILE];
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
                let short_block = format!("{header}\n{}...", &content[..remaining.saturating_sub(header.len() + 4)]);
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
        Some(r) => filepath.strip_prefix(r).unwrap_or(filepath).display().to_string(),
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
        .map(|name| name.starts_with('.') || name.ends_with(".egg-info") || ALWAYS_SKIP_DIRS.contains(name))
        .unwrap_or(false)
}

static ALWAYS_SKIP_FILES: Lazy<HashSet<&str>> =
    Lazy::new(|| [".DS_Store", "Thumbs.db"].iter().copied().collect());

fn is_likely_text(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        let lower = ext.to_lowercase();
        let text_exts = [
            "py", "pyi", "pyx", "c", "cpp", "cc", "cxx", "h", "hpp", "hxx", "rs",
            "go", "java", "kt", "scala", "swift", "js", "jsx", "ts", "tsx", "mjs",
            "cjs", "rb", "rake", "php", "pl", "pm", "sh", "bash", "zsh", "fish",
            "html", "htm", "css", "scss", "sass", "less", "json", "yaml", "yml",
            "toml", "ini", "cfg", "conf", "xml", "svg", "rss", "md", "markdown",
            "rst", "txt", "tex", "sql", "r", "jl", "lua", "zig", "nim", "ex", "exs",
            "cmake", "make", "mk", "dockerfile", "env", "gitignore", "editorconfig",
        ];
        if text_exts.contains(&lower.as_str()) {
            return true;
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let text_files = [
            "makefile", "dockerfile", "license", "changelog", "authors", "todo",
        ];
        if text_files.contains(&name.to_lowercase().as_str()) {
            return true;
        }
    }
    false
}

fn matches_glob(name: &str, glob: &str) -> bool {
    // Handle brace expansion like *.{js,ts}
    if let Some(caps) = Regex::new(r"^(.*)\{([^}]+)\}(.*)$")
        .unwrap()
        .captures(glob)
    {
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
    let pattern = pattern
        .replace('.', "\\.")
        .replace('*', ".*")
        .replace('?', ".");
    Regex::new(&format!("^{pattern}$"))
        .map(|re| re.is_match(name))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
    fn tool_definitions_has_eleven_tools() {
        assert_eq!(tool_definitions().len(), 11);
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
        assert_eq!(names.len(), 11);
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
        assert_eq!(parsed.as_array().unwrap().len(), 11);
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
        let result = replace_in_file(
            path.to_str().unwrap().into(),
            "aaa".into(),
            "x".into(),
        )
        .await;
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
        let result = execute_tool("nonexistent_tool", &args).await;
        assert!(result.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn execute_tool_dispatches_read_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dispatch_test.txt");
        std::fs::write(&path, "dispatch content").unwrap();
        let args = serde_json::json!({"path": path.to_str().unwrap()});
        let result = execute_tool("read_file", &args).await;
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
}
