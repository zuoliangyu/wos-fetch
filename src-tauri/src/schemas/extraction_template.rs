//! Extraction template constants & normalization.
//!
//! Port of `schemas/extraction_template.py`. Defines the canonical list of
//! review-evidence export columns (used by `core::table_io::prepare_export_table`)
//! plus the LLM-emitted extraction template normalizer used by the evidence
//! extraction pipeline (downstream in wos-review).

use serde_json::{Map, Value};

use crate::{AppError, AppResult};

/// Canonical Chinese-language column names every exported row must carry.
pub const REVIEW_EVIDENCE_EXPORT_COLUMNS: &[&str] = &[
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
];

fn as_string_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => {
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                }
                _ => {
                    let text = v.to_string();
                    let trimmed = text.trim_matches('"').trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                }
            })
            .collect(),
        Value::String(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Vec::new()
                } else {
                    vec![trimmed.to_string()]
                }
            }
        Value::Null => Vec::new(),
        other => vec![other.to_string()],
    }
}

/// Reshape an LLM-supplied extraction template into the canonical structure
/// downstream code expects. Returns an `AppError::BadInput` on malformed input.
pub fn normalize_extraction_template(payload: &Value) -> AppResult<Value> {
    let payload = payload
        .as_object()
        .ok_or_else(|| AppError::BadInput("Extraction template JSON must be an object.".into()))?;

    let fields_value = payload.get("fields").cloned().unwrap_or(Value::Null);
    let fields_array = fields_value.as_array().cloned().unwrap_or_default();
    if fields_array.is_empty() {
        return Err(AppError::BadInput(
            "Extraction template must include a non-empty fields list.".into(),
        ));
    }

    let mut normalized_fields: Vec<Value> = Vec::new();
    for item in &fields_array {
        let Some(obj) = item.as_object() else { continue };
        let field_name = obj
            .get("field_name")
            .or_else(|| obj.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if field_name.is_empty() {
            continue;
        }
        let mut field_obj = Map::new();
        field_obj.insert("field_name".into(), Value::String(field_name.clone()));
        if let Some(desc) = obj.get("description").and_then(Value::as_str) {
            field_obj.insert("description".into(), Value::String(desc.trim().to_string()));
        }
        if let Some(notes_value) = obj.get("notes") {
            let notes = as_string_list(notes_value);
            field_obj.insert(
                "notes".into(),
                Value::Array(notes.into_iter().map(Value::String).collect()),
            );
        }
        normalized_fields.push(Value::Object(field_obj));
    }

    let mut output_schema = Map::new();
    for column in REVIEW_EVIDENCE_EXPORT_COLUMNS {
        output_schema.entry((*column).to_string()).or_insert(Value::String(String::new()));
    }
    for field in &normalized_fields {
        if let Some(name) = field.get("field_name").and_then(Value::as_str) {
            output_schema.entry(name.to_string()).or_insert(Value::String(String::new()));
        }
    }

    let template_name = payload
        .get("template_name")
        .and_then(Value::as_str)
        .unwrap_or("知识抽取标引模板")
        .trim()
        .to_string();
    let expert_role = payload
        .get("expert_role")
        .and_then(Value::as_str)
        .unwrap_or("你是该领域的文献综述证据抽取专家。")
        .trim()
        .to_string();
    let general_instruction = payload
        .get("general_instruction")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let mut root = Map::new();
    root.insert("template_name".into(), Value::String(template_name));
    root.insert("expert_role".into(), Value::String(expert_role));
    root.insert("general_instruction".into(), Value::String(general_instruction));
    root.insert("fields".into(), Value::Array(normalized_fields));
    root.insert("output_schema".into(), Value::Object(output_schema));
    Ok(Value::Object(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_rejects_non_object() {
        let value = json!([1, 2, 3]);
        assert!(normalize_extraction_template(&value).is_err());
    }

    #[test]
    fn normalize_rejects_empty_fields() {
        let value = json!({ "fields": [] });
        assert!(normalize_extraction_template(&value).is_err());
    }

    #[test]
    fn normalize_keeps_canonical_columns() {
        let value = json!({
            "fields": [{ "field_name": "证据强度" }]
        });
        let out = normalize_extraction_template(&value).unwrap();
        let schema = out.get("output_schema").unwrap().as_object().unwrap();
        assert!(schema.contains_key("证据角色"));
        assert!(schema.contains_key("证据强度"));
    }
}
