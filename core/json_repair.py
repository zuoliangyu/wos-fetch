from __future__ import annotations

import json
import re


def extract_json_candidate(text: str) -> str:
    candidate = str(text or "").strip().lstrip("﻿")
    fence = re.fullmatch(r"```(?:json)?\s*(.*?)\s*```", candidate, flags=re.DOTALL | re.IGNORECASE)
    if fence:
        candidate = fence.group(1).strip()

    if candidate.startswith("{") or candidate.startswith("["):
        return candidate

    object_start = candidate.find("{")
    array_start = candidate.find("[")
    starts = [index for index in (object_start, array_start) if index >= 0]
    if not starts:
        return candidate

    start = min(starts)
    opening = candidate[start]
    closing = "}" if opening == "{" else "]"
    end = candidate.rfind(closing)
    if end <= start:
        return candidate[start:]
    return candidate[start : end + 1].strip()


def _fix_unescaped_chars_in_strings(text: str) -> str:
    result = []
    in_string = False
    escape_next = False
    i = 0
    while i < len(text):
        char = text[i]
        if escape_next:
            result.append(char)
            escape_next = False
            i += 1
            continue
        if char == '\\':
            result.append(char)
            escape_next = True
            i += 1
            continue
        if char == '"':
            in_string = not in_string
            result.append(char)
            i += 1
            continue
        if in_string:
            if char == '\n':
                result.append('\\n')
            elif char == '\r':
                result.append('\\r')
            elif char == '\t':
                result.append('\\t')
            else:
                result.append(char)
        else:
            result.append(char)
        i += 1
    return ''.join(result)


def _escape_inner_quotes(text: str) -> str:
    result: list[str] = []
    in_string = False
    escape_next = False
    i = 0
    length = len(text)
    while i < length:
        char = text[i]
        if escape_next:
            result.append(char)
            escape_next = False
            i += 1
            continue
        if char == "\\":
            result.append(char)
            escape_next = True
            i += 1
            continue
        if char == '"':
            if not in_string:
                in_string = True
                result.append(char)
                i += 1
                continue
            j = i + 1
            while j < length and text[j].isspace():
                j += 1
            if j < length and text[j] not in [",", "}", "]", ":"]:
                result.append('\\"')
                i += 1
                continue
            in_string = False
            result.append(char)
            i += 1
            continue
        result.append(char)
        i += 1
    return "".join(result)


def _balance_truncated_json(candidate: str) -> str:
    text = str(candidate or "").rstrip()
    if not text:
        return text
    stack: list[str] = []
    in_string = False
    escape_next = False
    for char in text:
        if escape_next:
            escape_next = False
            continue
        if char == "\\":
            escape_next = True
            continue
        if char == '"':
            in_string = not in_string
            continue
        if in_string:
            continue
        if char in "{[":
            stack.append("}" if char == "{" else "]")
        elif char in "}]":
            if stack and stack[-1] == char:
                stack.pop()
    if in_string:
        text += '"'
    if stack:
        text += "".join(reversed(stack))
    return text


def _candidate_trim_positions(text: str) -> list[int]:
    """Return structurally sensible byte offsets at which to attempt truncation.

    Only considers positions just after a closing quote, comma, or matching
    bracket at top-level depth (depth <= 1), so we don't try splitting in the
    middle of a number or unquoted key. Caps at 64 positions.
    """
    positions: list[int] = []
    in_string = False
    escape_next = False
    depth = 0
    for i, char in enumerate(text):
        if escape_next:
            escape_next = False
            continue
        if char == "\\":
            escape_next = True
            continue
        if char == '"':
            in_string = not in_string
            if not in_string and depth <= 1:
                positions.append(i + 1)
            continue
        if in_string:
            continue
        if char in "{[":
            depth += 1
        elif char in "}]":
            depth -= 1
            if depth <= 0:
                positions.append(i + 1)
        elif char == "," and depth == 1:
            positions.append(i + 1)
    return sorted(set(positions), reverse=True)[:64]


def _trim_to_last_complete_pair(candidate: str) -> str:
    text = str(candidate or "").strip()
    if not text:
        return text
    try:
        json.loads(text)
        return text
    except Exception:
        pass
    best: str | None = None
    for end in _candidate_trim_positions(text):
        chunk = text[:end].rstrip().rstrip(",")
        if not chunk:
            continue
        repaired = _balance_truncated_json(re.sub(r",\s*([}\]])", r"\1", chunk))
        try:
            parsed = json.loads(repaired)
        except Exception:
            continue
        if isinstance(parsed, dict) and parsed:
            return repaired
        if isinstance(parsed, list) and parsed:
            return repaired
        if best is None:
            best = repaired
    return best if best is not None else text


def repair_json_text(text: str) -> str:
    candidate = extract_json_candidate(text)
    candidate = candidate.replace("“", '"').replace("”", '"')
    candidate = candidate.replace("‘", "'").replace("’", "'")
    candidate = re.sub(r",\s*([}\]])", r"\1", candidate)
    try:
        candidate = _fix_unescaped_chars_in_strings(candidate)
    except Exception:
        pass
    try:
        candidate = _escape_inner_quotes(candidate)
    except Exception:
        pass
    candidate = _balance_truncated_json(candidate)
    candidate = _trim_to_last_complete_pair(candidate)
    return candidate.strip()
