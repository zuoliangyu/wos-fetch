//! WoS query + review-plan builders.
//!
//! Port of `skills/query_builder.py`. Two LLM-driven entry points:
//!
//! - `build_wos_search_query` turns a freeform Chinese topic description into
//!   a single canonical WoS Advanced Search expression.
//! - `build_review_plan` produces a structured planning JSON (scope,
//!   inclusion/exclusion criteria, search directions, etc.) used to drive
//!   downstream search + screening.

use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::core::json_protocol::{load_json_object, strip_markdown_fence};
use crate::core::llm_client::{chat_text, ChatMessage, LlmConfig};
use crate::core::text_normalize::{clean_search_query, validate_wos_search_query};
use crate::{AppError, AppResult};

static QUERY_LABEL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*(?i)(?:检索式|Search query|Query)\s*[:：]\s*").unwrap());
static LIST_SPLIT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[;\n]+").unwrap());

pub const PLANNER_SYSTEM_PROMPT: &str =
    "你是学术综述任务规划器。遵守给定协议，只输出可解析 JSON。";

pub const PLANNER_PROTOCOL: &str = r#"根据用户研究主题，生成综述检索与写作计划。计划必须连接后续 WoS 检索、全文获取、证据抽取和综述写作。

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

至少一个方向必须是 review-only，且 search_query 包含 DT=(Review)。"#;

pub const WOS_QUERY_SYSTEM_PROMPT: &str = concat!(
    "你是 Web of Science 高级检索式构建器。请把用户的中文研究主题、限制条件、日期范围、",
    "文献类型和排除条件转换成一条可以直接使用的 Web of Science 高级检索表达式。",
    "使用 TS=、TI=、AB=、PY=、DT= 等 WoS 字段标签。关键词使用英文，并包含必要的常见同义词和变体。",
    "使用清晰的 AND、OR、NOT 和括号表达布尔逻辑。",
    "如果用户提到出版年份、年份范围、日期范围、近几年、过去 N 年或中文等价表达，返回表达式中必须包含 PY=(...)。",
    "转换相对年份范围时，使用当前年份作为当前年份。除非用户明确要求，不要自行添加年份范围。",
    "只返回检索表达式，不要解释、markdown 或代码块。",
);

/// Inject hint when `oa_only` mode is on. Appended to the system prompt.
pub const OA_ONLY_PROMPT_HINT: &str = concat!(
    "用户启用了「仅 OA 期刊」模式：在生成的表达式末尾追加 ` AND OA=(\"All Open Access\")` ",
    "把检索范围限定到开放获取文献，避免抓取付费墙文献触发出版商反爬。",
);

const OA_CONSTRAINT_SUFFIX: &str = " AND OA=(\"All Open Access\")";

/// Append the OA filter to a WoS query if it does not already restrict OA.
/// Idempotent: returns the input unchanged when `OA=` already appears.
pub fn append_oa_constraint(query: &str) -> String {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    if trimmed.to_ascii_uppercase().contains("OA=") {
        return trimmed.to_string();
    }
    format!("({trimmed}){OA_CONSTRAINT_SUFFIX}")
}

fn strip_wrapping_quotes(value: &str) -> String {
    let text = value.trim();
    if text.len() >= 2 {
        let first = text.chars().next().unwrap();
        let last = text.chars().last().unwrap();
        if first == last && (first == '"' || first == '\'') {
            return text[first.len_utf8()..text.len() - last.len_utf8()].trim().to_string();
        }
    }
    text.to_string()
}

pub async fn build_wos_search_query(
    topic_text: &str,
    config: &LlmConfig,
    oa_only: bool,
) -> AppResult<String> {
    let topic = topic_text.trim();
    if topic.is_empty() {
        return Err(AppError::BadInput("请输入研究主题。".into()));
    }
    if config.model.trim().is_empty() || config.api_key.trim().is_empty() {
        return Err(AppError::BadInput("请配置 model 和 api_key。".into()));
    }
    let system_prompt = if oa_only {
        format!("{WOS_QUERY_SYSTEM_PROMPT}\n{OA_ONLY_PROMPT_HINT}")
    } else {
        WOS_QUERY_SYSTEM_PROMPT.into()
    };
    let messages = vec![
        ChatMessage { role: "system".into(), content: system_prompt },
        ChatMessage {
            role: "user".into(),
            content: format!("中文研究主题或限制条件：\n{topic}"),
        },
    ];
    let cfg = LlmConfig { temperature: Some(0.2), ..config.clone() };
    let raw = chat_text(&cfg, &messages).await?;
    let fence_stripped = strip_markdown_fence(&raw);
    let unquoted = strip_wrapping_quotes(&fence_stripped);
    let cleaned = QUERY_LABEL_RE.replace(&unquoted, "").to_string();
    let with_oa = if oa_only { append_oa_constraint(&cleaned) } else { cleaned };
    let validated = validate_wos_search_query(&with_oa)?;
    if validated.is_empty() {
        return Err(AppError::Llm("模型返回了空检索式。".into()));
    }
    Ok(clean_search_query(&validated))
}

fn as_string_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => s.trim().to_string(),
                other => other.to_string().trim().to_string(),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                LIST_SPLIT_RE
                    .split(trimmed)
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            }
        }
        Value::Null => Vec::new(),
        other => {
            let text = other.to_string().trim().to_string();
            if text.is_empty() { Vec::new() } else { vec![text] }
        }
    }
}

fn as_string_dict(value: &Value) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let Some(obj) = value.as_object() else { return result };
    for (key, item) in obj {
        let key_text = key.trim();
        if key_text.is_empty() {
            continue;
        }
        let str_value = match item {
            Value::Object(_) | Value::Array(_) => serde_json::to_string(item).unwrap_or_default(),
            Value::Null => String::new(),
            Value::String(s) => s.trim().to_string(),
            other => other.to_string(),
        };
        result.insert(key_text.to_string(), str_value);
    }
    result
}

fn extract_str(item: &Map<String, Value>, keys: &[&str]) -> String {
    for k in keys {
        if let Some(Value::String(s)) = item.get(*k) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if let Some(other) = item.get(*k) {
            let text = other.to_string();
            let trimmed = text.trim().trim_matches('"');
            if !trimmed.is_empty() && trimmed != "null" {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn normalize_directions(value: &Value) -> AppResult<Vec<Map<String, Value>>> {
    let items = value.as_array().cloned().unwrap_or_default();
    let mut directions: Vec<Map<String, Value>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else { continue };
        let query_raw = extract_str(obj, &["search_query", "wos_query", "query"]);
        if query_raw.is_empty() {
            continue;
        }
        let validated = validate_wos_search_query(&query_raw)?;
        let mut entry = Map::new();
        entry.insert(
            "direction_index".into(),
            Value::String(
                extract_str(obj, &["direction_index"])
                    .as_str()
                    .trim()
                    .to_string(),
            ),
        );
        let direction_index = entry
            .get("direction_index")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default();
        if direction_index.is_empty() {
            entry.insert("direction_index".into(), Value::String((index + 1).to_string()));
        }
        let name = {
            let n = extract_str(obj, &["direction_name", "name"]);
            if n.is_empty() { format!("Direction {}", index + 1) } else { n }
        };
        entry.insert("direction_name".into(), Value::String(name));
        entry.insert(
            "purpose".into(),
            Value::String(extract_str(obj, &["purpose", "reason"])),
        );
        entry.insert(
            "suggested_section".into(),
            Value::String(extract_str(obj, &["suggested_section"])),
        );
        entry.insert("search_query".into(), Value::String(validated));
        entry.insert(
            "expected_records".into(),
            Value::String(extract_str(obj, &["expected_records"])),
        );
        entry.insert(
            "include_hint".into(),
            Value::String(extract_str(obj, &["include_hint", "inclusion_hint"])),
        );
        entry.insert(
            "exclude_hint".into(),
            Value::String(extract_str(obj, &["exclude_hint", "exclusion_hint"])),
        );
        directions.push(entry);
    }
    if directions.is_empty() {
        return Err(AppError::BadInput(
            "规划结果中没有可用的检索方向（search_directions）。".into(),
        ));
    }
    Ok(directions)
}

fn normalize_named_items(value: &Value, name_key: &str) -> Vec<Map<String, Value>> {
    let Some(items) = value.as_array() else { return Vec::new() };
    let mut result: Vec<Map<String, Value>> = Vec::new();
    for item in items {
        if let Some(obj) = item.as_object() {
            let name = extract_str(obj, &[name_key, "name", "title"]);
            if name.is_empty() {
                continue;
            }
            let mut entry: Map<String, Value> = Map::new();
            for (k, v) in obj {
                if k.trim().is_empty() {
                    continue;
                }
                let value_str = match v {
                    Value::String(s) => s.trim().to_string(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                entry.insert(k.clone(), Value::String(value_str));
            }
            entry
                .entry(name_key.to_string())
                .or_insert(Value::String(name));
            result.push(entry);
        } else if let Some(name_str) = item.as_str() {
            let name = name_str.trim().to_string();
            if !name.is_empty() {
                let mut entry = Map::new();
                entry.insert(name_key.into(), Value::String(name));
                result.push(entry);
            }
        }
    }
    result
}

pub fn normalize_review_plan(payload: Value) -> AppResult<Value> {
    let payload = match payload {
        Value::Object(map) => map,
        Value::Array(arr) => {
            let mut wrap = Map::new();
            wrap.insert("search_directions".into(), Value::Array(arr));
            wrap
        }
        _ => Map::new(),
    };

    let directions_source = payload
        .get("search_directions")
        .or_else(|| payload.get("directions"))
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let directions = normalize_directions(&directions_source)?;

    let mut plan = Map::new();
    plan.insert(
        "normalized_topic".into(),
        Value::String(
            extract_str(&payload, &["normalized_topic", "topic"]).to_string(),
        ),
    );
    plan.insert(
        "inferred_domain".into(),
        Value::String(extract_str(&payload, &["inferred_domain", "domain"]).to_string()),
    );
    plan.insert(
        "review_objective".into(),
        Value::String(extract_str(&payload, &["review_objective", "objective"]).to_string()),
    );
    plan.insert(
        "research_questions".into(),
        Value::Array(
            as_string_list(payload.get("research_questions").unwrap_or(&Value::Null))
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    let scope_dict = as_string_dict(payload.get("scope_boundaries").unwrap_or(&Value::Null));
    plan.insert(
        "scope_boundaries".into(),
        Value::Object(scope_dict.into_iter().map(|(k, v)| (k, Value::String(v))).collect()),
    );
    plan.insert(
        "inclusion_criteria".into(),
        Value::Array(
            as_string_list(payload.get("inclusion_criteria").unwrap_or(&Value::Null))
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    plan.insert(
        "exclusion_criteria".into(),
        Value::Array(
            as_string_list(payload.get("exclusion_criteria").unwrap_or(&Value::Null))
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    plan.insert(
        "search_element_table".into(),
        payload
            .get("search_element_table")
            .cloned()
            .unwrap_or(Value::Array(Vec::new())),
    );
    let sections = normalize_named_items(
        payload.get("suggested_sections").unwrap_or(&Value::Null),
        "section_title",
    );
    plan.insert(
        "suggested_sections".into(),
        Value::Array(sections.into_iter().map(Value::Object).collect()),
    );
    plan.insert(
        "evidence_to_section_map".into(),
        payload
            .get("evidence_to_section_map")
            .cloned()
            .unwrap_or(Value::Array(Vec::new())),
    );
    plan.insert(
        "search_directions".into(),
        Value::Array(directions.iter().cloned().map(Value::Object).collect()),
    );
    plan.insert(
        "plan_risks".into(),
        payload.get("plan_risks").cloned().unwrap_or(Value::Array(Vec::new())),
    );

    let has_review_only = directions.iter().any(|d| {
        d.get("search_query")
            .and_then(Value::as_str)
            .map(|q| q.to_ascii_uppercase().contains("DT=(REVIEW)"))
            .unwrap_or(false)
    });
    if !has_review_only {
        return Err(AppError::BadInput(
            "规划结果中必须至少包含一个 DT=(Review) 的综述专项检索方向。".into(),
        ));
    }

    Ok(Value::Object(plan))
}

fn direction_count_instruction(direction_count: &str) -> String {
    let raw = direction_count.trim().to_ascii_lowercase();
    if raw.is_empty() || raw == "auto" || raw == "0" {
        return "由模型根据主题宽窄决定检索方向数量：窄主题 3-5 条，中等主题 6-8 条，宽主题 8-10 条；最终必须在 3 到 10 条之间。".into();
    }
    match raw.parse::<i32>() {
        Ok(n) => {
            let clamped = n.clamp(3, 10);
            format!("生成 {clamped} 条检索方向。")
        }
        Err(_) => "由模型根据主题宽窄决定检索方向数量，最终必须在 3 到 10 条之间。".into(),
    }
}

pub async fn build_review_plan(
    topic_text: &str,
    config: &LlmConfig,
    direction_count: &str,
    oa_only: bool,
) -> AppResult<Value> {
    let topic = topic_text.trim();
    if topic.is_empty() {
        return Err(AppError::BadInput("请输入研究主题。".into()));
    }
    if config.model.trim().is_empty() || config.api_key.trim().is_empty() {
        return Err(AppError::BadInput("请配置 model 和 api_key。".into()));
    }
    let count_instruction = direction_count_instruction(direction_count);
    let oa_block = if oa_only {
        format!("\nOA 限定：{OA_ONLY_PROMPT_HINT} 每个 direction 的 search_query 都必须包含 `OA=(\"All Open Access\")`。")
    } else {
        String::new()
    };
    let user_prompt = format!(
        "## 规划协议\n{PLANNER_PROTOCOL}\n\n## 当前任务\n研究主题：{topic}\n检索方向数量规则：{count_instruction}{oa_block}\n\n优先保持计划简洁实用，不要为了完整性堆叠过多层级。如果主题边界清晰，优先返回更少但更高质量的检索方向。\n\n请只返回符合协议的 JSON 对象，不要返回 Markdown、解释或代码块。",
    );
    let messages = vec![
        ChatMessage { role: "system".into(), content: PLANNER_SYSTEM_PROMPT.into() },
        ChatMessage { role: "user".into(), content: user_prompt },
    ];
    let cfg = LlmConfig { temperature: Some(0.25), ..config.clone() };
    let raw = chat_text(&cfg, &messages).await?;
    let parsed = load_json_object(&raw)?;
    let mut plan = normalize_review_plan(parsed)?;
    if oa_only {
        enforce_oa_on_plan(&mut plan);
    }
    Ok(plan)
}

/// Walk every `search_directions[*].search_query` and ensure it carries an
/// `OA=` constraint. Run after `normalize_review_plan` when `oa_only` is on.
fn enforce_oa_on_plan(plan: &mut Value) {
    let Some(directions) = plan.get_mut("search_directions").and_then(Value::as_array_mut) else {
        return;
    };
    for direction in directions {
        let Some(obj) = direction.as_object_mut() else { continue };
        let Some(query) = obj.get("search_query").and_then(Value::as_str) else { continue };
        let updated = append_oa_constraint(query);
        obj.insert("search_query".into(), Value::String(updated));
    }
}

// Silence unused-import warnings when only some helpers are reachable in tests.
#[allow(dead_code)]
fn _unused() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_balanced_quotes() {
        assert_eq!(strip_wrapping_quotes("\"hello\""), "hello");
        assert_eq!(strip_wrapping_quotes("'hi'"), "hi");
        assert_eq!(strip_wrapping_quotes("no quotes"), "no quotes");
    }

    #[test]
    fn list_split_on_semicolons() {
        let v = Value::String("a; b\nc".into());
        let got = as_string_list(&v);
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn append_oa_wraps_and_appends() {
        let got = append_oa_constraint("TS=(foo)");
        assert_eq!(got, "(TS=(foo)) AND OA=(\"All Open Access\")");
    }

    #[test]
    fn append_oa_is_idempotent() {
        let query = "TS=(foo) AND OA=(\"All Open Access\")";
        assert_eq!(append_oa_constraint(query), query);
    }

    #[test]
    fn append_oa_skips_empty() {
        assert_eq!(append_oa_constraint("   "), "");
    }

    #[test]
    fn append_oa_passes_wos_validator() {
        let appended = append_oa_constraint("TS=(machine learning) AND PY=(2020-2024)");
        let validated = validate_wos_search_query(&appended).expect("should validate");
        assert!(validated.contains("OA="));
    }

    #[test]
    fn enforce_oa_rewrites_every_direction() {
        let mut plan = json!({
            "search_directions": [
                { "search_query": "TS=(a) AND DT=(Review)" },
                { "search_query": "TS=(b)" },
                { "search_query": "TS=(c) AND OA=(\"All Open Access\")" },
            ]
        });
        enforce_oa_on_plan(&mut plan);
        let dirs = plan["search_directions"].as_array().unwrap();
        for d in dirs {
            assert!(d["search_query"].as_str().unwrap().to_ascii_uppercase().contains("OA="));
        }
        // idempotent on the third entry
        assert_eq!(
            dirs[2]["search_query"].as_str().unwrap(),
            "TS=(c) AND OA=(\"All Open Access\")"
        );
    }

    #[test]
    fn direction_count_auto() {
        let got = direction_count_instruction("auto");
        assert!(got.contains("3-5"));
        let got = direction_count_instruction("5");
        assert!(got.contains("5"));
        let got = direction_count_instruction("99");
        assert!(got.contains("10"));
    }

    #[test]
    fn normalize_plan_rejects_no_review_direction() {
        let payload = json!({
            "search_directions": [
                { "search_query": "TS=(machine learning)", "direction_name": "X" }
            ]
        });
        let err = normalize_review_plan(payload).unwrap_err();
        assert!(matches!(err, AppError::BadInput(_)));
    }

    #[test]
    fn normalize_plan_accepts_review_direction() {
        let payload = json!({
            "search_directions": [
                { "search_query": "TS=(machine learning) AND DT=(Review)", "direction_name": "R" }
            ]
        });
        let got = normalize_review_plan(payload).unwrap();
        let dirs = got.get("search_directions").unwrap().as_array().unwrap();
        assert_eq!(dirs.len(), 1);
    }
}
