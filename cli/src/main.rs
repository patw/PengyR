use pengy_core::chat_manager::{self, Chat, ChatMessage};
use pengy_core::config::{self, Config};
use pengy_core::llm_client::{self, Confirmation, LlmEvent, ToolConfirmation};
use pengy_core::tools;

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
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let no_save = args.iter().any(|a| a == "--no-save");
    let prompt_args: Vec<&str> = args[1..]
        .iter()
        .filter(|a| *a != "--no-save")
        .map(|s| s.as_str())
        .collect();

    let mut cli = PengyCli::new(no_save);

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
    rt: tokio::runtime::Runtime,
}

impl PengyCli {
    fn new(no_save: bool) -> Self {
        let config = config::load_config();
        *tools::USER_AGENT.lock().unwrap() = config.user_agent.clone();
        *tools::TOOL_TIMEOUT.lock().unwrap() = config.tool_timeout;

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

        Self {
            config,
            current_chat: None,
            no_save,
            yolo_this_turn: false,
            rt,
        }
    }

    fn run_interactive(&mut self) {
        println!();
        println!(
            "{}{}Pengy CLI{}",
            BOLD, BLUE, RESET
        );
        println!(
            "Type your message and press Enter.  {}Try /help for available commands.{}",
            DIM, RESET
        );

        let chats = chat_manager::load_chats();
        if !chats.is_empty() {
            self.current_chat = Some(chats[0].clone());
            println!(
                "{}Resumed chat:{} {}{}{}",
                DIM, RESET, BOLD, self.current_chat.as_ref().unwrap().title, RESET
            );
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
            let line = match self.prompt(&format!("\n{}{}You{} ", BOLD, BLUE, RESET)) {
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
            });

            if chat.title == "New Chat" {
                chat.title = truncate(&final_text, 50);
            }

            self.drive_chat();
        }

        self.clear_sudo_provider();
        println!("\n{}Goodbye!{}", DIM, RESET);
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
        let cancel2 = cancel.clone();

        self.rt.spawn(async move {
            llm_client::chat(&bu, &ak, &md, messages, tc_mode, event_tx, confirm_rx, cancel2)
                .await;
        });

        self.yolo_this_turn = false;
        eprint!("{}Thinking...{}", DIM, RESET);

        let mut expecting_api = true;

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
                        && !(tc_mode == ToolConfirmation::Safe
                            && tools::is_readonly_tool(&name))
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
                            _ => {
                                let _ = confirm_tx.send(Confirmation {
                                    tool_call_id,
                                    confirmed: false,
                                    yolo_turn: false,
                                });
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
                        });
                    eprint!("{}Thinking...{}", DIM, RESET);
                }
                Some(LlmEvent::FinalResponse { content, usage }) => {
                    if expecting_api {
                        eprint!("\r{}\r", " ".repeat(40));
                    }
                    self.render_final(&content, &usage);
                    self.current_chat
                        .as_mut()
                        .unwrap()
                        .messages
                        .push(ChatMessage {
                            role: "assistant".into(),
                            content: Some(serde_json::Value::String(content)),
                            tool_calls: vec![],
                            tool_call_id: None,
                        });
                    if !self.no_save {
                        chat_manager::save_chat(self.current_chat.as_ref().unwrap()).ok();
                    }
                    break;
                }
                None => {
                    eprint!("\r{}\r", " ".repeat(40));
                    println!("{}Chat ended unexpectedly.{}", RED, RESET);
                    break;
                }
            }
        }
    }

    // ── Rendering ────────────────────────────────────────────────

    fn render_tool_request(&self, name: &str, args: &serde_json::Value) {
        let args_str = serde_json::to_string_pretty(args).unwrap_or_default();
        let preview = args
            .as_object()
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| {
                        let vs = match v {
                            serde_json::Value::String(s) => {
                                if s.len() > 30 {
                                    format!("{}...", &s[..27])
                                } else {
                                    s.clone()
                                }
                            }
                            other => other.to_string(),
                        };
                        format!("{}={}", k, vs)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();

        println!();
        println!(
            "{}--- Tool: {}{}{} [{}] ---{}",
            YELLOW, BOLD, name, RESET, truncate(&preview, 60), RESET
        );
        if args_str.len() <= 2000 {
            println!("{}{}{}", DIM, args_str, RESET);
        } else {
            println!("{}{}{}", DIM, &args_str[..2000], RESET);
            println!("{}[... truncated]{}", DIM, RESET);
        }
    }

    fn render_tool_result(&self, content: &str, declined: bool) {
        if declined {
            println!("{}Declined{}", RED, RESET);
            return;
        }

        let display = if content.len() > 2000 {
            format!("{}\n\n[... truncated ...]", &content[..2000])
        } else {
            content.to_string()
        };

        println!(
            "{}--- Output ---{}",
            DIM, RESET
        );
        println!("{}", display);
    }

    fn render_final(&self, content: &str, usage: &llm_client::Usage) {
        if content.trim().is_empty() {
            println!("{}(empty response){}", DIM, RESET);
        } else {
            println!();
            println!(
                "{}--- Assistant ---{}",
                GREEN, RESET
            );
            println!("{}", content);
        }

        if usage.total_tokens > 0 {
            println!(
                "{}Tokens: {} in / {} out ({} total){}",
                DIM, usage.prompt_tokens, usage.completion_tokens, usage.total_tokens, RESET
            );
        }
    }

    // ── Tool confirmation ────────────────────────────────────────

    fn prompt_tool_confirmation(&self) -> u8 {
        loop {
            let input = self.prompt(&format!(
                "  [1] Execute  [2] Yes to all this turn  [3] Decline  {}[1/2/3]{} ",
                BOLD, RESET
            ));
            match input.as_deref().unwrap_or("1").trim() {
                "1" | "" => return 1,
                "2" => return 2,
                "3" => return 3,
                _ => println!("{}Please enter 1, 2, or 3.{}", RED, RESET),
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
        println!("{}{}{}",
            DIM,
            "-".repeat(60),
            RESET
        );
        let cmds = [
            ("/help", "Show this help"),
            ("/new", "Start a new chat"),
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
        println!("{}New chat created.{}", GREEN, RESET);
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
        println!("{}{}{}", DIM, "-".repeat(70), RESET);
        println!(
            "  {}{:<4} {:<40} {:>6}  {}{}",
            DIM, "#", "Title", "Msgs", "Created", RESET
        );

        let current_id = self
            .current_chat
            .as_ref()
            .map(|c| c.id.as_str())
            .unwrap_or("");

        for (i, chat) in chats.iter().enumerate() {
            let prefix = if chat.id == current_id { ">" } else { " " };
            let created = if chat.created_at.len() >= 16 {
                chat.created_at[..16].replace('T', " ")
            } else {
                chat.created_at.clone()
            };
            println!(
                "  {}{:<4} {:<40} {:>6}  {}",
                prefix,
                i + 1,
                truncate(&chat.title, 38),
                chat.messages.len(),
                created
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
                println!("{}Invalid index. Use /list to see available chats.{}", RED, RESET);
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
            "{}Loaded:{} {}{}{} ({} messages)",
            GREEN, RESET, BOLD, c.title, RESET, c.messages.len()
        );
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
            GREEN, RESET, BOLD, self.confirm_display(), RESET
        );
    }

    fn cmd_baseurl(&mut self, args: &[&str]) {
        if args.is_empty() {
            println!(
                "{}Current base URL:{} {}",
                DIM, RESET, self.config.base_url
            );
            println!(
                "{}Usage: /baseurl <url>{}",
                DIM, RESET
            );
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
            _ => println!(
                "{}Invalid number. Usage: /timeout <seconds>{}",
                RED, RESET
            ),
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
            _ => "Confirm",
        }
    }

    fn save_config(&self) {
        config::save_config(&self.config).ok();
    }

    fn prompt(&self, prompt: &str) -> Option<String> {
        print!("{}", prompt);
        io::stdout().flush().ok();
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end_matches('\n').trim_end_matches('\r').to_string()),
            Err(_) => None,
        }
    }
}

// ── Free functions ────────────────────────────────────────────────

fn build_messages(chat: &Chat, config: &Config) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if !config.system_message.is_empty() {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String(
                config::render_system_message(&config.system_message),
            )),
            tool_calls: vec![],
            tool_call_id: None,
        });
    }
    let raw = chat_manager::clean_dangling_tool_calls(&chat.messages);
    let raw = chat_manager::elide_old_tool_results(&raw, config.context_keep_turns);
    messages.extend(raw);
    messages
}

fn truncate(text: &str, max_len: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text);
    if first_line.len() <= max_len {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..max_len.saturating_sub(3)])
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
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
}
