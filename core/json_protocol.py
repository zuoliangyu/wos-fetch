from __future__ import annotations

import json
import re
from typing import Any

from .json_repair import repair_json_text


def strip_markdown_fence(text: str) -> str:
    candidate = str(text or "").strip()
    match = re.fullmatch(r"```(?:json)?\s*(.*?)\s*```", candidate, flags=re.DOTALL | re.IGNORECASE)
    if match:
        return match.group(1).strip()
    return candidate


def parse_json_object_with_diagnostics(text: str) -> tuple[dict[str, Any], dict[str, Any]]:
    raw_candidate = strip_markdown_fence(text)
    repaired_candidate = repair_json_text(raw_candidate)
    diagnostics = {
        "used_repair": False,
        "raw_candidate": raw_candidate,
        "repaired_candidate": repaired_candidate,
    }
    try:
        payload = json.loads(raw_candidate)
    except json.JSONDecodeError:
        payload = json.loads(repaired_candidate)
        diagnostics["used_repair"] = True

    if isinstance(payload, list):
        return {"search_directions": payload}, diagnostics
    if not isinstance(payload, dict):
        raise ValueError("Model output must be a JSON object.")
    return payload, diagnostics


def load_json_object(text: str) -> dict[str, Any]:
    payload, _diagnostics = parse_json_object_with_diagnostics(text)
    return payload
