//! High-level "fetch full text via the running browser" entry point.
//!
//! Port target: `skills/wos_browser/browser.py` — performs navigation,
//! click-through to PDF/HTML, body extraction, and challenge detection.

#![allow(dead_code)]

#[derive(Debug)]
pub struct BrowserFulltextPayload {
    pub url: String,
    pub article_text: String,
    pub html: String,
    pub challenge_detected: bool,
    pub not_found_detected: bool,
}

pub async fn fetch_fulltext_via_browser(
    _url: &str,
    _port: u16,
) -> crate::AppResult<BrowserFulltextPayload> {
    // TODO(task-5): port from skills/wos_browser/browser.py
    Err(crate::AppError::Browser(
        "fetch_fulltext_via_browser not yet implemented".into(),
    ))
}
