from __future__ import annotations

import json
import re
from typing import Any

from core.json_protocol import load_json_object
from core.llm_client import chat_text


def _text(value: Any) -> str:
    text = str(value or "")
    if re.search(r"[ÃÂÄÅ][\x80-\xff]|(?:ä|å|æ|ç|è|é|ï|ð)", text):
        for encoding in ("latin1", "cp1252"):
            try:
                repaired = text.encode(encoding).decode("utf-8")
            except UnicodeError:
                continue
            if repaired and repaired != text and re.search(r"[一-鿿]", repaired):
                text = repaired
                break
    return re.sub(r"\s+", " ", text).strip()


def _level_from_score(score: int) -> str:
    if score >= 80:
        return "核心"
    if score >= 60:
        return "辅助"
    if score >= 40:
        return "背景"
    return "排除"


def _recommendation_from_score(score: int) -> str:
    if score >= 80:
        return "核心纳入"
    if score >= 60:
        return "辅助纳入"
    if score >= 40:
        return "背景参考"
    return "排除"


def _clamp_int(value: Any, low: int, high: int) -> int:
    match = re.search(r"-?\d+(?:\.\d+)?", str(value or ""))
    if not match:
        return low
    number = int(round(float(match.group(0))))
    return max(low, min(high, number))


def _record_for_llm(record: dict[str, Any], index: int) -> dict[str, str]:
    return {
        "record_id": f"R{index:03d}",
        "article_title": _text(record.get("Article Title") or record.get("article_title") or record.get("title"))[:500],
        "abstract": _text(record.get("Abstract") or record.get("abstract") or record.get("summary"))[:2500],
        "document_type": _text(record.get("Document Type") or record.get("document_type"))[:160],
        "source_title": _text(record.get("Source Title") or record.get("source_title") or record.get("journal"))[:240],
        "author_keywords": _text(record.get("Author Keywords") or record.get("author_keywords"))[:500],
        "keywords_plus": _text(record.get("Keywords Plus") or record.get("keywords_plus"))[:500],
    }


def _normalize_llm_score(item: dict[str, Any]) -> dict[str, str]:
    topic = _clamp_int(item.get("主题匹配度评分"), 0, 30)
    evidence = _clamp_int(item.get("证据可用性评分"), 0, 30)
    section = _clamp_int(item.get("章节适配度评分"), 0, 20)
    method = _clamp_int(item.get("对象方法适配度评分"), 0, 20)
    total = topic + evidence + section + method
    reason = _text(item.get("主题相关性理由"))[:500] or "LLM 根据标题、摘要、检索方向和建议章节进行相关性初筛。"
    downgrade = "NA" if total >= 80 else (_text(item.get("排除或降权原因"))[:500] or reason)
    return {
        "主题匹配度评分": str(topic),
        "证据可用性评分": str(evidence),
        "章节适配度评分": str(section),
        "对象方法适配度评分": str(method),
        "主题相关性总分": str(total),
        "相关性等级": _level_from_score(total),
        "主题相关性理由": reason,
        "纳入建议": _recommendation_from_score(total),
        "排除或降权原因": downgrade,
        "relevance_score_source": "llm_title_abstract",
    }


def _score_batch_with_llm(batch, context, *, model, base_url, api_key, timeout):
    records = [_record_for_llm(record, local_index + 1) for local_index, (_, record) in enumerate(batch)]
    index_by_record_id = {f"R{local_index + 1:03d}": original_index for local_index, (original_index, _) in enumerate(batch)}
    payload = {
        "review_topic": _text(context.get("topic_text") or context.get("topic"))[:1000],
        "search_direction": _text(context.get("direction_name") or context.get("direction"))[:500],
        "suggested_section": _text(context.get("suggested_section") or context.get("section"))[:500],
        "wos_query": _text(context.get("search_query") or context.get("query"))[:1200],
        "scoring_rule": {
            "主题匹配度评分": "0-30，标题和摘要与综述主题、检索方向、核心问题的匹配程度。",
            "证据可用性评分": "0-30，摘要是否提供可写入综述的具体方法、结果、指标、对比、局限或趋势。",
            "章节适配度评分": "0-20，文献是否能明确支撑建议章节或 Review Plan 章节。",
            "对象方法适配度评分": "0-20，研究对象、方法、场景、指标与综述范围的吻合程度。",
            "主题相关性总分": "必须等于四个分项之和。",
        },
        "level_rule": "总分 >=80 为核心，60-79 为辅助，40-59 为背景，<40 为排除。",
        "records": records,
    }
    system_prompt = (
        "你是文献综述检索初筛专家。只能根据给定的题名、摘要、关键词、检索方向和建议章节打相关性分。"
        "不要编造摘要中不存在的信息。只返回合法 JSON，不要 markdown 代码块。"
        "所有字符串值必须在同一行内，不得包含换行符、制表符或未转义的引号。"
    )
    user_prompt = (
        "请对 records 中每篇文献进行固定维度相关性评分。"
        "返回 JSON 对象，顶层 key 为 scores。scores 是数组，每个对象必须包含："
        "record_id, 主题匹配度评分, 证据可用性评分, 章节适配度评分, 对象方法适配度评分, "
        "主题相关性总分, 相关性等级, 主题相关性理由, 纳入建议, 排除或降权原因。"
        "主题相关性总分必须等于四个分项之和。"
        "主题相关性理由和排除或降权原因字段必须是单行纯文本，不超过100字，不含引号和换行。\n\n"
        f"{json.dumps(payload, ensure_ascii=False)}"
    )
    text = chat_text(
        base_url=base_url, api_key=api_key, model=model,
        messages=[{"role": "system", "content": system_prompt}, {"role": "user", "content": user_prompt}],
        timeout=timeout, temperature=0.0,
    )
    parsed = load_json_object(text)
    items = parsed.get("scores")
    if not isinstance(items, list):
        raise ValueError("LLM relevance scoring did not return a scores array.")
    output: dict[int, dict[str, str]] = {}
    for item in items:
        if not isinstance(item, dict):
            continue
        record_id = str(item.get("record_id") or "").strip()
        if record_id not in index_by_record_id:
            continue
        output[index_by_record_id[record_id]] = _normalize_llm_score(item)
    return output


def score_relevance(
    records: list[dict[str, Any]],
    context: dict[str, Any] | None = None,
    *,
    model: str = "",
    base_url: str = "",
    api_key: str = "",
    timeout: int = 120,
    batch_size: int = 12,
) -> list[dict[str, Any]]:
    if not records:
        return []
    if not model.strip() or not api_key.strip():
        return [dict(record) for record in records]

    context = context or {}
    scored = [dict(record) for record in records]
    indexed = list(enumerate(scored))
    size = max(1, min(int(batch_size or 12), 20))
    for start in range(0, len(indexed), size):
        batch = indexed[start : start + size]
        try:
            scores = _score_batch_with_llm(
                batch, context,
                model=model, base_url=base_url or "https://api.openai.com/v1", api_key=api_key, timeout=timeout,
            )
        except Exception as exc:
            batch_error = str(exc)[:500]
            for original_index, record in batch:
                scored[original_index]["relevance_score_source"] = "llm_score_error"
                scored[original_index]["relevance_score_error"] = f"batch_failed:{batch_error}"
            continue
        for original_index, item in scores.items():
            scored[original_index].update(item)
    return scored
