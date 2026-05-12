//! Protocol helpers for parsing structured JSON the LLM is asked to return.
//!
//! Port target: `core/json_protocol.py`. Wraps `json_repair` plus diagnostics
//! used by the relevance-screening and extraction pipelines.

#![allow(dead_code)]

#[derive(Debug, Default)]
pub struct ParseDiagnostics {
    pub repaired: bool,
    pub raw_length: usize,
    pub message: Option<String>,
}

/// Parse a JSON object emitted by the LLM, returning the value and diagnostics.
pub fn parse_json_object_with_diagnostics(
    _raw: &str,
) -> (Option<serde_json::Value>, ParseDiagnostics) {
    // TODO(task-2): port from core/json_protocol.py
    (None, ParseDiagnostics::default())
}
