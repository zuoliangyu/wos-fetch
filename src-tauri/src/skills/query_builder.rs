//! Build WoS Advanced Search query strings.
//!
//! Port target: `skills/query_builder.py`. Pure logic — combines user-supplied
//! topic, year range, document types, etc., into a valid WoS query expression.

#![allow(dead_code)]

#[derive(Debug, Default)]
pub struct QuerySpec {
    pub topic: String,
    pub year_from: Option<i32>,
    pub year_to: Option<i32>,
    pub document_types: Vec<String>,
}

pub fn build_wos_query(_spec: &QuerySpec) -> String {
    // TODO(task-4): port from skills/query_builder.py
    String::new()
}
