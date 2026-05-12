//! Browser detection, launch, and HTTP-based debug target management.
//!
//! Port target: `skills/wos_browser/tools.py` (browser launch parts) plus the
//! HTTP `/json/list`, `/json/new`, `/json/close` endpoints duplicated in
//! `skills/wos_browser/browser.py`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::{AppError, AppResult};

use super::default_profile_dir;

pub const DEFAULT_DEBUG_PORT: u16 = 9222;
pub const WOS_START_URL: &str = "https://www.webofscience.com/wos/woscc/advanced-search";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebugTarget {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "webSocketDebuggerUrl")]
    pub websocket_url: String,
}

fn http_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(AppError::from)
}

/// `GET http://127.0.0.1:<port>/json/list`
pub async fn get_debug_targets(port: u16) -> AppResult<Vec<DebugTarget>> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let client = http_client()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| AppError::Browser(format!("debug port unreachable on {port}: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::Browser(format!(
            "debug port returned HTTP {} on /json/list",
            resp.status().as_u16()
        )));
    }
    let value: Value = resp.json().await.map_err(AppError::from)?;
    let arr = value.as_array().cloned().unwrap_or_default();
    let mut targets: Vec<DebugTarget> = Vec::with_capacity(arr.len());
    for item in arr {
        let target: DebugTarget = serde_json::from_value(item).unwrap_or_default();
        targets.push(target);
    }
    Ok(targets)
}

pub async fn get_page_targets(port: u16) -> AppResult<Vec<DebugTarget>> {
    Ok(get_debug_targets(port)
        .await?
        .into_iter()
        .filter(|t| t.kind == "page")
        .collect())
}

/// `PUT http://127.0.0.1:<port>/json/new?<url>`
pub async fn open_debug_target(url: &str, port: u16) -> AppResult<DebugTarget> {
    let encoded = urlencoding_encode(url.is_empty().then_some("about:blank").unwrap_or(url));
    let endpoint = format!("http://127.0.0.1:{port}/json/new?{encoded}");
    let client = http_client()?;
    let resp = client
        .put(&endpoint)
        .send()
        .await
        .map_err(|e| AppError::Browser(format!("could not open new debug target: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::Browser(format!(
            "/json/new returned HTTP {}",
            resp.status().as_u16()
        )));
    }
    let value: Value = resp.json().await.map_err(AppError::from)?;
    if !value.is_object() {
        return Err(AppError::Browser(
            "Chrome did not return a valid debug target.".into(),
        ));
    }
    serde_json::from_value(value).map_err(AppError::from)
}

pub async fn close_debug_target(target_id: &str, port: u16) -> AppResult<()> {
    if target_id.is_empty() {
        return Ok(());
    }
    let url = format!("http://127.0.0.1:{port}/json/close/{target_id}");
    let client = http_client()?;
    let _ = client.get(&url).send().await;
    Ok(())
}

fn urlencoding_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// WoS target finding helpers
// ---------------------------------------------------------------------------

fn find_wos_target_impl<'a>(
    targets: &'a [DebugTarget],
    preferred_patterns: &[&str],
) -> Option<&'a DebugTarget> {
    let page_targets: Vec<&DebugTarget> = targets.iter().filter(|t| t.kind == "page").collect();
    let wos_targets: Vec<&DebugTarget> = page_targets
        .iter()
        .copied()
        .filter(|t| t.url.to_ascii_lowercase().contains("webofscience"))
        .collect();
    if !wos_targets.is_empty() {
        for pattern in preferred_patterns {
            for target in wos_targets.iter().rev() {
                let url_lower = target.url.to_ascii_lowercase();
                let title_lower = target.title.to_ascii_lowercase();
                if url_lower.contains(pattern) || title_lower.contains(pattern) {
                    return Some(*target);
                }
            }
        }
        return wos_targets.last().copied();
    }
    page_targets.last().copied()
}

pub async fn find_wos_target(port: u16, preferred_patterns: &[&str]) -> AppResult<DebugTarget> {
    let targets = get_debug_targets(port).await?;
    find_wos_target_impl(&targets, preferred_patterns)
        .cloned()
        .ok_or_else(|| AppError::Browser("No browser page target was found on the debug port.".into()))
}

pub async fn find_wos_search_target(port: u16) -> AppResult<DebugTarget> {
    find_wos_target(
        port,
        &["advanced-search", "basic-search", "smart-search", "search"],
    )
    .await
}

pub async fn find_wos_results_target(port: u16) -> AppResult<DebugTarget> {
    let targets = get_debug_targets(port).await?;
    let page_targets: Vec<&DebugTarget> = targets.iter().filter(|t| t.kind == "page").collect();
    let wos_targets: Vec<&DebugTarget> = page_targets
        .iter()
        .copied()
        .filter(|t| t.url.to_ascii_lowercase().contains("webofscience"))
        .collect();
    for pattern in ["summary", "results", "result"] {
        for target in wos_targets.iter().rev() {
            let url_lower = target.url.to_ascii_lowercase();
            let title_lower = target.title.to_ascii_lowercase();
            if url_lower.contains(pattern) || title_lower.contains(pattern) {
                return Ok((*target).clone());
            }
        }
    }
    if !wos_targets.is_empty() {
        let preview = wos_targets
            .iter()
            .rev()
            .take(3)
            .map(|t| format!("{} | {}", t.title, t.url))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(AppError::Browser(format!(
            "No Web of Science results page was found on the debug port. \
             Open the WoS results/summary page before scraping. Current WoS pages: {preview}"
        )));
    }
    if !page_targets.is_empty() {
        return Err(AppError::Browser(
            "No Web of Science page was found on the debug port.".into(),
        ));
    }
    Err(AppError::Browser(
        "No browser page target was found on the debug port.".into(),
    ))
}

// ---------------------------------------------------------------------------
// Browser executable detection + launch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct DetectedBrowser {
    pub browser_name: String,
    pub executable_path: String,
    pub debug_port: u16,
    pub note: String,
}

fn expand_env_path(raw: &str) -> Option<PathBuf> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(end) = bytes[i + 1..].iter().position(|&b| b == b'%') {
                let name = &raw[i + 1..i + 1 + end];
                if let Ok(val) = std::env::var(name) {
                    out.push_str(&val);
                    i += end + 2;
                    continue;
                }
                out.push('%');
                out.push_str(name);
                out.push('%');
                i += end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    let path = PathBuf::from(out);
    if path.exists() { Some(path) } else { None }
}

const BROWSER_CANDIDATES: &[(&str, &[&str])] = &[
    (
        "Microsoft Edge",
        &[
            r"%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe",
            r"%ProgramFiles%\Microsoft\Edge\Application\msedge.exe",
            r"%LocalAppData%\Microsoft\Edge\Application\msedge.exe",
        ],
    ),
    (
        "Google Chrome",
        &[
            r"%ProgramFiles%\Google\Chrome\Application\chrome.exe",
            r"%ProgramFiles(x86)%\Google\Chrome\Application\chrome.exe",
            r"%LocalAppData%\Google\Chrome\Application\chrome.exe",
        ],
    ),
    (
        "Brave",
        &[
            r"%ProgramFiles%\BraveSoftware\Brave-Browser\Application\brave.exe",
            r"%ProgramFiles(x86)%\BraveSoftware\Brave-Browser\Application\brave.exe",
            r"%LocalAppData%\BraveSoftware\Brave-Browser\Application\brave.exe",
        ],
    ),
];

pub fn detect_default_browser() -> DetectedBrowser {
    for (name, paths) in BROWSER_CANDIDATES {
        for raw_path in *paths {
            if let Some(path) = expand_env_path(raw_path) {
                return DetectedBrowser {
                    browser_name: (*name).to_string(),
                    executable_path: path.display().to_string(),
                    debug_port: DEFAULT_DEBUG_PORT,
                    note: String::new(),
                };
            }
        }
    }
    DetectedBrowser {
        browser_name: "unknown".into(),
        executable_path: String::new(),
        debug_port: DEFAULT_DEBUG_PORT,
        note: "No Chromium-compatible browser executable was found.".into(),
    }
}

/// Spawn a Chromium-family browser with remote debugging enabled.
pub async fn launch_chrome_debug(
    chrome_path: &str,
    start_url: &str,
    user_data_dir: &Path,
    port: u16,
) -> AppResult<()> {
    if chrome_path.trim().is_empty() {
        return Err(AppError::Browser("Browser executable path is empty.".into()));
    }
    if !Path::new(chrome_path).exists() {
        return Err(AppError::Browser(format!(
            "Browser executable was not found: {chrome_path}"
        )));
    }
    tokio::fs::create_dir_all(user_data_dir).await?;
    let user_data_arg = format!("--user-data-dir={}", user_data_dir.display());
    let port_arg = format!("--remote-debugging-port={port}");
    let url_arg = if start_url.is_empty() { "about:blank" } else { start_url };
    Command::new(chrome_path)
        .arg(&port_arg)
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-allow-origins=*")
        .arg(&user_data_arg)
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-mode")
        .arg("--disable-session-crashed-bubble")
        .arg("--window-position=40,40")
        .arg("--window-size=1320,900")
        .arg("--new-window")
        .arg(url_arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| AppError::Browser(format!("Browser launch failed: {e}")))?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct LaunchResult {
    pub browser_name: String,
    pub executable_path: String,
    pub user_data_dir: String,
    pub debug_port: u16,
    pub start_url: String,
    pub reused_existing_debug_port: bool,
    pub page_count: usize,
}

/// Reuse an existing debug session if one is up; otherwise launch a new
/// Chromium with the WoS profile dir, wait for a page target to appear, and
/// return launch info.
pub async fn launch_wos_browser(
    chrome_path: Option<&str>,
    start_url: Option<&str>,
    user_data_dir: Option<&Path>,
    port: u16,
    verify_timeout_seconds: f64,
) -> AppResult<LaunchResult> {
    let resolved_start_url = start_url.unwrap_or(WOS_START_URL).to_string();

    // First try reusing an existing debug port session.
    if let Ok(targets) = get_debug_targets(port).await {
        let page_targets: Vec<&DebugTarget> =
            targets.iter().filter(|t| t.kind == "page").collect();
        if !page_targets.is_empty() {
            return Ok(LaunchResult {
                browser_name: "existing Chromium browser".into(),
                executable_path: String::new(),
                user_data_dir: String::new(),
                debug_port: port,
                start_url: resolved_start_url,
                reused_existing_debug_port: true,
                page_count: page_targets.len(),
            });
        }
    }

    let detected = detect_default_browser();
    let explicit = chrome_path.unwrap_or("").trim().to_string();
    let exec_path: String = if !explicit.is_empty() && Path::new(&explicit).exists() {
        explicit
    } else if !detected.executable_path.is_empty() {
        detected.executable_path.clone()
    } else {
        return Err(AppError::Browser(
            "No Chromium-compatible browser executable was found. Install Chrome or Edge, or pass chrome_path explicitly.".into(),
        ));
    };

    let resolved_user_data_dir = user_data_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_profile_dir);

    launch_chrome_debug(&exec_path, &resolved_start_url, &resolved_user_data_dir, port).await?;

    // Poll for a page target to appear.
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs_f64(verify_timeout_seconds.max(1.0));
    let mut page_count = 0;
    while tokio::time::Instant::now() < deadline {
        if let Ok(targets) = get_debug_targets(port).await {
            page_count = targets.iter().filter(|t| t.kind == "page").count();
            if page_count > 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    if page_count == 0 {
        return Err(AppError::Browser(format!(
            "Browser launch command was sent, but no page target became available on debug port {port}.",
        )));
    }

    Ok(LaunchResult {
        browser_name: detected.browser_name,
        executable_path: exec_path,
        user_data_dir: resolved_user_data_dir.display().to_string(),
        debug_port: port,
        start_url: resolved_start_url,
        reused_existing_debug_port: false,
        page_count,
    })
}
