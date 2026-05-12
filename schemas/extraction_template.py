from __future__ import annotations

from typing import Any


REVIEW_EVIDENCE_EXPORT_COLUMNS: tuple[str, ...] = (
    "UT号",
    "中文标题",
    "原始检索方向",
    "原始建议章节",
    "修正后综述章节",
    "证据角色",
    "支撑的核心论点",
    "可写入综述的关键证据",
    "可比较维度",
    "适合综述表格的列",
    "应避免的误读",
    "研究局限",
    "未来趋势",
)


def _as_string_list(value: Any) -> list[str]:
    if isinstance(value, list):
        return [str(item).strip() for item in value if str(item).strip()]
    text = str(value or "").strip()
    return [text] if text else []


def normalize_extraction_template(payload: Any) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise ValueError("Extraction template JSON must be an object.")
    fields = payload.get("fields")
    if not isinstance(fields, list) or not fields:
        raise ValueError("Extraction template must include a non-empty fields list.")

    normalized_fields: list[dict[str, Any]] = []
    for item in fields:
        if not isinstance(item, dict):
            continue
        field_name = str(item.get("field_name") or item.get("name") or "").strip()
        if not field_name:
            continue
        normalized_fields.append(
            {
                "field_name": field_name,
                "definition": str(item.get("definition") or item.get("description") or "").strip(),
                "output_rule": str(item.get("output_rule") or item.get("rule") or "").strip(),
                "allowed_values": _as_string_list(item.get("allowed_values") or []),
                "examples": _as_string_list(item.get("examples") or []),
                "required": bool(item.get("required", True)),
            }
        )
    if not normalized_fields:
        raise ValueError("Extraction template did not contain usable fields.")

    output_schema = payload.get("output_schema")
    if not isinstance(output_schema, dict):
        output_schema = {}
    output_schema = {str(key).strip(): "" for key in output_schema if str(key).strip()}
    for field in REVIEW_EVIDENCE_EXPORT_COLUMNS:
        output_schema.setdefault(field, "")
    for field in normalized_fields:
        output_schema.setdefault(field["field_name"], "")

    return {
        "template_name": str(payload.get("template_name") or "知识抽取标引模板").strip(),
        "expert_role": str(payload.get("expert_role") or "你是该领域的文献综述证据抽取专家。").strip(),
        "general_instruction": str(payload.get("general_instruction") or "").strip(),
        "fields": normalized_fields,
        "output_schema": output_schema,
    }
