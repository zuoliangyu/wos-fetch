//! Submit a Web of Science Advanced Search query inside the running browser.
//!
//! Port of `skills/wos_browser/search.py`. The JS payloads are preserved
//! verbatim (modulo Rust string escaping); they are the WoS-DOM-specific bits
//! that have been hardened against multiple WoS UI revisions.

use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

use super::cdp::{
    dispatch_key_event, evaluate_js, get_page_snapshot, goto_url, prepare_page_session,
    wait_for_condition, CdpSession,
};
use super::tools::find_wos_search_target;
use crate::{AppError, AppResult};

const ADVANCED_SEARCH_URL: &str = "https://www.webofscience.com/wos/woscc/advanced-search";

const ADVANCED_SEARCH_READY_JS: &str = r#"
(() => {
  const url = String(location.href || '');
  const input = document.querySelector('#advancedSearchInputArea, textarea[name="search"], textarea.search-criteria-input');
  const visible = !!(input && (input.offsetParent !== null || input.getClientRects().length));
  return url.includes('/advanced-search') && visible;
})()
"#;

#[derive(Debug, Serialize)]
pub struct OpenedQueryBuilder {
    pub ok: bool,
    pub reason: String,
    pub action: String,
    pub before_url: String,
    pub before_title: String,
    pub after_url: String,
    pub after_title: String,
}

pub async fn open_wos_query_builder(
    session: &mut CdpSession,
    timeout_seconds: f64,
) -> AppResult<OpenedQueryBuilder> {
    let before_url = evaluate_js(session, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let before_title = evaluate_js(session, "document.title")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();

    if !goto_url(session, ADVANCED_SEARCH_URL).await? {
        return Ok(OpenedQueryBuilder {
            ok: false,
            reason: "advanced_search_navigation_failed".into(),
            action: String::new(),
            before_url,
            before_title,
            after_url: String::new(),
            after_title: String::new(),
        });
    }

    let ready = wait_for_condition(session, ADVANCED_SEARCH_READY_JS, timeout_seconds, 0.35).await;
    if !ready {
        return Ok(OpenedQueryBuilder {
            ok: false,
            reason: "advanced_search_not_ready".into(),
            action: String::new(),
            before_url,
            before_title,
            after_url: String::new(),
            after_title: String::new(),
        });
    }

    let after_url = evaluate_js(session, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let after_title = evaluate_js(session, "document.title")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    Ok(OpenedQueryBuilder {
        ok: true,
        reason: String::new(),
        action: "opened_query_builder".into(),
        before_url,
        before_title,
        after_url,
        after_title,
    })
}

async fn dispatch_mouse_click_at(session: &mut CdpSession, x: f64, y: f64) -> AppResult<()> {
    super::cdp::dispatch_mouse_click(session, x, y).await
}

fn wait_for_search_started_js(before_url: &str) -> String {
    let escaped_before = serde_json::to_string(before_url).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"
(() => {{
  const before = {escaped_before};
  const url = String(location.href || '');
  const text = String(document.body ? document.body.innerText : '');
  const onResults = url.includes('/summary/') || url.includes('/results/') || url.includes('/relevance/') || /results/i.test(document.title || '');
  const leftAdvancedSearch = before && url !== before && !url.includes('/advanced-search');
  const recordLinks = document.querySelectorAll('a[href*="/full-record/"], a[href*="full-record"]').length;
  const recordNodes = document.querySelectorAll('app-record, app-summary-record, [class*="summary-record" i], [class*="record-card" i]').length;
  return onResults || leftAdvancedSearch || recordLinks > 0 || recordNodes > 0;
}})()
"#
    )
}

async fn wait_for_search_started(
    session: &mut CdpSession,
    before_url: &str,
    timeout_seconds: f64,
) -> bool {
    let js = wait_for_search_started_js(before_url);
    wait_for_condition(session, &js, timeout_seconds, 0.35).await
}

const FILL_AND_SUBMIT_TEMPLATE: &str = r#"
(() => {
  const query = __QUERY__;
  const input = document.querySelector('#advancedSearchInputArea, textarea[name="search"], textarea.search-criteria-input');

  if (!input) {
    return { ok: false, reason: 'search_input_not_found' };
  }

  const setNativeValue = (element, value) => {
    const prototype = element.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const descriptor = Object.getOwnPropertyDescriptor(prototype, 'value');
    if (descriptor && descriptor.set) descriptor.set.call(element, value);
    else element.value = value;
  };

  input.focus();
  setNativeValue(input, query);
  input.selectionStart = input.value.length;
  input.selectionEnd = input.value.length;

  input.dispatchEvent(new Event('input', { bubbles: true }));
  input.dispatchEvent(new Event('change', { bubbles: true }));
  input.dispatchEvent(new KeyboardEvent('keyup', { key: ' ', code: 'Space', bubbles: true }));

  const buttonSelectors = [
    'button[data-ta="run-search"]',
    '[data-ta="run-search"]',
    'button[aria-label*="Search" i]',
    'button[title*="Search" i]',
    'button.search',
    'button[type="submit"]',
    'input[type="submit"]'
  ];

  const buttonCandidates = [];
  for (const selector of buttonSelectors) {
    const node = document.querySelector(selector);
    const disabled = !node || node.disabled || node.getAttribute('aria-disabled') === 'true' || String(node.className || '').includes('disabled');
    const visible = !!(node && (node.offsetParent !== null || node.getClientRects().length));
    if (!disabled && visible) {
      const rect = node.getBoundingClientRect();
      buttonCandidates.push({selector, label: selector, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2});
    }
  }

  const visible = (node) => {
    if (!node) return false;
    const rect = node.getBoundingClientRect();
    return rect.width > 8 && rect.height > 8 && (node.offsetParent !== null || node.getClientRects().length);
  };
  const buttons = Array.from(document.querySelectorAll('button, input[type="submit"], [role="button"], a[role="button"], [data-ta]'));
  const scored = buttons
    .filter(node => visible(node) && !node.disabled && node.getAttribute('aria-disabled') !== 'true')
    .map(node => {
      const label = [node.innerText, node.value, node.getAttribute('aria-label'), node.getAttribute('title'), node.getAttribute('data-ta'), node.id, node.className]
        .filter(Boolean).join(' ').replace(/\s+/g, ' ').trim();
      const lower = label.toLowerCase();
      let score = 0;
      if (/run-search|run search|search|submit|检索|搜索|查询/.test(lower)) score += 10;
      if (/advanced|query|search-button|mat-button|primary/.test(lower)) score += 3;
      if (/clear|reset|cancel|export|save|history|帮助|清除|取消|导出/.test(lower)) score -= 20;
      return {node, label, score};
    })
    .filter(item => item.score > 0)
    .sort((a, b) => b.score - a.score);
  if (scored[0]) {
    const rect = scored[0].node.getBoundingClientRect();
    buttonCandidates.push({selector: 'scored_button', label: scored[0].label, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2});
  }

  if (buttonCandidates[0]) {
    return { ok: true, action: 'button_found_for_mouse_click', button: buttonCandidates[0] };
  }

  const form = input.closest('form');
  if (form && typeof form.requestSubmit === 'function') {
    form.requestSubmit();
    return { ok: true, action: 'form_request_submit' };
  }

  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', bubbles: true }));
  input.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', code: 'Enter', bubbles: true }));
  return { ok: true, action: 'pressed_enter_fallback' };
})()
"#;

pub async fn fill_advanced_search_and_submit(
    session: &mut CdpSession,
    query: &str,
) -> AppResult<Value> {
    let before_url = evaluate_js(session, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let escaped_query = serde_json::to_string(query)?;
    let js = FILL_AND_SUBMIT_TEMPLATE.replace("__QUERY__", &escaped_query);
    let result = evaluate_js(session, &js).await?;
    let mut payload = match result {
        Value::Object(obj) => obj,
        _ => {
            let mut m = serde_json::Map::new();
            m.insert("ok".into(), Value::Bool(false));
            m.insert("reason".into(), Value::String("unexpected_result".into()));
            m
        }
    };
    if !payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(Value::Object(payload));
    }

    let button_opt = payload.get("button").and_then(|b| b.as_object()).cloned();
    if let Some(button) = button_opt {
        let x = button.get("x").and_then(Value::as_f64);
        let y = button.get("y").and_then(Value::as_f64);
        if let (Some(x), Some(y)) = (x, y) {
            dispatch_mouse_click_at(session, x, y).await?;
            let mut click_info = serde_json::Map::new();
            click_info.insert(
                "button_label".into(),
                button
                    .get("label")
                    .cloned()
                    .unwrap_or(Value::String(String::new())),
            );
            click_info.insert(
                "selector".into(),
                button
                    .get("selector")
                    .cloned()
                    .unwrap_or(Value::String(String::new())),
            );
            click_info.insert("x".into(), Value::from(x));
            click_info.insert("y".into(), Value::from(y));
            payload.insert("mouse_click".into(), Value::Object(click_info));
            if wait_for_search_started(session, &before_url, 8.0).await {
                payload.insert("navigation_started".into(), Value::Bool(true));
                return Ok(Value::Object(payload));
            }
        }
    }

    if wait_for_search_started(session, &before_url, 2.0).await {
        payload.insert("navigation_started".into(), Value::Bool(true));
        return Ok(Value::Object(payload));
    }

    // Last resort: CDP Input.dispatchKeyEvent for Enter.
    dispatch_key_event(session, "rawKeyDown", "Enter", "Enter", 13, 0).await?;
    dispatch_key_event(session, "keyUp", "Enter", "Enter", 13, 0).await?;
    payload.insert("enter_cdp_fallback".into(), Value::Bool(true));
    let started = wait_for_search_started(session, &before_url, 8.0).await;
    payload.insert("navigation_started".into(), Value::Bool(started));
    if !started {
        payload.insert("ok".into(), Value::Bool(false));
        payload.insert(
            "reason".into(),
            Value::String("search_submit_did_not_start".into()),
        );
    }
    Ok(Value::Object(payload))
}

const RESULTS_READY_JS: &str = r#"
(() => {
  const url = String(location.href || '');
  const text = String(document.body ? document.body.innerText : '');
  const onResults = url.includes('/summary/') || url.includes('/results/') || url.includes('/relevance/');
  const fullRecordLinks = document.querySelectorAll('a[href*="/full-record/"], a[href*="full-record"]').length;
  const recordNodes = document.querySelectorAll('app-record, app-summary-record, [class*="summary-record" i], [class*="record-card" i]').length;
  const hasPositiveResults = /[1-9][\d,]*\s+results\s+from/i.test(text) || /\b[1-9][\d,]*\s+Documents\b/i.test(text);
  const noResults = !hasPositiveResults && (/\b0\s+results\s+from\b|\b0\s+Documents\b|no results found|no records found|your search did not match/i.test(text));
  const loadingText = /loading|please wait/i.test(text.slice(0, 1200));
  const loadingNode = Array.from(document.querySelectorAll('[aria-busy="true"], .mat-progress-spinner, mat-spinner, [class*="loading" i]'))
    .some((node) => node.offsetParent !== null || node.getClientRects().length);
  return onResults && text.length > 500 && (fullRecordLinks > 0 || recordNodes > 0 || noResults) && (fullRecordLinks > 0 || recordNodes > 0 || (!loadingText && !loadingNode));
})()
"#;

#[derive(Debug, Serialize, Default)]
pub struct ResultsReady {
    pub results_ready: bool,
    pub results_page_reached: bool,
    pub results_url: String,
    pub results_title: String,
    pub results_text_chars: i64,
    pub results_record_links: i64,
}

pub async fn wait_for_wos_results_ready(
    session: &mut CdpSession,
    timeout_seconds: f64,
) -> ResultsReady {
    let ready = wait_for_condition(session, RESULTS_READY_JS, timeout_seconds, 0.25).await;
    let results_url = evaluate_js(session, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let results_title = evaluate_js(session, "document.title")
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let lowered_url = results_url.to_ascii_lowercase();
    let lowered_title = results_title.to_ascii_lowercase();
    let results_page_reached = ["/summary/", "/results/", "/relevance/", "search-results"]
        .iter()
        .any(|m| lowered_url.contains(m))
        || lowered_title.contains("results");
    let record_links = evaluate_js(
        session,
        "document.querySelectorAll('a[href*=\"/full-record/\"], a[href*=\"full-record\"]').length",
    )
    .await
    .ok()
    .and_then(|v| v.as_i64())
    .unwrap_or(0);
    let results_text_chars = evaluate_js(
        session,
        "document.body ? document.body.innerText.length : 0",
    )
    .await
    .ok()
    .and_then(|v| v.as_i64())
    .unwrap_or(0);
    ResultsReady {
        results_ready: ready || (results_page_reached && record_links > 0),
        results_page_reached,
        results_url,
        results_title,
        results_text_chars,
        results_record_links: record_links,
    }
}

#[derive(Debug, Serialize)]
pub struct SearchSubmissionResult {
    pub submitted: bool,
    pub action: String,
    pub navigation_action: String,
    pub before_url: String,
    pub after_url: String,
    pub after_title: String,
    #[serde(flatten)]
    pub ready_info: ResultsReady,
}

pub async fn run_wos_search(
    port: u16,
    query: &str,
    wait_seconds: f64,
) -> AppResult<SearchSubmissionResult> {
    let query = query.trim();
    if query.is_empty() {
        return Err(AppError::BadInput("Search query cannot be empty.".into()));
    }
    let target = find_wos_search_target(port).await?;
    if target.websocket_url.is_empty() {
        return Err(AppError::Browser(
            "The active browser target does not expose a debugger websocket.".into(),
        ));
    }
    let mut session = CdpSession::connect(&target.websocket_url).await?;
    prepare_page_session(&mut session).await?;

    let before = get_page_snapshot(&mut session, 100_000).await;
    let opened = open_wos_query_builder(&mut session, 20.0).await?;
    if !opened.ok {
        let reason = if opened.reason.is_empty() {
            "unknown_error".into()
        } else {
            opened.reason
        };
        session.close().await;
        return Err(AppError::Browser(format!(
            "Could not open the WoS query builder: {reason}"
        )));
    }
    // Small grace period before injecting the query (matches Python ~0.5s).
    sleep(Duration::from_millis(500)).await;

    let submit_result = fill_advanced_search_and_submit(&mut session, query).await?;
    let submit_obj = submit_result.as_object().cloned().unwrap_or_default();
    if !submit_obj
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let reason = submit_obj
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error")
            .to_string();
        session.close().await;
        return Err(AppError::Browser(format!(
            "Could not submit the WoS search: {reason}"
        )));
    }

    let ready_info = wait_for_wos_results_ready(&mut session, wait_seconds.max(1.0)).await;
    let after = get_page_snapshot(&mut session, 100_000).await;
    let action = submit_obj
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let result = SearchSubmissionResult {
        submitted: true,
        action,
        navigation_action: opened.action,
        before_url: before.url,
        after_url: after.url,
        after_title: after.title,
        ready_info,
    };
    session.close().await;
    let _ = json!({}); // avoid unused-import warning for serde_json::json
    Ok(result)
}
