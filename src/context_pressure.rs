//! Text-only, provider-usage-calibrated pressure. No tokenizer/model defaults.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

#[derive(Clone, Debug, Default)]
pub struct Pressure {
    pub sample: Option<(usize, u64)>,
    rearm: Option<u64>,
    identity: String,
    prefix: Vec<String>,
    window: u64,
}
static SAMPLES: LazyLock<Mutex<HashMap<String, Pressure>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
fn hashes(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|m| format!("{:x}", Sha256::digest(m.to_string().as_bytes())))
        .collect()
}
impl Pressure {
    pub fn load(key: &str, messages: &[Value], identity: &str, window: u64) -> Self {
        if !key.is_empty() {
            if let Some(p) = SAMPLES.lock().unwrap().get(key).cloned() {
                if p.identity == identity
                    && p.window == window
                    && hashes(messages).starts_with(&p.prefix)
                {
                    return p;
                }
            }
        }
        Self {
            identity: identity.into(),
            window,
            ..Self::default()
        }
    }
    pub fn set_window(&mut self, window: u64) {
        if self.window != window {
            self.rearm = None;
            self.window = window;
        }
    }
    pub fn record(&mut self, chars: Option<usize>, tokens: Option<u64>) {
        if let (Some(c), Some(t)) = (chars, tokens) {
            if c > 0 && t > 0 {
                self.sample = Some((c, t));
            }
        }
    }
    pub fn save(&mut self, key: &str, messages: &[Value], identity: &str) {
        self.identity = identity.into();
        self.prefix = hashes(messages);
        if !key.is_empty() {
            let mut cache = SAMPLES.lock().unwrap();
            if cache.len() >= 256 && !cache.contains_key(key) {
                cache.clear();
            }
            cache.insert(key.into(), self.clone());
        }
    }
    pub fn estimate(&self, chars: Option<usize>) -> Option<u64> {
        let (last_chars, tokens) = self.sample?;
        if last_chars == 0 {
            return None;
        }
        let n = (chars? as u128 * tokens as u128).div_ceil(last_chars as u128);
        Some(n.min(u64::MAX as u128) as u64)
    }
    pub fn should_compact(&self, estimate: Option<u64>, threshold: u64, hard_budget: u64) -> bool {
        let Some(n) = estimate else {
            return false;
        };
        self.window > 0 && n >= threshold && (n >= hard_budget || self.rearm.is_none_or(|r| n >= r))
    }
    pub fn compacted(&mut self, after: Option<u64>, threshold: u64) {
        self.rearm = after
            .filter(|n| *n < threshold)
            .map(|_| threshold.saturating_add(self.window.div_ceil(20)));
    }
}
/// Count actual input text, keys/structure included consistently, never base64 media.
/// Returning unknown for media avoids learning a bogus text density from vision tokens.
pub fn input_chars(messages: &Value, tools: &Value) -> Option<usize> {
    fn count(v: &Value) -> Option<usize> {
        match v {
            Value::String(s) => Some(s.chars().count()),
            Value::Array(a) => a
                .iter()
                .try_fold(0usize, |n, v| Some(n.saturating_add(count(v)?))),
            Value::Object(o) => {
                if o.contains_key("image_url")
                    || o.contains_key("input_audio")
                    || o.contains_key("file")
                {
                    return None;
                }
                o.iter().try_fold(0usize, |n, (k, v)| {
                    Some(
                        n.saturating_add(k.chars().count())
                            .saturating_add(count(v)?),
                    )
                })
            }
            Value::Null => Some(0),
            _ => Some(v.to_string().chars().count()),
        }
    }
    Some(count(messages)?.saturating_add(count(tools)?))
}
pub fn budgets(window: u64, percent: u64, output: u64, input_only: bool) -> (u64, u64) {
    let hard = if input_only {
        window
    } else {
        window.saturating_sub(output)
    };
    (
        ((window as u128 * percent as u128 / 100).min(hard as u128)) as u64,
        hard,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn shared_measured_density_fixtures() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/context_pressure.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let mut p = Pressure::default();
            if let Some(a) = case["sample"].as_array() {
                p.record(Some(a[0].as_u64().unwrap() as usize), a[1].as_u64());
            }
            assert_eq!(
                p.estimate(case["next_chars"].as_u64().map(|n| n as usize)),
                case["expected"].as_u64(),
                "{}",
                case["name"]
            );
        }
    }
    #[test]
    fn arithmetic_edges_reserve_and_unknown() {
        let mut p = Pressure {
            window: 262144,
            ..Pressure::default()
        };
        assert_eq!(p.estimate(Some(100)), None);
        p.record(Some(400), Some(200));
        assert_eq!(p.estimate(Some(401)), Some(201));
        assert_eq!(p.estimate(Some(200)), Some(100));
        let (at, hard) = budgets(262144, 75, 0, false);
        assert_eq!(at, 196608);
        assert!(!p.should_compact(Some(at - 1), at, hard));
        assert!(p.should_compact(Some(at), at, hard));
        p.record(None, Some(0));
        assert_eq!(p.sample, Some((400, 200)));
        assert_eq!(budgets(262144, 75, 100000, false), (162144, 162144));
        assert_eq!(budgets(262144, 75, 100000, true), (196608, 262144));
        p.window = 0;
        assert!(!p.should_compact(Some(at), at, hard));
    }
    #[test]
    fn hysteresis_and_still_over_threshold() {
        let mut p = Pressure {
            window: 262144,
            ..Pressure::default()
        };
        let (at, hard) = budgets(p.window, 75, 0, false);
        p.compacted(Some(at - 1), at);
        assert!(!p.should_compact(Some(at), at, hard));
        assert!(!p.should_compact(Some(at + 13107), at, hard));
        assert!(p.should_compact(Some(at + 13108), at, hard));
        assert!(p.should_compact(Some(hard), at, hard));
        p.compacted(Some(at), at);
        assert!(p.should_compact(Some(at), at, hard));
    }
    #[test]
    fn actual_text_schema_arguments_unicode_and_media() {
        let m = json!([{"role":"user","content":"🐧\n"}]);
        assert_eq!(input_chars(&m, &json!(null)), Some(17));
        assert!(input_chars(&m, &json!({"description":"lots of schema"})).unwrap() > 17);
        assert_eq!(
            input_chars(&json!([{"image_url":{"url":"data:base64"}}]), &json!(null)),
            None
        );
    }
    #[test]
    fn samples_survive_turns_but_not_edits_or_route_view_changes() {
        let key = "pressure-test-isolated";
        let mut messages = vec![json!({"role":"user","content":"old"})];
        let mut p = Pressure::load(key, &messages, "view", 262144);
        p.record(Some(400), Some(100));
        p.save(key, &messages, "view");
        messages.push(json!({"role":"assistant","content":"answer"}));
        assert_eq!(
            Pressure::load(key, &messages, "view", 262144).sample,
            Some((400, 100))
        );
        assert!(Pressure::load(key, &messages, "different", 262144)
            .sample
            .is_none());
        messages[0]["content"] = json!("edited");
        assert!(Pressure::load(key, &messages, "view", 262144)
            .sample
            .is_none());
    }
}
