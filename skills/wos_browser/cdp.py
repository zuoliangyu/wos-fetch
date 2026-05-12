from __future__ import annotations

import json
import time
from typing import Any

import websocket


class CDPSession:
    def __init__(self, websocket_url: str):
        self.ws = websocket.create_connection(websocket_url, timeout=120)
        self._default_timeout = 120
        self.ws.settimeout(self._default_timeout)
        self._message_id = 0
        self._responses: dict[int, dict[str, Any]] = {}
        self._event_buffer: list[dict[str, Any]] = []

    def close(self) -> None:
        self.ws.close()

    def _recv_message(self, timeout: float | None = None) -> dict[str, Any] | None:
        if timeout is not None:
            self.ws.settimeout(timeout)
        try:
            raw = self.ws.recv()
        except websocket.WebSocketTimeoutException:
            return None
        except (BlockingIOError, OSError) as exc:
            winerror = getattr(exc, "winerror", None)
            if winerror == 10035:
                return None
            message = str(exc).lower()
            if "10035" in message or "non-blocking" in message or "would block" in message:
                return None
            raise
        finally:
            if timeout is not None:
                self.ws.settimeout(self._default_timeout)

        message = json.loads(raw)
        if "id" in message:
            self._responses[int(message["id"])] = message
        elif "method" in message:
            self._event_buffer.append(message)
        return message

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        self._message_id += 1
        message_id = self._message_id
        payload = {"id": message_id, "method": method, "params": params or {}}
        self.ws.send(json.dumps(payload))
        while True:
            if message_id in self._responses:
                message = self._responses.pop(message_id)
            else:
                self._recv_message()
                continue
            if "error" in message:
                raise RuntimeError(message["error"])
            return message.get("result", {})

    def poll_messages(self, timeout_seconds: float = 0.1, max_messages: int = 200) -> int:
        if max_messages <= 0:
            return 0
        deadline = time.time() + max(0.0, timeout_seconds)
        received = 0
        while received < max_messages:
            remaining = max(0.0, deadline - time.time())
            timeout = remaining if timeout_seconds > 0 else 0.01
            message = self._recv_message(timeout=timeout)
            if message is None:
                break
            received += 1
            if timeout_seconds <= 0 and received >= max_messages:
                break
        return received

    def consume_events(self, methods: list[str] | set[str] | tuple[str, ...] | None = None) -> list[dict[str, Any]]:
        if not self._event_buffer:
            return []
        if methods is None:
            events = self._event_buffer[:]
            self._event_buffer.clear()
            return events
        allowed = {str(method) for method in methods}
        matched: list[dict[str, Any]] = []
        remaining: list[dict[str, Any]] = []
        for event in self._event_buffer:
            if str(event.get("method", "")) in allowed:
                matched.append(event)
            else:
                remaining.append(event)
        self._event_buffer = remaining
        return matched


def evaluate_js(session: CDPSession, expression: str, await_promise: bool = False) -> Any:
    result = session.call(
        "Runtime.evaluate",
        {
            "expression": expression,
            "returnByValue": True,
            "awaitPromise": await_promise,
        },
    )
    return result.get("result", {}).get("value")


def safe_evaluate_js(session: CDPSession, expression: str, default: Any = "", await_promise: bool = False) -> Any:
    try:
        return evaluate_js(session, expression, await_promise=await_promise)
    except Exception as exc:
        message = str(exc)
        if "maximum size" in message.lower() or "exceeded" in message.lower():
            return default
        return default


def wait_for_condition(
    session: CDPSession,
    expression: str,
    timeout_seconds: float = 15.0,
    poll_interval: float = 0.3,
) -> bool:
    deadline = time.time() + timeout_seconds
    while time.time() < deadline:
        try:
            if bool(evaluate_js(session, expression)):
                return True
        except Exception:
            pass
        time.sleep(poll_interval)
    return False


def goto_url(session: CDPSession, url: str) -> bool:
    if not url:
        return False
    escaped = json.dumps(url)
    js = f"location.href = {escaped}; true;"
    return bool(evaluate_js(session, js))


def dispatch_key_event(
    session: CDPSession,
    *,
    event_type: str,
    key: str,
    code: str,
    windows_virtual_key_code: int,
    modifiers: int = 0,
) -> None:
    session.call(
        "Input.dispatchKeyEvent",
        {
            "type": event_type,
            "key": key,
            "code": code,
            "windowsVirtualKeyCode": windows_virtual_key_code,
            "nativeVirtualKeyCode": windows_virtual_key_code,
            "modifiers": modifiers,
        },
    )


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
