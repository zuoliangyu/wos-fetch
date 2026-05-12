//! Scrape the current WoS results page(s) for records.
//!
//! Port target: `skills/wos_browser/scraper.py` — the JS injection that walks
//! `.summary-record` / `app-record` nodes and extracts DOI + title + link.

#![allow(dead_code)]

#[derive(Debug)]
pub struct ScrapedRecord {
    pub row_index: i32,
    pub article_title: String,
    pub doi: String,
    pub full_record_link: String,
}

pub async fn scrape_wos_pages(
    _port: u16,
    _max_pages: u32,
    _max_records: u32,
) -> crate::AppResult<Vec<ScrapedRecord>> {
    // TODO(task-5): port from skills/wos_browser/scraper.py
    Err(crate::AppError::Browser(
        "scrape_wos_pages not yet implemented".into(),
    ))
}
