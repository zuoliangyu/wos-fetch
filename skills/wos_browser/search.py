from __future__ import annotations

import json
import time
from typing import Any

from .cdp import CDPSession, evaluate_js, wait_for_condition, goto_url, get_page_snapshot
from .browser import launch_chrome_debug, find_wos_search_target, DEFAULT_DEBUG_PORT


def open_wos_query_builder(session: CDPSession, timeout_seconds: float = 20.0) -> dict[str, Any]:
    target_url = "https://www.webofscience.com/wos/woscc/advanced-search"
    before_url = str(evaluate_js(session, "location.href") or "")
    before_title = str(evaluate_js(session, "document.title") or "")

    if not goto_url(session, target_url):
        return {"ok": False, "reason": "advanced_search_navigation_failed"}

    ready = wait_for_condition(
        session,
        r"""
(() => {
  const url = String(location.href || '');
  const input = document.querySelector('#advancedSearchInputArea, textarea[name="search"], textarea.search-criteria-input');
  const visible = !!(input && (input.offsetParent !== null || input.getClientRects().length));
  return url.includes('/advanced-search') && visible;
})()
""",
        timeout_seconds=timeout_seconds,
    )
    if not ready:
        return {"ok": False, "reason": "advanced_search_not_ready"}

    return {
        "ok": True,
        "action": "opened_query_builder",
        "before_url": before_url,
        "before_title": before_title,
        "after_url": str(evaluate_js(session, "location.href") or ""),
        "after_title": str(evaluate_js(session, "document.title") or ""),
    }


def _dispatch_mouse_click(session: CDPSession, x: float, y: float) -> None:
    session.call("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x, "y": y, "button": "none"})
    session.call("Input.dispatchMouseEvent", {"type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1})
    session.call("Input.dispatchMouseEvent", {"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1})


def _wait_for_search_started(session: CDPSession, before_url: str, timeout_seconds: float = 8.0) -> bool:
    escaped_before = json.dumps(before_url or "")
    return wait_for_condition(
        session,
        rf"""
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
""",
        timeout_seconds=timeout_seconds,
        poll_interval=0.35,
    )


def _find_search_button(session: CDPSession) -> dict[str, Any]:
    result = evaluate_js(
        session,
        r"""
(() => {
  const visible = (node) => {
    if (!node) return false;
    const rect = node.getBoundingClientRect();
    return rect.width > 8 && rect.height > 8 && (node.offsetParent !== null || node.getClientRects().length);
  };
  const disabled = (node) => node.disabled || node.getAttribute('aria-disabled') === 'true' || /\bdisabled\b/i.test(String(node.className || ''));
  const labelOf = (node) => [
    node.innerText,
    node.value,
    node.getAttribute('aria-label'),
    node.getAttribute('title'),
    node.getAttribute('data-ta'),
    node.id,
    node.className
  ].filter(Boolean).join(' ').replace(/\s+/g, ' ').trim();
  const nodes = Array.from(document.querySelectorAll('button, input[type="submit"], [role="button"], a[role="button"], [data-ta]'));
  const scored = nodes
    .filter(node => visible(node) && !disabled(node))
    .map(node => {
      const label = labelOf(node);
      const lower = label.toLowerCase();
      let score = 0;
      if (/run-search|run search|search|submit|检索|搜索|查询/.test(lower)) score += 10;
      if (/advanced|query|search-button|mat-button|primary/.test(lower)) score += 3;
      if (/clear|reset|cancel|export|save|history|帮助|清除|取消|导出/.test(lower)) score -= 20;
      const rect = node.getBoundingClientRect();
      return {score, label, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2};
    })
    .filter(item => item.score > 0)
    .sort((a, b) => b.score - a.score);
  return scored[0] || {};
})()
""",
    )
    return result if isinstance(result, dict) else {}


def fill_advanced_search_and_submit(session: CDPSession, query: str) -> dict[str, Any]:
    before_url = str(evaluate_js(session, "location.href") or "")
    escaped_query = json.dumps(query)
    js = f"""
(() => {{
  const query = {escaped_query};
  const input = document.querySelector('#advancedSearchInputArea, textarea[name="search"], textarea.search-criteria-input');

  if (!input) {{
    return {{ ok: false, reason: 'search_input_not_found' }};
  }}

  const setNativeValue = (element, value) => {{
    const prototype = element.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const descriptor = Object.getOwnPropertyDescriptor(prototype, 'value');
    if (descriptor && descriptor.set) descriptor.set.call(element, value);
    else element.value = value;
  }};

  input.focus();
  setNativeValue(input, query);
  input.selectionStart = input.value.length;
  input.selectionEnd = input.value.length;

  input.dispatchEvent(new Event('input', {{ bubbles: true }}));
  input.dispatchEvent(new Event('change', {{ bubbles: true }}));
  input.dispatchEvent(new KeyboardEvent('keyup', {{ key: ' ', code: 'Space', bubbles: true }}));

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
  for (const selector of buttonSelectors) {{
    const node = document.querySelector(selector);
    const disabled = !node || node.disabled || node.getAttribute('aria-disabled') === 'true' || String(node.className || '').includes('disabled');
    const visible = !!(node && (node.offsetParent !== null || node.getClientRects().length));
    if (!disabled && visible) {{
      const rect = node.getBoundingClientRect();
      buttonCandidates.push({{selector, label: selector, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2}});
    }}
  }}

  const visible = (node) => {{
    if (!node) return false;
    const rect = node.getBoundingClientRect();
    return rect.width > 8 && rect.height > 8 && (node.offsetParent !== null || node.getClientRects().length);
  }};
  const buttons = Array.from(document.querySelectorAll('button, input[type="submit"], [role="button"], a[role="button"], [data-ta]'));
  const scored = buttons
    .filter(node => visible(node) && !node.disabled && node.getAttribute('aria-disabled') !== 'true')
    .map(node => {{
      const label = [node.innerText, node.value, node.getAttribute('aria-label'), node.getAttribute('title'), node.getAttribute('data-ta'), node.id, node.className]
        .filter(Boolean).join(' ').replace(/\\s+/g, ' ').trim();
      const lower = label.toLowerCase();
      let score = 0;
      if (/run-search|run search|search|submit|检索|搜索|查询/.test(lower)) score += 10;
      if (/advanced|query|search-button|mat-button|primary/.test(lower)) score += 3;
      if (/clear|reset|cancel|export|save|history|帮助|清除|取消|导出/.test(lower)) score -= 20;
      return {{node, label, score}};
    }})
    .filter(item => item.score > 0)
    .sort((a, b) => b.score - a.score);
  if (scored[0]) {{
    const rect = scored[0].node.getBoundingClientRect();
    buttonCandidates.push({{selector: 'scored_button', label: scored[0].label, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2}});
  }}

  if (buttonCandidates[0]) {{
    return {{ ok: true, action: 'button_found_for_mouse_click', button: buttonCandidates[0] }};
  }}

  const form = input.closest('form');
  if (form && typeof form.requestSubmit === 'function') {{
    form.requestSubmit();
    return {{ ok: true, action: 'form_request_submit' }};
  }}

  input.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'Enter', code: 'Enter', bubbles: true }}));
  input.dispatchEvent(new KeyboardEvent('keyup', {{ key: 'Enter', code: 'Enter', bubbles: true }}));
  return {{ ok: true, action: 'pressed_enter_fallback' }};
}})()
"""
    result = evaluate_js(session, js)
    payload = result if isinstance(result, dict) else {"ok": False, "reason": "unexpected_result"}
    if not payload.get("ok"):
        return payload
    button = payload.get("button") if isinstance(payload.get("button"), dict) else _find_search_button(session)
    if button.get("x") is not None and button.get("y") is not None:
        _dispatch_mouse_click(session, float(button["x"]), float(button["y"]))
        payload["mouse_click"] = {"button_label": button.get("label", ""), "selector": button.get("selector", ""), "x": button.get("x"), "y": button.get("y")}
        if _wait_for_search_started(session, before_url, timeout_seconds=8.0):
            payload["navigation_started"] = True
            return payload

    if _wait_for_search_started(session, before_url, timeout_seconds=2.0):
        payload["navigation_started"] = True
        return payload

    session.call("Input.dispatchKeyEvent", {"type": "rawKeyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13})
    session.call("Input.dispatchKeyEvent", {"type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13})
    payload["enter_cdp_fallback"] = True
    payload["navigation_started"] = _wait_for_search_started(session, before_url, timeout_seconds=8.0)
    if not payload["navigation_started"]:
        return {"ok": False, "reason": "search_submit_did_not_start", **payload}
    return payload


def wait_for_wos_results_ready(session: CDPSession, timeout_seconds: float = 30.0) -> dict[str, Any]:
    ready = wait_for_condition(
        session,
        """
(() => {
  const url = String(location.href || '');
  const text = String(document.body ? document.body.innerText : '');
  const onResults = url.includes('/summary/') || url.includes('/results/') || url.includes('/relevance/');
  const fullRecordLinks = document.querySelectorAll('a[href*="/full-record/"], a[href*="full-record"]').length;
  const recordNodes = document.querySelectorAll('app-record, app-summary-record, [class*="summary-record" i], [class*="record-card" i]').length;
  const hasPositiveResults = /[1-9][\\d,]*\\s+results\\s+from/i.test(text) || /\\b[1-9][\\d,]*\\s+Documents\\b/i.test(text);
  const noResults = !hasPositiveResults && (/\\b0\\s+results\\s+from\\b|\\b0\\s+Documents\\b|no results found|no records found|your search did not match/i.test(text));
  const loadingText = /loading|please wait/i.test(text.slice(0, 1200));
  const loadingNode = Array.from(document.querySelectorAll('[aria-busy="true"], .mat-progress-spinner, mat-spinner, [class*="loading" i]'))
    .some((node) => node.offsetParent !== null || node.getClientRects().length);
    return onResults && text.length > 500 && (fullRecordLinks > 0 || recordNodes > 0 || noResults) && (fullRecordLinks > 0 || recordNodes > 0 || (!loadingText && !loadingNode));
})()
""",
        timeout_seconds=timeout_seconds,
        poll_interval=0.25,
    )
    results_url = str(evaluate_js(session, "location.href") or "")
    results_title = str(evaluate_js(session, "document.title") or "")
    lowered_url = results_url.lower()
    lowered_title = results_title.lower()
    results_page_reached = any(marker in lowered_url for marker in ("/summary/", "/results/", "/relevance/", "search-results")) or "results" in lowered_title
    record_links = int(evaluate_js(session, "document.querySelectorAll('a[href*=\\\"/full-record/\\\"], a[href*=\\\"full-record\\\"]').length") or 0)
    return {
        "results_ready": bool(ready) or (bool(results_page_reached) and record_links > 0),
        "results_page_reached": bool(results_page_reached),
        "results_url": results_url,
        "results_title": results_title,
        "results_text_chars": int(evaluate_js(session, "document.body ? document.body.innerText.length : 0") or 0),
        "results_record_links": record_links,
    }


def run_wos_search(port: int = DEFAULT_DEBUG_PORT, query: str = "", wait_seconds: float = 30.0) -> dict[str, Any]:
    if not query.strip():
        raise ValueError("Search query cannot be empty.")

    target = find_wos_search_target(port)
    ws_url = target.get("webSocketDebuggerUrl")
    if not ws_url:
        raise ValueError("The active browser target does not expose a debugger websocket.")

    session = CDPSession(ws_url)
    try:
        session.call("Page.enable")
        session.call("Runtime.enable")
        before = get_page_snapshot(session)
        opened = open_wos_query_builder(session)
        if not opened.get("ok"):
            raise ValueError(f"Could not open the WoS query builder: {opened.get('reason', 'unknown_error')}")
        result = fill_advanced_search_and_submit(session, query.strip())
        if not result.get("ok"):
            raise ValueError(f"Could not submit the WoS search: {result.get('reason', 'unknown_error')}")

        ready_info = wait_for_wos_results_ready(session, timeout_seconds=max(1.0, float(wait_seconds or 30.0)))
        after = get_page_snapshot(session)
        return {
            "submitted": True,
            "action": result.get("action", ""),
            "navigation_action": opened.get("action", ""),
            "before_url": before.get("url", ""),
            "after_url": after.get("url", ""),
            "after_title": after.get("title", ""),
            **ready_info,
        }
    finally:
        session.close()
