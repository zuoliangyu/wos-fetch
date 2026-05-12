//! OpenAI-compatible chat completion client.
//!
//! Port target: `core/llm_client.py` (~291 lines). Provides `chat_text`,
//! `chat_json`, and connection validation against `/chat/completions` and
//! `/messages` endpoints. Reqwest-based, async via tokio.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Send a chat completion request and return the plain text reply.
pub async fn chat_text(
    _config: &LlmConfig,
    _messages: &[ChatMessage],
) -> crate::AppResult<String> {
    // TODO(task-2): port from core/llm_client.py
    Err(crate::AppError::Llm("llm_client not yet implemented".into()))
}
