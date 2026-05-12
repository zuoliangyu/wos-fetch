from __future__ import annotations

import json
import random
import time
from collections.abc import Iterator
from typing import Any, Callable

import requests


class AgentLLMError(Exception):
    pass


_RETRYABLE_STATUS_CODES = frozenset({429, 500, 502, 503, 504})
_MAX_RETRY_ATTEMPTS = 4
_RETRY_BASE_DELAY_SECONDS = 2.0


def _is_connection_failure(exc: Exception) -> bool:
    text = str(exc)
    return isinstance(exc, (requests.ConnectionError, requests.Timeout)) or "WinError 10013" in text


def _retryable_post(perform: Callable[[], requests.Response]) -> requests.Response:
    """Issue an HTTP request with exponential-backoff retry on 429 / 5xx and
    transient network failures. Honours an ``Retry-After`` header if present.
    """
    for attempt in range(_MAX_RETRY_ATTEMPTS):
        try:
            response = perform()
        except (requests.ConnectionError, requests.Timeout) as exc:
            if attempt == _MAX_RETRY_ATTEMPTS - 1:
                raise
            delay = _RETRY_BASE_DELAY_SECONDS * (2 ** attempt) + random.uniform(0, 0.5)
            print(
                f"[LLM] network error on attempt {attempt + 1}/{_MAX_RETRY_ATTEMPTS}: "
                f"{type(exc).__name__}; sleeping {delay:.1f}s",
                flush=True,
            )
            time.sleep(delay)
            continue
        if response.status_code in _RETRYABLE_STATUS_CODES and attempt < _MAX_RETRY_ATTEMPTS - 1:
            retry_after = response.headers.get("Retry-After")
            try:
                delay = float(retry_after) if retry_after else _RETRY_BASE_DELAY_SECONDS * (2 ** attempt)
            except (TypeError, ValueError):
                delay = _RETRY_BASE_DELAY_SECONDS * (2 ** attempt)
            delay = max(delay, _RETRY_BASE_DELAY_SECONDS) + random.uniform(0, 0.5)
            print(
                f"[LLM] HTTP {response.status_code} on attempt {attempt + 1}/{_MAX_RETRY_ATTEMPTS}; "
                f"backing off {delay:.1f}s",
                flush=True,
            )
            response.close()
            time.sleep(delay)
            continue
        return response
    raise AgentLLMError("Exhausted retry attempts without obtaining a response.")


def build_chat_completions_url(base_url: str) -> str:
    normalized = (base_url or "").strip().rstrip("/")
    if normalized.endswith("/chat/completions"):
        return normalized
    if normalized.endswith("/responses"):
        return f"{normalized[:-len('/responses')]}/chat/completions"
    return f"{normalized}/chat/completions"


def build_responses_url(base_url: str) -> str:
    normalized = (base_url or "").strip().rstrip("/")
    if normalized.endswith("/responses"):
        return normalized
    if normalized.endswith("/chat/completions"):
        return f"{normalized[:-len('/chat/completions')]}/responses"
    return f"{normalized}/responses"


def prefers_responses_api(base_url: str) -> bool:
    return (base_url or "").strip().rstrip("/").endswith("/responses")


def _extract_text_fragments(value: Any) -> list[str]:
    fragments: list[str] = []
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        for item in value:
            fragments.extend(_extract_text_fragments(item))
        return fragments
    if isinstance(value, dict):
        for key in ("text", "output_text", "content", "message", "refusal"):
            if key in value:
                fragments.extend(_extract_text_fragments(value.get(key)))
    return fragments


def extract_output_text(payload: dict[str, Any]) -> str:
    if isinstance(payload.get("output_text"), str):
        return payload["output_text"]
    choices = payload.get("choices")
    if isinstance(choices, list):
        chunks: list[str] = []
        for choice in choices:
            chunks.extend(_extract_text_fragments(choice))
        text = "\n".join(chunk.strip() for chunk in chunks if chunk.strip()).strip()
        if text:
            return text
    output_items = payload.get("output")
    if isinstance(output_items, list):
        chunks = []
        for item in output_items:
            chunks.extend(_extract_text_fragments(item))
        text = "\n".join(chunk.strip() for chunk in chunks if chunk.strip()).strip()
        if text:
            return text
    chunks = _extract_text_fragments(payload)
    return "\n".join(chunk.strip() for chunk in chunks if chunk.strip()).strip()


def extract_response_error(payload: dict[str, Any]) -> str:
    error = payload.get("error")
    if isinstance(error, dict):
        message = str(error.get("message", "") or "").strip()
        code = str(error.get("code", "") or "").strip()
        return f"{code}: {message}" if code and message else message
    if isinstance(error, str):
        return error
    status = str(payload.get("status", "") or "").strip().lower()
    if status == "failed":
        return "response status is failed"
    return ""


def _post_chat_completion(*, base_url, api_key, model, messages, timeout, temperature, tools=None, tool_choice=None):
    url = build_chat_completions_url(base_url)
    headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
    payload = {"model": model, "messages": messages, "temperature": temperature}
    if tools:
        payload["tools"] = tools
        payload["tool_choice"] = tool_choice or "auto"
    started = time.monotonic()
    session = requests.Session()
    try:
        response = _retryable_post(lambda: session.post(url, headers=headers, json=payload, timeout=timeout))
        response.encoding = "utf-8"
        elapsed = time.monotonic() - started
        print(f"[LLM] POST {url} status={response.status_code} elapsed={elapsed:.1f}s", flush=True)
        try:
            data = response.json()
        except Exception:
            data = {"error": {"message": response.text[:1000]}}
        if response.status_code >= 400:
            raise AgentLLMError(extract_response_error(data) or f"HTTP {response.status_code}")
        if not isinstance(data, dict):
            raise AgentLLMError("API returned a non-JSON response.")
        detail = extract_response_error(data)
        if detail:
            raise AgentLLMError(detail)
        return data
    finally:
        session.close()


def _post_response(*, base_url, api_key, model, messages, timeout, temperature):
    url = build_responses_url(base_url)
    headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
    input_items = [
        {"role": item.get("role", "user"), "content": item.get("content", "")}
        for item in messages
        if str(item.get("content", "")).strip()
    ]
    payload = {"model": model, "input": input_items, "temperature": temperature}
    started = time.monotonic()
    session = requests.Session()
    try:
        response = _retryable_post(lambda: session.post(url, headers=headers, json=payload, timeout=timeout))
        response.encoding = "utf-8"
        elapsed = time.monotonic() - started
        print(f"[LLM] POST {url} status={response.status_code} elapsed={elapsed:.1f}s", flush=True)
        try:
            data = response.json()
        except Exception:
            data = {"error": {"message": response.text[:1000]}}
        if response.status_code >= 400:
            raise AgentLLMError(extract_response_error(data) or f"HTTP {response.status_code}")
        if not isinstance(data, dict):
            raise AgentLLMError("API returned a non-JSON response.")
        detail = extract_response_error(data)
        if detail:
            raise AgentLLMError(detail)
        return data
    finally:
        session.close()


def _stream_chat_completion_text(*, base_url, api_key, model, messages, timeout, temperature):
    url = build_chat_completions_url(base_url)
    headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
    payload = {"model": model, "messages": messages, "temperature": temperature, "stream": True}
    started = time.monotonic()
    chunks: list[str] = []
    session = requests.Session()
    try:
        response = _retryable_post(lambda: session.post(url, headers=headers, json=payload, timeout=timeout, stream=True))
        response.encoding = "utf-8"
        if response.status_code >= 400:
            try:
                data = response.json()
                detail = extract_response_error(data)
            except Exception:
                detail = response.text[:1000]
            raise AgentLLMError(detail or f"HTTP {response.status_code}")
        for raw_line in response.iter_lines(decode_unicode=True):
            if not raw_line:
                continue
            line = str(raw_line).strip()
            if line.startswith("data:"):
                line = line[5:].strip()
            if not line or line == "[DONE]":
                continue
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(data, dict):
                continue
            detail = extract_response_error(data)
            if detail:
                raise AgentLLMError(detail)
            for choice in data.get("choices", []) or []:
                if not isinstance(choice, dict):
                    continue
                delta = choice.get("delta")
                if isinstance(delta, dict) and isinstance(delta.get("content"), str):
                    chunks.append(delta["content"])
                message = choice.get("message")
                if isinstance(message, dict) and isinstance(message.get("content"), str):
                    chunks.append(message["content"])
                text = choice.get("text")
                if isinstance(text, str):
                    chunks.append(text)
        text = "".join(chunks).strip()
        elapsed = time.monotonic() - started
        print(f"[LLM] STREAM {url} chars={len(text)} elapsed={elapsed:.1f}s", flush=True)
        if not text:
            raise AgentLLMError("stream returned empty text")
        return text
    finally:
        session.close()


def chat_text(*, base_url, api_key, model, messages, timeout, temperature=0.0):
    errors: list[str] = []
    if prefers_responses_api(base_url):
        try:
            data = _post_response(base_url=base_url, api_key=api_key, model=model, messages=messages, timeout=timeout, temperature=temperature)
            text = extract_output_text(data).strip()
            if text:
                return text
            raise AgentLLMError("Responses API returned empty text.")
        except Exception as exc:
            if _is_connection_failure(exc):
                raise AgentLLMError(f"Model API connection failed: {exc}") from exc
            errors.append(f"responses:{type(exc).__name__}: {exc}")
    try:
        return _stream_chat_completion_text(base_url=base_url, api_key=api_key, model=model, messages=messages, timeout=timeout, temperature=temperature)
    except Exception as exc:
        if _is_connection_failure(exc):
            raise AgentLLMError(f"Model API connection failed: {exc}") from exc
        errors.append(f"chat_stream:{type(exc).__name__}: {exc}")
        try:
            data = _post_chat_completion(base_url=base_url, api_key=api_key, model=model, messages=messages, timeout=timeout, temperature=temperature)
            text = extract_output_text(data).strip()
            if text:
                return text
            raise AgentLLMError("Chat Completions API returned empty text.")
        except Exception as post_exc:
            errors.append(f"chat_post:{type(post_exc).__name__}: {post_exc}")
    if not prefers_responses_api(base_url):
        try:
            data = _post_response(base_url=base_url, api_key=api_key, model=model, messages=messages, timeout=timeout, temperature=temperature)
            text = extract_output_text(data).strip()
            if text:
                return text
            raise AgentLLMError("Responses API returned empty text.")
        except Exception as exc:
            errors.append(f"responses:{type(exc).__name__}: {exc}")
    raise AgentLLMError("Model API call failed. " + " | ".join(errors[-3:]))
