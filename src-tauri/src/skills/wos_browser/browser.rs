//! High-level: fetch full text via a Chromium tab.
//!
//! Port of `skills/wos_browser/browser.py`. Opens a new debug target, waits
//! for the page to settle, optionally clicks a "Full text"-style entry point,
//! and harvests body + article text via `Runtime.evaluate`.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::sleep;

use super::cdp::{evaluate_js, prepare_page_session, wait_for_condition, CdpSession};
use super::tools::{close_debug_target, get_page_targets, open_debug_target, DebugTarget};
use crate::{AppError, AppResult};

const ARTICLE_PAYLOAD_JS: &str = r#"
(() => {
  const MAX_HTML_CHARS = 120000;
  const MAX_BODY_TEXT_CHARS = 180000;
  const MAX_ARTICLE_TEXT_CHARS = 240000;
  const MAX_ARTICLE_HTML_CHARS = 120000;
  const clean = (value) => String(value || '')
    .replace(/\r\n/g, '\n')
    .replace(/\r/g, '\n')
    .replace(/[ \t\f\v]+/g, ' ')
    .replace(/\n{3,}/g, '\n\n')
    .trim();
  const clip = (value, limit) => String(value || '').slice(0, limit);
  const bodyText = clean(document.body ? document.body.innerText : '');
  const currentUrl = String(location.href || '');
  const title = clean(document.title || '');

  const contentNodes = Array.from(document.querySelectorAll(
    'article, main, [role="main"], .article-body, .article__body, .main-content, #main-content, .content, #content'
  ));
  let articleText = '';
  let articleHtml = '';
  for (const node of contentNodes) {
    const text = clean(node.innerText || '');
    if (text.length > articleText.length) {
      articleText = text;
      articleHtml = node.outerHTML || '';
    }
  }

  const scanText = `${currentUrl}\n${title}\n${bodyText.slice(0, 4000)}`.toLowerCase();
  const challengeDetected =
    scanText.includes('validate.perfdrive.com') ||
    scanText.includes('captcha') ||
    scanText.includes('verify you are human') ||
    scanText.includes('checking your browser') ||
    scanText.includes('access denied') ||
    scanText.includes('unusual traffic') ||
    scanText.includes('bot verification');
  const notFoundDetected =
    /\b404\b/.test(scanText) ||
    scanText.includes('page not found') ||
    scanText.includes('document not found') ||
    scanText.includes('article not found') ||
    scanText.includes('not found | ieee xplore') ||
    scanText.includes('the page you requested was not found');

  return {
    url: currentUrl,
    title,
    html: clip(document.documentElement ? document.documentElement.outerHTML : '', MAX_HTML_CHARS),
    bodyText: clip(bodyText, MAX_BODY_TEXT_CHARS),
    articleText: clip(articleText, MAX_ARTICLE_TEXT_CHARS),
    articleHtml: clip(articleHtml, MAX_ARTICLE_HTML_CHARS),
    textChars: bodyText.length,
    articleTextChars: articleText.length, challengeDetected, notFoundDetected, readyState: document.readyState,
  };
})()
"#;

const ARTICLE_CLICK_JS: &str = r#"
(() => {
  const visible = (node) => !!(node && (node.offsetParent !== null || node.getClientRects().length));
  const disabled = (node) => !!(
    !node || node.disabled || node.getAttribute('aria-disabled') === 'true' ||
    String(node.className || '').toLowerCase().includes('disabled')
  );
  const candidates = Array.from(document.querySelectorAll('a[href], button, [role="button"]'));
  const patterns = [
    /\b(full text|full text access|read full text|read article|view full text|html full text)\b/i,
    /\bfull text\b/i, /\bview article\b/i,
  ];
  const clean = (value) => (value || '').replace(/\s+/g, ' ').trim();
  const scored = [];
  for (const node of candidates) {
    if (!visible(node) || disabled(node)) continue;
    const label = [
      node.innerText, node.getAttribute('aria-label'), node.getAttribute('title'),
      node.getAttribute('data-track-action'), node.getAttribute('data-ta'), node.className,
    ].filter(Boolean).join(' ');
    const href = String(node.href || '');
    if (!patterns.some((pattern) => pattern.test(label) || pattern.test(href))) continue;
    const combined = `${label} ${href}`.toLowerCase();
    if (/\b(download pdf|view pdf|pdf)\b/i.test(combined)) continue;
    let score = 0;
    if (/\b(full text access|read full text|view full text|html full text|full text)\b/i.test(combined)) score += 5;
    if (combined.includes('/fulltext/')) score += 5;
    if (combined.includes('/science/article/pii/')) score += 4;
    if (combined.includes('/doi/full/')) score += 4;
    if (combined.includes('showall=true')) score += 4;
    if (combined.includes('/abs/') || combined.includes('/abstract')) score -= 4;
    if (score <= 0) continue;
    scored.push({ node, label: clean(label), href, score });
  }
  scored.sort((a, b) => b.score - a.score);
  const best = scored[0];
  if (best) {
    best.node.click();
    return { clicked: true, label: best.label, href: best.href, score: best.score };
  }
  return { clicked: false };
})()
"#;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BrowserFulltextPayload {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub html: String,
    #[serde(default, rename = "bodyText")]
    pub body_text: String,
    #[serde(default, rename = "articleText")]
    pub article_text: String,
    #[serde(default, rename = "articleHtml")]
    pub article_html: String,
    #[serde(default, rename = "textChars")]
    pub text_chars: i64,
    #[serde(default, rename = "articleTextChars")]
    pub article_text_chars: i64,
    #[serde(default, rename = "challengeDetected")]
    pub challenge_detected: bool,
    #[serde(default, rename = "notFoundDetected")]
    pub not_found_detected: bool,
    #[serde(default, rename = "readyState")]
    pub ready_state: String,
    #[serde(default, rename = "clickedEntrypoint")]
    pub clicked_entrypoint: bool,
    #[serde(default, rename = "activeTargetId")]
    pub active_target_id: String,
    #[serde(default, rename = "payloadError")]
    pub payload_error: String,
}

async fn collect_article_page_payload(
    session: &mut CdpSession,
) -> AppResult<BrowserFulltextPayload> {
    let value = match evaluate_js(session, ARTICLE_PAYLOAD_JS).await {
        Ok(v) => v,
        Err(err) => {
            let snapshot = super::cdp::get_page_snapshot(session, 100_000).await;
            return Ok(BrowserFulltextPayload {
                url: snapshot.url,
                title: snapshot.title,
                html: snapshot.html.clone(),
                body_text: snapshot.text.clone(),
                article_text: snapshot.text.clone(),
                article_html: snapshot.html,
                text_chars: snapshot.text_chars,
                article_text_chars: snapshot.text_chars,
                challenge_detected: false,
                not_found_detected: false,
                ready_state: String::new(),
                clicked_entrypoint: false,
                active_target_id: String::new(),
                payload_error: err.to_string().chars().take(500).collect(),
            });
        }
    };
    let payload: BrowserFulltextPayload = serde_json::from_value(value).map_err(AppError::from)?;
    Ok(payload)
}

#[derive(Debug, Serialize)]
struct ArticleClickResult {
    clicked: bool,
    label: String,
    href: String,
    score: i64,
}

async fn click_article_entrypoint(session: &mut CdpSession) -> AppResult<ArticleClickResult> {
    let value = evaluate_js(session, ARTICLE_CLICK_JS).await?;
    let obj = match value {
        Value::Object(m) => m,
        _ => {
            return Ok(ArticleClickResult {
                clicked: false,
                label: String::new(),
                href: String::new(),
                score: 0,
            })
        }
    };
    Ok(ArticleClickResult {
        clicked: obj.get("clicked").and_then(Value::as_bool).unwrap_or(false),
        label: obj
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        href: obj
            .get("href")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        score: obj.get("score").and_then(Value::as_i64).unwrap_or(0),
    })
}

fn should_click_article_entrypoint(
    current_url: &str,
    elapsed_on_page: Duration,
    min_wait_seconds: f64,
) -> bool {
    let lowered = current_url.to_ascii_lowercase();
    if lowered.contains("ieeexplore.ieee.org/document/") {
        return false;
    }
    let threshold = min_wait_seconds.clamp(0.0, 4.0);
    elapsed_on_page.as_secs_f64() >= threshold
}

async fn wait_for_new_page_target(
    known_target_ids: &HashSet<String>,
    port: u16,
    timeout_seconds: f64,
    poll_interval_seconds: f64,
) -> Option<DebugTarget> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(0.0));
    let interval = Duration::from_secs_f64(poll_interval_seconds.max(0.05));
    while Instant::now() < deadline {
        let targets = get_page_targets(port).await.unwrap_or_default();
        for target in targets.iter().rev() {
            if target.id.is_empty() {
                continue;
            }
            if !known_target_ids.contains(&target.id) {
                return Some(target.clone());
            }
        }
        sleep(interval).await;
    }
    None
}

/// Main entry point: open `url` in a new debug target, wait for it to settle,
/// optionally click a "Full text" entry point, and return the harvested
/// payload. Closes the temporary target when sufficient text was acquired.
pub async fn fetch_fulltext_via_browser(url: &str, port: u16) -> AppResult<BrowserFulltextPayload> {
    fetch_fulltext_via_browser_full(url, port, 18.0, 4000, 0.0).await
}

pub async fn fetch_fulltext_via_browser_full(
    url: &str,
    port: u16,
    wait_seconds: f64,
    min_text_chars: i64,
    min_wait_seconds: f64,
) -> AppResult<BrowserFulltextPayload> {
    let initial_target = open_debug_target(url, port).await?;
    if initial_target.websocket_url.is_empty() {
        let _ = close_debug_target(&initial_target.id, port).await;
        return Err(AppError::Browser(
            "The temporary browser target does not expose a debugger websocket.".into(),
        ));
    }

    let mut session = CdpSession::connect(&initial_target.websocket_url).await?;
    let mut opened_targets: Vec<String> = vec![initial_target.id.clone()];
    let mut known_target_ids: HashSet<String> = get_page_targets(port)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| if t.id.is_empty() { None } else { Some(t.id) })
        .collect();
    // We may swap to a new session if clicking opens a new tab. Track them all
    // so the `defer` block can close all of them.
    let mut sessions_to_close: Vec<CdpSession> = Vec::new();
    let mut active_target = initial_target.clone();

    let close_path =
        async |sessions_to_close: Vec<CdpSession>, opened_targets: Vec<String>, port: u16| {
            for s in sessions_to_close.into_iter().rev() {
                s.close().await;
            }
            for tid in opened_targets.into_iter().rev() {
                let _ = close_debug_target(&tid, port).await;
            }
        };

    let result = (async {
        prepare_page_session(&mut session).await?;
        wait_for_condition(
            &mut session,
            "(() => { const ready = document.readyState === 'interactive' || document.readyState === 'complete'; return ready && !!document.body; })()",
            wait_seconds.min(12.0),
            0.4,
        )
        .await;

        let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds.max(8.0));
        let mut clicked = false;
        let mut payload;
        let mut wait_anchor_url = url.to_string();
        let mut wait_anchor_at = Instant::now();

        while Instant::now() < deadline {
            payload = collect_article_page_payload(&mut session).await?;
            let current_url = payload.url.clone();
            if !current_url.is_empty() && current_url != wait_anchor_url {
                wait_anchor_url = current_url.clone();
                wait_anchor_at = Instant::now();
            }
            if payload.not_found_detected {
                break;
            }
            if (payload.article_text_chars >= min_text_chars
                || payload.text_chars >= min_text_chars)
                && !payload.challenge_detected
            {
                break;
            }
            if !clicked
                && should_click_article_entrypoint(
                    &current_url,
                    wait_anchor_at.elapsed(),
                    min_wait_seconds,
                )
            {
                let click = click_article_entrypoint(&mut session).await?;
                if click.clicked {
                    clicked = true;
                    wait_anchor_at = Instant::now();
                    let escaped =
                        serde_json::to_string(&current_url).unwrap_or_else(|_| "\"\"".into());
                    let condition = format!("(() => location.href !== {escaped})()");
                    wait_for_condition(&mut session, &condition, 8.0, 0.4).await;
                    if let Some(new_target) = wait_for_new_page_target(
                        &known_target_ids,
                        port,
                        5.0,
                        0.3,
                    )
                    .await
                    {
                        if !new_target.id.is_empty() {
                            known_target_ids.insert(new_target.id.clone());
                            opened_targets.push(new_target.id.clone());
                        }
                        if !new_target.websocket_url.is_empty() {
                            let new_session =
                                CdpSession::connect(&new_target.websocket_url).await?;
                            let old = std::mem::replace(&mut session, new_session);
                            sessions_to_close.push(old);
                            prepare_page_session(&mut session).await?;
                            active_target = new_target.clone();
                            wait_anchor_url = if new_target.url.is_empty() {
                                wait_anchor_url
                            } else {
                                new_target.url
                            };
                            wait_anchor_at = Instant::now();
                            wait_for_condition(
                                &mut session,
                                "(() => { const ready = document.readyState === 'interactive' || document.readyState === 'complete'; return ready && !!document.body; })()",
                                12.0,
                                0.4,
                            )
                            .await;
                        }
                    }
                    sleep(Duration::from_millis(800)).await;
                    continue;
                }
            }
            sleep(Duration::from_millis(800)).await;
        }

        payload = collect_article_page_payload(&mut session).await?;
        payload.active_target_id = active_target.id.clone();
        payload.clicked_entrypoint = clicked;
        Ok::<_, AppError>(payload)
    })
    .await;

    let payload = match result {
        Ok(p) => p,
        Err(err) => {
            sessions_to_close.push(session);
            close_path(sessions_to_close, opened_targets, port).await;
            return Err(err);
        }
    };

    let has_sufficient_text = payload.article_text_chars >= 4000 || payload.text_chars >= 4000;
    if has_sufficient_text {
        sessions_to_close.push(session);
        close_path(sessions_to_close, opened_targets, port).await;
    } else {
        // Even for insufficient text we still close — matches Python's finally block.
        sessions_to_close.push(session);
        close_path(sessions_to_close, opened_targets, port).await;
    }
    Ok(payload)
}
