from __future__ import annotations

import re
from typing import Any

from core.json_protocol import load_json_object, strip_markdown_fence
from core.llm_client import chat_text
from core.text_normalize import clean_search_query, validate_wos_search_query


PLANNER_SYSTEM_PROMPT = "你是学术综述任务规划器。遵守给定协议，只输出可解析 JSON。"

PLANNER_PROTOCOL = """
根据用户研究主题，生成综述检索与写作计划。计划必须连接后续 WoS 检索、全文获取、证据抽取和综述写作。

必须包含：
- normalized_topic
- inferred_domain
- review_objective
- research_questions
- scope_boundaries
- inclusion_criteria
- exclusion_criteria
- search_element_table
- suggested_sections
- evidence_to_section_map
- search_directions
- plan_risks

每个检索方向必须包含：
- direction_index
- direction_name
- purpose
- suggested_section
- search_query
- expected_records
- include_hint
- exclude_hint

至少一个方向必须是 review-only，且 search_query 包含 DT=(Review)。
""".strip()

WOS_QUERY_SYSTEM_PROMPT = (
    "你是 Web of Science 高级检索式构建器。请把用户的中文研究主题、限制条件、日期范围、"
    "文献类型和排除条件转换成一条可以直接使用的 Web of Science 高级检索表达式。"
    "使用 TS=、TI=、AB=、PY=、DT= 等 WoS 字段标签。关键词使用英文，并包含必要的常见同义词和变体。"
    "使用清晰的 AND、OR、NOT 和括号表达布尔逻辑。"
    "如果用户提到出版年份、年份范围、日期范围、近几年、过去 N 年或中文等价表达，返回表达式中必须包含 PY=(...)。"
    "转换相对年份范围时，使用当前年份作为当前年份。除非用户明确要求，不要自行添加年份范围。"
    "只返回检索表达式，不要解释、markdown 或代码块。"
)


def _strip_wrapping_quotes(value: str) -> str:
    text = str(value or "").strip()
    if len(text) >= 2 and text[0] == text[-1] and text[0] in {"'", '"'}:
        return text[1:-1].strip()
    return text


def build_wos_search_query(
    *,
    topic_text: str,
    model: str,
    base_url: str,
    api_key: str,
    timeout: int = 120,
) -> str:
    topic = str(topic_text or "").strip()
    if not topic:
        raise ValueError("请输入研究主题。")
    if not model.strip() or not api_key.strip():
        raise ValueError("请配置 model 和 api_key。")

    output_text = chat_text(
        base_url=base_url,
        api_key=api_key,
        model=model,
        messages=[
            {"role": "system", "content": WOS_QUERY_SYSTEM_PROMPT},
            {"role": "user", "content": f"中文研究主题或限制条件：\n{topic}"},
        ],
        timeout=timeout,
        temperature=0.2,
    )
    output = _strip_wrapping_quotes(strip_markdown_fence(output_text)).strip()
    output = re.sub(r"^\s*(?:检索式|Search query|Query)\s*[:：]\s*", "", output, flags=re.IGNORECASE)
    output = validate_wos_search_query(output)
    if not output:
        raise ValueError("模型返回了空检索式。")
    return output


def _as_string_list(value: Any) -> list[str]:
    if isinstance(value, list):
        return [str(item).strip() for item in value if str(item).strip()]
    text = str(value or "").strip()
    if not text:
        return []
    return [part.strip() for part in re.split(r"[;\n]+", text) if part.strip()]


def _as_string_dict(value: Any) -> dict[str, str]:
    import json
    if not isinstance(value, dict):
        return {}
    result: dict[str, str] = {}
    for key, item in value.items():
        key_text = str(key or "").strip()
        if not key_text:
            continue
        if isinstance(item, (dict, list)):
            result[key_text] = json.dumps(item, ensure_ascii=False)
        else:
            result[key_text] = str(item or "").strip()
    return result


def _normalize_directions(value: Any) -> list[dict[str, str]]:
    items = value if isinstance(value, list) else []
    directions: list[dict[str, str]] = []
    for index, item in enumerate(items, start=1):
        if not isinstance(item, dict):
            continue
        query = str(item.get("search_query") or item.get("wos_query") or item.get("query") or "").strip()
        if not query:
            continue
        query = validate_wos_search_query(query)
        directions.append({
            "direction_index": str(item.get("direction_index") or index).strip(),
            "direction_name": str(item.get("direction_name") or item.get("name") or f"Direction {index}").strip(),
            "purpose": str(item.get("purpose") or item.get("reason") or "").strip(),
            "suggested_section": str(item.get("suggested_section") or "").strip(),
            "search_query": query,
            "expected_records": str(item.get("expected_records") or "").strip(),
            "include_hint": str(item.get("include_hint") or item.get("inclusion_hint") or "").strip(),
            "exclude_hint": str(item.get("exclude_hint") or item.get("exclusion_hint") or "").strip(),
        })
    if not directions:
        raise ValueError("规划结果中没有可用的检索方向（search_directions）。")
    return directions


def _normalize_named_items(value: Any, name_key: str) -> list[dict[str, str]]:
    result: list[dict[str, str]] = []
    if not isinstance(value, list):
        return result
    for item in value:
        if isinstance(item, dict):
            name = str(item.get(name_key) or item.get("name") or item.get("title") or "").strip()
            if not name:
                continue
            entry = {str(k): str(v or "").strip() for k, v in item.items() if str(k).strip()}
            entry.setdefault(name_key, name)
            result.append(entry)
        else:
            name = str(item or "").strip()
            if name:
                result.append({name_key: name})
    return result


def normalize_review_plan(payload: Any) -> dict[str, Any]:
    if not isinstance(payload, dict):
        payload = {"search_directions": payload if isinstance(payload, list) else []}
    directions = _normalize_directions(
        payload.get("search_directions") or payload.get("directions") or []
    )
    plan = {
        "normalized_topic": str(payload.get("normalized_topic") or payload.get("topic") or "").strip(),
        "inferred_domain": str(payload.get("inferred_domain") or payload.get("domain") or "").strip(),
        "review_objective": str(payload.get("review_objective") or payload.get("objective") or "").strip(),
        "research_questions": _as_string_list(payload.get("research_questions") or []),
        "scope_boundaries": _as_string_dict(payload.get("scope_boundaries") or {}),
        "inclusion_criteria": _as_string_list(payload.get("inclusion_criteria") or []),
        "exclusion_criteria": _as_string_list(payload.get("exclusion_criteria") or []),
        "search_element_table": payload.get("search_element_table") or [],
        "suggested_sections": _normalize_named_items(payload.get("suggested_sections") or [], "section_title"),
        "evidence_to_section_map": payload.get("evidence_to_section_map") or [],
        "search_directions": directions,
        "plan_risks": payload.get("plan_risks") or [],
    }
    if not any("DT=(REVIEW)" in d["search_query"].upper() for d in directions):
        raise ValueError("规划结果中必须至少包含一个 DT=(Review) 的综述专项检索方向。")
    return plan


def _direction_count_instruction(direction_count: str) -> str:
    raw = str(direction_count or "auto").strip().lower()
    if raw in {"", "auto", "0"}:
        return "由模型根据主题宽窄决定检索方向数量：窄主题 3-5 条，中等主题 6-8 条，宽主题 8-10 条；最终必须在 3 到 10 条之间。"
    try:
        count = max(3, min(int(raw), 10))
    except ValueError:
        return "由模型根据主题宽窄决定检索方向数量，最终必须在 3 到 10 条之间。"
    return f"生成 {count} 条检索方向。"


def build_review_plan(
    *,
    topic_text: str,
    model: str,
    base_url: str,
    api_key: str,
    timeout: int = 180,
    direction_count: str = "auto",
) -> dict[str, Any]:
    topic = str(topic_text or "").strip()
    if not topic:
        raise ValueError("请输入研究主题。")
    if not model.strip() or not api_key.strip():
        raise ValueError("请配置 model 和 api_key。")

    user_prompt = (
        f"## 规划协议\n{PLANNER_PROTOCOL}\n\n"
        "## 当前任务\n"
        f"研究主题：{topic}\n"
        f"检索方向数量规则：{_direction_count_instruction(direction_count)}\n\n"
        "优先保持计划简洁实用，不要为了完整性堆叠过多层级。"
        "如果主题边界清晰，优先返回更少但更高质量的检索方向。\n\n"
        "请只返回符合协议的 JSON 对象，不要返回 Markdown、解释或代码块。"
    )
    output = chat_text(
        base_url=base_url,
        api_key=api_key,
        model=model,
        messages=[
            {"role": "system", "content": PLANNER_SYSTEM_PROMPT},
            {"role": "user", "content": user_prompt},
        ],
        timeout=timeout,
        temperature=0.25,
    )
    return normalize_review_plan(load_json_object(output))
