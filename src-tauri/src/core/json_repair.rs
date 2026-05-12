//! Tolerant JSON parser for malformed LLM output.
//!
//! Port of `core/json_repair.py`. Applies heuristics in order:
//!   1. Strip markdown code fences (```json ... ```)
//!   2. Find the first { or [ if the model added prose before it
//!   3. Normalize smart quotes / curly quotes to plain ASCII
//!   4. Drop trailing commas before } or ]
//!   5. Escape stray \n \r \t inside string literals
//!   6. Escape stray inner double-quotes that aren't terminators
//!   7. Close any unbalanced brackets / unterminated strings
//!   8. As a last resort, walk back to the last position that parses cleanly

use once_cell::sync::Lazy;
use regex::Regex;

static FENCE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)\A```(?:json)?\s*(.*?)\s*```\z").unwrap());
static TRAILING_COMMA_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r",\s*([}\]])").unwrap());

/// Pull a likely-JSON substring out of an LLM reply. Strips code fences and,
/// failing that, returns the slice from the first opening bracket to the last
/// matching closing bracket.
pub fn extract_json_candidate(text: &str) -> String {
    let mut candidate = text.trim().trim_start_matches('\u{FEFF}').to_string();

    if let Some(caps) = FENCE_RE.captures(&candidate) {
        candidate = caps.get(1).unwrap().as_str().trim().to_string();
    }

    if candidate.starts_with('{') || candidate.starts_with('[') {
        return candidate;
    }

    let object_start = candidate.find('{');
    let array_start = candidate.find('[');
    let starts: Vec<usize> = [object_start, array_start].into_iter().flatten().collect();
    let start = match starts.into_iter().min() {
        Some(s) => s,
        None => return candidate,
    };

    let opening = candidate.as_bytes()[start] as char;
    let closing = if opening == '{' { '}' } else { ']' };
    if let Some(end) = candidate.rfind(closing) {
        if end > start {
            return candidate[start..=end].trim().to_string();
        }
    }
    candidate[start..].to_string()
}

/// Escape literal newlines / carriage returns / tabs that appear *inside*
/// quoted strings (where the model forgot to escape them).
fn fix_unescaped_chars_in_strings(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escape_next = false;
    for &ch in &chars {
        if escape_next {
            out.push(ch);
            escape_next = false;
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            out.push(ch);
            continue;
        }
        if in_string {
            match ch {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Escape inner `"` characters that aren't followed by a structural separator
/// (i.e. they aren't really terminating the string).
fn escape_inner_quotes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escape_next = false;
    let mut i = 0usize;
    while i < len {
        let ch = chars[i];
        if escape_next {
            out.push(ch);
            escape_next = false;
            i += 1;
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            escape_next = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            if !in_string {
                in_string = true;
                out.push(ch);
                i += 1;
                continue;
            }
            // Look ahead past whitespace.
            let mut j = i + 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
            if j < len && !matches!(chars[j], ',' | '}' | ']' | ':') {
                out.push_str("\\\"");
                i += 1;
                continue;
            }
            in_string = false;
            out.push(ch);
            i += 1;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Close unbalanced brackets / quotes at end-of-text by appending the missing
/// closers in reverse order.
fn balance_truncated_json(candidate: &str) -> String {
    let text = candidate.trim_end();
    if text.is_empty() {
        return String::new();
    }
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;
    for ch in text.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.last() == Some(&ch) => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = text.to_string();
    if in_string {
        out.push('"');
    }
    for &close in stack.iter().rev() {
        out.push(close);
    }
    out
}

/// Structurally-sensible byte offsets where we could trim and still produce
/// valid JSON. Mirrors the cap-at-64 behavior of the Python version.
fn candidate_trim_positions(text: &str) -> Vec<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut positions: Vec<usize> = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;
    let mut depth: i32 = 0;
    for (i, &ch) in chars.iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            if !in_string && depth <= 1 {
                positions.push(i + 1);
            }
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth <= 0 {
                    positions.push(i + 1);
                }
            }
            ',' if depth == 1 => positions.push(i + 1),
            _ => {}
        }
    }
    positions.sort_unstable();
    positions.dedup();
    positions.reverse();
    positions.truncate(64);
    positions
}

/// As a last-resort fallback: walk back through the structurally-safe trim
/// positions, attempting to balance + reparse each prefix; keep the first
/// non-empty parse result.
fn trim_to_last_complete_pair(candidate: &str) -> String {
    let text = candidate.trim();
    if text.is_empty() {
        return String::new();
    }
    if serde_json::from_str::<serde_json::Value>(text).is_ok() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut best: Option<String> = None;
    for end in candidate_trim_positions(text) {
        let chunk_chars = &chars[..end];
        let chunk_str: String = chunk_chars.iter().collect();
        let trimmed = chunk_str.trim_end().trim_end_matches(',').to_string();
        if trimmed.is_empty() {
            continue;
        }
        let no_trailing = TRAILING_COMMA_RE.replace_all(&trimmed, "$1").to_string();
        let repaired = balance_truncated_json(&no_trailing);
        match serde_json::from_str::<serde_json::Value>(&repaired) {
            Ok(serde_json::Value::Object(map)) if !map.is_empty() => return repaired,
            Ok(serde_json::Value::Array(arr)) if !arr.is_empty() => return repaired,
            Ok(_) => {
                if best.is_none() {
                    best = Some(repaired);
                }
            }
            Err(_) => continue,
        }
    }
    best.unwrap_or_else(|| text.to_string())
}

/// Best-effort repair pipeline. Returns the repaired JSON *text* (caller is
/// expected to feed it back into `serde_json::from_str` for the actual parse).
pub fn repair_json_text(text: &str) -> String {
    let mut candidate = extract_json_candidate(text);
    candidate = candidate
        .replace(['\u{201C}', '\u{201D}'], "\"")
        .replace(['\u{2018}', '\u{2019}'], "'");
    candidate = TRAILING_COMMA_RE.replace_all(&candidate, "$1").to_string();
    candidate = fix_unescaped_chars_in_strings(&candidate);
    candidate = escape_inner_quotes(&candidate);
    candidate = balance_truncated_json(&candidate);
    candidate = trim_to_last_complete_pair(&candidate);
    candidate.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_code_fence() {
        let raw = "```json\n{\"a\": 1}\n```";
        assert_eq!(extract_json_candidate(raw), "{\"a\": 1}");
    }

    #[test]
    fn repairs_trailing_comma() {
        let raw = "{\"a\": 1,}";
        let repaired = repair_json_text(raw);
        let value: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(value["a"], 1);
    }

    #[test]
    fn closes_truncated_object() {
        let raw = "{\"a\": 1, \"b\": [1, 2";
        let repaired = repair_json_text(raw);
        let value: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(value["a"], 1);
    }

    #[test]
    fn handles_smart_quotes() {
        let raw = "{\u{201C}a\u{201D}: 1}";
        let repaired = repair_json_text(raw);
        let value: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(value["a"], 1);
    }
}
