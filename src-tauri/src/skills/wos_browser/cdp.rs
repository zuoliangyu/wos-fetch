//! Raw Chrome DevTools Protocol (CDP) session over WebSocket.
//!
//! Port target: `skills/wos_browser/cdp.py`. Uses tokio-tungstenite for the
//! WebSocket transport and serde_json for the JSON-RPC payloads. The session
//! is single-tasked (one outstanding call at a time) — matches the Python
//! design and keeps the implementation borrow-checker friendly.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::{AppError, AppResult};

const DEFAULT_TIMEOUT_SECONDS: u64 = 120;

pub struct CdpSession {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: Vec<Value>,
}

impl CdpSession {
    pub async fn connect(url: &str) -> AppResult<Self> {
        let (ws, _) = connect_async(url)
            .await
            .map_err(|e| AppError::Browser(format!("CDP WebSocket connect failed: {e}")))?;
        Ok(Self {
            ws,
            next_id: 0,
            events: Vec::new(),
        })
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    /// Send a JSON-RPC command and block until the matching response arrives.
    /// Unsolicited events that show up before the response are buffered for
    /// later inspection via `drain_events`.
    pub async fn call(&mut self, method: &str, params: Value) -> AppResult<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let payload = json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Message::Text(payload.to_string()))
            .await
            .map_err(|e| AppError::Browser(format!("CDP send failed: {e}")))?;

        let deadline = Duration::from_secs(DEFAULT_TIMEOUT_SECONDS);
        let recv = async {
            loop {
                let Some(msg) = self.ws.next().await else {
                    return Err(AppError::Browser("CDP socket closed unexpectedly".into()));
                };
                let msg =
                    msg.map_err(|e| AppError::Browser(format!("CDP recv error: {e}")))?;
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Close(_) => {
                        return Err(AppError::Browser("CDP socket closed by peer".into()))
                    }
                    _ => continue,
                };
                let value: Value = serde_json::from_str(&text)
                    .map_err(|e| AppError::Browser(format!("CDP JSON parse failed: {e}")))?;
                if value.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(err) = value.get("error") {
                        return Err(AppError::Browser(err.to_string()));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
                if value.get("method").is_some() {
                    self.events.push(value);
                }
            }
        };

        match timeout(deadline, recv).await {
            Ok(result) => result,
            Err(_) => Err(AppError::Browser(format!(
                "CDP call '{method}' timed out after {DEFAULT_TIMEOUT_SECONDS}s"
            ))),
        }
    }

    /// Drain any buffered events. Pass `None` to take everything.
    pub fn drain_events(&mut self, methods: Option<&[&str]>) -> Vec<Value> {
        if self.events.is_empty() {
            return Vec::new();
        }
        match methods {
            None => std::mem::take(&mut self.events),
            Some(allowed) => {
                let (matched, remaining): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.events).into_iter().partition(|ev| {
                        ev.get("method")
                            .and_then(Value::as_str)
                            .map(|m| allowed.contains(&m))
                            .unwrap_or(false)
                    });
                self.events = remaining;
                matched
            }
        }
    }
}

// ---------------------------------------------------------------------------
// High-level helpers (mirror Python evaluate_js / safe_evaluate_js / etc.)
// ---------------------------------------------------------------------------

/// Run a JS expression on the page; returns the JSON-RPC `result.result.value`.
pub async fn evaluate_js(session: &mut CdpSession, expression: &str) -> AppResult<Value> {
    evaluate_js_with(session, expression, false).await
}

pub async fn evaluate_js_with(
    session: &mut CdpSession,
    expression: &str,
    await_promise: bool,
) -> AppResult<Value> {
    let params = json!({
        "expression": expression,
        "returnByValue": true,
        "awaitPromise": await_promise,
    });
    let result = session.call("Runtime.evaluate", params).await?;
    Ok(result
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Like `evaluate_js` but swallows errors and falls back to `default`.
pub async fn safe_evaluate_js(
    session: &mut CdpSession,
    expression: &str,
    default: Value,
) -> Value {
    match evaluate_js(session, expression).await {
        Ok(v) if !v.is_null() => v,
        _ => default,
    }
}

/// Poll a JS predicate until it returns truthy or the deadline elapses.
pub async fn wait_for_condition(
    session: &mut CdpSession,
    expression: &str,
    timeout_seconds: f64,
    poll_interval_seconds: f64,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(0.0));
    let interval = Duration::from_secs_f64(poll_interval_seconds.max(0.05));
    while Instant::now() < deadline {
        match evaluate_js(session, expression).await {
            Ok(Value::Bool(true)) => return true,
            Ok(Value::Number(n)) if n.as_f64().map(|f| f != 0.0).unwrap_or(false) => return true,
            Ok(Value::String(s)) if !s.is_empty() => return true,
            _ => {}
        }
        sleep(interval).await;
    }
    false
}

/// Navigate the page by assigning `location.href` from JS.
pub async fn goto_url(session: &mut CdpSession, url: &str) -> AppResult<bool> {
    if url.is_empty() {
        return Ok(false);
    }
    let escaped = serde_json::to_string(url)?;
    let js = format!("location.href = {escaped}; true;");
    Ok(matches!(evaluate_js(session, &js).await?, Value::Bool(true)))
}

pub async fn dispatch_mouse_click(session: &mut CdpSession, x: f64, y: f64) -> AppResult<()> {
    session
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none" }),
        )
        .await?;
    session
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )
        .await?;
    session
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )
        .await?;
    Ok(())
}

pub async fn dispatch_key_event(
    session: &mut CdpSession,
    event_type: &str,
    key: &str,
    code: &str,
    windows_virtual_key_code: i32,
    modifiers: i32,
) -> AppResult<()> {
    session
        .call(
            "Input.dispatchKeyEvent",
            json!({
                "type": event_type,
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": windows_virtual_key_code,
                "nativeVirtualKeyCode": windows_virtual_key_code,
                "modifiers": modifiers,
            }),
        )
        .await?;
    Ok(())
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PageSnapshot {
    pub url: String,
    pub title: String,
    pub html: String,
    pub text: String,
    pub html_chars: i64,
    pub text_chars: i64,
}

pub async fn get_page_snapshot(session: &mut CdpSession, max_chars: i64) -> PageSnapshot {
    let limit = max_chars.clamp(1000, 120_000);
    let html_expr = format!(
        "(document.documentElement ? document.documentElement.outerHTML : '').slice(0, {limit})"
    );
    let text_expr = format!("(document.body ? document.body.innerText : '').slice(0, {limit})");
    PageSnapshot {
        url: safe_evaluate_js(session, "location.href", Value::Null)
            .await
            .as_str()
            .unwrap_or("")
            .to_string(),
        title: safe_evaluate_js(session, "document.title", Value::Null)
            .await
            .as_str()
            .unwrap_or("")
            .to_string(),
        html: safe_evaluate_js(session, &html_expr, Value::Null)
            .await
            .as_str()
            .unwrap_or("")
            .to_string(),
        text: safe_evaluate_js(session, &text_expr, Value::Null)
            .await
            .as_str()
            .unwrap_or("")
            .to_string(),
        html_chars: safe_evaluate_js(
            session,
            "document.documentElement ? document.documentElement.outerHTML.length : 0",
            Value::from(0),
        )
        .await
        .as_i64()
        .unwrap_or(0),
        text_chars: safe_evaluate_js(
            session,
            "document.body ? document.body.innerText.length : 0",
            Value::from(0),
        )
        .await
        .as_i64()
        .unwrap_or(0),
    }
}

/// Set up the standard CDP domains used throughout the WoS workflow.
pub async fn prepare_page_session(session: &mut CdpSession) -> AppResult<()> {
    session.call("Page.enable", json!({})).await?;
    session.call("Runtime.enable", json!({})).await?;
    session.call("Network.enable", json!({})).await?;
    Ok(())
}
