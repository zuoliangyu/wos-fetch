//! HTTP-first full-text acquisition, with browser fallback hook.
//!
//! Port of `skills/fulltext_acquisition.py`. Implements:
//!
//! - DOI normalization and resolution via doi.org redirect
//! - Per-host politeness throttle (>= 1s between same-host GETs)
//! - HTML body extraction with noise-node stripping
//! - Candidate URL probing for publishers that hide the full text behind an
//!   abstract page (Elsevier /abs/, Wiley /doi/abs/, etc.)
//! - Optional browser fallback when HTTP doesn't yield enough HTML

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use reqwest::Client;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{AppError, AppResult};

pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) ",
    "AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/124.0.0.0 Safari/537.36"
);
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
pub const HTML_MIN_CHARS: usize = 6000;
pub const MIN_INTER_REQUEST_SECONDS: f64 = 1.0;

static DOI_PREFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^10\.\d{4,9}").unwrap());
static DOI_URL_PREFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^https?://(dx\.)?doi\.org/").unwrap());
static WS_HORIZONTAL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t\f\x0B]+").unwrap());
static WS_MULTILINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());
static PII_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)/(?:retrieve/pii|science/article/(?:abs/)?pii)/([A-Z0-9]+)").unwrap()
});

static PUBLISHER_MAP: Lazy<HashMap<&'static str, (&'static str, &'static str)>> = Lazy::new(|| {
    [
        ("10.1016", ("Elsevier (ScienceDirect)", "https://www.sciencedirect.com")),
        ("10.1038", ("Nature Portfolio", "https://www.nature.com")),
        ("10.1039", ("Royal Society of Chemistry", "https://pubs.rsc.org")),
        ("10.1021", ("ACS Publications", "https://pubs.acs.org")),
        ("10.1002", ("Wiley Online Library", "https://onlinelibrary.wiley.com")),
        ("10.1111", ("Wiley Online Library", "https://onlinelibrary.wiley.com")),
        ("10.1007", ("Springer", "https://link.springer.com")),
        ("10.1186", ("BioMed Central", "https://www.biomedcentral.com")),
        ("10.1080", ("Taylor & Francis", "https://www.tandfonline.com")),
        ("10.1093", ("Oxford University Press", "https://academic.oup.com")),
        ("10.1126", ("Science (AAAS)", "https://www.science.org")),
        ("10.1136", ("BMJ", "https://www.bmj.com")),
        ("10.1073", ("PNAS", "https://www.pnas.org")),
        ("10.1103", ("American Physical Society", "https://journals.aps.org")),
        ("10.1017", ("Cambridge University Press", "https://www.cambridge.org")),
        ("10.1109", ("IEEE Xplore", "https://ieeexplore.ieee.org")),
        ("10.1145", ("ACM Digital Library", "https://dl.acm.org")),
        ("10.1056", ("New England Journal of Medicine", "https://www.nejm.org")),
        ("10.1001", ("JAMA Network", "https://jamanetwork.com")),
        ("10.1177", ("SAGE Journals", "https://journals.sagepub.com")),
        ("10.1097", ("Wolters Kluwer", "https://journals.lww.com")),
        ("10.1371", ("PLOS", "https://plos.org")),
        ("10.3390", ("MDPI", "https://www.mdpi.com")),
        ("10.3389", ("Frontiers", "https://www.frontiersin.org")),
        ("10.1152", ("American Physiological Society", "https://journals.physiology.org")),
        ("10.1128", ("American Society for Microbiology", "https://journals.asm.org")),
        ("10.4049", ("The Journal of Immunology", "https://www.jimmunol.org")),
        ("10.2147", ("Dove Medical Press", "https://www.dovepress.com")),
    ]
    .into_iter()
    .collect()
});

static HOST_LAST_REQUEST: Lazy<Mutex<HashMap<String, Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn cheap_jitter_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.subsec_micros() % 300) as u64)
        .unwrap_or(0)
}

async fn throttle_per_host(url: &str, min_seconds: f64) {
    if min_seconds <= 0.0 {
        return;
    }
    let host = match Url::parse(url) {
        Ok(u) => u.host_str().unwrap_or("").to_ascii_lowercase(),
        Err(_) => String::new(),
    };
    if host.is_empty() {
        return;
    }
    let wait = {
        let mut guard = HOST_LAST_REQUEST.lock();
        let now = Instant::now();
        let last = guard.get(&host).copied().unwrap_or(now - Duration::from_secs(3600));
        let min_dur = Duration::from_secs_f64(min_seconds);
        let scheduled = std::cmp::max(now, last + min_dur);
        guard.insert(host, scheduled);
        scheduled.saturating_duration_since(now)
    };
    let total = wait + Duration::from_millis(cheap_jitter_ms());
    if !total.is_zero() {
        tokio::time::sleep(total).await;
    }
}

pub fn normalize_doi(value: &str) -> String {
    let raw = value.trim();
    let stripped = DOI_URL_PREFIX_RE.replace(raw, "").to_string();
    stripped.trim().to_string()
}

pub fn publisher_from_doi(doi: &str) -> Option<(&'static str, &'static str)> {
    let normalized = normalize_doi(doi);
    let m = DOI_PREFIX_RE.find(&normalized)?;
    PUBLISHER_MAP.get(m.as_str()).copied()
}

#[derive(Debug, Serialize)]
pub struct PublisherSummary {
    pub name: String,
    pub url: String,
    pub count: u32,
}

pub fn get_required_publishers(dois: &[String]) -> Vec<PublisherSummary> {
    let mut seen: HashMap<&'static str, PublisherSummary> = HashMap::new();
    for doi in dois {
        if let Some((name, url)) = publisher_from_doi(doi) {
            let entry = seen.entry(name).or_insert(PublisherSummary {
                name: name.into(),
                url: url.into(),
                count: 0,
            });
            entry.count += 1;
        }
    }
    seen.into_values().collect()
}

fn build_http_client(timeout_seconds: u64) -> AppResult<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(timeout_seconds.max(1)))
        .build()
        .map_err(AppError::from)
}

fn normalize_url(input: &str, base: &str) -> String {
    if input.trim().is_empty() {
        return String::new();
    }
    let resolved = if let Ok(base_url) = Url::parse(base) {
        match base_url.join(input.trim()) {
            Ok(u) => u,
            Err(_) => return String::new(),
        }
    } else {
        match Url::parse(input.trim()) {
            Ok(u) => u,
            Err(_) => return String::new(),
        }
    };
    // Drop fragment.
    let mut cleaned = resolved.clone();
    cleaned.set_fragment(None);
    cleaned.into()
}

fn replace_url_path(url: &str, source: &str, target: &str) -> String {
    let Ok(parsed) = Url::parse(url) else { return String::new() };
    if !parsed.path().contains(source) {
        return String::new();
    }
    let new_path = parsed.path().replacen(source, target, 1);
    let mut updated = parsed.clone();
    updated.set_path(&new_path);
    updated.into()
}

pub fn build_candidate_urls(landing_url: &str, html: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let lower = landing_url.to_ascii_lowercase();
    let pii = PII_RE
        .captures(landing_url)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_default();

    if lower.contains("sciencedirect.com") {
        candidates.push(replace_url_path(landing_url, "/science/article/abs/pii/", "/science/article/pii/"));
        candidates.push(replace_url_path(landing_url, "/science/article/abs/", "/science/article/"));
    }
    if (lower.contains("sciencedirect.com") || lower.contains("linkinghub.elsevier.com")) && !pii.is_empty() {
        candidates.push(format!("https://www.sciencedirect.com/science/article/pii/{pii}"));
    }
    for host in ["wiley.com", "onlinelibrary.wiley.com", "tandfonline.com", "sagepub.com"] {
        if lower.contains(host) {
            candidates.push(replace_url_path(landing_url, "/doi/abs/", "/doi/full/"));
        }
    }

    if !html.is_empty() {
        let document = Html::parse_document(html);
        for name in ["citation_fulltext_html_url", "citation_full_html_url", "citation_html_url"] {
            let selector_str = format!("meta[name=\"{name}\"]");
            // Discard the Err before it leaves scope so its lifetime doesn't escape.
            let sel = Selector::parse(&selector_str).ok();
            if let Some(sel) = sel {
                for element in document.select(&sel) {
                    if let Some(content) = element.value().attr("content") {
                        candidates.push(normalize_url(content, landing_url));
                    }
                }
            }
        }
        if let Ok(link_sel) = Selector::parse("a[href]") {
            for link in document.select(&link_sel) {
                let href = link.value().attr("href").unwrap_or("").trim().to_string();
                let label_text: String = link.text().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
                let href_lower = href.to_ascii_lowercase();
                let matches_label = ["full text", "full article", "html"]
                    .iter()
                    .any(|t| label_text.contains(t));
                let matches_href = ["/fulltext", "/doi/full"].iter().any(|t| href_lower.contains(t));
                if matches_label || matches_href {
                    candidates.push(normalize_url(&href, landing_url));
                }
            }
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let landing_norm = normalize_url(landing_url, landing_url);
    let mut deduped: Vec<String> = Vec::new();
    for raw in candidates {
        let normalized = normalize_url(&raw, landing_url);
        if normalized.is_empty() || normalized == landing_norm || !seen.insert(normalized.clone()) {
            continue;
        }
        deduped.push(normalized);
    }
    deduped
}

fn normalize_extracted_text(text: &str) -> String {
    let unified = text.replace('\r', "\n");
    let collapsed = WS_HORIZONTAL_RE.replace_all(&unified, " ").to_string();
    let limited = WS_MULTILINE_RE.replace_all(&collapsed, "\n\n").to_string();
    limited.trim().to_string()
}

const NOISE_TAGS: &[&str] = &[
    "script", "style", "noscript", "svg", "canvas", "nav", "header", "footer", "aside", "form",
    "dialog", "button", "input", "select", "textarea",
];
const NOISE_CLASS_KEYWORDS: &[&str] = &[
    "cookie", "advertisement", "share", "social", "related", "recommended",
];

fn is_noise_node(element: &ElementRef) -> bool {
    let tag = element.value().name();
    if NOISE_TAGS.contains(&tag) {
        return true;
    }
    let class_attr = element.value().attr("class").unwrap_or("").to_ascii_lowercase();
    if class_attr.is_empty() {
        return false;
    }
    NOISE_CLASS_KEYWORDS.iter().any(|kw| class_attr.contains(kw))
}

fn collect_text_from(element: ElementRef) -> String {
    let mut buffer: Vec<String> = Vec::new();
    collect_text_recursive(element, &mut buffer);
    buffer.join("\n")
}

fn collect_text_recursive(element: ElementRef, out: &mut Vec<String>) {
    if is_noise_node(&element) {
        return;
    }
    for node in element.children() {
        if let Some(text) = node.value().as_text() {
            let line = text.trim();
            if !line.is_empty() {
                out.push(line.to_string());
            }
        } else if let Some(_) = node.value().as_element() {
            if let Some(child) = ElementRef::wrap(node) {
                collect_text_recursive(child, out);
            }
        }
    }
}

fn score_text(text: &str) -> (usize, i32, usize) {
    let lower = text.to_ascii_lowercase();
    let section_hits: i32 = ["abstract", "introduction", "methods", "results", "discussion", "references"]
        .iter()
        .map(|m| if lower.contains(m) { 1 } else { 0 })
        .sum();
    let noise_hits: i32 = ["cookie", "sign in", "recommended articles", "related articles"]
        .iter()
        .map(|m| if lower.contains(m) { 1 } else { 0 })
        .sum();
    let paragraphs = lower.matches("\n\n").count().max(lower.matches('\n').count());
    (text.len(), section_hits - noise_hits, paragraphs)
}

pub fn extract_html_text(html: &str) -> String {
    let document = Html::parse_document(html);
    let mut candidates: Vec<String> = Vec::new();
    let selectors = [
        "article",
        "main",
        "[role=\"main\"]",
        ".article",
        ".article-body",
        ".article-content",
        ".article__body",
        ".article-text",
        ".main-content",
        "#main-content",
        "#article-content",
        "[data-article-body]",
        "[data-test=\"article-body\"]",
    ];
    for sel_str in selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            for node in document.select(&sel) {
                let text = normalize_extracted_text(&collect_text_from(node));
                if !text.is_empty() {
                    candidates.push(text);
                }
            }
        }
    }
    // Fallback: whole document body.
    if let Ok(body_sel) = Selector::parse("body") {
        if let Some(body) = document.select(&body_sel).next() {
            let text = normalize_extracted_text(&collect_text_from(body));
            if !text.is_empty() {
                candidates.push(text);
            }
        }
    }
    candidates
        .into_iter()
        .max_by_key(|text| score_text(text))
        .map(|t| normalize_extracted_text(&t))
        .unwrap_or_default()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FulltextResult {
    pub doi_normalized: String,
    pub landing_url: String,
    pub source_url: String,
    pub acquisition_type: String,
    pub acquisition_status: String,
    pub content_text: String,
    pub content_chars: usize,
    pub error_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_fallback_error: Option<String>,
}

/// HTTP-only acquisition. Returns a structured result describing what was
/// found (or why it failed). Will not raise on transient HTTP errors — the
/// caller can decide whether to fall back to the browser.
pub async fn acquire_article_fulltext(
    doi: &str,
    timeout_seconds: u64,
    html_min_chars: usize,
) -> FulltextResult {
    let normalized = normalize_doi(doi);
    let mut result = FulltextResult {
        doi_normalized: normalized.clone(),
        ..Default::default()
    };
    if normalized.is_empty() {
        result.acquisition_status = "error".into();
        result.error_message = "missing_doi".into();
        return result;
    }

    let client = match build_http_client(timeout_seconds) {
        Ok(c) => c,
        Err(err) => {
            result.acquisition_status = "error".into();
            result.error_message = format!("client_build_failed:{err}");
            return result;
        }
    };

    let initial_url = format!("https://doi.org/{normalized}");
    throttle_per_host(&initial_url, MIN_INTER_REQUEST_SECONDS).await;
    let response = match client.get(&initial_url).send().await {
        Ok(r) => r,
        Err(err) => {
            result.acquisition_status = "error".into();
            let trimmed: String = err.to_string().chars().take(300).collect();
            result.error_message = trimmed;
            return result;
        }
    };
    let landing_url = response.url().to_string();
    result.landing_url = landing_url.clone();
    let status = response.status();
    if !status.is_success() {
        result.acquisition_status = "error".into();
        result.error_message = format!("HTTP {}", status.as_u16());
        return result;
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.is_empty() && !content_type.contains("html") && !content_type.contains("xml") {
        result.source_url = landing_url.clone();
        result.acquisition_type = "non_html".into();
        result.acquisition_status = "blocked".into();
        let preview: String = content_type.chars().take(80).collect();
        result.error_message = format!("non_html_response:{preview}");
        return result;
    }

    let html = response.text().await.unwrap_or_default();
    let mut best_source_url = landing_url.clone();
    let mut best_text = extract_html_text(&html);
    if best_text.chars().count() >= html_min_chars {
        result.source_url = landing_url;
        result.acquisition_type = "html".into();
        result.acquisition_status = "ok".into();
        result.content_chars = best_text.chars().count();
        result.content_text = best_text;
        return result;
    }

    let candidates = build_candidate_urls(&landing_url, &html);
    for candidate_url in candidates {
        throttle_per_host(&candidate_url, MIN_INTER_REQUEST_SECONDS).await;
        let response = match client.get(&candidate_url).send().await {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !response.status().is_success() {
            continue;
        }
        let candidate_ct = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !candidate_ct.is_empty()
            && !candidate_ct.contains("html")
            && !candidate_ct.contains("xml")
        {
            continue;
        }
        let final_url = response.url().to_string();
        let body = response.text().await.unwrap_or_default();
        let candidate_text = extract_html_text(&body);
        if score_text(&candidate_text) > score_text(&best_text) {
            best_text = candidate_text.clone();
            best_source_url = final_url.clone();
        }
        if candidate_text.chars().count() >= html_min_chars {
            result.source_url = final_url;
            result.acquisition_type = "html".into();
            result.acquisition_status = "ok".into();
            result.content_chars = candidate_text.chars().count();
            result.content_text = candidate_text;
            return result;
        }
    }

    let (status, error) = if best_text.is_empty() {
        ("blocked", "no_usable_content")
    } else {
        ("insufficient_html", "html_too_short")
    };
    result.source_url = best_source_url;
    result.acquisition_type = "html".into();
    result.acquisition_status = status.into();
    result.content_chars = best_text.chars().count();
    result.content_text = best_text;
    result.error_message = error.into();
    result
}

/// HTTP-first acquisition with browser fallback. Currently calls into the
/// `wos_browser::browser::fetch_fulltext_via_browser` stub; that function
/// will fully exist after task #5 is complete.
pub async fn acquire_article_fulltext_with_browser_fallback(
    doi: &str,
    debug_port: u16,
    timeout_seconds: u64,
    html_min_chars: usize,
) -> FulltextResult {
    let mut result = acquire_article_fulltext(doi, timeout_seconds, html_min_chars).await;
    if result.acquisition_status == "ok" {
        return result;
    }
    let landing_url = if !result.landing_url.is_empty() {
        result.landing_url.clone()
    } else {
        let normalized = normalize_doi(doi);
        if normalized.is_empty() {
            return result;
        }
        format!("https://doi.org/{normalized}")
    };
    if landing_url.is_empty() {
        return result;
    }
    match crate::skills::wos_browser::browser::fetch_fulltext_via_browser(&landing_url, debug_port).await {
        Ok(payload) => {
            let source_url = if payload.url.is_empty() {
                landing_url.clone()
            } else {
                payload.url
            };
            if payload.challenge_detected {
                result.acquisition_status = "blocked".into();
                result.acquisition_type = "browser_html".into();
                result.source_url = source_url;
                result.error_message = "browser_challenge_detected".into();
            } else if payload.article_text.chars().count() >= html_min_chars {
                result.acquisition_status = "ok".into();
                result.acquisition_type = "browser_html".into();
                result.content_chars = payload.article_text.chars().count();
                result.content_text = payload.article_text;
                result.source_url = source_url;
                result.error_message = String::new();
            } else if !payload.article_text.is_empty() {
                result.acquisition_status = "insufficient_html".into();
                result.acquisition_type = "browser_html".into();
                result.content_chars = payload.article_text.chars().count();
                result.content_text = payload.article_text;
                result.source_url = source_url;
            } else {
                result.acquisition_type = "browser_html".into();
            }
        }
        Err(err) => {
            let trimmed: String = err.to_string().chars().take(300).collect();
            result.browser_fallback_error = Some(trimmed);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_doi_org_prefix() {
        assert_eq!(normalize_doi("https://doi.org/10.1234/abc"), "10.1234/abc");
        assert_eq!(normalize_doi("http://dx.doi.org/10.1/abc"), "10.1/abc");
        assert_eq!(normalize_doi("  10.1/abc  "), "10.1/abc");
    }

    #[test]
    fn publisher_from_known_prefix() {
        let (name, _) = publisher_from_doi("10.1016/j.example").unwrap();
        assert!(name.contains("Elsevier"));
        assert!(publisher_from_doi("10.9999/unknown").is_none());
    }

    #[test]
    fn extract_text_picks_article_body() {
        let html = r#"
            <html><body>
              <nav>nav stuff</nav>
              <article>
                <p>Abstract</p>
                <p>This paper studies foo.</p>
                <h2>Introduction</h2>
                <p>The introduction.</p>
              </article>
              <footer>footer</footer>
            </body></html>
        "#;
        let text = extract_html_text(html);
        assert!(text.contains("This paper studies foo."));
        assert!(text.contains("Introduction"));
        // Noise nav text should not dominate.
        assert!(!text.starts_with("nav stuff"));
    }
}
