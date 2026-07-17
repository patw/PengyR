use pengy_core::chat_manager::{self, Chat, ChatMessage};
use pengy_core::config::{self, Config};
use pengy_core::llm_client::{self, Confirmation, LlmEvent, ToolConfirmation};
use pengy_core::tools;

use rustyline::{Editor, history::FileHistory};

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BLUE: &str = "\x1b[34m";
const MAX_PANEL_WIDTH: usize = 140;
const MIN_PANEL_WIDTH: usize = 60;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("Pengy v{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Pengy CLI — chat with LLMs from the command line");
        println!();
        println!("Usage: pengy-cli [PROMPT...] [OPTIONS]");
        println!();
        println!("Arguments:");
        println!("  PROMPT...  Optional prompt for single-shot mode.");
        println!("             If omitted, starts interactive mode.");
        println!();
        println!("Options:");
        println!("  --no-save       Don't persist single-shot chats to history.");
        println!("  --model NAME    Set the model to use (overrides config).");
        println!("  --system MSG    Set the system message (overrides config).");
        println!("  --output FORMAT Output format: pretty, raw, json, silent (default: pretty).");
        println!("  --config-dir PATH  Use a custom config directory.");
        println!("  -v, --version   Show version information and exit.");
        println!("  -h, --help      Show this help message and exit.");
        return;
    }

    let no_save = args.iter().any(|a| a == "--no-save");
    let mut model_override: Option<String> = None;
    let mut system_override: Option<String> = None;
    let mut output_mode: String = "pretty".to_string();
    let mut config_dir: Option<String> = None;

    let mut skip_next = false;
    let prompt_args: Vec<&str> = args[1..]
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            if skip_next { skip_next = false; return false; }
            let a_str: &str = *a;
            if a_str == "--no-save" || a_str == "--help" || a_str == "-h" || a_str == "--version" || a_str == "-v" {
                return false;
            }
            if a_str == "--model" || a_str == "--system" || a_str == "--output" || a_str == "--config-dir" {
                skip_next = true;
                if a_str == "--model" { model_override = args.get(*i + 2).map(|s| s.clone()); }
                else if a_str == "--system" { system_override = args.get(*i + 2).map(|s| s.clone()); }
                else if a_str == "--output" { if let Some(v) = args.get(*i + 2) { output_mode = v.clone(); } }
                else if a_str == "--config-dir" { config_dir = args.get(*i + 2).map(|s| s.clone()); }
                return false;
            }
            true
        })
        .map(|(_, s)| s.as_str())
        .collect();

    if let Some(ref dir) = config_dir {
        pengy_core::config::set_config_dir(dir);
    }

    let mut cli = PengyCli::new(no_save);

    if let Some(ref model) = model_override {
        cli.config.model = model.clone();
    }
    if let Some(ref sys) = system_override {
        cli.config.system_message = sys.clone();
    }
    cli.output_mode = output_mode;

    if prompt_args.is_empty() {
        cli.run_interactive();
    } else {
        cli.run_single_shot(&prompt_args.join(" "));
    }
}

struct PengyCli {
    config: Config,
    current_chat: Option<Chat>,
    no_save: bool,
    yolo_this_turn: bool,
    output_mode: String,
    rt: tokio::runtime::Runtime,
    rl: Editor<(), FileHistory>,
    hist_path: std::path::PathBuf,
}

impl PengyCli {
    fn new(no_save: bool) -> Self {
        let config = config::load_config();
        *tools::USER_AGENT.lock().unwrap() = config.user_agent.clone();
        *tools::TOOL_TIMEOUT.lock().unwrap() = config.tool_timeout;

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

        // ── readline history ────────────────────────────────
        let hist_path = dirs_next()
            .unwrap_or_else(|| Path::new(".").to_path_buf())
            .join(".local")
            .join("state")
            .join("pengy")
            .join("cli_history");
        let hist_dir = hist_path.parent().map(|p| p.to_path_buf());
        if let Some(dir) = &hist_dir {
            let _ = std::fs::create_dir_all(dir);
        }

        let mut rl: Editor<(), FileHistory> = Editor::new().expect("rustyline editor");
        let _ = rl.load_history(&hist_path);

        Self {
            config,
            current_chat: None,
            no_save,
            yolo_this_turn: false,
            output_mode: "pretty".to_string(),
            rt,
            rl,
            hist_path,
        }
    }

    fn run_interactive(&mut self) {
        println!();
        print_box(
            "",
            &[
                "🐧 Pengy CLI".to_string(),
                "Type your message and press Enter.  Try /help for available commands.".to_string(),
            ],
            Some(71),
        );

        let chats = chat_manager::load_chats();
        if !chats.is_empty() {
            self.current_chat = Some(chats[0].clone());
            let chat = self.current_chat.as_ref().unwrap();
            let msg_count = chat.messages.len();
            let last_user = chat.messages.iter().rev()
                .find(|m| m.role == "user")
                .and_then(|m| m.content.as_ref())
                .and_then(|v| v.as_str())
                .map(|s| truncate(s, 80))
                .unwrap_or_default();

            println!(
                "{}Resumed:{} {}{}{} {}({} messages){}",
                DIM, RESET, BOLD, chat.title, RESET, DIM, msg_count, RESET
            );
            if !last_user.is_empty() {
                println!("{}Last:{} {}", DIM, RESET, last_user);
            }
        } else {
            self.current_chat = Some(chat_manager::create_chat("New Chat").unwrap());
            println!("{}New chat created.{}", DIM, RESET);
        }

        println!(
            "{}Model: {}  Tool Confirm: {}{}",
            DIM,
            self.config.model,
            self.confirm_display(),
            RESET
        );

        self.set_sudo_provider();

        loop {
            let title = self.current_chat.as_ref()
                .map(|c| truncate(&c.title, 30))
                .unwrap_or_else(|| "?".to_string());
            let prompt_label = format!("\n{} › You: ", title);
            let line = match self.prompt(&prompt_label) {
                Some(l) => l,
                None => break,
            };

            let text = line.trim();
            if text.is_empty() {
                continue;
            }

            if text.starts_with('/') {
                if !self.handle_slash(text) {
                    break;
                }
                continue;
            }

            let (resolved_text, file_content) = resolve_attachments(text);
            let final_text = if file_content.is_empty() {
                resolved_text
            } else {
                format!("{}\n{}", file_content, resolved_text)
            };

            let chat = self.current_chat.as_mut().unwrap();
            chat.messages.push(ChatMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String(final_text.clone())),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_details: None,
            });

            if chat.title == "New Chat" {
                chat.title = truncate(&final_text, 50);
            }

            self.drive_chat();
        }

        self.clear_sudo_provider();
        let _ = self.rl.save_history(&self.hist_path);
        println!("\nGoodbye!");
    }

    fn run_single_shot(&mut self, prompt_text: &str) {
        self.set_sudo_provider();

        if self.no_save {
            let chat = Chat {
                id: uuid_v4(),
                title: truncate(prompt_text, 50),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: Some(serde_json::Value::String(prompt_text.to_string())),
                    tool_calls: vec![],
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
                }],
                created_at: chrono_now(),
            };
            self.current_chat = Some(chat);
        } else {
            let mut chat = chat_manager::create_chat(&truncate(prompt_text, 50)).unwrap();
            chat.messages.push(ChatMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String(prompt_text.to_string())),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_details: None,
            });
            chat_manager::save_chat(&chat).ok();
            self.current_chat = Some(chat);
        }

        self.drive_chat();
        self.clear_sudo_provider();
    }

    // ── Chat driver ──────────────────────────────────────────────

    fn drive_chat(&mut self) {
        let chat = self.current_chat.as_ref().unwrap();
        let messages = build_messages(chat, &self.config);
        let tc_mode = ToolConfirmation::from_str(&self.config.tool_confirmation);

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (confirm_tx, confirm_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));

        let bu = self.config.base_url.clone();
        let ak = self.config.api_key.clone();
        let md = self.config.model.clone();
        let re = self.config.reasoning_effort.clone();
        let pr = self.config.preserve_reasoning;
        let cancel2 = cancel.clone();

        self.rt.spawn(async move {
            llm_client::chat(
                &bu, &ak, &md, messages, tc_mode, &re, pr, event_tx, confirm_rx, cancel2,
            )
            .await;
        });

        self.yolo_this_turn = false;
        eprint!("{}Thinking...{}", DIM, RESET);

        let mut expecting_api = true;
        let aborted = false;

        loop {
            match event_rx.blocking_recv() {
                Some(LlmEvent::AssistantToolCalls { message }) => {
                    if expecting_api {
                        eprint!("\r{}\r", " ".repeat(40));
                    }
                    expecting_api = false;
                    self.yolo_this_turn = false;
                    self.current_chat.as_mut().unwrap().messages.push(message);
                }
                Some(LlmEvent::ToolRequest {
                    name,
                    args,
                    tool_call_id,
                }) => {
                    if expecting_api {
                        eprint!("\r{}\r", " ".repeat(40));
                    }
                    expecting_api = false;

                    let needs_confirm = tc_mode != ToolConfirmation::All
                        && !(tc_mode == ToolConfirmation::Safe && tools::is_readonly_tool(&name))
                        && !self.yolo_this_turn;

                    self.render_tool_request(&name, &args);

                    if needs_confirm {
                        let choice = self.prompt_tool_confirmation();
                        match choice {
                            1 => {
                                let _ = confirm_tx.send(Confirmation {
                                    tool_call_id,
                                    confirmed: true,
                                    yolo_turn: false,
                                });
                            }
                            2 => {
                                self.yolo_this_turn = true;
                                let _ = confirm_tx.send(Confirmation {
                                    tool_call_id,
                                    confirmed: true,
                                    yolo_turn: true,
                                });
                            }
                            3 => {
                                let _ = confirm_tx.send(Confirmation {
                                    tool_call_id,
                                    confirmed: false,
                                    yolo_turn: false,
                                });
                            }
                            _ => {
                                // Abort
                                println!("{}Run aborted by user.{}", RED, RESET);
                                let _ = confirm_tx.send(Confirmation {
                                    tool_call_id,
                                    confirmed: false,
                                    yolo_turn: false,
                                });
                                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                }
                Some(LlmEvent::ToolResult {
                    tool_call_id,
                    content,
                    declined,
                    ..
                }) => {
                    expecting_api = true;
                    self.render_tool_result(&content, declined);
                    self.current_chat
                        .as_mut()
                        .unwrap()
                        .messages
                        .push(ChatMessage {
                            role: "tool".into(),
                            content: Some(serde_json::Value::String(content)),
                            tool_calls: vec![],
                            tool_call_id: Some(tool_call_id),
                            reasoning_content: None,
                            reasoning: None,
                            reasoning_details: None,
                        });
                    eprint!("{}Thinking...{}", DIM, RESET);
                }
                Some(LlmEvent::FinalResponse { content, message, usage }) => {
                    if expecting_api {
                        eprint!("\r{}\r", " ".repeat(40));
                    }
                    self.render_final(&content, &usage);
                    self.current_chat
                        .as_mut()
                        .unwrap()
                        .messages
                        .push(message.unwrap_or(ChatMessage {
                            role: "assistant".into(),
                            content: Some(serde_json::Value::String(content)),
                            tool_calls: vec![],
                            tool_call_id: None,
                            reasoning_content: None,
                            reasoning: None,
                            reasoning_details: None,
                        }));
                    if !self.no_save {
                        chat_manager::save_chat(self.current_chat.as_ref().unwrap()).ok();
                    }
                    break;
                }
                None => {
                    eprint!("\r{}\r", " ".repeat(40));
                    if !aborted {
                        println!("{}Chat ended unexpectedly.{}", RED, RESET);
                    }
                    break;
                }
            }
        }
    }

    // ── Rendering ────────────────────────────────────────────────

    fn render_tool_request(&self, name: &str, args: &serde_json::Value) {
        let mut args_str = serde_json::to_string_pretty(args).unwrap_or_default();
        if char_count(&args_str) > 4000 {
            args_str = format!("{}\n\n[... truncated ...]", take_chars(&args_str, 4000));
        }

        println!();
        print_box(
            &format!("🔧 Tool: {}", name),
            &[name.to_string(), args_str],
            None,
        );
    }

    fn render_tool_result(&self, content: &str, declined: bool) {
        if declined {
            print_box("Tool output", &["Declined".to_string()], None);
            return;
        }

        let display = if char_count(content) > 4000 {
            format!("{}\n\n[... truncated ...]", take_chars(content, 4000))
        } else {
            content.to_string()
        };

        print_box("Tool output", &[display], None);
    }

    fn render_final(&self, content: &str, usage: &llm_client::Usage) {
        match self.output_mode.as_str() {
            "silent" => return,
            "json" => {
                let result = serde_json::json!({"content": content, "usage": usage});
                println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
                return;
            }
            "raw" => {
                if !content.trim().is_empty() {
                    println!("{}", content);
                }
                return;
            }
            _ => {} // pretty — fall through
        }

        if content.trim().is_empty() {
            print_box("Assistant 🤖", &["(empty response)".to_string()], None);
        } else {
            println!();
            let rendered = render_markdown_terminal(content);
            print_box("Assistant 🤖", &[rendered], None);
        }

        if usage.total_tokens > 0 {
            println!(
                "{}Tokens: {} in / {} out ({} total){}",
                DIM, usage.prompt_tokens, usage.completion_tokens, usage.total_tokens, RESET
            );
        }
    }

    // ── Tool confirmation ────────────────────────────────────────

    fn prompt_tool_confirmation(&mut self) -> u8 {
        loop {
            let input = self.prompt(&format!(
                "  [1] Execute  [2] Yes to all this turn  [3] Decline  [4] Abort run  {}[1/2/3/4]{} ",
                BOLD, RESET
            ));
            match input.as_deref().unwrap_or("1").trim() {
                "1" | "" => return 1,
                "2" => return 2,
                "3" => return 3,
                "4" => return 4,
                _ => println!("{}Please enter 1, 2, 3, or 4.{}", RED, RESET),
            }
        }
    }

    // ── Sudo ─────────────────────────────────────────────────────

    fn set_sudo_provider(&self) {
        *tools::SUDO_PASSWORD_PROVIDER.lock().unwrap() = Some(Box::new(|| {
            eprint!("{}Enter sudo password: {}", YELLOW, RESET);
            io::stderr().flush().ok();
            rpassword::read_password().ok()
        }));
    }

    fn clear_sudo_provider(&self) {
        *tools::SUDO_PASSWORD_PROVIDER.lock().unwrap() = None;
        *tools::CACHED_SUDO_PASSWORD.lock().unwrap() = None;
    }

    // ── Slash commands ───────────────────────────────────────────

    fn handle_slash(&mut self, text: &str) -> bool {
        let parts: Vec<&str> = text.split_whitespace().collect();
        let cmd = parts[0].to_lowercase();
        let args = &parts[1..];

        match cmd.as_str() {
            "/quit" | "/exit" | "/q" => return false,
            "/help" => self.cmd_help(),
            "/new" => self.cmd_new(),
            "/show" => self.cmd_show(args),
            "/tail" => self.cmd_tail(args),
            "/rename" => self.cmd_rename(args),
            "/clear" => self.cmd_clear(),
            "/export" => self.cmd_export(args),
            "/config" => self.cmd_config(),
            "/model" => self.cmd_model(args),
            "/models" => self.cmd_models(),
            "/list" => self.cmd_list(),
            "/load" => self.cmd_load(args),
            "/delete" => self.cmd_delete(args),
            "/yolo" => self.cmd_yolo(args),
            "/baseurl" => self.cmd_baseurl(args),
            "/apikey" => self.cmd_apikey(args),
            "/timeout" => self.cmd_timeout(args),
            "/agent" => self.cmd_agent(args),
            "/context-keep" => self.cmd_context_keep(args),
            "/system" => self.cmd_system(args),
            "/attach" => self.cmd_attach(),
            "/compact" => self.cmd_compact(),
            _ => println!("{}Unknown command:{} {}  (try /help)", RED, RESET, cmd),
        }
        true
    }

    fn cmd_help(&self) {
        println!();
        println!("{}Slash Commands{}", BOLD, RESET);
        println!("{}{}{}", DIM, "-".repeat(60), RESET);
        let cmds = [
            ("/help", "Show this help"),
            ("/new", "Start a new chat"),
            ("/show [N]", "Show full conversation (optional: last N messages)"),
            ("/tail [N]", "Show the last N messages (default 5)"),
            ("/rename <title>", "Rename the current chat"),
            ("/clear", "Clear the terminal screen"),
            ("/export [path]", "Export current chat as Markdown"),
            ("/config", "Show current configuration"),
            ("/model <name>", "Change the model"),
            ("/models", "Fetch available models from the endpoint"),
            ("/baseurl <url>", "Set the API base URL"),
            ("/apikey <key>", "Set the API key"),
            ("/timeout <sec>", "Set tool execution timeout in seconds"),
            ("/agent <string>", "Set the user agent string"),
            ("/context-keep <n>", "Set how many recent turns to keep"),
            ("/yolo [all|safe|none]", "Set tool confirmation mode"),
            ("/system [message...]", "Show or set the system message"),
            ("/compact", "Compact context by eliding old tool results"),
            ("/list", "List recent chats"),
            ("/load <index>", "Load a chat by its /list index"),
            ("/delete <index>", "Delete a chat by its /list index"),
            ("/attach", "Show file attachment help"),
            ("/quit, /exit", "Exit Pengy CLI"),
        ];
        for (cmd, desc) in &cmds {
            println!("  {}{:<24}{} {}", CYAN, cmd, RESET, desc);
        }
    }

    fn cmd_new(&mut self) {
        self.current_chat = Some(chat_manager::create_chat("New Chat").unwrap());
        println!("{}✓ New chat created.{}", GREEN, RESET);
    }

    fn cmd_show(&self, args: &[&str]) {
        let chat = match self.current_chat.as_ref() {
            Some(c) => c,
            None => { println!("{}No active chat.{}", DIM, RESET); return; }
        };
        let msgs = &chat.messages;
        if msgs.is_empty() {
            println!("{}No messages in this chat.{}", DIM, RESET);
            return;
        }

        let limit: Option<usize> = if !args.is_empty() {
            match args[0].parse::<usize>() {
                Ok(n) if n > 0 => Some(n),
                _ => { println!("{}Usage: /show [N]  — show last N messages{}", RED, RESET); return; }
            }
        } else {
            None
        };

        let start = limit.map(|n| msgs.len().saturating_sub(n)).unwrap_or(0);
        let display = &msgs[start..];
        let total = msgs.len();

        println!();
        println!("{}Conversation:{} {}{}{} {}({} messages total{}){}",
            BOLD, RESET, BOLD, chat.title, RESET,
            DIM, total,
            if limit.is_some() { format!(", showing last {}", display.len()) } else { String::new() },
            RESET);
        println!("{}{}{}", DIM, "─".repeat(terminal_width().min(60)), RESET);

        for (j, msg) in display.iter().enumerate() {
            let i = start + j + 1;  // 1-based index
            let role = &msg.role;
            let content = msg.content.as_ref()
                .and_then(|v| if v.is_string() { v.as_str().map(String::from) }
                           else if v.is_array() {
                               let parts: Vec<String> = v.as_array().unwrap().iter().map(|p| {
                                   if let Some(t) = p.get("text").and_then(|t| t.as_str()) { t.to_string() }
                                   else if p.get("image_url").is_some() { "[image]".to_string() }
                                   else { String::new() }
                               }).collect();
                               Some(parts.join(" "))
                           } else { Some(v.to_string()) })
                .unwrap_or_default();

            if role == "user" {
                println!("{}{}#{} You:{}{} {}", BLUE, BOLD, i, RESET, RESET, truncate(&content, 200));
            } else if role == "assistant" {
                let tc_names: Vec<String> = msg.tool_calls.iter()
                    .map(|tc| tc.function.name.clone())
                    .collect();
                if tc_names.is_empty() {
                    println!("{}{}#{} Assistant:{}{}", GREEN, BOLD, i, RESET, RESET);
                } else {
                    println!("{}{}#{} Assistant:{}{} (tool calls: {}){}",
                        GREEN, BOLD, i, RESET, DIM, tc_names.join(", "), RESET);
                }
                if !content.is_empty() {
                    println!("{}  {}{}", DIM, truncate(&content, 100), RESET);
                }
            } else if role == "tool" {
                let tc_id = msg.tool_call_id.as_deref().unwrap_or("?");
                let short_id = tc_id.chars().take(8).collect::<String>();
                println!("{}{}#{} Tool [{}]:{}{} {}{}", DIM, DIM, i, short_id, RESET, DIM, truncate(&content, 80), RESET);
            } else if role == "system" {
                println!("{}{}#{} System:{}{} {}{}", DIM, DIM, i, RESET, DIM, truncate(&content, 100), RESET);
            }
        }

        println!("{}{}{}", DIM, "─".repeat(terminal_width().min(60)), RESET);
    }

    fn cmd_tail(&self, args: &[&str]) {
        let n: usize = if !args.is_empty() {
            args[0].parse().unwrap_or(5)
        } else {
            5
        };
        let n_str = n.to_string();
        let fake_args: [&str; 1] = [&n_str];
        self.cmd_show(&fake_args);
    }

    fn cmd_rename(&mut self, args: &[&str]) {
        let chat = match self.current_chat.as_mut() {
            Some(c) => c,
            None => { println!("{}No active chat.{}", DIM, RESET); return; }
        };
        if args.is_empty() {
            println!("{}Usage: /rename <new title>{}", DIM, RESET);
            return;
        }
        let new_title = args.join(" ");
        let old_title = chat.title.clone();
        chat.title = new_title.clone();
        chat_manager::save_chat(chat).ok();
        println!("{}✓ Renamed:{} {}{}{} → {}{}{}",
            GREEN, RESET, BOLD, old_title, RESET, BOLD, new_title, RESET);
    }

    fn cmd_clear(&self) {
        print!("\x1b[2J\x1b[H");
        println!("{}Screen cleared. Use /show to see conversation.{}", DIM, RESET);
    }

    fn cmd_export(&self, args: &[&str]) {
        let chat = match self.current_chat.as_ref() {
            Some(c) => c,
            None => { println!("{}No active chat.{}", DIM, RESET); return; }
        };

        let out_path = if !args.is_empty() {
            Path::new(args[0]).to_path_buf()
        } else {
            let safe_title: String = chat.title.chars()
                .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' })
                .collect();
            let safe_title = truncate(safe_title.trim(), 50);
            let name = if safe_title.is_empty() { "chat".to_string() } else { safe_title };
            dirs_next().unwrap_or_else(|| Path::new(".").to_path_buf()).join("Downloads").join(format!("{}.md", name))
        };

        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("# {}", chat.title));
        lines.push(format!("*Exported {}*", chrono_now().replace('T', " ")));
        lines.push(String::new());

        for msg in &chat.messages {
            let role = &msg.role;
            let content = msg.content.as_ref()
                .and_then(|v| if v.is_string() { v.as_str().map(String::from) }
                           else if v.is_array() {
                               let parts: Vec<String> = v.as_array().unwrap().iter().map(|p| {
                                   if let Some(t) = p.get("text").and_then(|t| t.as_str()) { t.to_string() }
                                   else if p.get("image_url").is_some() { "[image]".to_string() }
                                   else { String::new() }
                               }).collect();
                               Some(parts.join(" "))
                           } else { Some(v.to_string()) })
                .unwrap_or_default();

            if role == "user" {
                lines.push("### 🧑 You".to_string());
                lines.push(content);
                lines.push(String::new());
            } else if role == "assistant" {
                if !msg.tool_calls.is_empty() {
                    lines.push("### 🤖 Assistant (tool calls)".to_string());
                    for tc in &msg.tool_calls {
                        lines.push(format!("- **{}**", tc.function.name));
                        lines.push(format!("  ```json\n  {}\n  ```", tc.function.arguments));
                    }
                    lines.push(String::new());
                }
                if !content.is_empty() {
                    lines.push("### 🤖 Assistant".to_string());
                    lines.push(content);
                    lines.push(String::new());
                }
            } else if role == "tool" {
                let tc_id = msg.tool_call_id.as_deref().unwrap_or("?");
                lines.push(format!("#### 🔧 Tool result (`{}`)", tc_id));
                lines.push("```".to_string());
                lines.push(content);
                lines.push("```".to_string());
                lines.push(String::new());
            } else if role == "system" {
                lines.push(format!("*System: {}*", truncate(&content, 200)));
                lines.push(String::new());
            }
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        match std::fs::write(&out_path, lines.join("\n")) {
            Ok(_) => println!("{}✓ Exported to:{} {}{}{}", GREEN, RESET, BOLD, out_path.display(), RESET),
            Err(e) => println!("{}Error exporting:{} {}", RED, RESET, e),
        }
    }

    fn cmd_config(&self) {
        println!();
        println!("{}Configuration{}", BOLD, RESET);
        println!("{}{}{}", DIM, "-".repeat(50), RESET);
        println!("  {}Base URL:{} {}", CYAN, RESET, self.config.base_url);
        println!("  {}Model:{} {}", CYAN, RESET, self.config.model);
        let masked = if self.config.api_key.len() > 8 {
            format!(
                "{}...{}",
                &self.config.api_key[..4],
                &self.config.api_key[self.config.api_key.len() - 4..]
            )
        } else if self.config.api_key.is_empty() {
            "(not set)".into()
        } else {
            "****".into()
        };
        println!("  {}API Key:{} {}", CYAN, RESET, masked);
        println!(
            "  {}Tool Confirmation:{} {}",
            CYAN,
            RESET,
            self.confirm_display()
        );
        println!(
            "  {}Context Keep Turns:{} {}",
            CYAN, RESET, self.config.context_keep_turns
        );
        println!(
            "  {}Tool Timeout:{} {}s",
            CYAN, RESET, self.config.tool_timeout
        );
        println!("  {}User Agent:{} {}", CYAN, RESET, self.config.user_agent);
    }

    fn cmd_model(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!("{}Current model:{} {}", DIM, RESET, self.config.model);
            println!("{}Usage: /model <name>{}", DIM, RESET);
            return;
        }
        let old = self.config.model.clone();
        self.config.model = args[0].to_string();
        self.save_config();
        println!(
            "{}Model changed:{} {} -> {}{}{}",
            GREEN, RESET, old, BOLD, self.config.model, RESET
        );
    }

    fn cmd_models(&mut self) {
        let base_url = self.config.base_url.trim_end_matches('/');
        let url = format!("{}/models", base_url);
        println!("{}Fetching models from {}...{}", DIM, url, RESET);

        let api_key = self.config.api_key.clone();
        let user_agent = self.config.user_agent.clone();
        let current_model = self.config.model.clone();

        match self.rt.block_on(async {
            let client = reqwest::Client::new();
            let resp: serde_json::Value = client
                .get(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("api-key", &api_key)
                .header("User-Agent", &user_agent)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await?
                .json()
                .await?;
            Ok::<_, reqwest::Error>(resp)
        }) {
            Ok(data) => {
                let mut model_ids: Vec<String> = data["data"]
                    .as_array()
                    .map(|arr: &Vec<serde_json::Value>| {
                        arr.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                model_ids.sort();

                if model_ids.is_empty() {
                    println!("{}No models returned.{}", YELLOW, RESET);
                    return;
                }

                println!();
                println!("{}Available Models{}", BOLD, RESET);
                println!("{}{}{}", DIM, "-".repeat(50), RESET);
                for mid in &model_ids {
                    let marker = if *mid == current_model {
                        format!(" {}(current){}", GREEN, RESET)
                    } else {
                        String::new()
                    };
                    println!("  {}{}{}{}", CYAN, mid, RESET, marker);
                }
            }
            Err(e) => println!("{}Error fetching models:{} {}", RED, RESET, e),
        }
    }

    fn cmd_list(&self) {
        let chats = chat_manager::load_chats();
        if chats.is_empty() {
            println!("{}No saved chats.{}", DIM, RESET);
            return;
        }

        println!();
        println!("{}Chat History{}", BOLD, RESET);
        println!("{}{}{}", DIM, "-".repeat(80), RESET);
        println!(
            "  {}{:<4} {:<30} {:>6}  {:<30}{}",
            DIM, "#", "Title", "Msgs", "Preview", RESET
        );

        let current_id = self
            .current_chat
            .as_ref()
            .map(|c| c.id.as_str())
            .unwrap_or("");

        for (i, chat) in chats.iter().enumerate() {
            let prefix = if chat.id == current_id { ">" } else { " " };
            let preview = chat.messages.iter()
                .find(|m| m.role == "user")
                .and_then(|m| m.content.as_ref())
                .and_then(|v| v.as_str())
                .map(|s| truncate(s, 28))
                .unwrap_or_default();
            println!(
                "  {}{:<4} {:<30} {:>6}  {}",
                prefix,
                i + 1,
                truncate(&chat.title, 28),
                chat.messages.len(),
                preview,
            );
        }
    }

    fn cmd_load(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!(
                "{}Usage: /load <index>  (use /list to see indices){}",
                DIM, RESET
            );
            return;
        }
        let idx: usize = match args[0].parse::<usize>() {
            Ok(i) if i >= 1 => i - 1,
            _ => {
                println!(
                    "{}Invalid index. Use /list to see available chats.{}",
                    RED, RESET
                );
                return;
            }
        };

        let chats = chat_manager::load_chats();
        if idx >= chats.len() {
            println!("{}Index out of range.{}", RED, RESET);
            return;
        }

        if let Some(ref chat) = self.current_chat {
            chat_manager::save_chat(chat).ok();
        }

        self.current_chat = Some(chats[idx].clone());
        let c = self.current_chat.as_ref().unwrap();
        println!(
            "{}Loaded:{} {}{}{} {}({} messages){}",
            GREEN, RESET, BOLD, c.title, RESET, DIM, c.messages.len(), RESET
        );
        // Show tail for context
        self.cmd_tail(&["3"]);
    }

    fn cmd_delete(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!(
                "{}Usage: /delete <index>  (use /list to see indices){}",
                DIM, RESET
            );
            return;
        }
        let idx: usize = match args[0].parse::<usize>() {
            Ok(i) if i >= 1 => i - 1,
            _ => {
                println!("{}Invalid index.{}", RED, RESET);
                return;
            }
        };

        let chats = chat_manager::load_chats();
        if idx >= chats.len() {
            println!("{}Index out of range.{}", RED, RESET);
            return;
        }

        let target = &chats[idx];
        let title = target.title.clone();
        let is_current = self
            .current_chat
            .as_ref()
            .map(|c| c.id == target.id)
            .unwrap_or(false);

        chat_manager::delete_chat(&target.id).ok();
        println!("{}Deleted:{} {}{}{}", GREEN, RESET, BOLD, title, RESET);

        if is_current {
            let remaining = chat_manager::load_chats();
            if !remaining.is_empty() {
                self.current_chat = Some(remaining[0].clone());
                println!(
                    "{}Loaded:{} {}{}{}",
                    DIM,
                    RESET,
                    BOLD,
                    self.current_chat.as_ref().unwrap().title,
                    RESET
                );
            } else {
                self.current_chat = Some(chat_manager::create_chat("New Chat").unwrap());
                println!("{}New chat created.{}", DIM, RESET);
            }
        }
    }

    fn cmd_yolo(&mut self, args: &[&str]) {
        let modes = ["all", "safe", "none"];
        let current = &self.config.tool_confirmation;
        let new_mode = if !args.is_empty() && modes.contains(&args[0].to_lowercase().as_str()) {
            args[0].to_lowercase()
        } else {
            let idx = modes.iter().position(|m| m == current).unwrap_or(2);
            modes[(idx + 1) % 3].to_string()
        };

        self.config.tool_confirmation = new_mode;
        self.save_config();
        println!(
            "{}Tool Confirmation:{} {}{}{}",
            GREEN,
            RESET,
            BOLD,
            self.confirm_display(),
            RESET
        );
    }

    fn cmd_baseurl(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!("{}Current base URL:{} {}", DIM, RESET, self.config.base_url);
            println!("{}Usage: /baseurl <url>{}", DIM, RESET);
            return;
        }
        let old = self.config.base_url.clone();
        self.config.base_url = args[0].to_string();
        self.save_config();
        println!(
            "{}Base URL changed:{} {} -> {}{}{}",
            GREEN, RESET, old, BOLD, self.config.base_url, RESET
        );
    }

    fn cmd_apikey(&mut self, args: &[&str]) {
        if args.is_empty() {
            let masked = if self.config.api_key.len() > 8 {
                format!(
                    "{}...{}",
                    &self.config.api_key[..4],
                    &self.config.api_key[self.config.api_key.len() - 4..]
                )
            } else if self.config.api_key.is_empty() {
                "(not set)".into()
            } else {
                "****".into()
            };
            println!("{}Current API key:{} {}", DIM, RESET, masked);
            println!("{}Usage: /apikey <key>{}", DIM, RESET);
            return;
        }
        self.config.api_key = args[0].to_string();
        self.save_config();
        println!("{}API key updated.{}", GREEN, RESET);
    }

    fn cmd_timeout(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!(
                "{}Current timeout:{} {}s",
                DIM, RESET, self.config.tool_timeout
            );
            println!("{}Usage: /timeout <seconds>{}", DIM, RESET);
            return;
        }
        match args[0].parse::<u64>() {
            Ok(secs) if secs >= 1 => {
                let old = self.config.tool_timeout;
                self.config.tool_timeout = secs;
                self.save_config();
                *tools::TOOL_TIMEOUT.lock().unwrap() = secs;
                println!(
                    "{}Timeout changed:{} {}s -> {}{}s{}",
                    GREEN, RESET, old, BOLD, secs, RESET
                );
            }
            _ => println!("{}Invalid number. Usage: /timeout <seconds>{}", RED, RESET),
        }
    }

    fn cmd_agent(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!(
                "{}Current user agent:{} {}",
                DIM, RESET, self.config.user_agent
            );
            println!("{}Usage: /agent <string>{}", DIM, RESET);
            return;
        }
        let old = self.config.user_agent.clone();
        self.config.user_agent = args[0].to_string();
        self.save_config();
        *tools::USER_AGENT.lock().unwrap() = self.config.user_agent.clone();
        println!(
            "{}User agent changed:{} {} -> {}{}{}",
            GREEN, RESET, old, BOLD, self.config.user_agent, RESET
        );
    }

    fn cmd_context_keep(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!(
                "{}Current context keep turns:{} {}",
                DIM, RESET, self.config.context_keep_turns
            );
            println!(
                "{}Usage: /context-keep <turns> (0 = keep all){}",
                DIM, RESET
            );
            return;
        }
        match args[0].parse::<usize>() {
            Ok(turns) => {
                let old = self.config.context_keep_turns;
                self.config.context_keep_turns = turns;
                self.save_config();
                println!(
                    "{}Context keep turns changed:{} {} -> {}{}{}",
                    GREEN, RESET, old, BOLD, turns, RESET
                );
            }
            _ => println!(
                "{}Invalid number. Usage: /context-keep <turns>{}",
                RED, RESET
            ),
        }
    }

    fn cmd_system(&mut self, args: &[&str]) {
        if !args.is_empty() {
            let new_template = args.join(" ");
            self.config.system_message = new_template.clone();
            self.save_config();
            let rendered = config::render_system_message(&new_template);
            println!("{}System message updated.{}", GREEN, RESET);
            println!();
            println!("{}Template:{} {}", BOLD, RESET, new_template);
            println!("{}Rendered:{} {}", BOLD, RESET, rendered);
            return;
        }

        let rendered = if self.config.system_message.is_empty() {
            "(no system message)".to_string()
        } else {
            config::render_system_message(&self.config.system_message)
        };

        println!();
        println!("{}Template:{} {}", BOLD, RESET, self.config.system_message);
        println!("{}Rendered:{} {}", BOLD, RESET, rendered);
    }

    fn cmd_attach(&self) {
        println!("{}File attachment:{}", BOLD, RESET);
        println!(
            "  Use {}@path/to/file{} anywhere in your message to attach a text file.",
            CYAN, RESET
        );
        println!(
            "  Example: {}Look at @src/main.rs and fix the bug{}",
            DIM, RESET
        );
    }

    fn cmd_compact(&mut self) {
        let chat = match self.current_chat.as_mut() {
            Some(c) => c,
            None => {
                println!("{}No active chat.{}", DIM, RESET);
                return;
            }
        };
        let turns = if self.config.context_keep_turns == 0 {
            3
        } else {
            self.config.context_keep_turns
        };
        let old_count = chat.messages.len();
        chat.messages = chat_manager::elide_old_tool_results(&chat.messages, turns);
        let new_count = chat.messages.len();
        println!(
            "{}Compacted:{} elided tool results older than {} turns. ({} -> {} messages)",
            GREEN, RESET, turns, old_count, new_count
        );
        chat_manager::save_chat(chat).ok();
    }

    // ── Helpers ──────────────────────────────────────────────────

    fn confirm_display(&self) -> &str {
        match self.config.tool_confirmation.as_str() {
            "all" => "YOLO",
            "safe" => "Safe",
            _ => "None",
        }
    }

    fn save_config(&self) {
        config::save_config(&self.config).ok();
    }

    fn prompt(&mut self, prompt_str: &str) -> Option<String> {
        match self.rl.readline(prompt_str) {
            Ok(line) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    let _ = self.rl.add_history_entry(trimmed);
                }
                Some(line)
            }
            Err(rustyline::error::ReadlineError::Eof | rustyline::error::ReadlineError::Interrupted) => None,
            Err(_) => None,
        }
    }
}

// ── Free functions ────────────────────────────────────────────────

fn dirs_next() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|w| *w >= MIN_PANEL_WIDTH)
        .unwrap_or(120)
}

fn panel_width(requested: Option<usize>) -> usize {
    requested.unwrap_or_else(|| terminal_width().saturating_sub(2))
        .clamp(MIN_PANEL_WIDTH, MAX_PANEL_WIDTH)
}

fn visual_width(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '\u{1F300}'..='\u{1FAFF}' | '\u{2600}'..='\u{27BF}' => 2,
            '\u{1100}'..='\u{115F}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}' => 2,
            _ => 1,
        })
        .sum()
}

fn pad_to_width(s: &str, width: usize) -> String {
    let used = visual_width(s);
    if used >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - used))
    }
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn take_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in line.split_inclusive(' ') {
        let word_width = visual_width(word);
        if current_width > 0 && current_width + word_width > width {
            out.push(current.trim_end().to_string());
            current.clear();
            current_width = 0;
        }

        if word_width > width {
            for ch in word.chars() {
                let ch_width = visual_width(&ch.to_string());
                if current_width > 0 && current_width + ch_width > width {
                    out.push(current.trim_end().to_string());
                    current.clear();
                    current_width = 0;
                }
                current.push(ch);
                current_width += ch_width;
            }
        } else {
            current.push_str(word);
            current_width += word_width;
        }
    }

    if !current.is_empty() {
        out.push(current.trim_end().to_string());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn print_box(title: &str, blocks: &[String], requested_width: Option<usize>) {
    let width = panel_width(requested_width);
    let inner = width.saturating_sub(4);
    let title_width = visual_width(title);

    if title.is_empty() {
        println!("╭{}╮", "─".repeat(width.saturating_sub(2)));
    } else {
        let dash_count = width.saturating_sub(title_width + 6);
        println!("╭─ {}  {}╮", title, "─".repeat(dash_count));
    }

    for block in blocks {
        for raw_line in block.lines() {
            for line in wrap_line(raw_line, inner) {
                println!("│ {} │", pad_to_width(&line, inner));
            }
        }
        if block.ends_with('\n') {
            println!("│ {} │", " ".repeat(inner));
        }
    }

    println!("╰{}╯", "─".repeat(width.saturating_sub(2)));
}

fn build_messages(chat: &Chat, config: &Config) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if !config.system_message.is_empty() {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String(config::render_system_message(
                &config.system_message,
            ))),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
        });
    }
    let raw = chat_manager::clean_dangling_tool_calls(&chat.messages);
    let raw = chat_manager::elide_old_tool_results(&raw, config.context_keep_turns);
    messages.extend(raw);
    messages
}

fn truncate(text: &str, max_len: usize) -> String {
    let preview = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = preview.chars().count();
    if count <= max_len {
        preview
    } else {
        let keep = max_len.saturating_sub(3);
        format!("{}...", preview.chars().take(keep).collect::<String>())
    }
}

fn resolve_attachments(text: &str) -> (String, String) {
    let re = regex::Regex::new(r"@(\S+)").unwrap();
    let mut resolved = text.to_string();
    let mut blocks = Vec::new();

    for cap in re.captures_iter(text) {
        let raw = &cap[1];
        let path_str = raw.trim_end_matches(|c: char| ",;:.!?)]}".contains(c));
        if path_str.is_empty() {
            continue;
        }
        let path = Path::new(path_str);
        if path.exists() && path.is_file() {
            if let Ok(content) = std::fs::read_to_string(path) {
                blocks.push(format!(
                    "[File: {}]\n```\n{}\n```",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(path_str),
                    content
                ));
                resolved = resolved.replacen(&cap[0], "", 1);
            }
        }
    }

    (resolved.trim().to_string(), blocks.join("\n\n"))
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn chrono_now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

// ── Markdown-to-terminal renderer ───────────────────────────────────

fn render_markdown_terminal(text: &str) -> String {
    let mut out = String::new();
    let mut in_code_block = false;
    let mut in_list = false;
    let mut list_num = 0u32;
    let lines: Vec<&str> = text.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("```") {
            if in_code_block {
                in_code_block = false;
                out.push_str(&format!("{}{}\n", DIM, RESET));
            } else {
                if in_list {
                    out.push_str(&format!("{}\n", RESET));
                    in_list = false;
                }
                in_code_block = true;
                out.push_str(&format!("{}", DIM));
            }
            continue;
        }

        if in_code_block {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        let trimmed = line.trim();

        if trimmed.is_empty() {
            if in_list {
                out.push_str(&format!("{}\n", RESET));
                in_list = false;
            }
            if i + 1 < lines.len() && !lines[i + 1].trim().is_empty() {
                out.push('\n');
            }
            continue;
        }

        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            if in_list {
                out.push_str(&format!("{}\n", RESET));
                in_list = false;
            }
            out.push_str(&format!("{}{}{}\n", DIM, "─".repeat(terminal_width().min(60)), RESET));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("### ") {
            if in_list { out.push_str(&format!("{}\n", RESET)); in_list = false; }
            out.push_str(&format!("{}{}{}\n\n", BOLD, render_inline(rest), RESET));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if in_list { out.push_str(&format!("{}\n", RESET)); in_list = false; }
            out.push_str(&format!("{}{}{}\n\n", BOLD, render_inline(rest), RESET));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            if in_list { out.push_str(&format!("{}\n", RESET)); in_list = false; }
            out.push_str(&format!("{}{}{}\n\n", BOLD, render_inline(rest), RESET));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ") {
            if in_list { out.push_str(&format!("{}\n", RESET)); in_list = false; }
            out.push_str(&format!("  {}│{} {}\n", DIM, RESET, render_inline(rest)));
            continue;
        }
        if trimmed == ">" {
            if in_list { out.push_str(&format!("{}\n", RESET)); in_list = false; }
            out.push_str(&format!("  {}│{}\n", DIM, RESET));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            if !in_list {
                in_list = true;
            }
            out.push_str(&format!("  {}•{} {}\n", CYAN, RESET, render_inline(rest)));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("1. ") {
            if !in_list { in_list = true; list_num = 0; }
            list_num += 1;
            out.push_str(&format!("  {}{}.{} {}\n", CYAN, list_num, RESET, render_inline(rest)));
            continue;
        }
        if let Some(dot_pos) = trimmed.find(". ") {
            let prefix = &trimmed[..dot_pos];
            if prefix.chars().all(|c| c.is_ascii_digit()) && !prefix.is_empty() {
                if !in_list { in_list = true; list_num = 0; }
                list_num += 1;
                let rest = &trimmed[dot_pos + 2..];
                out.push_str(&format!("  {}{}.{} {}\n", CYAN, list_num, RESET, render_inline(rest)));
                continue;
            }
        }

        if in_list {
            out.push_str(&format!("{}\n", RESET));
            in_list = false;
        }
        out.push_str(&render_inline(trimmed));
        out.push('\n');
    }

    if in_code_block {
        out.push_str(&format!("{}\n", RESET));
    }
    if in_list {
        out.push_str(&format!("{}\n", RESET));
    }

    out.trim_end_matches('\n').to_string() + "\n"
}

fn render_inline(text: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                let code: String = chars[i + 1..i + 1 + end].iter().collect();
                result.push_str(&format!("{}{}{}{}{}", DIM, CYAN, code, RESET, RESET));
                i += end + 2;
                continue;
            }
        }

        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_double(&chars, i + 2, '*') {
                let inner: String = chars[i + 2..end].iter().collect();
                result.push_str(&format!("{}{}{}", BOLD, render_inline(&inner), RESET));
                i = end + 2;
                continue;
            }
        }

        if chars[i] == '*' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '*') {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                result.push_str(&format!("{}\x1b[3m{}{}", RESET, render_inline(&inner), RESET));
                i += end + 2;
                continue;
            }
        }

        if chars[i] == '[' {
            if let Some(bracket_end) = chars[i + 1..].iter().position(|&c| c == ']') {
                let after = i + 1 + bracket_end + 1;
                if after < chars.len() && chars[after] == '(' {
                    if let Some(paren_end) = chars[after + 1..].iter().position(|&c| c == ')') {
                        let link_text: String = chars[i + 1..i + 1 + bracket_end].iter().collect();
                        let url: String = chars[after + 1..after + 1 + paren_end].iter().collect();
                        result.push_str(&format!("{}{}{} ({})", CYAN, link_text, RESET, url));
                        i = after + 1 + paren_end + 1;
                        continue;
                    }
                }
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

fn find_double(chars: &[char], from: usize, ch: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == ch && chars[i + 1] == ch {
            return Some(i);
        }
        i += 1;
    }
    None
}
