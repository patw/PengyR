//! Schema-driven validation and coercion of tool-call arguments.
//!
//! Models — small local ones especially — routinely send arguments that are
//! *almost* right: `"3"` for an integer, `"true"` for a boolean, a bare string
//! where a list is expected, or a key in the wrong place.  Before this module
//! each tool handler read only the keys it knew about, so a misplaced key
//! (`dry_run` inside a per-file change) or a mistyped value (`dry_run: "true"`)
//! was silently replaced by its default — and a "preview" wrote files.
//!
//! `normalize_arguments()` runs once in `execute_tool` before any handler:
//! unambiguous slips are coerced silently (numeric strings, integral floats,
//! `"true"`/`"false"`, one value where an array is expected, a JSON string where
//! an array/object is expected, enum values in the wrong case); `null` is an
//! absent key; anything else — an unknown key at any depth, a missing required
//! key, a value that can't be coerced — rejects the whole call with an error
//! naming the offending key.  Nothing is executed.
//!
//! Keep the semantics and messages identical to `pengy/core/tool_args.py` and
//! `Tools::normalizeArguments` in the C++ edition.

use serde_json::{Map, Value};
use std::collections::HashSet;

const RAW_KEY: &str = "_raw_arguments";

fn describe(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

fn preview(v: &Value) -> String {
    let text = serde_json::to_string(v).unwrap_or_default();
    if text.chars().count() <= 40 {
        text
    } else {
        format!("{}...", text.chars().take(37).collect::<String>())
    }
}

fn is_int_string(s: &str) -> bool {
    let t = s.strip_prefix(['+', '-']).unwrap_or(s);
    !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())
}

fn coerce(schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>, top: &HashSet<String>) -> Value {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => {
            let out = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
                _ => {
                    errors.push(format!("{at}: expected a string, got {} {}", describe(value), preview(value)));
                    return Value::Null;
                }
            };
            if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
                let names: Vec<&str> = choices.iter().filter_map(Value::as_str).collect();
                if !names.contains(&out.as_str()) {
                    let folded = out.trim().to_lowercase();
                    if let Some(hit) = names.iter().find(|c| c.to_lowercase() == folded) {
                        return Value::String((*hit).to_string());
                    }
                    errors.push(format!("{at}: must be one of {} (got {})", names.join(", "), preview(&Value::String(out))));
                    return Value::Null;
                }
            }
            Value::String(out)
        }
        Some("integer") => {
            match value {
                Value::Number(n) if n.is_i64() || n.is_u64() => return value.clone(),
                Value::Number(n) => {
                    if let Some(f) = n.as_f64() {
                        if f.fract() == 0.0 && f.is_finite() {
                            return Value::from(f as i64);
                        }
                    }
                }
                Value::String(s) if is_int_string(s.trim()) => {
                    if let Ok(i) = s.trim().parse::<i64>() {
                        return Value::from(i);
                    }
                }
                _ => {}
            }
            errors.push(format!("{at}: expected an integer, got {} {}", describe(value), preview(value)));
            Value::Null
        }
        Some("boolean") => {
            match value {
                Value::Bool(_) => return value.clone(),
                Value::String(s) => match s.trim().to_lowercase().as_str() {
                    "true" => return Value::Bool(true),
                    "false" => return Value::Bool(false),
                    _ => {}
                },
                Value::Number(n) if n.as_i64() == Some(0) || n.as_i64() == Some(1) => {
                    return Value::Bool(n.as_i64() == Some(1));
                }
                _ => {}
            }
            errors.push(format!("{at}: expected true or false, got {} {}", describe(value), preview(value)));
            Value::Null
        }
        Some("array") => {
            let mut v = value.clone();
            if let Value::String(s) = value {
                if s.trim_start().starts_with('[') {
                    if let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(s) {
                        v = parsed;
                    }
                }
            }
            let items = match v {
                Value::Array(a) => a,
                other => vec![other],
            };
            match schema.get("items") {
                Some(item_schema) => Value::Array(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, x)| coerce(item_schema, x, &format!("{at}[{i}]"), errors, top))
                        .collect(),
                ),
                None => Value::Array(items),
            }
        }
        Some("object") => {
            let mut v = value.clone();
            if let Value::String(s) = value {
                if s.trim_start().starts_with('{') {
                    if let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(s) {
                        v = parsed;
                    }
                }
            }
            match v {
                Value::Object(map) => coerce_object(schema, &map, at, errors, top),
                _ => {
                    errors.push(format!("{at}: expected an object, got {} {}", describe(value), preview(value)));
                    Value::Null
                }
            }
        }
        _ => value.clone(),
    }
}

fn coerce_object(schema: &Value, map: &Map<String, Value>, at: &str, errors: &mut Vec<String>, top: &HashSet<String>) -> Value {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Value::Object(map.clone());
    };
    let prefix = if at.is_empty() { String::new() } else { format!("{at}.") };
    let mut out = Map::new();
    for (key, raw) in map {
        let Some(prop) = props.get(key) else {
            if !at.is_empty() && top.contains(key) {
                errors.push(format!("{prefix}{key}: not allowed here — {key} is a top-level argument"));
            } else {
                let valid: Vec<&str> = props.keys().map(String::as_str).collect();
                let valid = if valid.is_empty() { "none".to_string() } else { valid.join(", ") };
                errors.push(format!("{prefix}{key}: unknown key (valid keys: {valid})"));
            }
            continue;
        };
        if raw.is_null() {
            continue;
        }
        out.insert(key.clone(), coerce(prop, raw, &format!("{prefix}{key}"), errors, top));
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !out.contains_key(key) {
                errors.push(format!("{prefix}{key}: missing required key"));
            }
        }
    }
    Value::Object(out)
}

/// Validate `arguments` for tool `name` against its JSON-schema `parameters`.
///
/// Returns the coerced arguments, or the error text to hand back to the model
/// when the call must be rejected.  `parameters == None` (unknown tool) passes
/// the arguments through so the dispatcher can report the tool itself.
pub fn normalize_arguments(name: &str, arguments: &Value, parameters: Option<&Value>) -> Result<Value, String> {
    let Some(parameters) = parameters else {
        return Ok(arguments.clone());
    };
    let Value::Object(map) = arguments else {
        return Err(format!("Error: the arguments for {name} must be a JSON object. Nothing was run."));
    };
    if map.len() == 1 {
        if let Some(raw) = map.get(RAW_KEY) {
            let raw = raw.as_str().unwrap_or_default();
            let shown = if raw.chars().count() > 200 {
                format!("{}...", raw.chars().take(200).collect::<String>())
            } else {
                raw.to_string()
            };
            return Err(format!(
                "Error: the arguments for {name} were not valid JSON, so nothing was run. \
                 Resend the call with a single JSON object. Received: {shown}"
            ));
        }
    }
    let top: HashSet<String> = parameters
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    let mut errors = Vec::new();
    let out = coerce_object(parameters, map, "", &mut errors, &top);
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "Error: invalid arguments for {name}; nothing was run.\n{}",
            errors.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")
        ))
    }
}

/// Parse a tool call's raw JSON argument string.
///
/// An empty string is `{}`; one level of double encoding (a JSON string that
/// contains a JSON object) is unwrapped.  Anything that still isn't a JSON
/// object becomes `{"_raw_arguments": raw}` so `normalize_arguments` reports it
/// instead of the call running with no arguments.
pub fn parse_raw_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Map::new());
    }
    let parsed = match serde_json::from_str::<Value>(raw) {
        Ok(Value::String(inner)) => serde_json::from_str::<Value>(&inner).ok(),
        Ok(v) => Some(v),
        Err(_) => None,
    };
    match parsed {
        Some(v @ Value::Object(_)) => v,
        _ => serde_json::json!({ RAW_KEY: raw }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(name: &str) -> Value {
        crate::tools::tool_definitions_json()
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == name)
            .unwrap()["function"]["parameters"]
            .clone()
    }

    #[test]
    fn coerces_unambiguous_slips() {
        let out = normalize_arguments("read_file", &json!({"path": "f", "offset": "3", "limit": 2.0}), Some(&params("read_file"))).unwrap();
        assert_eq!(out, json!({"path": "f", "offset": 3, "limit": 2}));
        let out = normalize_arguments("apply_changes", &json!({"changes": [{"path": "f", "operations": [{"kind": "Replace", "old": "a", "new": "b"}]}], "dry_run": "true"}), Some(&params("apply_changes"))).unwrap();
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!(out["changes"][0]["operations"][0]["kind"], json!("replace"));
        let out = normalize_arguments("read_multiple_files", &json!({"paths": "ab"}), Some(&params("read_multiple_files"))).unwrap();
        assert_eq!(out["paths"], json!(["ab"]));
        let out = normalize_arguments("read_multiple_files", &json!({"paths": "[\"a\", \"b\"]"}), Some(&params("read_multiple_files"))).unwrap();
        assert_eq!(out["paths"], json!(["a", "b"]));
        let out = normalize_arguments("read_file", &json!({"path": "f", "offset": null}), Some(&params("read_file"))).unwrap();
        assert_eq!(out, json!({"path": "f"}));
    }

    #[test]
    fn rejects_unknown_misplaced_missing_and_bad_values() {
        let err = normalize_arguments("write_file", &json!({"path": "f", "content": "x", "append": true}), Some(&params("write_file"))).unwrap_err();
        assert!(err.contains("append: unknown key (valid keys: "), "{err}");
        assert!(err.contains("nothing was run"), "{err}");
        let err = normalize_arguments("apply_changes", &json!({"changes": [{"path": "f", "dry_run": true, "operations": [{"kind": "replace", "old": "a", "new": "b"}]}]}), Some(&params("apply_changes"))).unwrap_err();
        assert!(err.contains("changes[0].dry_run: not allowed here — dry_run is a top-level argument"), "{err}");
        let err = normalize_arguments("replace_in_file", &json!({"path": "f", "old_str": "a"}), Some(&params("replace_in_file"))).unwrap_err();
        assert!(err.contains("new_str: missing required key"), "{err}");
        let err = normalize_arguments("read_file", &json!({"path": "f", "offset": "three"}), Some(&params("read_file"))).unwrap_err();
        assert!(err.contains("offset: expected an integer, got string \"three\""), "{err}");
        let err = normalize_arguments("todowrite", &json!({"todos": [{"content": "x", "status": "done"}]}), Some(&params("todowrite"))).unwrap_err();
        assert!(err.contains("todos[0].status: must be one of pending, in_progress, completed (got \"done\")"), "{err}");
    }

    #[test]
    fn raw_argument_parsing() {
        assert_eq!(parse_raw_arguments(""), json!({}));
        assert_eq!(parse_raw_arguments("{\"a\": 1}"), json!({"a": 1}));
        assert_eq!(parse_raw_arguments("\"{\\\"a\\\": 1}\""), json!({"a": 1}));
        assert_eq!(parse_raw_arguments("{oops"), json!({"_raw_arguments": "{oops"}));
        let err = normalize_arguments("read_file", &json!({"_raw_arguments": "{oops"}), Some(&params("read_file"))).unwrap_err();
        assert!(err.contains("were not valid JSON"), "{err}");
    }
}
