//! Table read/write (CSV / XLSX / ZIP) for the fetch pipeline.
//!
//! Port target: `core/table_io.py`. Uses `calamine` for reading and
//! `rust_xlsxwriter` for writing. ZIP packaging via the `zip` crate.
//!
//! Note: per task #3 decision, we are NOT required to be byte-compatible with
//! the original Python (openpyxl) output. wos-review will be updated to
//! consume whatever schema we settle on here.

#![allow(dead_code)]

use std::path::Path;

#[derive(Debug)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

pub fn read_table(_path: &Path) -> crate::AppResult<Table> {
    // TODO(task-3): port read_table from core/table_io.py
    Err(crate::AppError::Other("table_io::read_table not yet implemented".into()))
}

pub fn write_xlsx(_path: &Path, _table: &Table) -> crate::AppResult<()> {
    // TODO(task-3): port write logic; choose final schema
    Err(crate::AppError::Other("table_io::write_xlsx not yet implemented".into()))
}
