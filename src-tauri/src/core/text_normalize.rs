//! WoS Advanced Search query normalization & validation.
//!
//! Port of `core/text_normalize.py`. The filename is a historical misnomer:
//! this module is really the WoS query parser/validator. Public entry points:
//!
//! - `normalize_wos_query_text` — convert full-width punctuation, unify dashes
//! - `clean_search_query` — normalize + collapse whitespace + PY=... canonicalize
//! - `auto_fix_wos_search_query` — quote multi-word NEAR operands, expand group NEAR group
//! - `validate_wos_search_query` — full grammar validation; returns the canonical form

use chrono::Datelike;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

use crate::{AppError, AppResult};

static UNICODE_DASHES_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new("[\u{2010}\u{2011}\u{2012}\u{2013}\u{2014}\u{2015}\u{2212}\u{FF0D}]").unwrap()
});
static BOOLEAN_AT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s+(?:AND|OR|NOT)\b").unwrap());
static YEAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d{4}").unwrap());
static YEAR_RANGE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?P<start>\d{4})\s*-\s*(?P<end>\d{4})|(?P<year>\d{4})").unwrap());
static PY_PREFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bPY\s*=\s*").unwrap());
static FIELD_PREFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9]*\s*=").unwrap());
static NEAR_OP_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?i)(NEAR|SAME)\s*/\s*\d+").unwrap());
static WORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_\-]*").unwrap());
static WS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());
static MULTIWORD_NEAR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"\b(NEAR|SAME)\s*/\s*(\d+)\s+([A-Za-z][A-Za-z0-9_\-]*(?:\s+[A-Za-z][A-Za-z0-9_\-]*)+)",
    )
    .unwrap()
});
static GROUP_NEAR_GROUP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)(?P<left>\((?:[^()"]|"[^"]*")+\))\s*(?P<op>NEAR|SAME)\s*/\s*(?P<n>\d+)\s*(?P<right>\((?:[^()"]|"[^"]*")+\))"#).unwrap()
});
static RELATIVE_EN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:last|past|recent|latest)\s+(\d{1,2})\s+years?\b").unwrap());
static RELATIVE_ZH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:近|最近|过去|近来|近年)\s*(\d{1,2})\s*年").unwrap());
static LEADING_YEAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d{4}").unwrap());

fn wos_field_tags() -> &'static HashSet<&'static str> {
    static TAGS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
        [
            "AB", "AD", "AI", "AK", "ALL", "AU", "CF", "CI", "CU", "DO", "DOP", "DT", "ED", "FD",
            "FG", "FO", "FPY", "FT", "GP", "IS", "KP", "OA", "OG", "OO", "PMID", "PS", "PUBL",
            "PY", "SA", "SDG", "SG", "SO", "SU", "TI", "TMAC", "TMIC", "TMSO", "TS", "UT", "WC",
            "ZP",
        ]
        .into_iter()
        .collect()
    });
    &TAGS
}

fn text_expression_fields() -> &'static HashSet<&'static str> {
    static FIELDS: Lazy<HashSet<&'static str>> =
        Lazy::new(|| ["TS", "TI", "AB", "AK", "KP", "FT"].into_iter().collect());
    &FIELDS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Field,
    Op,
    Term,
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
pub struct QueryToken {
    pub kind: TokenKind,
    pub value: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct QueryNode {
    has_and: bool,
    has_or: bool,
}

fn merge_nodes(left: QueryNode, right: QueryNode, op: &str) -> QueryNode {
    QueryNode {
        has_and: left.has_and || right.has_and || op == "AND" || op == "SAME",
        has_or: left.has_or || right.has_or || op == "OR",
    }
}

/// Translate full-width punctuation to ASCII, unify various Unicode dashes
/// to a hyphen.
pub fn normalize_wos_query_text(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let translated: String = trimmed
        .chars()
        .map(|c| match c {
            '\u{FF08}' => '(',
            '\u{FF09}' => ')',
            '\u{FF3B}' => '[',
            '\u{FF3D}' => ']',
            '\u{FF5B}' => '{',
            '\u{FF5D}' => '}',
            '\u{FF0C}' => ',',
            '\u{FF1B}' => ';',
            '\u{FF1A}' => ':',
            '\u{FF1D}' => '=',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2018}' | '\u{2019}' => '\'',
            other => other,
        })
        .collect();
    UNICODE_DASHES_RE.replace_all(&translated, "-").to_string()
}

pub fn clean_search_query(value: &str) -> String {
    let mut query = normalize_wos_query_text(value);
    query = normalize_wos_publication_year_filters(&query).unwrap_or(query);
    WS_RE.replace_all(query.trim(), " ").to_string()
}

pub fn auto_fix_wos_search_query(value: &str) -> String {
    let cleaned = clean_search_query(value);
    if cleaned.is_empty() {
        return cleaned;
    }
    let quoted = auto_quote_multiword_near_operands(&cleaned);
    let expanded = auto_expand_group_near_group(&quoted);
    clean_search_query(&expanded)
}

/// Full validation pipeline. Returns the canonical query string on success,
/// or `Err(AppError::BadInput(message))` with a Chinese diagnostic message.
pub fn validate_wos_search_query(value: &str) -> AppResult<String> {
    let query = auto_fix_wos_search_query(value);
    if query.is_empty() {
        return Err(AppError::BadInput("检索式为空。".into()));
    }
    validate_balanced_parentheses(&query)?;
    validate_balanced_quotes(&query)?;
    let tokens = tokenize_wos_query(&query)?;
    if tokens.is_empty() {
        return Err(AppError::BadInput("检索式为空。".into()));
    }
    validate_token_sequence(&tokens)?;
    validate_field_usage(&tokens)?;
    let mut parser = Parser::new(&tokens);
    parser.parse_expression(None)?;
    if parser.position != tokens.len() {
        let token = &tokens[parser.position];
        return Err(AppError::BadInput(format!(
            "检索式中位置 {} 附近存在未解析内容。",
            token.start + 1
        )));
    }
    Ok(query)
}

fn validate_balanced_parentheses(query: &str) -> AppResult<()> {
    let mut depth: i32 = 0;
    for ch in query.chars() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth < 0 {
                return Err(AppError::BadInput(
                    "检索式括号不配平：存在多余的右括号。".into(),
                ));
            }
        }
    }
    if depth != 0 {
        return Err(AppError::BadInput("检索式括号不配平：缺少右括号。".into()));
    }
    Ok(())
}

fn validate_balanced_quotes(query: &str) -> AppResult<()> {
    let mut in_quote = false;
    let mut quote_char: char = '\0';
    for ch in query.chars() {
        if ch == '"' || ch == '\'' {
            if in_quote && ch == quote_char {
                in_quote = false;
                quote_char = '\0';
            } else if !in_quote {
                in_quote = true;
                quote_char = ch;
            }
        }
    }
    if in_quote {
        return Err(AppError::BadInput("检索式引号不配平。".into()));
    }
    Ok(())
}

fn tokenize_wos_query(query: &str) -> AppResult<Vec<QueryToken>> {
    let chars: Vec<char> = query.chars().collect();
    let mut tokens: Vec<QueryToken> = Vec::new();
    let mut idx = 0usize;
    while idx < chars.len() {
        let ch = chars[idx];
        if ch.is_whitespace() {
            idx += 1;
            continue;
        }
        if ch == '(' || ch == ')' {
            tokens.push(QueryToken {
                kind: if ch == '(' {
                    TokenKind::LParen
                } else {
                    TokenKind::RParen
                },
                value: ch.to_string(),
                start: idx,
                end: idx + 1,
            });
            idx += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            let mut end = idx + 1;
            while end < chars.len() && chars[end] != ch {
                end += 1;
            }
            if end >= chars.len() {
                return Err(AppError::BadInput("检索式引号不配平。".into()));
            }
            end += 1;
            tokens.push(QueryToken {
                kind: TokenKind::Term,
                value: chars[idx..end].iter().collect(),
                start: idx,
                end,
            });
            idx = end;
            continue;
        }
        if ch.is_alphabetic() {
            let rest: String = chars[idx..].iter().collect();
            if let Some(m) = FIELD_PREFIX_RE.find(&rest) {
                let raw = m.as_str();
                let field = raw
                    .split_once('=')
                    .map(|(a, _)| a.trim().to_uppercase())
                    .unwrap_or_default();
                let len = raw.chars().count();
                tokens.push(QueryToken {
                    kind: TokenKind::Field,
                    value: field,
                    start: idx,
                    end: idx + len,
                });
                idx += len;
                continue;
            }
            if let Some(m) = NEAR_OP_RE.find(&rest) {
                let raw = m.as_str();
                let op = raw
                    .chars()
                    .take_while(|c| c.is_alphabetic())
                    .collect::<String>()
                    .to_uppercase();
                let len = raw.chars().count();
                tokens.push(QueryToken {
                    kind: TokenKind::Op,
                    value: op,
                    start: idx,
                    end: idx + len,
                });
                idx += len;
                continue;
            }
            if let Some(m) = WORD_RE.find(&rest) {
                let word = m.as_str().to_string();
                let upper = word.to_uppercase();
                let len = word.chars().count();
                let (kind, value) = if matches!(upper.as_str(), "AND" | "OR" | "NOT") {
                    (TokenKind::Op, upper)
                } else {
                    (TokenKind::Term, word)
                };
                tokens.push(QueryToken {
                    kind,
                    value,
                    start: idx,
                    end: idx + len,
                });
                idx += len;
                continue;
            }
        }
        // Fallback: consume non-space, non-paren run as a term.
        let mut end = idx + 1;
        while end < chars.len()
            && !chars[end].is_whitespace()
            && chars[end] != '('
            && chars[end] != ')'
        {
            end += 1;
        }
        tokens.push(QueryToken {
            kind: TokenKind::Term,
            value: chars[idx..end].iter().collect(),
            start: idx,
            end,
        });
        idx = end;
    }
    Ok(tokens)
}

fn validate_token_sequence(tokens: &[QueryToken]) -> AppResult<()> {
    let explicit_binary = ["AND", "OR", "SAME"];
    for (index, token) in tokens.iter().enumerate() {
        let prev = if index > 0 {
            Some(&tokens[index - 1])
        } else {
            None
        };
        let next = tokens.get(index + 1);
        match token.kind {
            TokenKind::Op => {
                if explicit_binary.contains(&token.value.as_str()) {
                    if prev.is_none()
                        || matches!(prev.unwrap().kind, TokenKind::Op | TokenKind::LParen)
                    {
                        return Err(AppError::BadInput(format!(
                            "检索式中 {} 左侧缺少有效表达式。",
                            token.value
                        )));
                    }
                    if next.is_none()
                        || matches!(next.unwrap().kind, TokenKind::Op | TokenKind::RParen)
                    {
                        return Err(AppError::BadInput(format!(
                            "检索式中 {} 右侧缺少有效表达式。",
                            token.value
                        )));
                    }
                } else if token.value == "NOT" {
                    if prev.is_none()
                        || matches!(prev.unwrap().kind, TokenKind::Op | TokenKind::LParen)
                    {
                        return Err(AppError::BadInput(
                            "检索式中 NOT 左侧缺少有效表达式。".into(),
                        ));
                    }
                    if next.is_none()
                        || matches!(next.unwrap().kind, TokenKind::Op | TokenKind::RParen)
                    {
                        return Err(AppError::BadInput(
                            "检索式中 NOT 右侧缺少有效表达式。".into(),
                        ));
                    }
                }
            }
            TokenKind::Field => {
                if next.map(|t| {
                    matches!(
                        t.kind,
                        TokenKind::LParen | TokenKind::Term | TokenKind::Field
                    )
                }) != Some(true)
                {
                    return Err(AppError::BadInput(format!(
                        "字段 {}= 后缺少检索内容。",
                        token.value
                    )));
                }
            }
            TokenKind::LParen => {
                if matches!(next.map(|t| t.kind), Some(TokenKind::RParen)) {
                    return Err(AppError::BadInput("检索式中存在空括号。".into()));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_field_usage(tokens: &[QueryToken]) -> AppResult<()> {
    let tags = wos_field_tags();
    for token in tokens {
        if token.kind == TokenKind::Field && !tags.contains(token.value.as_str()) {
            return Err(AppError::BadInput(format!(
                "检索式中存在未知字段标签：{}=",
                token.value
            )));
        }
    }
    Ok(())
}

struct Parser<'a> {
    tokens: &'a [QueryToken],
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [QueryToken]) -> Self {
        Self {
            tokens,
            position: 0,
        }
    }

    fn peek(&self) -> Option<&QueryToken> {
        self.tokens.get(self.position)
    }

    fn parse_expression(&mut self, stop: Option<TokenKind>) -> AppResult<QueryNode> {
        self.parse_or(stop)
    }

    fn parse_or(&mut self, stop: Option<TokenKind>) -> AppResult<QueryNode> {
        let mut node = self.parse_and(stop)?;
        while let Some(token) = self.peek() {
            if stop.is_some() && Some(token.kind) == stop {
                break;
            }
            if token.kind != TokenKind::Op || token.value != "OR" {
                break;
            }
            self.position += 1;
            let right = self.parse_and(stop)?;
            node = merge_nodes(node, right, "OR");
        }
        Ok(node)
    }

    fn parse_and(&mut self, stop: Option<TokenKind>) -> AppResult<QueryNode> {
        let mut node = self.parse_near(stop)?;
        while let Some(token) = self.peek() {
            if stop.is_some() && Some(token.kind) == stop {
                break;
            }
            if token.kind != TokenKind::Op || token.value != "AND" {
                break;
            }
            self.position += 1;
            let right = self.parse_near(stop)?;
            node = merge_nodes(node, right, "AND");
        }
        Ok(node)
    }

    fn parse_near(&mut self, stop: Option<TokenKind>) -> AppResult<QueryNode> {
        let mut node = self.parse_term(stop)?;
        while let Some(token) = self.peek() {
            if stop.is_some() && Some(token.kind) == stop {
                break;
            }
            if token.kind != TokenKind::Op || !matches!(token.value.as_str(), "SAME" | "NEAR") {
                break;
            }
            let op = token.value.clone();
            self.position += 1;
            let right = self.parse_term(stop)?;
            if node.has_and || right.has_and {
                return Err(AppError::BadInput(
                    "检索式中 NEAR/SAME 不能直接连接会形成隐式 AND 的复杂表达式。 请改写为逐对邻近，或改用普通 AND。".into(),
                ));
            }
            node = merge_nodes(node, right, &op);
        }
        Ok(node)
    }

    fn parse_term(&mut self, stop: Option<TokenKind>) -> AppResult<QueryNode> {
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| AppError::BadInput("检索式不完整。".into()))?;
        if token.kind == TokenKind::Op && token.value == "NOT" {
            self.position += 1;
            return self.parse_term(stop);
        }
        self.parse_factor(stop)
    }

    fn parse_factor(&mut self, _stop: Option<TokenKind>) -> AppResult<QueryNode> {
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| AppError::BadInput("检索式不完整。".into()))?;
        match token.kind {
            TokenKind::Field => {
                self.position += 1;
                self.parse_field_value(&token.value)
            }
            TokenKind::LParen => {
                self.position += 1;
                let node = self.parse_expression(Some(TokenKind::RParen))?;
                if !matches!(self.peek().map(|t| t.kind), Some(TokenKind::RParen)) {
                    return Err(AppError::BadInput("检索式括号不配平：缺少右括号。".into()));
                }
                self.position += 1;
                Ok(node)
            }
            TokenKind::Term => {
                self.position += 1;
                Ok(QueryNode::default())
            }
            _ => Err(AppError::BadInput(format!(
                "检索式中位置 {} 附近存在非法结构。",
                token.start + 1
            ))),
        }
    }

    fn parse_field_value(&mut self, field: &str) -> AppResult<QueryNode> {
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| AppError::BadInput(format!("字段 {}= 后缺少检索内容。", field)))?;
        let text_fields = text_expression_fields();
        let atomic = !text_fields.contains(field) && wos_field_tags().contains(field);

        if atomic && token.kind == TokenKind::LParen {
            self.position += 1;
            let mut depth: i32 = 1;
            let mut has_content = false;
            while self.position < self.tokens.len() && depth > 0 {
                let current = &self.tokens[self.position];
                match current.kind {
                    TokenKind::LParen => depth += 1,
                    TokenKind::RParen => {
                        depth -= 1;
                        if depth == 0 {
                            self.position += 1;
                            break;
                        }
                    }
                    _ => has_content = true,
                }
                self.position += 1;
            }
            if depth != 0 {
                return Err(AppError::BadInput("检索式括号不配平：缺少右括号。".into()));
            }
            if !has_content {
                return Err(AppError::BadInput("检索式中存在空括号。".into()));
            }
            return Ok(QueryNode::default());
        }

        if text_fields.contains(field) && token.kind == TokenKind::LParen {
            let start = self.position;
            let end = find_closing_paren_token(self.tokens, start)
                .ok_or_else(|| AppError::BadInput("检索式括号不配平：缺少右括号。".into()))?;
            let inner = tokens_with_implicit_and(&self.tokens[start + 1..end])?;
            self.position = end + 1;
            let mut inner_parser = Parser::new(&inner);
            return inner_parser.parse_expression(None);
        }

        if token.kind == TokenKind::LParen {
            self.position += 1;
            let node = self.parse_expression(Some(TokenKind::RParen))?;
            if !matches!(self.peek().map(|t| t.kind), Some(TokenKind::RParen)) {
                return Err(AppError::BadInput("检索式括号不配平：缺少右括号。".into()));
            }
            self.position += 1;
            return Ok(node);
        }

        self.parse_factor(None)
    }
}

fn find_closing_paren_token(tokens: &[QueryToken], start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn tokens_with_implicit_and(tokens: &[QueryToken]) -> AppResult<Vec<QueryToken>> {
    if tokens.is_empty() {
        return Err(AppError::BadInput("检索式中存在空括号。".into()));
    }
    let mut result: Vec<QueryToken> = Vec::with_capacity(tokens.len() * 2);
    let mut prev_term_like = false;
    for token in tokens {
        let current_term_like = matches!(
            token.kind,
            TokenKind::Term | TokenKind::Field | TokenKind::LParen
        );
        if prev_term_like && current_term_like {
            result.push(QueryToken {
                kind: TokenKind::Op,
                value: "AND".into(),
                start: token.start,
                end: token.start,
            });
        }
        result.push(token.clone());
        prev_term_like = matches!(token.kind, TokenKind::Term | TokenKind::RParen);
    }
    Ok(result)
}

fn auto_quote_multiword_near_operands(query: &str) -> String {
    let mut current = query.to_string();
    loop {
        let next = MULTIWORD_NEAR_RE
            .replace_all(&current, |caps: &regex::Captures| {
                format!(
                    "{}/{} \"{}\"",
                    caps.get(1).map(|m| m.as_str()).unwrap_or(""),
                    caps.get(2).map(|m| m.as_str()).unwrap_or(""),
                    caps.get(3).map(|m| m.as_str()).unwrap_or(""),
                )
            })
            .to_string();
        if next == current {
            return next;
        }
        current = next;
    }
}

fn auto_expand_group_near_group(query: &str) -> String {
    let mut current = query.to_string();
    loop {
        let next = GROUP_NEAR_GROUP_RE
            .replace_all(&current, |caps: &regex::Captures| {
                expand_group_near_group(caps)
            })
            .to_string();
        if next == current {
            return next;
        }
        current = next;
    }
}

fn expand_group_near_group(caps: &regex::Captures) -> String {
    let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string();
    let left_group = caps.name("left").map(|m| m.as_str()).unwrap_or("");
    let right_group = caps.name("right").map(|m| m.as_str()).unwrap_or("");
    let op = caps
        .name("op")
        .map(|m| m.as_str())
        .unwrap_or("")
        .to_uppercase();
    let distance = caps.name("n").map(|m| m.as_str()).unwrap_or("");
    let left_inner = strip_outer_parens(left_group);
    let right_inner = strip_outer_parens(right_group);
    let left_terms = split_top_level_or_terms(left_inner);
    let right_terms = split_top_level_or_terms(right_inner);
    if left_terms.len() <= 1 || right_terms.len() <= 1 {
        return whole;
    }
    let mut pairs: Vec<String> = Vec::with_capacity(left_terms.len() * right_terms.len());
    for left in &left_terms {
        let fixed_left = quote_if_multiword(left);
        for right in &right_terms {
            let fixed_right = quote_if_multiword(right);
            pairs.push(format!("({fixed_left} {op}/{distance} {fixed_right})"));
        }
    }
    format!("({})", pairs.join(" OR "))
}

fn strip_outer_parens(text: &str) -> &str {
    if text.len() >= 2 && text.starts_with('(') && text.ends_with(')') {
        &text[1..text.len() - 1]
    } else {
        text
    }
}

fn split_top_level_or_terms(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut current: Vec<char> = Vec::new();
    let mut depth: i32 = 0;
    let mut in_quote = false;
    let mut quote_char = '\0';
    let mut idx = 0usize;
    while idx < chars.len() {
        let ch = chars[idx];
        if ch == '"' || ch == '\'' {
            if in_quote && ch == quote_char {
                in_quote = false;
                quote_char = '\0';
            } else if !in_quote {
                in_quote = true;
                quote_char = ch;
            }
            current.push(ch);
            idx += 1;
            continue;
        }
        if !in_quote {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth = depth.max(1) - 1;
            } else if depth == 0
                && idx + 2 <= chars.len()
                && chars[idx..idx + 2]
                    .iter()
                    .collect::<String>()
                    .eq_ignore_ascii_case("OR")
            {
                let before_ok = idx == 0 || chars[idx - 1].is_whitespace();
                let after_idx = idx + 2;
                let after_ok = after_idx >= chars.len() || chars[after_idx].is_whitespace();
                if before_ok && after_ok {
                    let part: String = current.iter().collect::<String>().trim().to_string();
                    if !part.is_empty() {
                        parts.push(part);
                    }
                    current.clear();
                    idx = after_idx;
                    continue;
                }
            }
        }
        current.push(ch);
        idx += 1;
    }
    let tail: String = current.iter().collect::<String>().trim().to_string();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

fn quote_if_multiword(term: &str) -> String {
    let text = term.trim();
    if text.is_empty() || text.starts_with('"') || text.starts_with('\'') {
        return text.to_string();
    }
    if Regex::new(r"\s").unwrap().is_match(text) {
        return format!("\"{text}\"");
    }
    text.to_string()
}

/// Canonicalize PY=... blocks into `PY=(YYYY)` or `PY=(YYYY-YYYY)` form.
pub fn normalize_wos_publication_year_filters(query: &str) -> AppResult<String> {
    if query.is_empty() {
        return Ok(String::new());
    }
    let chars: Vec<char> = query.chars().collect();
    let len = chars.len();
    let mut result = String::new();
    let mut cursor = 0usize;
    let query_string: String = chars.iter().collect();
    for m in PY_PREFIX_RE.find_iter(&query_string) {
        // Convert byte offsets to char offsets.
        let start_char = byte_to_char_index(&query_string, m.start());
        let end_char = byte_to_char_index(&query_string, m.end());
        let pre: String = chars[cursor..start_char].iter().collect();
        result.push_str(&pre);
        let mut local_cursor = end_char;
        while local_cursor < len && chars[local_cursor].is_whitespace() {
            local_cursor += 1;
        }
        let raw_years: String;
        if local_cursor < len && chars[local_cursor] == '(' {
            let end = find_matching_parenthesis(&chars, local_cursor).ok_or_else(|| {
                AppError::BadInput("Invalid PY year format: missing closing parenthesis.".into())
            })?;
            raw_years = chars[local_cursor + 1..end].iter().collect();
            cursor = end + 1;
        } else {
            let end = find_bare_py_value_end(&chars, local_cursor);
            raw_years = chars[local_cursor..end].iter().collect();
            cursor = end;
        }
        let normalized = normalize_py_year_value(&raw_years)?;
        result.push_str(&format!("PY=({normalized})"));
    }
    let tail: String = chars[cursor..].iter().collect();
    result.push_str(&tail);
    Ok(result)
}

fn byte_to_char_index(s: &str, byte_idx: usize) -> usize {
    s.char_indices().take_while(|(b, _)| *b < byte_idx).count()
}

fn find_matching_parenthesis(chars: &[char], start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    for (index, &ch) in chars.iter().enumerate().skip(start) {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_bare_py_value_end(chars: &[char], start: usize) -> usize {
    let mut index = start;
    let s: String = chars.iter().collect();
    let mut char_to_byte: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    let mut byte_acc = 0usize;
    for &ch in chars {
        char_to_byte.push(byte_acc);
        byte_acc += ch.len_utf8();
    }
    char_to_byte.push(byte_acc);
    while index < chars.len() {
        if chars[index] == ')' {
            break;
        }
        let byte_idx = char_to_byte[index];
        if let Some(m) = BOOLEAN_AT_RE.find_at(&s, byte_idx) {
            if m.start() == byte_idx {
                let next_byte = m.end();
                let mut next_char_idx = byte_to_char_index(&s, next_byte);
                while next_char_idx < chars.len() && chars[next_char_idx].is_whitespace() {
                    next_char_idx += 1;
                }
                let tail: String = chars[next_char_idx..].iter().collect();
                if !LEADING_YEAR_RE.is_match(&tail) {
                    break;
                }
            }
        }
        index += 1;
    }
    index
}

pub fn normalize_py_year_value(value: &str) -> AppResult<String> {
    let translated = normalize_wos_query_text(value);
    let stripped = translated
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'')
        .to_string();
    let collapsed = WS_RE.replace_all(&stripped, " ").trim().to_string();
    if collapsed.is_empty() {
        return Err(AppError::BadInput(
            "Invalid PY year format: publication year cannot be empty.".into(),
        ));
    }
    if let Some(rel) = normalize_relative_year_value(&collapsed) {
        return Ok(rel);
    }
    if !YEAR_RE.is_match(&collapsed) {
        return Err(AppError::BadInput("Invalid PY year format.".into()));
    }
    let mut tokens: Vec<(String, i32, i32)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for caps in YEAR_RANGE_RE.captures_iter(&collapsed) {
        let (token, start_year, end_year) = if let Some(y) = caps.name("year") {
            let year: i32 = y
                .as_str()
                .parse()
                .map_err(|_| AppError::BadInput("Invalid PY year format.".into()))?;
            (format!("{:04}", year), year, year)
        } else {
            let start: i32 = caps
                .name("start")
                .unwrap()
                .as_str()
                .parse()
                .map_err(|_| AppError::BadInput("Invalid PY year format.".into()))?;
            let end: i32 = caps
                .name("end")
                .unwrap()
                .as_str()
                .parse()
                .map_err(|_| AppError::BadInput("Invalid PY year format.".into()))?;
            let (s, e) = if start > end {
                (end, start)
            } else {
                (start, end)
            };
            (format!("{:04}-{:04}", s, e), s, e)
        };
        if seen.insert(token.clone()) {
            tokens.push((token, start_year, end_year));
        }
    }
    if tokens.is_empty() {
        return Err(AppError::BadInput("Invalid PY year format.".into()));
    }
    if tokens.iter().all(|(_, s, e)| s == e) {
        let mut years: Vec<i32> = tokens.iter().map(|(_, s, _)| *s).collect();
        years.sort_unstable();
        years.dedup();
        if years.len() == 1 {
            return Ok(format!("{:04}", years[0]));
        }
        let contiguous: Vec<i32> = (years[0]..=*years.last().unwrap()).collect();
        if years == contiguous {
            return Ok(format!("{:04}-{:04}", years[0], years.last().unwrap()));
        }
        return Ok(years
            .iter()
            .map(|y| format!("{:04}", y))
            .collect::<Vec<_>>()
            .join(" OR "));
    }
    Ok(tokens
        .iter()
        .map(|(t, _, _)| t.clone())
        .collect::<Vec<_>>()
        .join(" OR "))
}

pub fn normalize_relative_year_value(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let count: i32 = if let Some(caps) = RELATIVE_EN_RE.captures(&lower) {
        caps.get(1)?.as_str().parse().ok()?
    } else if let Some(caps) = RELATIVE_ZH_RE.captures(value) {
        caps.get(1)?.as_str().parse().ok()?
    } else {
        return None;
    };
    let count = count.clamp(1, 50);
    let current_year = chrono::Local::now().year();
    Some(format!(
        "{:04}-{:04}",
        current_year - count + 1,
        current_year
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_full_width_punctuation() {
        let got = normalize_wos_query_text("TS=（machine learning）");
        assert_eq!(got, "TS=(machine learning)");
    }

    #[test]
    fn validates_simple_query() {
        let got = validate_wos_search_query("TS=(machine learning) AND PY=2020").unwrap();
        assert!(got.contains("TS=(machine learning)"));
        assert!(got.contains("PY=(2020)"));
    }

    #[test]
    fn rejects_unbalanced_parens() {
        let err = validate_wos_search_query("TS=(machine learning").unwrap_err();
        assert!(matches!(err, AppError::BadInput(_)));
    }

    #[test]
    fn auto_quotes_multiword_near() {
        let got = auto_fix_wos_search_query("solar NEAR/3 photovoltaic cells");
        assert!(got.contains("\"photovoltaic cells\""));
    }
}
