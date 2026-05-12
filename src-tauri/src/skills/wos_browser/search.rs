//! Submit a WoS Advanced Search query inside the running browser.
//!
//! Port target: `skills/wos_browser/search.py`.

#![allow(dead_code)]

#[derive(Debug)]
pub struct SearchSubmission {
    pub ok: bool,
    pub submitted: bool,
    pub reason: Option<String>,
}

pub async fn run_wos_search(
    _port: u16,
    _query: &str,
    _wait_seconds: f32,
) -> crate::AppResult<SearchSubmission> {
    // TODO(task-5): port from skills/wos_browser/search.py
    Err(crate::AppError::Browser(
        "run_wos_search not yet implemented".into(),
    ))
}
