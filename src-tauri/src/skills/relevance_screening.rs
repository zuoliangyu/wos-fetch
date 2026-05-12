//! LLM-driven relevance scoring of WoS records.
//!
//! Port of `skills/relevance_screening.py`. Batches records into LLM calls,
//! parses the structured score response, and writes per-row scoring fields
//! back onto each record.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::core::json_protocol::load_json_object;
use crate::core::llm_client::{chat_text, ChatMessage, LlmConfig};
use crate::{AppError, AppResult};

static MOJIBAKE_HINT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\u{C3}\u{C2}\u{C4}\u{C5}][\u{80}-\u{ff}]|(?:ä|å|æ|ç|è|é|ï|ð)").unwrap()
});
static CJK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\u{4E00}-\u{9FFF}]").unwrap());
static WS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());
static NUMBER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-?\d+(?:\.\d+)?").unwrap());

fn cell_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn first_nonblank(record: &Map<String, Value>, keys: &[&str]) -> String {
    for k in keys {
        let text = cell_string(record.get(*k));
        if !text.trim().is_empty() {
            return text;
        }
    }
    String::new()
}

fn text_cleanup(raw: &str) -> String {
    let mut text = raw.to_string();
    if MOJIBAKE_HINT_RE.is_match(&text) {
        for encoding in ["latin-1", "windows-1252"] {
            let Some(enc) = encoding_rs::Encoding::for_label(encoding.as_bytes()) else {
                continue;
            };
            let bytes: Vec<u8> = text
                .chars()
                .map(|c| c as u32)
                .filter(|c| *c < 256)
                .map(|c| c as u8)
                .collect();
            if bytes.len() != text.chars().count() {
                continue;
            }
            let (decoded, _, had_errors) = enc.decode(&bytes);
            if had_errors {
                continue;
            }
            let candidate = decoded.into_owned();
            if !candidate.is_empty() && candidate != text && CJK_RE.is_match(&candidate) {
                text = candidate;
                break;
            }
        }
    }
    WS_RE.replace_all(&text, " ").trim().to_string()
}

fn take_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn level_from_score(score: i64) -> &'static str {
    if score >= 80 {
        "核心"
    } else if score >= 60 {
        "辅助"
    } else if score >= 40 {
        "背景"
    } else {
        "排除"
    }
}

fn recommendation_from_score(score: i64) -> &'static str {
    if score >= 80 {
        "核心纳入"
    } else if score >= 60 {
        "辅助纳入"
    } else if score >= 40 {
        "背景参考"
    } else {
        "排除"
    }
}

fn clamp_int(value: &Value, low: i64, high: i64) -> i64 {
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let m = match NUMBER_RE.find(&text) {
        Some(m) => m,
        None => return low,
    };
    let parsed: f64 = m.as_str().parse().unwrap_or(low as f64);
    let rounded = parsed.round() as i64;
    rounded.clamp(low, high)
}

fn record_for_llm(record: &Map<String, Value>, local_index: usize) -> Map<String, Value> {
    let id = format!("R{:03}", local_index + 1);
    let title = text_cleanup(&first_nonblank(
        record,
        &["Article Title", "article_title", "title"],
    ));
    let abstract_text = text_cleanup(&first_nonblank(
        record,
        &["Abstract", "abstract", "summary"],
    ));
    let dtype = text_cleanup(&first_nonblank(record, &["Document Type", "document_type"]));
    let source = text_cleanup(&first_nonblank(
        record,
        &["Source Title", "source_title", "journal"],
    ));
    let author_keywords = text_cleanup(&first_nonblank(
        record,
        &["Author Keywords", "author_keywords"],
    ));
    let keywords_plus = text_cleanup(&first_nonblank(record, &["Keywords Plus", "keywords_plus"]));

    let mut map = Map::new();
    map.insert("record_id".into(), Value::String(id));
    map.insert(
        "article_title".into(),
        Value::String(take_chars(&title, 500)),
    );
    map.insert(
        "abstract".into(),
        Value::String(take_chars(&abstract_text, 2500)),
    );
    map.insert(
        "document_type".into(),
        Value::String(take_chars(&dtype, 160)),
    );
    map.insert(
        "source_title".into(),
        Value::String(take_chars(&source, 240)),
    );
    map.insert(
        "author_keywords".into(),
        Value::String(take_chars(&author_keywords, 500)),
    );
    map.insert(
        "keywords_plus".into(),
        Value::String(take_chars(&keywords_plus, 500)),
    );
    map
}

fn normalize_llm_score(item: &Map<String, Value>) -> Map<String, Value> {
    let null_value = Value::Null;
    let topic = clamp_int(item.get("主题匹配度评分").unwrap_or(&null_value), 0, 30);
    let evidence = clamp_int(item.get("证据可用性评分").unwrap_or(&null_value), 0, 30);
    let section = clamp_int(item.get("章节适配度评分").unwrap_or(&null_value), 0, 20);
    let method = clamp_int(item.get("对象方法适配度评分").unwrap_or(&null_value), 0, 20);
    let total = topic + evidence + section + method;
    let reason_raw = text_cleanup(&cell_string(item.get("主题相关性理由")));
    let reason = if reason_raw.is_empty() {
        "LLM 根据标题、摘要、检索方向和建议章节进行相关性初筛。".to_string()
    } else {
        take_chars(&reason_raw, 500)
    };
    let downgrade = if total >= 80 {
        "NA".to_string()
    } else {
        let raw = text_cleanup(&cell_string(item.get("排除或降权原因")));
        if raw.is_empty() {
            reason.clone()
        } else {
            take_chars(&raw, 500)
        }
    };
    let mut out = Map::new();
    out.insert("主题匹配度评分".into(), Value::String(topic.to_string()));
    out.insert("证据可用性评分".into(), Value::String(evidence.to_string()));
    out.insert("章节适配度评分".into(), Value::String(section.to_string()));
    out.insert(
        "对象方法适配度评分".into(),
        Value::String(method.to_string()),
    );
    out.insert("主题相关性总分".into(), Value::String(total.to_string()));
    out.insert(
        "相关性等级".into(),
        Value::String(level_from_score(total).into()),
    );
    out.insert("主题相关性理由".into(), Value::String(reason));
    out.insert(
        "纳入建议".into(),
        Value::String(recommendation_from_score(total).into()),
    );
    out.insert("排除或降权原因".into(), Value::String(downgrade));
    out.insert(
        "relevance_score_source".into(),
        Value::String("llm_title_abstract".into()),
    );
    out
}

#[derive(Debug, Default, Clone)]
pub struct ScreeningContext {
    pub topic: String,
    pub direction_name: String,
    pub suggested_section: String,
    pub search_query: String,
}

async fn score_batch_with_llm(
    batch: &[(usize, Map<String, Value>)],
    context: &ScreeningContext,
    config: &LlmConfig,
) -> AppResult<std::collections::HashMap<usize, Map<String, Value>>> {
    let records: Vec<Value> = batch
        .iter()
        .enumerate()
        .map(|(local_index, (_, record))| Value::Object(record_for_llm(record, local_index)))
        .collect();
    let index_by_record_id: std::collections::HashMap<String, usize> = batch
        .iter()
        .enumerate()
        .map(|(local_index, (original_index, _))| {
            (format!("R{:03}", local_index + 1), *original_index)
        })
        .collect();

    let payload = json!({
        "review_topic": take_chars(&text_cleanup(&context.topic), 1000),
        "search_direction": take_chars(&text_cleanup(&context.direction_name), 500),
        "suggested_section": take_chars(&text_cleanup(&context.suggested_section), 500),
        "wos_query": take_chars(&text_cleanup(&context.search_query), 1200),
        "scoring_rule": {
            "主题匹配度评分": "0-30，标题和摘要与综述主题、检索方向、核心问题的匹配程度。",
            "证据可用性评分": "0-30，摘要是否提供可写入综述的具体方法、结果、指标、对比、局限或趋势。",
            "章节适配度评分": "0-20，文献是否能明确支撑建议章节或 Review Plan 章节。",
            "对象方法适配度评分": "0-20，研究对象、方法、场景、指标与综述范围的吻合程度。",
            "主题相关性总分": "必须等于四个分项之和。",
        },
        "level_rule": "总分 >=80 为核心，60-79 为辅助，40-59 为背景，<40 为排除。",
        "records": records,
    });

    let system_prompt = concat!(
        "你是文献综述检索初筛专家。只能根据给定的题名、摘要、关键词、检索方向和建议章节打相关性分。",
        "不要编造摘要中不存在的信息。只返回合法 JSON，不要 markdown 代码块。",
        "所有字符串值必须在同一行内，不得包含换行符、制表符或未转义的引号。",
    );
    let user_prompt = format!(
        "请对 records 中每篇文献进行固定维度相关性评分。返回 JSON 对象，顶层 key 为 scores。scores 是数组，每个对象必须包含：record_id, 主题匹配度评分, 证据可用性评分, 章节适配度评分, 对象方法适配度评分, 主题相关性总分, 相关性等级, 主题相关性理由, 纳入建议, 排除或降权原因。主题相关性总分必须等于四个分项之和。主题相关性理由和排除或降权原因字段必须是单行纯文本，不超过100字，不含引号和换行。\n\n{}",
        serde_json::to_string(&payload).unwrap_or_default()
    );
    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: system_prompt.into(),
        },
        ChatMessage {
            role: "user".into(),
            content: user_prompt,
        },
    ];
    let cfg = LlmConfig {
        temperature: Some(0.0),
        ..config.clone()
    };
    let raw = chat_text(&cfg, &messages).await?;
    let parsed = load_json_object(&raw)?;
    let items = parsed
        .get("scores")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            AppError::Llm("LLM relevance scoring did not return a scores array.".into())
        })?;
    let mut output: std::collections::HashMap<usize, Map<String, Value>> =
        std::collections::HashMap::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let id = obj
            .get("record_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if let Some(&original_index) = index_by_record_id.get(&id) {
            output.insert(original_index, normalize_llm_score(obj));
        }
    }
    Ok(output)
}

/// Score relevance for all `records`. If model or api_key are blank, the
/// records pass through unchanged (matches Python behavior).
pub async fn score_relevance(
    records: &[Map<String, Value>],
    context: &ScreeningContext,
    config: &LlmConfig,
    batch_size: usize,
) -> AppResult<Vec<Map<String, Value>>> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let mut scored: Vec<Map<String, Value>> = records.to_vec();
    if config.model.trim().is_empty() || config.api_key.trim().is_empty() {
        return Ok(scored);
    }
    let size = batch_size.clamp(1, 20);
    let indexed: Vec<(usize, Map<String, Value>)> = scored.iter().cloned().enumerate().collect();
    let mut start = 0;
    while start < indexed.len() {
        let end = (start + size).min(indexed.len());
        let batch = &indexed[start..end];
        match score_batch_with_llm(batch, context, config).await {
            Ok(scores) => {
                for (original_index, item) in scores {
                    for (k, v) in item {
                        scored[original_index].insert(k, v);
                    }
                }
            }
            Err(exc) => {
                let truncated: String = exc.to_string().chars().take(500).collect();
                for (original_index, _) in batch {
                    scored[*original_index].insert(
                        "relevance_score_source".into(),
                        Value::String("llm_score_error".into()),
                    );
                    scored[*original_index].insert(
                        "relevance_score_error".into(),
                        Value::String(format!("batch_failed:{truncated}")),
                    );
                }
            }
        }
        start = end;
    }
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_thresholds() {
        assert_eq!(level_from_score(90), "核心");
        assert_eq!(level_from_score(65), "辅助");
        assert_eq!(level_from_score(50), "背景");
        assert_eq!(level_from_score(10), "排除");
    }

    #[test]
    fn clamp_int_extracts_number() {
        assert_eq!(clamp_int(&Value::String("25.7".into()), 0, 30), 26);
        assert_eq!(clamp_int(&Value::String("分数: 18".into()), 0, 20), 18);
        assert_eq!(clamp_int(&Value::String("nope".into()), 0, 30), 0);
        assert_eq!(clamp_int(&Value::Number(50.into()), 0, 30), 30);
    }

    #[test]
    fn normalize_score_sums_subparts() {
        let item = json!({
            "主题匹配度评分": 25,
            "证据可用性评分": 25,
            "章节适配度评分": 15,
            "对象方法适配度评分": 15,
            "主题相关性理由": "matches well"
        });
        let obj = item.as_object().unwrap();
        let out = normalize_llm_score(obj);
        assert_eq!(
            out.get("主题相关性总分").unwrap(),
            &Value::String("80".into())
        );
        assert_eq!(
            out.get("相关性等级").unwrap(),
            &Value::String("核心".into())
        );
        assert_eq!(
            out.get("纳入建议").unwrap(),
            &Value::String("核心纳入".into())
        );
    }
}
