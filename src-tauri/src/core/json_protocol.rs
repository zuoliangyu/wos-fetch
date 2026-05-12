//! Protocol helpers for parsing structured JSON the LLM is asked to return.
//!
//! Port of `core/json_protocol.py`. Tries to parse the raw model output as
//! JSON first; if that fails, runs the text through `json_repair::repair_json_text`
//! and retries. Diagnostics record whether repair was needed.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use super::json_repair::repair_json_text;
use crate::{AppError, AppResult};

static FENCE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)\A```(?:json)?\s*(.*?)\s*```\z").unwrap());

/// Strip a surrounding ```json ... ``` markdown fence if present.
pub fn strip_markdown_fence(text: &str) -> String {
    let candidate = text.trim();
    if let Some(caps) = FENCE_RE.captures(candidate) {
        return caps.get(1).unwrap().as_str().trim().to_string();
    }
    candidate.to_string()
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ParseDiagnostics {
    pub used_repair: bool,
    pub raw_candidate: String,
    pub repaired_candidate: String,
}

/// Parse the model's output into a JSON *object*. Bare arrays are wrapped as
/// `{"search_directions": <array>}` to match the Python behavior.
pub fn parse_json_object_with_diagnostics(text: &str) -> AppResult<(Value, ParseDiagnostics)> {
    let raw_candidate = strip_markdown_fence(text);
    let repaired_candidate = repair_json_text(&raw_candidate);
    let mut diagnostics = ParseDiagnostics {
        used_repair: false,
        raw_candidate: raw_candidate.clone(),
        repaired_candidate: repaired_candidate.clone(),
    };

    let value = match serde_json::from_str::<Value>(&raw_candidate) {
        Ok(v) => v,
        Err(_) => {
            diagnostics.used_repair = true;
            serde_json::from_str::<Value>(&repaired_candidate)?
        }
    };

    match value {
        Value::Array(arr) => Ok((json!({ "search_directions": arr }), diagnostics)),
        Value::Object(_) => Ok((value, diagnostics)),
        _ => Err(AppError::BadInput(
            "Model output must be a JSON object.".into(),
        )),
    }
}

/// Convenience wrapper that drops the diagnostics.
pub fn load_json_object(text: &str) -> AppResult<Value> {
    let (value, _) = parse_json_object_with_diagnostics(text)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_object() {
        let (v, diag) = parse_json_object_with_diagnostics("{\"a\": 1}").unwrap();
        assert_eq!(v["a"], 1);
        assert!(!diag.used_repair);
    }

    #[test]
    fn wraps_array() {
        let (v, _) = parse_json_object_with_diagnostics("[1, 2, 3]").unwrap();
        assert_eq!(v["search_directions"][1], 2);
    }

    #[test]
    fn uses_repair_on_trailing_comma() {
        let (v, diag) = parse_json_object_with_diagnostics("{\"a\": 1,}").unwrap();
        assert_eq!(v["a"], 1);
        assert!(diag.used_repair);
    }
}
