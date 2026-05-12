//! HTTP-first full-text acquisition with per-host politeness throttling.
//!
//! Port target: `skills/fulltext_acquisition.py`. Falls back to browser
//! acquisition via `wos_browser::browser` when HTTP yields too-short or
//! blocked content.

#![allow(dead_code)]

use std::time::Duration;

pub const MIN_INTER_REQUEST_SECONDS_PER_HOST: Duration = Duration::from_secs(1);
pub const HTML_MIN_CHARS: usize = 6000;

#[derive(Debug)]
pub struct FulltextResult {
    pub source_url: String,
    pub status: String,
    pub content_text: String,
    pub content_chars: usize,
}

pub async fn fetch_fulltext_http(_doi_or_url: &str) -> crate::AppResult<FulltextResult> {
    // TODO(task-4): port from skills/fulltext_acquisition.py
    Err(crate::AppError::Other(
        "fulltext_acquisition not yet implemented".into(),
    ))
}
