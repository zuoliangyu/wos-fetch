//! OpenAI-compatible chat client.
//!
//! Port of `core/llm_client.py`. Supports both `/chat/completions` (streaming
//! preferred, fallback to non-stream) and `/responses` endpoints, with
//! exponential-backoff retry on 429 / 5xx / transient network errors.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{AppError, AppResult};

const RETRYABLE_STATUS: &[u16] = &[429, 500, 502, 503, 504];
const MAX_RETRY_ATTEMPTS: u32 = 4;
const RETRY_BASE_DELAY_MS: u64 = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Per-request timeout, seconds.
    pub timeout_seconds: u64,
    /// Optional sampling temperature; defaults to 0.0 when None.
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

pub fn build_chat_completions_url(base_url: &str) -> String {
    let normalized = base_url.trim().trim_end_matches('/');
    if normalized.ends_with("/chat/completions") {
        return normalized.to_string();
    }
    if let Some(stripped) = normalized.strip_suffix("/responses") {
        return format!("{stripped}/chat/completions");
    }
    format!("{normalized}/chat/completions")
}

pub fn build_responses_url(base_url: &str) -> String {
    let normalized = base_url.trim().trim_end_matches('/');
    if normalized.ends_with("/responses") {
        return normalized.to_string();
    }
    if let Some(stripped) = normalized.strip_suffix("/chat/completions") {
        return format!("{stripped}/responses");
    }
    format!("{normalized}/responses")
}

pub fn prefers_responses_api(base_url: &str) -> bool {
    base_url
        .trim()
        .trim_end_matches('/')
        .ends_with("/responses")
}

/// Walk a JSON tree looking for text fragments under `text`, `output_text`,
/// `content`, `message`, or `refusal` keys.
fn extract_text_fragments(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                extract_text_fragments(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["text", "output_text", "content", "message", "refusal"] {
                if let Some(child) = map.get(key) {
                    extract_text_fragments(child, out);
                }
            }
        }
        _ => {}
    }
}

/// Best-effort text extraction from an OpenAI-style response body.
pub fn extract_output_text(payload: &Value) -> String {
    if let Some(direct) = payload.get("output_text").and_then(Value::as_str) {
        return direct.to_string();
    }

    fn flatten(chunks: &[String]) -> String {
        chunks
            .iter()
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
        let mut chunks: Vec<String> = Vec::new();
        for choice in choices {
            extract_text_fragments(choice, &mut chunks);
        }
        let text = flatten(&chunks);
        if !text.is_empty() {
            return text;
        }
    }

    if let Some(output_items) = payload.get("output").and_then(Value::as_array) {
        let mut chunks: Vec<String> = Vec::new();
        for item in output_items {
            extract_text_fragments(item, &mut chunks);
        }
        let text = flatten(&chunks);
        if !text.is_empty() {
            return text;
        }
    }

    let mut chunks: Vec<String> = Vec::new();
    extract_text_fragments(payload, &mut chunks);
    flatten(&chunks)
}

/// Pull a human-readable error message out of an API response body.
pub fn extract_response_error(payload: &Value) -> String {
    if let Some(error) = payload.get("error") {
        if let Some(obj) = error.as_object() {
            let message = obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let code = obj.get("code").and_then(Value::as_str).unwrap_or("").trim();
            return if !code.is_empty() && !message.is_empty() {
                format!("{code}: {message}")
            } else {
                message.to_string()
            };
        }
        if let Some(text) = error.as_str() {
            return text.to_string();
        }
    }
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if status == "failed" {
        return "response status is failed".to_string();
    }
    String::new()
}

fn cheap_jitter_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.subsec_micros() % 500) as u64)
        .unwrap_or(0)
}

fn is_connection_failure(err: &reqwest::Error) -> bool {
    err.is_timeout()
        || err.is_connect()
        || err.is_request()
        || err.to_string().contains("WinError 10013")
}

/// Run an HTTP request closure with exponential-backoff retry on
/// 429 / 5xx and transient network errors. Honours `Retry-After` if present.
async fn retryable_send<F, Fut>(perform: F) -> AppResult<Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<Response>>,
{
    let mut last_err: Option<String> = None;
    for attempt in 0..MAX_RETRY_ATTEMPTS {
        match perform().await {
            Ok(response) => {
                let status = response.status().as_u16();
                if RETRYABLE_STATUS.contains(&status) && attempt < MAX_RETRY_ATTEMPTS - 1 {
                    let retry_after_ms = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|s| (s * 1000.0) as u64);
                    let backoff =
                        retry_after_ms.unwrap_or_else(|| RETRY_BASE_DELAY_MS * (1u64 << attempt));
                    let total = backoff.max(RETRY_BASE_DELAY_MS) + cheap_jitter_ms();
                    tracing::warn!(
                        "[LLM] HTTP {} on attempt {}/{}; backing off {}ms",
                        status,
                        attempt + 1,
                        MAX_RETRY_ATTEMPTS,
                        total
                    );
                    drop(response);
                    tokio::time::sleep(Duration::from_millis(total)).await;
                    continue;
                }
                return Ok(response);
            }
            Err(err) => {
                if attempt == MAX_RETRY_ATTEMPTS - 1 || !is_connection_failure(&err) {
                    return Err(err.into());
                }
                let backoff = RETRY_BASE_DELAY_MS * (1u64 << attempt) + cheap_jitter_ms();
                tracing::warn!(
                    "[LLM] network error on attempt {}/{}: {}; sleeping {}ms",
                    attempt + 1,
                    MAX_RETRY_ATTEMPTS,
                    err,
                    backoff
                );
                last_err = Some(err.to_string());
                tokio::time::sleep(Duration::from_millis(backoff)).await;
            }
        }
    }
    Err(AppError::Llm(last_err.unwrap_or_else(|| {
        "Exhausted retry attempts without obtaining a response.".into()
    })))
}

fn build_client(timeout_seconds: u64) -> AppResult<Client> {
    Client::builder()
        .timeout(Duration::from_secs(timeout_seconds.max(1)))
        .user_agent("wos-fetch/0.1")
        .build()
        .map_err(AppError::from)
}

async fn post_chat_completion(config: &LlmConfig, messages: &[ChatMessage]) -> AppResult<Value> {
    let url = build_chat_completions_url(&config.base_url);
    let temperature = config.temperature.unwrap_or(0.0);
    let body = json!({
        "model": config.model,
        "messages": messages,
        "temperature": temperature,
    });
    let client = build_client(config.timeout_seconds)?;
    let started = Instant::now();
    let perform = || async {
        client
            .post(&url)
            .bearer_auth(&config.api_key)
            .json(&body)
            .send()
            .await
    };
    let response = retryable_send(perform).await?;
    let status = response.status();
    let text = response.text().await?;
    tracing::info!(
        "[LLM] POST {} status={} elapsed={:.1}s",
        url,
        status,
        started.elapsed().as_secs_f64()
    );
    let data: Value = serde_json::from_str(&text).unwrap_or_else(
        |_| json!({ "error": { "message": text.chars().take(1000).collect::<String>() } }),
    );
    if status.as_u16() >= 400 {
        let detail = extract_response_error(&data);
        return Err(AppError::Llm(if detail.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            detail
        }));
    }
    let detail = extract_response_error(&data);
    if !detail.is_empty() {
        return Err(AppError::Llm(detail));
    }
    Ok(data)
}

async fn post_response_api(config: &LlmConfig, messages: &[ChatMessage]) -> AppResult<Value> {
    let url = build_responses_url(&config.base_url);
    let temperature = config.temperature.unwrap_or(0.0);
    let input_items: Vec<Value> = messages
        .iter()
        .filter(|m| !m.content.trim().is_empty())
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect();
    let body = json!({
        "model": config.model,
        "input": input_items,
        "temperature": temperature,
    });
    let client = build_client(config.timeout_seconds)?;
    let started = Instant::now();
    let perform = || async {
        client
            .post(&url)
            .bearer_auth(&config.api_key)
            .json(&body)
            .send()
            .await
    };
    let response = retryable_send(perform).await?;
    let status = response.status();
    let text = response.text().await?;
    tracing::info!(
        "[LLM] POST {} status={} elapsed={:.1}s",
        url,
        status,
        started.elapsed().as_secs_f64()
    );
    let data: Value = serde_json::from_str(&text).unwrap_or_else(
        |_| json!({ "error": { "message": text.chars().take(1000).collect::<String>() } }),
    );
    if status.as_u16() >= 400 {
        let detail = extract_response_error(&data);
        return Err(AppError::Llm(if detail.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            detail
        }));
    }
    let detail = extract_response_error(&data);
    if !detail.is_empty() {
        return Err(AppError::Llm(detail));
    }
    Ok(data)
}

async fn stream_chat_completion_text(
    config: &LlmConfig,
    messages: &[ChatMessage],
) -> AppResult<String> {
    let url = build_chat_completions_url(&config.base_url);
    let temperature = config.temperature.unwrap_or(0.0);
    let body = json!({
        "model": config.model,
        "messages": messages,
        "temperature": temperature,
        "stream": true,
    });
    let client = build_client(config.timeout_seconds)?;
    let started = Instant::now();
    let perform = || async {
        client
            .post(&url)
            .bearer_auth(&config.api_key)
            .json(&body)
            .send()
            .await
    };
    let response = retryable_send(perform).await?;
    let status = response.status();
    if status.as_u16() >= 400 {
        let body = response.text().await.unwrap_or_default();
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let detail = extract_response_error(&parsed);
        return Err(AppError::Llm(if detail.is_empty() {
            format!(
                "HTTP {} {}",
                status.as_u16(),
                body.chars().take(1000).collect::<String>()
            )
        } else {
            detail
        }));
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut byte_stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(item) = byte_stream.next().await {
        let bytes = item?;
        buffer.extend_from_slice(&bytes);
        while let Some(newline_pos) = buffer.iter().position(|&b| b == b'\n') {
            let raw_line: Vec<u8> = buffer.drain(..=newline_pos).collect();
            let line = String::from_utf8_lossy(&raw_line).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(&line);
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let data: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let detail = extract_response_error(&data);
            if !detail.is_empty() {
                return Err(AppError::Llm(detail));
            }
            let Some(choices) = data.get("choices").and_then(Value::as_array) else {
                continue;
            };
            for choice in choices {
                if let Some(content) = choice.pointer("/delta/content").and_then(Value::as_str) {
                    chunks.push(content.to_string());
                }
                if let Some(content) = choice.pointer("/message/content").and_then(Value::as_str) {
                    chunks.push(content.to_string());
                }
                if let Some(text) = choice.get("text").and_then(Value::as_str) {
                    chunks.push(text.to_string());
                }
            }
        }
    }
    let text = chunks.join("").trim().to_string();
    tracing::info!(
        "[LLM] STREAM {} chars={} elapsed={:.1}s",
        url,
        text.chars().count(),
        started.elapsed().as_secs_f64()
    );
    if text.is_empty() {
        return Err(AppError::Llm("stream returned empty text".into()));
    }
    Ok(text)
}

/// Main entry point. Tries (1) `/responses` if the base URL points there;
/// (2) streaming `/chat/completions`; (3) non-stream `/chat/completions`;
/// (4) falls back to `/responses` as a last resort.
pub async fn chat_text(config: &LlmConfig, messages: &[ChatMessage]) -> AppResult<String> {
    let mut errors: Vec<String> = Vec::new();

    if prefers_responses_api(&config.base_url) {
        match post_response_api(config, messages).await {
            Ok(data) => {
                let text = extract_output_text(&data).trim().to_string();
                if !text.is_empty() {
                    return Ok(text);
                }
                errors.push("responses:empty".into());
            }
            Err(AppError::Http(e)) if e.is_timeout() || e.is_connect() => {
                return Err(AppError::Llm(format!("Model API connection failed: {e}")));
            }
            Err(e) => errors.push(format!("responses:{e}")),
        }
    }

    match stream_chat_completion_text(config, messages).await {
        Ok(text) => return Ok(text),
        Err(AppError::Http(e)) if e.is_timeout() || e.is_connect() => {
            return Err(AppError::Llm(format!("Model API connection failed: {e}")));
        }
        Err(e) => errors.push(format!("chat_stream:{e}")),
    }

    match post_chat_completion(config, messages).await {
        Ok(data) => {
            let text = extract_output_text(&data).trim().to_string();
            if !text.is_empty() {
                return Ok(text);
            }
            errors.push("chat_post:empty".into());
        }
        Err(e) => errors.push(format!("chat_post:{e}")),
    }

    if !prefers_responses_api(&config.base_url) {
        match post_response_api(config, messages).await {
            Ok(data) => {
                let text = extract_output_text(&data).trim().to_string();
                if !text.is_empty() {
                    return Ok(text);
                }
                errors.push("responses:empty".into());
            }
            Err(e) => errors.push(format!("responses:{e}")),
        }
    }

    Err(AppError::Llm(format!(
        "Model API call failed. {}",
        errors
            .iter()
            .rev()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ")
    )))
}

/// Build the GET endpoint for OpenAI-compatible `list models`.
///
/// Honors a `base_url` that already ends with `/v1` (don't double up); falls
/// back to appending `/v1/models` otherwise. Trailing `/chat/completions` or
/// `/responses` are stripped so the same input that drives chat also works
/// here.
pub fn build_models_url(base_url: &str) -> String {
    let normalized = base_url.trim().trim_end_matches('/');
    let stem = normalized
        .strip_suffix("/chat/completions")
        .or_else(|| normalized.strip_suffix("/responses"))
        .unwrap_or(normalized);
    if stem.ends_with("/v1") {
        format!("{stem}/models")
    } else {
        format!("{stem}/v1/models")
    }
}

/// Fetch the available models from an OpenAI-compatible `/v1/models` endpoint.
/// Returns model ids sorted alphabetically. Tolerates the two common shapes
/// (`{"data":[{"id":...}]}` or `{"models":[{"id":...}]}`) and also a bare
/// array of strings/objects.
pub async fn list_models(config: &LlmConfig) -> AppResult<Vec<String>> {
    if config.api_key.trim().is_empty() {
        return Err(AppError::BadInput("请先填写 API Key。".into()));
    }
    let url = build_models_url(&config.base_url);
    let client = Client::builder()
        .timeout(Duration::from_secs(config.timeout_seconds.max(15)))
        .build()
        .map_err(|e| AppError::Other(format!("无法构建 HTTP 客户端：{e}")))?;
    let resp = client
        .get(&url)
        .bearer_auth(config.api_key.trim())
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| AppError::Other(format!("请求 {url} 失败：{e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| AppError::Other(format!("读取响应失败：{e}")))?;
    if !status.is_success() {
        let snippet: String = body.chars().take(300).collect();
        return Err(AppError::Llm(format!(
            "模型列表接口返回 HTTP {status}：{snippet}"
        )));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::Llm(format!("响应不是合法 JSON：{e}")))?;
    let items = parsed
        .get("data")
        .or_else(|| parsed.get("models"))
        .or(Some(&parsed))
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Llm("响应中找不到模型列表（data / models 字段）。".into()))?;
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            Value::String(s) => Some(s.clone()),
            Value::Object(_) => item
                .get("id")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .map(|s| s.to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Lightweight reachability check used by the UI's "validate connection" button.
pub async fn validate_llm_connection(config: &LlmConfig) -> AppResult<String> {
    let messages = vec![ChatMessage {
        role: "user".into(),
        content: "ping".into(),
    }];
    chat_text(config, &messages).await
}

// Silence unused-import warnings in builds that don't pull in StatusCode.
#[allow(dead_code)]
fn _ensure_status_code_used(_: StatusCode) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_builders() {
        assert_eq!(
            build_chat_completions_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            build_chat_completions_url("https://x.com/v1/responses"),
            "https://x.com/v1/chat/completions"
        );
        assert_eq!(
            build_responses_url("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/responses"
        );
        assert!(prefers_responses_api("https://x.com/v1/responses"));
        assert!(!prefers_responses_api("https://x.com/v1"));
    }

    #[test]
    fn models_url_handles_v1_suffix() {
        assert_eq!(
            build_models_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://api.openai.com"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://x.com/v1/chat/completions"),
            "https://x.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://x.com/v1/responses"),
            "https://x.com/v1/models"
        );
    }

    #[test]
    fn extracts_text_from_choices() {
        let payload = json!({
            "choices": [
                { "message": { "content": "hello" } }
            ]
        });
        assert_eq!(extract_output_text(&payload), "hello");
    }

    #[test]
    fn extracts_text_from_output_text() {
        let payload = json!({ "output_text": "world" });
        assert_eq!(extract_output_text(&payload), "world");
    }

    #[test]
    fn extracts_error_message() {
        let payload = json!({ "error": { "code": "rate_limit", "message": "slow down" } });
        assert_eq!(extract_response_error(&payload), "rate_limit: slow down");
    }
}
