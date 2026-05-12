//! Unified error type for the application.
//!
//! Serializes to a plain string so Tauri commands can return it directly to
//! the JS side without exposing internal Rust types.

use serde::{Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Excel error: {0}")]
    Excel(String),

    #[error("Browser error: {0}")]
    Browser(String),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Bad input: {0}")]
    BadInput(String),

    #[error("{0}")]
    Other(String),
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
