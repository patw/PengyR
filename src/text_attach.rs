//! Size limits for text files attached to a prompt.
//!
//! Attached text goes into the prompt verbatim, so its size is paid for in
//! context: a file that fits the window is a normal turn, a file that doesn't is
//! a context-limit error.  Bytes-to-tokens is not a constant across tokenizers,
//! so the conversion is deliberately pessimistic: measured against the fleet's
//! own engine (Swift 1.5 on basalt), 40 KB of prose tokenised at 4.13
//! bytes/token and 40 KB of Python source at 3.97, and denser tokenizers exist.
//! 3.5 bytes/token is used here, so a file deemed safe stays safe on a tokenizer
//! roughly 15% hungrier than the measured ones.
//!
//! The budget is a share of the model's own context window, which Pengy tracks
//! per model (`context_window`, `context_windows`).  A 95%-of-context attachment
//! on a 256K window (243K tokens, ~950 KB) leaves ~13K tokens for the system
//! prompt, the tool definitions, the chat history and the reply: that is a
//! guaranteed context-limit error, not a usable turn, so a single file gets a
//! quarter of the window.

use crate::config::Config;

/// Conservative: measured 3.97 (source) to 4.13 (prose) bytes/token.
pub const BYTES_PER_TOKEN: f64 = 3.5;
/// Share of the model's context window one attached file may take.
pub const CONTEXT_SHARE: f64 = 0.25;
/// Context window assumed when the model's is unknown (`context_window: 0`).
pub const UNKNOWN_CONTEXT_TOKENS: u64 = 256_000;
/// Above this a file is not prompt input at all: refuse it at attach time.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Floor and ceiling on the per-file embed budget.
pub const MIN_EMBED_BYTES: usize = 32 * 1024;
pub const MAX_EMBED_BYTES: usize = 1024 * 1024;
/// One message may carry several files; the total is this many per-file budgets.
pub const TOTAL_BUDGETS: usize = 2;

/// The context window Pengy has for `model`, falling back to 256K.
pub fn context_tokens_for(config: &Config, model: &str) -> u64 {
    let key = if model.is_empty() { config.model.as_str() } else { model };
    let per_model = config.context_windows.get(key).copied().unwrap_or(0);
    let window = if per_model > 0 { per_model } else { config.context_window };
    if window > 0 { window } else { UNKNOWN_CONTEXT_TOKENS }
}

/// Bytes one attached text file may contribute to the prompt.
pub fn embed_budget_bytes(context_tokens: u64) -> usize {
    let tokens = if context_tokens > 0 { context_tokens } else { UNKNOWN_CONTEXT_TOKENS };
    let budget = (tokens as f64 * BYTES_PER_TOKEN * CONTEXT_SHARE) as usize;
    budget.clamp(MIN_EMBED_BYTES, MAX_EMBED_BYTES)
}

/// Per-file budget for the model in play (empty `model` means the configured one).
pub fn budget_for_model(config: &Config, model: &str) -> usize {
    embed_budget_bytes(context_tokens_for(config, model))
}

/// Budget for all text attachments in a single message.
pub fn total_budget_bytes(context_tokens: u64) -> usize {
    embed_budget_bytes(context_tokens) * TOTAL_BUDGETS
}

/// Why a file of `size_bytes` is refused, or `None` if it is acceptable.
pub fn oversized_reason(size_bytes: u64) -> Option<String> {
    if size_bytes <= MAX_FILE_BYTES {
        return None;
    }
    Some(format!(
        "{:.1} MB is over the {} KB attachment limit",
        size_bytes as f64 / (1024.0 * 1024.0),
        MAX_FILE_BYTES / 1024
    ))
}

/// Same cap as `read_text_attachment`, for bytes already in hand.  Content is cut
/// on a byte boundary and decoded lossily; the note announces the cut, because
/// the model is being asked about less than the file the user attached.
pub fn read_text_attachment_bytes(data: &[u8], budget: usize) -> (String, String) {
    if data.len() <= budget {
        return (String::from_utf8_lossy(data).into_owned(), String::new());
    }
    let head = String::from_utf8_lossy(&data[..budget]).into_owned();
    let note = format!(
        "\n[... {} of {} bytes embedded; the rest is truncated, not omitted silently]",
        budget,
        data.len()
    );
    (head, note)
}

/// Read a file for embedding under `budget`.
pub fn read_text_attachment(path: &std::path::Path, budget: usize) -> std::io::Result<(String, String)> {
    let data = std::fs::read(path)?;
    Ok(read_text_attachment_bytes(&data, budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_a_quarter_of_the_context_window() {
        assert_eq!(embed_budget_bytes(256_000), 224_000);
        assert_eq!(embed_budget_bytes(262_144), 229_376);
        assert_eq!(embed_budget_bytes(1_000_000), 875_000);
        assert_eq!(embed_budget_bytes(1), MIN_EMBED_BYTES);
        assert_eq!(embed_budget_bytes(8_000_000), MAX_EMBED_BYTES);
    }

    #[test]
    fn bytes_per_token_sits_below_the_measured_ratio() {
        // Measured on the fleet engine: 3.97 (source) .. 4.13 (prose).
        assert!(BYTES_PER_TOKEN < 3.97);
    }

    #[test]
    fn unknown_window_is_256k_not_zero() {
        let config = Config::default();
        assert_eq!(context_tokens_for(&config, "anything"), UNKNOWN_CONTEXT_TOKENS);
        assert!(config.context_window == 0);
        assert!(embed_budget_bytes(context_tokens_for(&config, "anything")) > 0);
    }

    #[test]
    fn per_model_window_wins() {
        let mut config = Config::default();
        config.context_window = 131_072;
        config.context_windows.insert("big".into(), 1_000_000);
        assert_eq!(context_tokens_for(&config, "big"), 1_000_000);
        assert_eq!(context_tokens_for(&config, "other"), 131_072);
        assert_eq!(budget_for_model(&config, "big"), embed_budget_bytes(1_000_000));
    }

    #[test]
    fn oversized_reason_bounds() {
        assert!(oversized_reason(MAX_FILE_BYTES).is_none());
        assert!(oversized_reason(MAX_FILE_BYTES + 1).is_some());
        assert!(oversized_reason(5 * 1024 * 1024).unwrap().contains("MB"));
    }

    #[test]
    fn attachment_is_capped_and_announces_it() {
        let data = vec![b'x'; MIN_EMBED_BYTES + 5_000];
        let (head, note) = read_text_attachment_bytes(&data, MIN_EMBED_BYTES);
        assert_eq!(head.len(), MIN_EMBED_BYTES);
        assert!(note.contains("truncated"));
        assert!(note.contains(&format!("{}", data.len())));
        let (whole, note) = read_text_attachment_bytes(b"hello", 1024);
        assert_eq!(whole, "hello");
        assert!(note.is_empty());
    }

    #[test]
    fn total_budget_covers_several_files() {
        assert_eq!(total_budget_bytes(256_000), 2 * embed_budget_bytes(256_000));
    }
}
