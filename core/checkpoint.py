from __future__ import annotations

import json
import re
import time
from pathlib import Path
from typing import Any

import pandas as pd

CHECKPOINT_ROOT = Path(__file__).resolve().parent.parent / ".checkpoints"


def checkpoint_job_dir(resume_key: str) -> Path:
    return CHECKPOINT_ROOT / resume_key


def checkpoint_meta_path(resume_key: str) -> Path:
    return checkpoint_job_dir(resume_key) / "meta.json"


def checkpoint_result_path(resume_key: str) -> Path:
    return checkpoint_job_dir(resume_key) / "final.xlsx"


def checkpoint_result_zip_path(resume_key: str) -> Path:
    return checkpoint_job_dir(resume_key) / "final.zip"


def _safe_resume_key(resume_key: str) -> str:
    key = str(resume_key or "").strip()
    if not key:
        raise ValueError("resume_key is required.")
    safe = re.sub(r"[^A-Za-z0-9_.-]+", "_", key).strip("._")
    if not safe:
        raise ValueError("resume_key must contain at least one safe character.")
    return safe[:160]


def _atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_suffix(path.suffix + ".tmp")
    temp_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    temp_path.replace(path)


def save_checkpoint_meta(resume_key, *, source_name, status, completed_rows, total_rows, summary=None):
    safe_key = _safe_resume_key(resume_key)
    existing = load_checkpoint_meta(safe_key)
    payload = {
        **existing,
        "source_name": source_name,
        "status": status,
        "completed_rows": int(completed_rows),
        "total_rows": int(total_rows),
        "updated_at": int(time.time()),
    }
    if summary is not None:
        payload["summary"] = summary
    _atomic_write_json(checkpoint_meta_path(safe_key), payload)
    return payload


def request_checkpoint_cancel(resume_key: str, *, reason: str = "") -> dict[str, Any]:
    safe_key = _safe_resume_key(resume_key)
    existing = load_checkpoint_meta(safe_key)
    payload = {
        **existing,
        "cancel_requested": True,
        "cancel_reason": str(reason or "").strip()[:300],
        "cancel_requested_at": int(time.time()),
        "updated_at": int(time.time()),
    }
    if str(payload.get("status") or "").strip().lower() == "running":
        payload["status"] = "cancelling"
    _atomic_write_json(checkpoint_meta_path(safe_key), payload)
    return payload


def checkpoint_cancel_requested(resume_key: str) -> bool:
    meta = load_checkpoint_meta(resume_key)
    return bool(meta.get("cancel_requested"))


def _jsonable(value: Any) -> Any:
    if value is None:
        return None
    if isinstance(value, (str, bool, int)):
        return value
    if isinstance(value, float):
        if value != value or value in (float("inf"), float("-inf")):
            return None
        return value
    if isinstance(value, (list, tuple)):
        return [_jsonable(item) for item in value]
    if isinstance(value, dict):
        return {str(key): _jsonable(item) for key, item in value.items()}
    try:
        if pd.isna(value):
            return None
    except (TypeError, ValueError):
        pass
    return str(value)


def append_checkpoint_batch(resume_key: str, batch_index: int, rows: list[dict[str, Any]]) -> Path:
    safe_key = _safe_resume_key(resume_key)
    path = checkpoint_job_dir(safe_key) / f"batch_{int(batch_index):05d}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_suffix(path.suffix + ".tmp")
    payload = [_jsonable(row) for row in rows if isinstance(row, dict)]
    temp_path.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    temp_path.replace(path)
    return path


def save_checkpoint_result_file(resume_key: str, payload: bytes, result_format: str = "xlsx") -> Path:
    safe_key = _safe_resume_key(resume_key)
    suffix = "zip" if str(result_format or "").lower() == "zip" else "xlsx"
    path = checkpoint_job_dir(safe_key) / f"final.{suffix}"
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_suffix(path.suffix + ".tmp")
    temp_path.write_bytes(payload)
    temp_path.replace(path)
    return path


def load_checkpoint_meta(resume_key: str) -> dict[str, Any]:
    path = checkpoint_meta_path(resume_key)
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return {}


def load_checkpoint_rows(resume_key: str) -> list[dict[str, Any]]:
    job_dir = checkpoint_job_dir(resume_key)
    if not job_dir.exists():
        return []
    rows: list[dict[str, Any]] = []
    for batch_path in sorted(job_dir.glob("batch_*.json")):
        try:
            batch_rows = json.loads(batch_path.read_text(encoding="utf-8"))
        except Exception:
            continue
        if isinstance(batch_rows, list):
            rows.extend(item for item in batch_rows if isinstance(item, dict))
    return rows


def list_checkpoints() -> list[dict[str, Any]]:
    if not CHECKPOINT_ROOT.exists():
        return []
    items: list[dict[str, Any]] = []
    for job_dir in sorted(CHECKPOINT_ROOT.iterdir(), key=lambda p: p.stat().st_mtime, reverse=True):
        if not job_dir.is_dir():
            continue
        resume_key = job_dir.name
        meta = load_checkpoint_meta(resume_key)
        batch_paths = list(job_dir.glob("batch_*.json"))
        result_path = checkpoint_result_path(resume_key)
        result_zip_path = checkpoint_result_zip_path(resume_key)
        row_count = int(meta.get("completed_rows", 0) or 0)
        items.append({
            "resume_key": resume_key,
            "source_name": str(meta.get("source_name", "") or resume_key),
            "status": str(meta.get("status", "unknown") or "unknown"),
            "completed_rows": row_count,
            "total_rows": int(meta.get("total_rows", 0) or 0),
            "batch_count": len(batch_paths),
            "has_final_result": result_path.exists() or result_zip_path.exists(),
            "updated_at": int(meta.get("updated_at", 0) or 0),
        })
    return items


def export_checkpoint_result(resume_key: str) -> tuple[bytes, dict[str, Any], str]:
    from .table_io import serialize_result_table
    meta = load_checkpoint_meta(resume_key)
    for result_path in (checkpoint_result_zip_path(resume_key), checkpoint_result_path(resume_key)):
        if not result_path.exists():
            continue
        summary = meta.get("summary", {}) if isinstance(meta.get("summary"), dict) else {}
        return result_path.read_bytes(), {**summary, "final_export": True}, result_path.name
    rows = load_checkpoint_rows(resume_key)
    if not rows:
        raise ValueError(f"No checkpoint rows found for '{resume_key}'.")
    table = pd.DataFrame(rows)
    if "__row_id" in table.columns:
        table = table.sort_values("__row_id").drop(columns=["__row_id"])
    payload, _packaged, result_format, fulltext_artifacts = serialize_result_table(table)
    summary = {
        "rows": int(len(table)),
        "checkpoint_status": str(meta.get("status", "unknown") or "unknown"),
        "partial_export": True,
        "result_format": result_format,
        "fulltext_artifact_count": fulltext_artifacts,
    }
    return payload, summary, f"partial_result_{len(table)}_rows.{result_format}"
