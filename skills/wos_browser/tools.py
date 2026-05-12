from __future__ import annotations

import base64
import json
import os
import re
import shutil
import subprocess
import time
from pathlib import Path
from typing import Any
from urllib.parse import quote
from uuid import uuid4

import requests
import websocket

from .browser import fetch_fulltext_via_browser as enhanced_fetch_fulltext_via_browser
from .scraper import scrape_wos_pages as legacy_scrape_wos_pages
from .search import run_wos_search as legacy_run_wos_search

try:
    import winreg
except ImportError:  # pragma: no cover - non-Windows fallback
    winreg = None


DEFAULT_DEBUG_PORT = 9222
DOI_PATTERN = re.compile(r"10\.\d{4,9}/[-._;()/:A-Z0-9]+", re.IGNORECASE)
WOS_START_URL = "https://www.webofscience.com/wos/woscc/advanced-search"
DEFAULT_BROWSER_PROFILE_DIR = Path.home() / "doi-fulltext-agent-profile" / "browser-wos-profile"
DEFAULT_BROWSER_PROFILE_ROOT = Path.home() / "doi-fulltext-agent-profile"
_LAUNCHED_BROWSER_PROCESSES: list[subprocess.Popen] = []

CHROMIUM_BROWSER_CANDIDATES: tuple[tuple[str, tuple[str, ...], tuple[str, ...]], ...] = (
    (
        "Microsoft Edge",
        ("MSEdgeHTM", "MSEdgeHTML"),
        (
            r"%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe",
            r"%ProgramFiles%\Microsoft\Edge\Application\msedge.exe",
            r"%LocalAppData%\Microsoft\Edge\Application\msedge.exe",
        ),
    ),
    (
        "Google Chrome",
        ("ChromeHTML",),
        (
            r"%ProgramFiles%\Google\Chrome\Application\chrome.exe",
            r"%ProgramFiles(x86)%\Google\Chrome\Application\chrome.exe",
            r"%LocalAppData%\Google\Chrome\Application\chrome.exe",
        ),
    ),
    (
        "Brave",
        ("BraveHTML",),
        (
            r"%ProgramFiles%\BraveSoftware\Brave-Browser\Application\brave.exe",
            r"%ProgramFiles(x86)%\BraveSoftware\Brave-Browser\Application\brave.exe",
            r"%LocalAppData%\BraveSoftware\Brave-Browser\Application\brave.exe",
        ),
    ),
)
NON_CDP_BROWSER_PROGIDS: dict[str, str] = {
    "FirefoxURL": "Mozilla Firefox",
    "FirefoxHTML": "Mozilla Firefox",
    "IE.HTTP": "Internet Explorer",
}


class CDPSession:
    def __init__(self, websocket_url: str, timeout: int = 120):
        self.ws = websocket.create_connection(websocket_url, timeout=timeout)
        self._timeout = timeout
        self.ws.settimeout(timeout)
        self._message_id = 0
        self._responses: dict[int, dict[str, Any]] = {}
        self._events: list[dict[str, Any]] = []

    def close(self) -> None:
        self.ws.close()

    def _recv(self, timeout: float | None = None) -> dict[str, Any] | None:
        if timeout is not None:
            self.ws.settimeout(timeout)
        try:
            raw = self.ws.recv()
        except websocket.WebSocketTimeoutException:
            return None
        finally:
            if timeout is not None:
                self.ws.settimeout(self._timeout)
        message = json.loads(raw)
        if "id" in message:
            self._responses[int(message["id"])] = message
        elif "method" in message:
            self._events.append(message)
        return message

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        self._message_id += 1
        message_id = self._message_id
        self.ws.send(json.dumps({"id": message_id, "method": method, "params": params or {}}))
        while True:
            if message_id in self._responses:
                message = self._responses.pop(message_id)
            else:
                self._recv()
                continue
            if "error" in message:
                raise RuntimeError(message["error"])
            return message.get("result", {})

    def poll(self, timeout_seconds: float = 0.1, max_messages: int = 100) -> None:
        deadline = time.time() + max(0.0, timeout_seconds)
        count = 0
        while count < max_messages and time.time() <= deadline:
            if self._recv(timeout=max(0.01, deadline - time.time())) is None:
                break
            count += 1


def evaluate_js(session: CDPSession, expression: str, await_promise: bool = False) -> Any:
    result = session.call(
        "Runtime.evaluate",
        {"expression": expression, "returnByValue": True, "awaitPromise": await_promise},
    )
    return result.get("result", {}).get("value")


def safe_evaluate_js(session: CDPSession, expression: str, default: Any = "", await_promise: bool = False) -> Any:
    try:
        return evaluate_js(session, expression, await_promise=await_promise)
    except Exception:
        return default


def wait_for_condition(
    session: CDPSession,
    expression: str,
    timeout_seconds: float = 15.0,
    poll_interval: float = 0.3,
) -> bool:
    deadline = time.time() + timeout_seconds
    while time.time() < deadline:
        if safe_evaluate_js(session, expression, False):
            return True
        time.sleep(poll_interval)
    return False


def get_page_snapshot(session: CDPSession, max_chars: int = 120_000) -> dict[str, Any]:
    limit = max(1000, min(int(max_chars or 120_000), 120_000))
    return {
        "url": safe_evaluate_js(session, "location.href", ""),
        "title": safe_evaluate_js(session, "document.title", ""),
        "html": safe_evaluate_js(session, f"(document.documentElement ? document.documentElement.outerHTML : '').slice(0, {limit})", ""),
        "text": safe_evaluate_js(session, f"(document.body ? document.body.innerText : '').slice(0, {limit})", ""),
        "html_chars": safe_evaluate_js(session, "document.documentElement ? document.documentElement.outerHTML.length : 0", 0),
        "text_chars": safe_evaluate_js(session, "document.body ? document.body.innerText.length : 0", 0),
    }


def _expand_existing_path(raw_path: str) -> str:
    cleaned = str(raw_path or "").strip().strip('"').strip("'")
    path = Path(os.path.expandvars(cleaned)).expanduser()
    return str(path) if path.exists() else ""


def _browser_name_from_executable(executable: str, fallback: str = "unknown") -> str:
    name = Path(str(executable or "")).name.lower()
    if "chrome" in name:
        return "Google Chrome"
    if "msedge" in name or "edge" in name:
        return "Microsoft Edge"
    return fallback


def _read_default_https_progid() -> str:
    if winreg is None:
        return ""
    key_path = r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice"
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, key_path) as key:
            value, _ = winreg.QueryValueEx(key, "ProgId")
    except OSError:
        return ""
    return str(value or "").strip()


def _find_candidate_executable(name_hint: str = "") -> tuple[str, str]:
    progid = name_hint.strip()
    ordered = list(CHROMIUM_BROWSER_CANDIDATES)
    if progid:
        ordered.sort(key=lambda item: 0 if progid in item[1] else 1)

    for browser_name, progids, paths in ordered:
        if progid and progid not in progids:
            continue
        for raw_path in paths:
            executable = _expand_existing_path(raw_path)
            if executable:
                return executable, browser_name

    for browser_name, _progids, paths in CHROMIUM_BROWSER_CANDIDATES:
        for raw_path in paths:
            executable = _expand_existing_path(raw_path)
            if executable:
                return executable, browser_name

    for command, browser_name in (("msedge", "Microsoft Edge"), ("chrome", "Google Chrome"), ("brave", "Brave")):
        executable = shutil.which(command)
        if executable:
            return executable, browser_name
    return "", ""


def _browser_launch_candidates(explicit_path: str = "") -> list[tuple[str, str]]:
    candidates: list[tuple[str, str]] = []
    explicit = _expand_existing_path(explicit_path) if explicit_path else ""
    if explicit:
        candidates.append((explicit, _browser_name_from_executable(explicit)))
    detected = detect_default_browser()
    executable = str(detected.get("executable_path") or "")
    name = str(detected.get("browser_name") or "")
    if executable:
        candidates.append((executable, name))
    for hint in ("MSEdgeHTM", "ChromeHTML", ""):
        path, browser_name = _find_candidate_executable(hint)
        if path:
            candidates.append((path, browser_name))
    deduped: list[tuple[str, str]] = []
    seen: set[str] = set()
    for path, browser_name in candidates:
        key = str(Path(path)).lower()
        if key in seen:
            continue
        seen.add(key)
        deduped.append((path, browser_name))
    return deduped


def _is_external_page_target(target: dict[str, Any]) -> bool:
    url = str(target.get("url") or "").lower()
    return url.startswith("http://") or url.startswith("https://")


def detect_default_browser() -> dict[str, Any]:
    progid = _read_default_https_progid()
    executable, browser_name = _find_candidate_executable(progid)
    default_name = browser_name
    fallback_used = False

    if progid and not any(progid in progids for _, progids, _ in CHROMIUM_BROWSER_CANDIDATES):
        default_name = NON_CDP_BROWSER_PROGIDS.get(progid, progid)
        fallback_used = bool(executable)

    return {
        "default_https_progid": progid,
        "browser_name": default_name or browser_name or "unknown",
        "executable_path": executable,
        "cdp_supported": bool(executable),
        "fallback_used": fallback_used,
        "debug_port": DEFAULT_DEBUG_PORT,
        "note": "" if executable else "No Chromium-compatible browser executable was found.",
    }


def launch_chrome_debug(chrome_path: str, start_url: str, user_data_dir: str, port: int = DEFAULT_DEBUG_PORT) -> subprocess.Popen:
    executable = str(chrome_path or "").strip().strip('"').strip("'")
    profile_dir = str(user_data_dir or "").strip().strip('"').strip("'")
    if not executable:
        raise ValueError("Browser executable path is empty.")
    if not Path(executable).exists() and not shutil.which(executable):
        raise ValueError(f"Browser executable was not found: {executable}")
    if not profile_dir:
        profile_dir = str(DEFAULT_BROWSER_PROFILE_DIR)
    Path(profile_dir).mkdir(parents=True, exist_ok=True)
    try:
        process = subprocess.Popen(
            [
                executable,
                f"--remote-debugging-port={port}",
                "--remote-debugging-address=127.0.0.1",
                "--remote-allow-origins=*",
                f"--user-data-dir={profile_dir}",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-mode",
                "--disable-session-crashed-bubble",
                "--window-position=40,40",
                "--window-size=1320,900",
                "--new-window",
                str(start_url or "about:blank"),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            shell=False,
        )
        _LAUNCHED_BROWSER_PROCESSES.append(process)
        return process
    except OSError as exc:
        raise ValueError(
            f"Browser launch failed: {exc}. executable={executable}; user_data_dir={profile_dir}. "
            "Remove extra quotes from chrome_path, or omit chrome_path and let the agent use the detected browser."
        ) from exc


def launch_wos_browser(
    *,
    chrome_path: str = "",
    start_url: str = WOS_START_URL,
    user_data_dir: str = "",
    port: int = DEFAULT_DEBUG_PORT,
    verify_timeout_seconds: float = 20.0,
) -> dict[str, Any]:
    try:
        existing_targets = get_debug_targets(port)
        page_targets = [
            target
            for target in existing_targets
            if str(target.get("type", "")) == "page" and _is_external_page_target(target)
        ]
        if not page_targets:
            try:
                open_debug_target(start_url, port=port)
                time.sleep(1.0)
                existing_targets = get_debug_targets(port)
                page_targets = [
                    target
                    for target in existing_targets
                    if str(target.get("type", "")) == "page" and _is_external_page_target(target)
                ]
            except Exception:
                page_targets = []
        if page_targets:
            return {
                "browser_name": "existing Chromium browser",
                "executable_path": "",
                "user_data_dir": "",
                "debug_port": port,
                "start_url": start_url,
                "fallback_used": False,
                "targets_count": len(existing_targets),
                "page_count": len(page_targets),
                "target_titles": [str(target.get("title") or "") for target in page_targets[:5]],
                "target_urls": [str(target.get("url") or "") for target in page_targets[:5]],
                "reused_existing_debug_port": True,
            }
    except Exception:
        pass

    detected = detect_default_browser()
    explicit = str(chrome_path or "").strip().strip('"').strip("'")
    candidates = _browser_launch_candidates(explicit)
    if not candidates:
        raise ValueError("No Chromium-compatible browser executable was found. Install Chrome or Edge, or pass chrome_path explicitly.")
    errors: list[str] = []
    for executable, browser_name in candidates:
        profile_dir = str(user_data_dir or "").strip().strip('"').strip("'")
        if not profile_dir:
            profile_dir = str(DEFAULT_BROWSER_PROFILE_ROOT / f"browser-wos-profile-{port}-{uuid4().hex[:8]}")
        process = launch_chrome_debug(chrome_path=executable, start_url=start_url, user_data_dir=profile_dir, port=port)
        deadline = time.monotonic() + max(float(verify_timeout_seconds or 0), 1.0)
        last_error = ""
        targets: list[dict[str, Any]] = []
        page_targets: list[dict[str, Any]] = []
        stable_hits = 0
        while time.monotonic() < deadline:
            if process.poll() is not None:
                last_error = f"browser exited with code {process.returncode}"
                break
            try:
                targets = get_debug_targets(port)
                page_targets = [target for target in targets if str(target.get("type", "")) == "page"]
                matching_pages = [
                    target
                    for target in page_targets
                    if str(start_url or "").split("/")[2].lower() in str(target.get("url", "")).lower()
                ] if "://" in str(start_url or "") else page_targets
                if matching_pages or page_targets:
                    stable_hits += 1
                    if stable_hits >= 3:
                        time.sleep(1.5)
                        get_debug_targets(port)
                        return {
                            "browser_name": _browser_name_from_executable(executable, browser_name or str(detected.get("browser_name") or "unknown")),
                            "executable_path": executable,
                            "user_data_dir": profile_dir,
                            "debug_port": port,
                            "start_url": start_url,
                            "fallback_used": bool(detected.get("fallback_used")),
                            "targets_count": len(targets),
                            "page_count": len(page_targets),
                            "target_titles": [str(target.get("title") or "") for target in page_targets[:5]],
                            "target_urls": [str(target.get("url") or "") for target in page_targets[:5]],
                        }
                else:
                    stable_hits = 0
            except Exception as exc:
                last_error = str(exc)
                stable_hits = 0
            time.sleep(0.35)
        errors.append(f"{executable}: {last_error or 'no page targets'}")
    raise RuntimeError(
        f"Browser launch command was sent, but no page target became available on debug port {port}. "
        + " | ".join(errors[-3:])
    )


def get_debug_targets(port: int = DEFAULT_DEBUG_PORT) -> list[dict[str, Any]]:
    response = requests.get(f"http://127.0.0.1:{port}/json/list", timeout=5)
    response.raise_for_status()
    targets = response.json()
    return targets if isinstance(targets, list) else []


def close_debug_target(target_id: str, port: int = DEFAULT_DEBUG_PORT) -> None:
    if not str(target_id or "").strip():
        return
    requests.get(f"http://127.0.0.1:{port}/json/close/{target_id}", timeout=5)


def inspect_browser_targets(port: int = DEFAULT_DEBUG_PORT) -> list[dict[str, Any]]:
    return get_debug_targets(port)


def get_page_targets(port: int = DEFAULT_DEBUG_PORT) -> list[dict[str, Any]]:
    return [
        target
        for target in get_debug_targets(port)
        if str(target.get("type", "")) == "page" and _is_external_page_target(target)
    ]


def open_debug_target(url: str, port: int = DEFAULT_DEBUG_PORT) -> dict[str, Any]:
    encoded_url = quote(str(url or "about:blank"), safe="")
    response = requests.put(f"http://127.0.0.1:{port}/json/new?{encoded_url}", timeout=10)
    response.raise_for_status()
    target = response.json()
    if not isinstance(target, dict):
        raise ValueError("Browser did not return a valid debug target.")
    return target


def find_wos_target(port: int = DEFAULT_DEBUG_PORT, preferred_patterns: list[str] | None = None) -> dict[str, Any]:
    patterns = preferred_patterns or ["webofscience.com", "wos"]
    page_targets = get_page_targets(port)
    for target in page_targets:
        haystack = f"{target.get('url', '')} {target.get('title', '')}".lower()
        if any(pattern.lower() in haystack for pattern in patterns):
            return target
    return open_debug_target(WOS_START_URL, port=port)


def find_wos_search_target(port: int = DEFAULT_DEBUG_PORT) -> dict[str, Any]:
    return find_wos_target(port, preferred_patterns=["advanced-search", "basic-search", "smart-search", "search", "webofscience.com"])


def find_wos_results_target(port: int = DEFAULT_DEBUG_PORT) -> dict[str, Any]:
    return find_wos_target(port, preferred_patterns=["summary", "search-results", "full-record", "webofscience.com"])


def _target_session(target: dict[str, Any]) -> CDPSession:
    websocket_url = str(target.get("webSocketDebuggerUrl") or "")
    if not websocket_url:
        raise ValueError("Target does not expose webSocketDebuggerUrl.")
    session = CDPSession(websocket_url)
    session.call("Runtime.enable")
    session.call("Page.enable")
    return session


def _js_string(value: str) -> str:
    return json.dumps(str(value or ""))


def open_wos_query_builder(session: CDPSession, timeout_seconds: float = 20.0) -> dict[str, Any]:
    session.call("Page.navigate", {"url": WOS_START_URL})
    wait_for_condition(session, "document.body && document.body.innerText.length > 0", timeout_seconds=timeout_seconds)
    return {
        "url": safe_evaluate_js(session, "location.href", ""),
        "title": safe_evaluate_js(session, "document.title", ""),
    }


def fill_advanced_search_and_submit(session: CDPSession, query: str) -> dict[str, Any]:
    query_js = _js_string(query)
    return safe_evaluate_js(
        session,
        f"""
(() => {{
  const query = {query_js};
  const input = [...document.querySelectorAll('textarea, input[type="text"], input[type="search"], [contenteditable="true"]')]
    .find(el => {{
      const box = el.getBoundingClientRect();
      return box.width > 80 && box.height > 15;
    }});
  if (!input) return {{ok:false, reason:'query_input_not_found'}};
  input.focus();
  if (input.isContentEditable) input.textContent = query;
  else input.value = query;
  input.dispatchEvent(new InputEvent('input', {{bubbles:true, inputType:'insertText', data:query}}));
  input.dispatchEvent(new Event('change', {{bubbles:true}}));
  const button = [...document.querySelectorAll('button, input[type="submit"], [role="button"]')]
    .find(el => /search|检索|搜索/i.test(el.innerText || el.value || el.getAttribute('aria-label') || ''));
  if (!button) return {{ok:true, submitted:false, reason:'search_button_not_found'}};
  button.click();
  return {{ok:true, submitted:true}};
}})()
""",
        {"ok": False, "reason": "evaluation_failed"},
    )


def submit_wos_search(query: str, port: int = DEFAULT_DEBUG_PORT, wait_seconds: float = 1.5) -> dict[str, Any]:
    return legacy_run_wos_search(port=port, query=query, wait_seconds=wait_seconds)


def _submit_wos_search_lightweight(query: str, port: int = DEFAULT_DEBUG_PORT, wait_seconds: float = 1.5) -> dict[str, Any]:
    target = find_wos_target(port=port, preferred_patterns=["advanced-search", "webofscience.com", "wos"])
    session = _target_session(target)
    try:
        query_js = _js_string(query)
        action = safe_evaluate_js(
            session,
            f"""
(() => {{
  const query = {query_js};
  const candidates = [
    ...document.querySelectorAll('textarea, input[type="text"], input[type="search"], [contenteditable="true"]')
  ].filter(el => {{
    const box = el.getBoundingClientRect();
    const label = `${{el.getAttribute('aria-label') || ''}} ${{el.getAttribute('placeholder') || ''}} ${{el.id || ''}} ${{el.className || ''}}`.toLowerCase();
    return box.width > 80 && box.height > 15 && (label.includes('query') || label.includes('search') || label.includes('advanced') || el.tagName === 'TEXTAREA');
  }});
  const input = candidates[0] || document.querySelector('textarea, input[type="text"], input[type="search"], [contenteditable="true"]');
  if (!input) return {{ok:false, reason:'query_input_not_found'}};
  input.focus();
  if (input.isContentEditable) input.textContent = query;
  else input.value = query;
  input.dispatchEvent(new InputEvent('input', {{bubbles:true, inputType:'insertText', data:query}}));
  input.dispatchEvent(new Event('change', {{bubbles:true}}));
  const buttons = [...document.querySelectorAll('button, input[type="submit"], [role="button"]')];
  const button = buttons.find(el => /search|检索|搜索/i.test(el.innerText || el.value || el.getAttribute('aria-label') || ''));
  if (!button) return {{ok:true, submitted:false, reason:'search_button_not_found'}};
  button.click();
  return {{ok:true, submitted:true}};
}})()
""",
            {"ok": False, "reason": "evaluation_failed"},
        )
        time.sleep(max(0.0, float(wait_seconds or 0)))
        wait_for_condition(session, "document.readyState === 'complete'", timeout_seconds=20)
        snapshot = get_page_snapshot(session, max_chars=20_000)
        return {"action": action, "target": {"id": target.get("id"), "url": target.get("url"), "title": target.get("title")}, "snapshot": snapshot}
    finally:
        session.close()


def _normalize_doi(value: str) -> str:
    match = DOI_PATTERN.search(str(value or ""))
    return match.group(0).rstrip(".,;)]}") if match else ""


def _first_pdf_link(html: str, base_url: str = "") -> str:
    matches = re.findall(r"""href=["']([^"']+\.pdf(?:\?[^"']*)?)["']""", str(html or ""), flags=re.IGNORECASE)
    if not matches:
        return ""
    candidate = matches[0].strip()
    if candidate.startswith("//"):
        return "https:" + candidate
    if candidate.startswith("http://") or candidate.startswith("https://"):
        return candidate
    if candidate.startswith("/") and base_url.startswith(("http://", "https://")):
        from urllib.parse import urljoin

        return urljoin(base_url, candidate)
    return candidate


def _recover_wos_detail_doi(session: CDPSession, url: str, timeout_seconds: float = 8.0) -> dict[str, Any]:
    if not str(url or "").strip():
        return {}
    url_js = _js_string(str(url))
    clicked = safe_evaluate_js(
        session,
        f"""
(() => {{
  const targetUrl = {url_js};
  const links = [...document.querySelectorAll('a[href]')];
  const link = links.find(a => a.href === targetUrl) || links.find(a => a.href && a.href.includes(targetUrl.split('/').pop()));
  if (!link) return false;
  link.scrollIntoView({{block:'center'}});
  link.click();
  return true;
}})()
""",
        False,
    )
    if not clicked:
        session.call("Page.navigate", {"url": str(url)})
    wait_for_condition(
        session,
        """
(() => {
  const text = document.body ? document.body.innerText : '';
  return location.href.includes('/full-record/') && (text.includes('DOI') || text.includes('Accession Number') || text.length > 2500);
})()
""",
        timeout_seconds=max(timeout_seconds, 18.0),
    )
    time.sleep(1.5)
    snapshot = get_page_snapshot(session, max_chars=30_000)
    text = f"{snapshot.get('text', '')}\n{snapshot.get('html', '')}"
    doi = _normalize_doi(text)
    return {
        "doi": doi,
        "detail_url": snapshot.get("url") or url,
        "detail_title": snapshot.get("title") or "",
    }


def scrape_current_wos_results(
    port: int = DEFAULT_DEBUG_PORT,
    max_pages: int = 5,
    detail_workers: int = 2,
    max_records: int = 0,
) -> dict[str, Any]:
    return legacy_scrape_wos_pages(port=port, max_pages=max_pages, detail_workers=detail_workers, max_records=max_records)


def _scrape_current_wos_results_lightweight(port: int = DEFAULT_DEBUG_PORT, max_pages: int = 5, detail_workers: int = 2) -> dict[str, Any]:
    target = find_wos_target(port=port, preferred_patterns=["summary", "search-results", "webofscience.com", "wos"])
    session = _target_session(target)
    try:
        wait_for_condition(session, "document.body && document.body.innerText.length > 200", timeout_seconds=10)
        records = safe_evaluate_js(
            session,
            """
(() => {
  const doiRe = /10\\.\\d{4,9}\\/[-._;()/:A-Z0-9]+/ig;
  const selectors = [
    'app-record', '.summary-record', '.search-results-item', '[data-ta*="summary-record"]',
    '[data-testid*="summary-record"]', 'article', '.record', '.result'
  ];
  let nodes = [];
  for (const selector of selectors) {
    nodes = [...document.querySelectorAll(selector)].filter(el => (el.innerText || '').trim().length > 80);
    if (nodes.length) break;
  }
  if (!nodes.length) {
    nodes = [...document.querySelectorAll('a')].filter(el => /full record|view record|doi|摘要|标题/i.test(el.innerText || '')).slice(0, 50);
  }
  return nodes.slice(0, 100).map((node, index) => {
    const text = (node.innerText || '').replace(/\\s+/g, ' ').trim();
    const titleNode = node.querySelector?.('h2,h3,h4,a[data-ta*="title"],a') || node;
    const title = (titleNode.innerText || titleNode.textContent || '').replace(/\\s+/g, ' ').trim();
    const linkNode = node.querySelector?.('a[href]') || (node.href ? node : null);
    const href = linkNode ? linkNode.href : '';
    const doi = ((text.match(doiRe) || [])[0] || '').replace(/[.,;\\])}]+$/, '');
    return { row_index: index + 1, title, doi, full_record_link: href, raw_text: text.slice(0, 2000) };
  }).filter(item => item.title || item.doi || item.raw_text);
})()
""",
            [],
        )
        if not isinstance(records, list):
            records = []
        normalized = []
        result_url = safe_evaluate_js(session, "location.href", "")
        for item in records:
            if not isinstance(item, dict):
                continue
            raw_text = str(item.get("raw_text") or "")
            doi = str(item.get("doi") or "") or _normalize_doi(raw_text)
            detail = {}
            full_record_link = str(item.get("full_record_link") or "")
            if not doi and full_record_link and len(normalized) < max(1, min(int(detail_workers or 0) * 3, 10)):
                detail = _recover_wos_detail_doi(session, full_record_link)
                doi = str(detail.get("doi") or "")
                if result_url:
                    session.call("Page.navigate", {"url": result_url})
                    wait_for_condition(session, "document.body && document.body.innerText.length > 200", timeout_seconds=8)
            normalized.append(
                {
                    "row_index": item.get("row_index"),
                    "Article Title": str(item.get("title") or "").strip()[:500],
                    "DOI": doi,
                    "full_record_link": full_record_link,
                    "doi_recovery_source": "detail_page" if detail.get("doi") else "",
                    "detail_url": str(detail.get("detail_url") or ""),
                    "raw_snippet": raw_text[:2000],
                }
            )
        snapshot = get_page_snapshot(session, max_chars=10_000)
        return {
            "records": normalized,
            "record_count": len(normalized),
            "pages_requested": int(max_pages or 1),
            "pages_scraped": 1,
            "detail_workers": int(detail_workers or 0),
            "page_url": snapshot.get("url"),
            "page_title": snapshot.get("title"),
            "note": "Agent-native scraper currently extracts the active results page; use the browser to move pages before scraping more.",
        }
    finally:
        session.close()


def _extract_pdf_from_network(session: CDPSession) -> dict[str, Any]:
    session.call("Network.enable")
    session.poll(timeout_seconds=1.0, max_messages=200)
    return {}


def get_response_body_bytes(session: CDPSession, request_id: str) -> bytes:
    body = session.call("Network.getResponseBody", {"requestId": request_id})
    text = str(body.get("body", "") or "")
    if body.get("base64Encoded"):
        return base64.b64decode(text)
    return text.encode("utf-8", errors="ignore")


def browser_fetch_fulltext(url: str, port: int = DEFAULT_DEBUG_PORT, **kwargs: Any) -> dict[str, Any]:
    try:
        payload = enhanced_fetch_fulltext_via_browser(url=url, port=port, **kwargs)
    except Exception as exc:
        return {
            "source_url": url,
            "acquisition_type": "browser_html",
            "acquisition_status": "error",
            "content_text": "",
            "content_chars": 0,
            "error_message": str(exc)[:500],
        }

    text = str(payload.get("articleText") or payload.get("bodyText") or "")
    html = str(payload.get("html") or "")
    pdf_url = str(payload.get("pdfUrl") or payload.get("networkPdfUrl") or "")
    doi_candidates = sorted(set(DOI_PATTERN.findall(f"{text}\n{html}")), key=str.lower)
    status = "ok" if text else "empty"
    if payload.get("challengeDetected"):
        status = "blocked"
    if payload.get("notFoundDetected"):
        status = "not_found"
    return {
        "source_url": payload.get("url") or url,
        "source_title": payload.get("title") or "",
        "acquisition_type": "browser_html",
        "acquisition_status": status,
        "content_text": text,
        "content_chars": len(text),
        "html": html,
        "html_chars": len(html),
        "article_html": str(payload.get("articleHtml") or ""),
        "body_text": str(payload.get("bodyText") or ""),
        "doi_candidates": doi_candidates[:20],
        "pdf_url": pdf_url,
        "pdf_candidates": [candidate for candidate in [payload.get("pdfUrl"), payload.get("networkPdfUrl")] if candidate],
        "pdf_network_artifact": {
            "url": payload.get("networkPdfUrl") or "",
            "bytes": payload.get("networkPdfBytes") or b"",
            "via": payload.get("networkPdfVia") or "",
        },
        "download_pdf_bytes": payload.get("downloadPdfBytes") or b"",
        "download_pdf_file_name": payload.get("downloadPdfFileName") or "",
        "clicked_entrypoint": bool(payload.get("clickedEntrypoint")),
        "challenge_detected": bool(payload.get("challengeDetected")),
        "not_found_detected": bool(payload.get("notFoundDetected")),
        "agent_payload": payload,
    }
