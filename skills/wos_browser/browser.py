from __future__ import annotations

import json
from pathlib import Path
import re
import subprocess
import time
from typing import Any
from urllib.parse import quote

import requests

from .cdp import CDPSession, evaluate_js, wait_for_condition, get_page_snapshot


DEFAULT_DEBUG_PORT = 9222
DOI_PATTERN = re.compile(r"10\.\d{4,9}/[-._;()/:A-Z0-9]+", re.IGNORECASE)


def launch_chrome_debug(
    chrome_path: str,
    start_url: str,
    user_data_dir: str,
    port: int = DEFAULT_DEBUG_PORT,
) -> None:
    Path(user_data_dir).mkdir(parents=True, exist_ok=True)
    subprocess.Popen(
        [
            chrome_path,
            f"--remote-debugging-port={port}",
            "--remote-allow-origins=*",
            f"--user-data-dir={user_data_dir}",
            "--new-window",
            start_url,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        shell=False,
    )


def get_debug_targets(port: int = DEFAULT_DEBUG_PORT) -> list[dict[str, Any]]:
    response = requests.get(f"http://127.0.0.1:{port}/json/list", timeout=5)
    response.raise_for_status()
    targets = response.json()
    return targets if isinstance(targets, list) else []


def get_page_targets(port: int = DEFAULT_DEBUG_PORT) -> list[dict[str, Any]]:
    return [target for target in get_debug_targets(port) if str(target.get("type", "")) == "page"]


def open_debug_target(url: str, port: int = DEFAULT_DEBUG_PORT) -> dict[str, Any]:
    encoded_url = quote(str(url or "about:blank"), safe="")
    response = requests.put(f"http://127.0.0.1:{port}/json/new?{encoded_url}", timeout=10)
    response.raise_for_status()
    target = response.json()
    if not isinstance(target, dict):
        raise ValueError("Chrome did not return a valid debug target.")
    return target


def close_debug_target(target_id: str, port: int = DEFAULT_DEBUG_PORT) -> None:
    if not target_id:
        return
    try:
        requests.get(f"http://127.0.0.1:{port}/json/close/{target_id}", timeout=10)
    except Exception:
        return


def wait_for_new_page_target(
    known_target_ids: set[str],
    *,
    port: int = DEFAULT_DEBUG_PORT,
    timeout_seconds: float = 8.0,
    poll_interval: float = 0.4,
) -> dict[str, Any] | None:
    deadline = time.time() + timeout_seconds
    while time.time() < deadline:
        try:
            page_targets = get_page_targets(port)
        except Exception:
            page_targets = []
        for target in reversed(page_targets):
            target_id = str(target.get("id", "") or "")
            if not target_id or target_id in known_target_ids:
                continue
            return target
        time.sleep(poll_interval)
    return None


def find_wos_target(port: int = DEFAULT_DEBUG_PORT, preferred_patterns: list[str] | None = None) -> dict[str, Any]:
    targets = get_debug_targets(port)
    page_targets = [target for target in targets if target.get("type") == "page"]
    wos_targets = [target for target in page_targets if "webofscience" in str(target.get("url", "")).lower()]
    if wos_targets:
        patterns = preferred_patterns or []
        for pattern in patterns:
            for target in reversed(wos_targets):
                url = str(target.get("url", "")).lower()
                title = str(target.get("title", "")).lower()
                if pattern in url or pattern in title:
                    return target
        return wos_targets[-1]
    if page_targets:
        return page_targets[-1]
    raise ValueError("No browser page target was found on the debug port.")


def find_wos_search_target(port: int = DEFAULT_DEBUG_PORT) -> dict[str, Any]:
    return find_wos_target(port, preferred_patterns=["advanced-search", "basic-search", "smart-search", "search"])


def find_wos_results_target(port: int = DEFAULT_DEBUG_PORT) -> dict[str, Any]:
    targets = get_debug_targets(port)
    page_targets = [target for target in targets if target.get("type") == "page"]
    wos_targets = [target for target in page_targets if "webofscience" in str(target.get("url", "")).lower()]
    result_patterns = ["summary", "results", "result"]
    for pattern in result_patterns:
        for target in reversed(wos_targets):
            url = str(target.get("url", "")).lower()
            title = str(target.get("title", "")).lower()
            if pattern in url or pattern in title:
                return target
    if wos_targets:
        page_preview = "; ".join(
            f"{target.get('title', '')} | {target.get('url', '')}" for target in wos_targets[-3:]
        )
        raise ValueError(
            "No Web of Science results page was found on the debug port. "
            "Open the WoS results/summary page before scraping. "
            f"Current WoS pages: {page_preview}"
        )
    if page_targets:
        raise ValueError("No Web of Science page was found on the debug port.")
    raise ValueError("No browser page target was found on the debug port.")


def collect_cookies(session: CDPSession, urls: list[str] | None = None) -> list[dict[str, Any]]:
    params: dict[str, Any] = {}
    cleaned_urls = [str(url).strip() for url in (urls or []) if str(url).strip()]
    if cleaned_urls:
        params["urls"] = cleaned_urls
    try:
        result = session.call("Network.getCookies", params)
    except Exception:
        return []
    cookies = result.get("cookies", [])
    return cookies if isinstance(cookies, list) else []


def prepare_page_session(session: CDPSession) -> None:
    session.call("Page.enable")
    session.call("Runtime.enable")
    session.call("Network.enable")


def collect_article_page_payload(session: CDPSession) -> dict[str, Any]:
    js = """
(() => {
  const MAX_HTML_CHARS = 120000;
  const MAX_BODY_TEXT_CHARS = 180000;
  const MAX_ARTICLE_TEXT_CHARS = 240000;
  const MAX_ARTICLE_HTML_CHARS = 120000;
  const clean = (value) => String(value || '')
    .replace(/\\r\\n/g, '\\n')
    .replace(/\\r/g, '\\n')
    .replace(/[ \\t\\f\\v]+/g, ' ')
    .replace(/\\n{3,}/g, '\\n\\n')
    .trim();
  const clip = (value, limit) => String(value || '').slice(0, limit);
  const visible = (node) => !!(node && (node.offsetParent !== null || node.getClientRects().length));
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

  const scanText = `${currentUrl}\\n${title}\\n${bodyText.slice(0, 4000)}`.toLowerCase();
  const challengeDetected =
    scanText.includes('validate.perfdrive.com') ||
    scanText.includes('captcha') ||
    scanText.includes('verify you are human') ||
    scanText.includes('checking your browser') ||
    scanText.includes('access denied') ||
    scanText.includes('unusual traffic') ||
    scanText.includes('bot verification');
  const notFoundDetected =
    /\\b404\\b/.test(scanText) ||
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
"""
    try:
        result = evaluate_js(session, js)
        return result if isinstance(result, dict) else {}
    except Exception as exc:
        snapshot = get_page_snapshot(session, max_chars=100_000)
        text = str(snapshot.get("text", "") or "")
        html = str(snapshot.get("html", "") or "")
        return {
            "url": str(snapshot.get("url", "") or ""),
            "title": str(snapshot.get("title", "") or ""),
            "html": html,
            "bodyText": text,
            "articleText": text,
            "articleHtml": html,
            "textChars": int(snapshot.get("text_chars", 0) or len(text)),
            "articleTextChars": int(snapshot.get("text_chars", 0) or len(text)),
            "challengeDetected": False,
            "notFoundDetected": False,
            "readyState": "",
            "payloadError": str(exc)[:500],
        }


def click_article_entrypoint(session: CDPSession) -> dict[str, Any]:
    js = """
(() => {
  const visible = (node) => !!(node && (node.offsetParent !== null || node.getClientRects().length));
  const disabled = (node) => !!(
    !node || node.disabled || node.getAttribute('aria-disabled') === 'true' ||
    String(node.className || '').toLowerCase().includes('disabled')
  );
  const candidates = Array.from(document.querySelectorAll('a[href], button, [role="button"]'));
  const patterns = [
    /\\b(full text|full text access|read full text|read article|view full text|html full text)\\b/i,
    /\\bfull text\\b/i, /\\bview article\\b/i,
  ];
  const clean = (value) => (value || '').replace(/\\s+/g, ' ').trim();
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
    if (/\\b(download pdf|view pdf|pdf)\\b/i.test(combined)) continue;
    let score = 0;
    if (/\\b(full text access|read full text|view full text|html full text|full text)\\b/i.test(combined)) score += 5;
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
"""
    result = evaluate_js(session, js)
    return result if isinstance(result, dict) else {"clicked": False}


def should_click_article_entrypoint(
    *,
    current_url: str,
    elapsed_on_page: float,
    min_wait_seconds: float,
) -> bool:
    lowered_url = str(current_url or "").lower()
    if "ieeexplore.ieee.org/document/" in lowered_url:
        return False
    if elapsed_on_page < min(4.0, max(0.0, float(min_wait_seconds))):
        return False
    return True


def fetch_fulltext_via_browser(
    url: str,
    port: int = DEFAULT_DEBUG_PORT,
    wait_seconds: float = 18.0,
    min_text_chars: int = 4000,
    include_cookies: bool = False,
    min_wait_seconds: float = 0.0,
    follow_pdf_link: bool = True,
    click_pdf_links: bool = True,
) -> dict[str, Any]:
    del follow_pdf_link, click_pdf_links
    target = open_debug_target(url, port=port)
    ws_url = target.get("webSocketDebuggerUrl")
    if not ws_url:
        close_debug_target(str(target.get("id", "")), port=port)
        raise ValueError("The temporary browser target does not expose a debugger websocket.")

    session = CDPSession(ws_url)
    opened_targets: list[str] = [str(target.get("id", "") or "")]
    known_page_target_ids = {
        str(item.get("id", "") or "")
        for item in get_page_targets(port)
        if str(item.get("id", "") or "")
    }
    sessions_to_close: list[CDPSession] = [session]
    try:
        prepare_page_session(session)

        wait_for_condition(
            session,
            "(() => { const ready = document.readyState === 'interactive' || document.readyState === 'complete'; return ready && !!document.body; })()",
            timeout_seconds=min(wait_seconds, 12.0),
            poll_interval=0.4,
        )

        deadline = time.time() + max(wait_seconds, 8.0)
        clicked = False
        payload: dict[str, Any] = {}
        active_target = target
        wait_anchor_url = str(url or "")
        wait_anchor_at = time.time()

        while time.time() < deadline:
            payload = collect_article_page_payload(session)
            current_url = str(payload.get("url", "") or "")
            if current_url and current_url != wait_anchor_url:
                wait_anchor_url = current_url
                wait_anchor_at = time.time()
            text_chars = int(payload.get("articleTextChars", 0) or 0)
            body_chars = int(payload.get("textChars", 0) or 0)
            challenge_detected = bool(payload.get("challengeDetected"))
            not_found_detected = bool(payload.get("notFoundDetected"))
            if not_found_detected:
                break
            if (text_chars >= min_text_chars or body_chars >= min_text_chars) and not challenge_detected:
                break
            if not clicked and should_click_article_entrypoint(
                current_url=current_url,
                elapsed_on_page=time.time() - wait_anchor_at,
                min_wait_seconds=min_wait_seconds,
            ):
                click_result = click_article_entrypoint(session)
                if click_result.get("clicked"):
                    clicked = True
                    wait_anchor_at = time.time()
                    wait_for_condition(
                        session,
                        f"(() => location.href !== {json.dumps(current_url)})()",
                        timeout_seconds=8.0,
                        poll_interval=0.4,
                    )
                    new_target = wait_for_new_page_target(known_page_target_ids, port=port, timeout_seconds=5.0, poll_interval=0.3)
                    if new_target:
                        new_target_id = str(new_target.get("id", "") or "")
                        new_ws_url = str(new_target.get("webSocketDebuggerUrl", "") or "")
                        if new_target_id:
                            known_page_target_ids.add(new_target_id)
                            opened_targets.append(new_target_id)
                        if new_ws_url:
                            new_session = CDPSession(new_ws_url)
                            sessions_to_close.append(new_session)
                            prepare_page_session(new_session)
                            session = new_session
                            active_target = new_target
                            wait_anchor_url = str(new_target.get("url", "") or wait_anchor_url)
                            wait_anchor_at = time.time()
                            wait_for_condition(
                                session,
                                "(() => { const ready = document.readyState === 'interactive' || document.readyState === 'complete'; return ready && !!document.body; })()",
                                timeout_seconds=12.0,
                                poll_interval=0.4,
                            )
                    time.sleep(0.8)
                    continue
            time.sleep(0.8)

        payload = collect_article_page_payload(session)
        text_chars = int(payload.get("articleTextChars", 0) or 0)
        body_chars = int(payload.get("textChars", 0) or 0)
        has_sufficient_text = (text_chars >= min_text_chars or body_chars >= min_text_chars)
        payload["pdfUrl"] = ""
        payload["downloadPdfBytes"] = b""
        payload["downloadPdfFileName"] = ""
        payload["networkPdfUrl"] = ""
        payload["networkPdfBytes"] = b""
        payload["networkPdfVia"] = ""
        payload["activeTargetId"] = str(active_target.get("id", "") or "")
        if include_cookies:
            payload["cookies"] = collect_cookies(
                session,
                urls=[url, str(payload.get("url", "") or "")],
            )
        payload["clickedEntrypoint"] = clicked

        # If we have sufficient text, close tabs immediately to avoid keeping browser open
        if has_sufficient_text:
            for current_session in reversed(sessions_to_close):
                try:
                    current_session.close()
                except Exception:
                    pass
            sessions_to_close.clear()
            for target_id in reversed(opened_targets):
                close_debug_target(target_id, port=port)
            opened_targets.clear()

        return payload
    finally:
        for current_session in reversed(sessions_to_close):
            try:
                current_session.close()
            except Exception:
                pass
        for target_id in reversed(opened_targets):
            close_debug_target(target_id, port=port)
