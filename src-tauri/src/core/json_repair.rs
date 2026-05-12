//! Tolerant JSON parser for malformed LLM output.
//!
//! Port target: `core/json_repair.py`. The Python version performs several
//! heuristic fix-ups (closing brackets, escaping stray quotes, trimming code
//! fences). This module will reimplement the same heuristics on top of
//! `serde_json::Value`.

#![allow(dead_code)]

/// Best-effort repair of a JSON string returned by an LLM.
///
/// Returns the parsed value on success, or `None` if repair could not produce
/// valid JSON. (Task #2 will implement this.)
pub fn repair_json_text(_raw: &str) -> Option<serde_json::Value> {
    // TODO(task-2): port heuristics from core/json_repair.py
    None
}
