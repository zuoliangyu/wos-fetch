//! LLM-driven relevance scoring of WoS records.
//!
//! Port target: `skills/relevance_screening.py`. Iterates over rows, calls
//! the LLM with a relevance prompt, parses the structured response via
//! `core::json_protocol`, and writes scores back to the table.

#![allow(dead_code)]

use crate::core::table_io::Table;

pub async fn screen_records(
    _table: &mut Table,
    _llm: &crate::core::llm_client::LlmConfig,
) -> crate::AppResult<()> {
    // TODO(task-4): port from skills/relevance_screening.py
    Err(crate::AppError::Other(
        "relevance_screening not yet implemented".into(),
    ))
}
