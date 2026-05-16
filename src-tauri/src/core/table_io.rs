//! Table read / write (XLSX, XLS, CSV, ZIP) and result packaging.
//!
//! Port of `core/table_io.py`. The Rust version drops pandas in favor of a
//! lightweight `Table` abstraction (column order + per-row HashMap), and uses
//! `calamine` for spreadsheet reads, `rust_xlsxwriter` for writes, the `csv`
//! crate for CSV, `encoding_rs` for non-UTF-8 CSV fallback, and `zip` for
//! packaging.
//!
//! Output format note: per task #3 decision, we are NOT byte-compatible with
//! the Python openpyxl output. wos-review will be updated to consume whatever
//! schema this module emits.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};

use calamine::{open_workbook_from_rs, Data, Reader, Xls, Xlsx};
use once_cell::sync::Lazy;
use regex::Regex;
use rust_xlsxwriter::Workbook;
use serde_json::{Map, Value};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::schemas::extraction_template::REVIEW_EVIDENCE_EXPORT_COLUMNS;
use crate::{AppError, AppResult};

pub const EXCEL_CELL_CHAR_LIMIT: usize = 32_767;
pub const FULLTEXT_DIR: &str = "fulltext";

static EXCEL_ILLEGAL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\x00-\x08\x0B-\x0C\x0E-\x1F]").unwrap());
static FILENAME_SANITIZE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^A-Za-z0-9._\-]+").unwrap());
static ALPHANUM_SPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9 ]+").unwrap());
static WS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());
static MULTI_NEWLINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());

const SECTION_ALIASES: &[(&str, &[&str])] = &[
    ("title", &["title", "article title"]),
    ("abstract", &["abstract", "summary"]),
    ("introduction", &["introduction", "background", "overview"]),
    (
        "methods",
        &[
            "methods",
            "materials and methods",
            "methodology",
            "experimental",
            "experiment",
            "experiments",
        ],
    ),
    ("results", &["results", "findings"]),
    ("discussion", &["discussion", "results and discussion"]),
    (
        "conclusion",
        &["conclusion", "conclusions", "concluding remarks"],
    ),
    (
        "references",
        &[
            "references",
            "bibliography",
            "literature cited",
            "works cited",
        ],
    ),
];

const UT_FILENAME_COLUMNS: &[&str] = &[
    "UT (Unique ID)",
    "UT (Unique WOS ID)",
    "ut_unique_id",
    "ut_unique_wos_id",
    "UT",
    "ut",
    "Accession Number",
    "accession_number",
];

const CSV_ENCODING_CANDIDATES: &[&str] = &["utf-8-sig", "utf-8", "gb18030", "cp936", "latin-1"];

// ---------------------------------------------------------------------------
// Table abstraction
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<HashMap<String, Value>>,
}

impl Table {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_column(&self, name: &str) -> bool {
        self.columns.iter().any(|c| c == name)
    }

    pub fn ensure_column(&mut self, name: &str) {
        if !self.has_column(name) {
            self.columns.push(name.to_string());
        }
    }

    pub fn cell(&self, row: usize, column: &str) -> Option<&Value> {
        self.rows.get(row).and_then(|r| r.get(column))
    }

    pub fn set_cell(&mut self, row: usize, column: &str, value: Value) {
        self.ensure_column(column);
        if let Some(r) = self.rows.get_mut(row) {
            r.insert(column.to_string(), value);
        }
    }

    pub fn nrows(&self) -> usize {
        self.rows.len()
    }
}

fn cell_as_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

fn is_blank(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null)) || cell_as_string(value).trim().is_empty()
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

pub fn read_table(bytes: &[u8], filename: &str) -> AppResult<Table> {
    let name = filename.to_ascii_lowercase();
    if name.ends_with(".csv") {
        return read_csv_with_encoding_fallback(bytes);
    }
    if name.ends_with(".xlsx") || name.ends_with(".xls") {
        return read_spreadsheet_bytes(bytes);
    }
    if name.ends_with(".json") {
        return read_json_bytes(bytes);
    }
    if name.ends_with(".zip") {
        return read_zip_table(bytes);
    }
    Err(AppError::BadInput(
        "Only .xlsx, .xls, .csv, .json, and .zip files are supported.".into(),
    ))
}

/// CFB / OLE2 compound document magic — used by legacy binary .xls (BIFF8).
const OLE_CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// ZIP local file header magic — XLSX is a ZIP container.
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

fn read_spreadsheet_bytes(bytes: &[u8]) -> AppResult<Table> {
    if bytes.len() >= OLE_CFB_MAGIC.len() && bytes.starts_with(&OLE_CFB_MAGIC) {
        return read_xls_bytes(bytes);
    }
    if bytes.len() >= ZIP_MAGIC.len() && bytes.starts_with(&ZIP_MAGIC) {
        return read_xlsx_bytes(bytes);
    }
    // Unknown magic — try xlsx first, then xls, so we surface the more
    // informative error if both fail.
    match read_xlsx_bytes(bytes) {
        Ok(t) => Ok(t),
        Err(xlsx_err) => read_xls_bytes(bytes).map_err(|xls_err| {
            AppError::Excel(format!(
                "Could not parse spreadsheet as XLSX ({xlsx_err}) or XLS ({xls_err})"
            ))
        }),
    }
}

fn decode_csv_with_encoding(bytes: &[u8], encoding_label: &str) -> Option<String> {
    if encoding_label == "utf-8-sig" {
        let stripped = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        return std::str::from_utf8(stripped).ok().map(|s| s.to_string());
    }
    if encoding_label == "utf-8" {
        return std::str::from_utf8(bytes).ok().map(|s| s.to_string());
    }
    let canonical = match encoding_label {
        "gb18030" => "gb18030",
        "cp936" => "gbk",
        "latin-1" => "windows-1252",
        other => other,
    };
    let enc = encoding_rs::Encoding::for_label(canonical.as_bytes())?;
    let (decoded, _, had_errors) = enc.decode(bytes);
    if had_errors {
        None
    } else {
        Some(decoded.into_owned())
    }
}

fn read_csv_with_encoding_fallback(bytes: &[u8]) -> AppResult<Table> {
    let mut last_error: Option<String> = None;
    for label in CSV_ENCODING_CANDIDATES {
        if let Some(text) = decode_csv_with_encoding(bytes, label) {
            match parse_csv_text(&text) {
                Ok(table) => return Ok(table),
                Err(err) => last_error = Some(err.to_string()),
            }
        }
    }
    // Last-resort lossy UTF-8
    let lossy = String::from_utf8_lossy(bytes).into_owned();
    parse_csv_text(&lossy).map_err(|err| {
        AppError::BadInput(format!(
            "Could not decode CSV; tried {:?}. Last error: {}",
            CSV_ENCODING_CANDIDATES,
            last_error.unwrap_or_else(|| err.to_string())
        ))
    })
}

fn parse_csv_text(text: &str) -> AppResult<Table> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| AppError::BadInput(format!("CSV header parse error: {e}")))?
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let mut rows: Vec<HashMap<String, Value>> = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| AppError::BadInput(format!("CSV row parse error: {e}")))?;
        let mut row: HashMap<String, Value> = HashMap::new();
        for (i, value) in record.iter().enumerate() {
            if let Some(name) = headers.get(i) {
                row.insert(name.clone(), Value::String(value.to_string()));
            }
        }
        rows.push(row);
    }
    Ok(Table {
        columns: headers,
        rows,
    })
}

fn read_xlsx_bytes(bytes: &[u8]) -> AppResult<Table> {
    let cursor = Cursor::new(bytes.to_vec());
    let mut workbook: Xlsx<_> = open_workbook_from_rs(cursor)
        .map_err(|e| AppError::Excel(format!("Failed to open xlsx: {e}")))?;
    let first_sheet = workbook
        .sheet_names()
        .first()
        .cloned()
        .ok_or_else(|| AppError::Excel("xlsx contains no sheets".into()))?;
    let range = workbook
        .worksheet_range(&first_sheet)
        .map_err(|e| AppError::Excel(format!("Failed to read xlsx sheet '{first_sheet}': {e}")))?;
    table_from_range(&range)
}

fn read_xls_bytes(bytes: &[u8]) -> AppResult<Table> {
    let cursor = Cursor::new(bytes.to_vec());
    let mut workbook: Xls<_> = open_workbook_from_rs(cursor)
        .map_err(|e| AppError::Excel(format!("Failed to open xls: {e}")))?;
    let first_sheet = workbook
        .sheet_names()
        .first()
        .cloned()
        .ok_or_else(|| AppError::Excel("xls contains no sheets".into()))?;
    let range = workbook
        .worksheet_range(&first_sheet)
        .map_err(|e| AppError::Excel(format!("Failed to read xls sheet '{first_sheet}': {e}")))?;
    table_from_range(&range)
}

/// Read a JSON document as a Table.
///
/// Accepted shapes:
/// - Array of objects: `[{"col": "val", ...}, ...]` (column order = order of first
///   appearance of each key across the whole array, so later rows can introduce
///   new columns without dropping cells).
/// - Object with explicit shape: `{"columns": ["a","b"], "rows": [{"a":1,"b":2}, ...]}`.
fn read_json_bytes(bytes: &[u8]) -> AppResult<Table> {
    let stripped = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let parsed: Value = serde_json::from_slice(stripped)
        .map_err(|e| AppError::BadInput(format!("Invalid JSON: {e}")))?;
    match parsed {
        Value::Array(items) => table_from_json_rows(&items, None),
        Value::Object(map) => {
            let rows_val = map.get("rows").or_else(|| map.get("data")).ok_or_else(|| {
                AppError::BadInput(
                    "JSON object must contain a 'rows' (or 'data') array of records.".into(),
                )
            })?;
            let rows = rows_val.as_array().ok_or_else(|| {
                AppError::BadInput("JSON 'rows'/'data' must be an array of objects.".into())
            })?;
            let explicit_columns = map.get("columns").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|c| c.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            });
            table_from_json_rows(rows, explicit_columns.as_deref())
        }
        _ => Err(AppError::BadInput(
            "JSON must be an array of objects, or an object with a 'rows' array.".into(),
        )),
    }
}

fn table_from_json_rows(items: &[Value], explicit_columns: Option<&[String]>) -> AppResult<Table> {
    let mut columns: Vec<String> = explicit_columns.map(|c| c.to_vec()).unwrap_or_default();
    let mut seen: HashSet<String> = columns.iter().cloned().collect();
    let mut rows: Vec<HashMap<String, Value>> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| {
            AppError::BadInput(format!(
                "JSON row {i} is not an object; every record must be a JSON object."
            ))
        })?;
        let mut row: HashMap<String, Value> = HashMap::with_capacity(obj.len());
        for (key, value) in obj {
            if seen.insert(key.clone()) {
                columns.push(key.clone());
            }
            row.insert(key.clone(), value.clone());
        }
        rows.push(row);
    }
    Ok(Table { columns, rows })
}

fn table_from_range(range: &calamine::Range<Data>) -> AppResult<Table> {
    let mut rows_iter = range.rows();
    let header_row = rows_iter
        .next()
        .ok_or_else(|| AppError::Excel("empty sheet".into()))?;
    let headers: Vec<String> = header_row
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            let name = data_cell_string(cell);
            if name.trim().is_empty() {
                format!("col_{}", i + 1)
            } else {
                name
            }
        })
        .collect();
    let mut rows: Vec<HashMap<String, Value>> = Vec::new();
    for row in rows_iter {
        let mut map: HashMap<String, Value> = HashMap::new();
        for (i, cell) in row.iter().enumerate() {
            if let Some(name) = headers.get(i) {
                map.insert(name.clone(), data_cell_to_value(cell));
            }
        }
        rows.push(map);
    }
    Ok(Table {
        columns: headers,
        rows,
    })
}

fn data_cell_to_value(cell: &Data) -> Value {
    match cell {
        Data::Empty => Value::Null,
        Data::String(s) => Value::String(s.clone()),
        Data::Bool(b) => Value::Bool(*b),
        Data::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Data::Int(i) => Value::Number((*i).into()),
        Data::DateTime(dt) => Value::String(dt.to_string()),
        Data::DateTimeIso(s) | Data::DurationIso(s) => Value::String(s.clone()),
        Data::Error(e) => Value::String(format!("#ERR:{e:?}")),
    }
}

fn data_cell_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Bool(b) => b.to_string(),
        Data::Float(f) => f.to_string(),
        Data::Int(i) => i.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR:{e:?}"),
    }
}

pub fn read_zip_table(bytes: &[u8]) -> AppResult<Table> {
    let reader = Cursor::new(bytes.to_vec());
    let mut archive = ZipArchive::new(reader)
        .map_err(|e| AppError::BadInput(format!("Failed to open ZIP: {e}")))?;
    let mut names: Vec<String> = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| AppError::BadInput(format!("ZIP index error: {e}")))?;
        names.push(file.name().to_string());
    }
    let mut preferred: Vec<&String> = names
        .iter()
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            !n.ends_with('/')
                && (l.ends_with(".xlsx") || l.ends_with(".xls") || l.ends_with(".csv"))
        })
        .collect();
    if preferred.is_empty() {
        return Err(AppError::BadInput(
            "ZIP does not contain a supported table file.".into(),
        ));
    }
    preferred.sort_by_key(|n| {
        let lower = n.to_ascii_lowercase();
        (!lower.ends_with(".xlsx"), (*n).clone())
    });
    let chosen = preferred[0].clone();

    let chosen_bytes = {
        let mut file = archive
            .by_name(&chosen)
            .map_err(|e| AppError::BadInput(format!("ZIP entry '{chosen}' missing: {e}")))?;
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)?;
        buf
    };

    let mut table = read_table(&chosen_bytes, &chosen)?;

    if table.has_column("content_text") {
        let name_set: HashSet<String> = names.iter().cloned().collect();
        for row_idx in 0..table.nrows() {
            let value = table.cell(row_idx, "content_text").cloned();
            let path_text = cell_as_string(value.as_ref()).replace('\\', "/");
            if !path_text.starts_with(&format!("{FULLTEXT_DIR}/")) {
                continue;
            }
            if !name_set.contains(&path_text) {
                continue;
            }
            let mut buf = Vec::new();
            if let Ok(mut entry) = archive.by_name(&path_text) {
                if entry.read_to_end(&mut buf).is_ok() {
                    let decoded = String::from_utf8_lossy(&buf).into_owned();
                    table.set_cell(row_idx, "content_text", Value::String(decoded));
                }
            }
        }
    }

    Ok(table)
}

// ---------------------------------------------------------------------------
// Column aliasing / export prep
// ---------------------------------------------------------------------------

fn first_existing_column<'a>(table: &Table, candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates.iter().copied().find(|c| table.has_column(c))
}

/// Flatten an embedded JSON column: each cell that is a JSON object contributes
/// its keys as new columns (only filling in where the row's existing column
/// value is blank).
pub fn flatten_json_column(table: &mut Table, column: &str) {
    if !table.has_column(column) {
        return;
    }
    // Collect new column keys
    let mut new_cols: Vec<String> = Vec::new();
    for row in &table.rows {
        if let Some(Value::Object(obj)) = row.get(column) {
            for key in obj.keys() {
                if !new_cols.contains(key) && !table.has_column(key) {
                    new_cols.push(key.clone());
                }
            }
        }
    }
    for col in &new_cols {
        table.columns.push(col.clone());
    }
    // Backfill cells. For existing columns, only overwrite blanks.
    let column_set: Vec<String> = table.columns.clone();
    for row in &mut table.rows {
        let Some(Value::Object(obj)) = row.get(column).cloned() else {
            continue;
        };
        for (k, v) in obj {
            if column_set.contains(&k) {
                let existing = row.get(&k).cloned();
                if matches!(existing, None | Some(Value::Null))
                    || cell_as_string(existing.as_ref()).trim().is_empty()
                {
                    row.insert(k, v);
                }
            }
        }
    }
}

pub fn prepare_export_table(table: &Table) -> Table {
    type AliasMap<'a> = &'a [(&'a str, &'a [&'a str])];
    let export_aliases: AliasMap = &[
        (
            "Article Title",
            &[
                "Article Title",
                "article_title",
                "title",
                "Document Title",
                "中文标题",
            ],
        ),
        (
            "Authors",
            &[
                "Authors",
                "authors",
                "Author Full Names",
                "Author(s)",
                "Byline",
                "basic_info_authors",
            ],
        ),
        (
            "Source Title",
            &[
                "Source Title",
                "source_title",
                "journal",
                "journal_title",
                "Publication Name",
                "Publication Title",
                "basic_info_journal",
            ],
        ),
        ("DOI", &["doi_normalized", "DOI", "doi", "basic_info_doi"]),
        (
            "Publication Year",
            &[
                "Publication Year",
                "publication_year",
                "year",
                "published_year",
                "Pub Year",
                "PY",
                "basic_info_year",
            ],
        ),
        ("Volume", &["Volume", "volume", "Vol", "basic_info_volume"]),
        (
            "Issue",
            &["Issue", "issue", "Number", "No", "basic_info_issue"],
        ),
        (
            "Pages",
            &[
                "Pages",
                "pages",
                "Page Range",
                "page_range",
                "basic_info_pages",
            ],
        ),
        (
            "Start Page",
            &["Start Page", "start_page", "Beginning Page", "BP"],
        ),
        ("End Page", &["End Page", "end_page", "Ending Page", "EP"]),
        (
            "Document Type",
            &[
                "Document Type",
                "document_type",
                "record_type",
                "Publication Type",
                "basic_info_document_type",
            ],
        ),
        (
            "Abstract",
            &["Abstract", "abstract", "summary", "basic_info_abstract"],
        ),
        (
            "Author Keywords",
            &["Author Keywords", "author_keywords", "Keywords"],
        ),
        ("Keywords Plus", &["Keywords Plus", "keywords_plus"]),
        (
            "UT (Unique ID)",
            &[
                "UT (Unique ID)",
                "UT (Unique WOS ID)",
                "ut_unique_id",
                "ut_unique_wos_id",
                "ut",
                "UT",
                "UT号",
                "Accession Number",
                "accession_number",
            ],
        ),
        (
            "Search Direction",
            &[
                "Search Direction",
                "search_direction",
                "direction_name",
                "原始检索方向",
            ],
        ),
        (
            "Suggested Section",
            &["Suggested Section", "suggested_section", "原始建议章节"],
        ),
        ("Direction Index", &["Direction Index", "direction_index"]),
        (
            "Matched Direction Count",
            &["Matched Direction Count", "matched_direction_count"],
        ),
        ("主题匹配度评分", &["主题匹配度评分"]),
        ("证据可用性评分", &["证据可用性评分"]),
        ("章节适配度评分", &["章节适配度评分"]),
        ("对象方法适配度评分", &["对象方法适配度评分"]),
        (
            "主题相关性总分",
            &["主题相关性总分", "topic_relevance_score", "relevance_score"],
        ),
        ("相关性等级", &["相关性等级", "relevance_level"]),
        ("主题相关性理由", &["主题相关性理由"]),
        ("纳入建议", &["纳入建议", "inclusion_recommendation"]),
        ("排除或降权原因", &["排除或降权原因"]),
        ("相关性评分来源", &["relevance_score_source"]),
        ("Extraction Quality Status", &["extraction_quality_status"]),
        ("Extraction Quality Reason", &["extraction_quality_reason"]),
        ("Extraction Prompt Preview", &["extraction_prompt_preview"]),
    ];

    let mut output = table.clone();
    let alias_target_names: Vec<String> = export_aliases
        .iter()
        .map(|(t, _)| (*t).to_string())
        .collect();

    for (target, candidates) in export_aliases {
        let source_column: Option<String> =
            first_existing_column(&output, candidates).map(|s| s.to_string());
        match source_column {
            None => {
                output.ensure_column(target);
                for row in &mut output.rows {
                    row.entry((*target).to_string())
                        .or_insert(Value::String(String::new()));
                }
            }
            Some(src) => {
                output.ensure_column(target);
                for row in &mut output.rows {
                    let val = row.get(&src).cloned().unwrap_or(Value::Null);
                    let normalized = if matches!(val, Value::Null) {
                        Value::String(String::new())
                    } else {
                        val
                    };
                    row.insert((*target).to_string(), normalized);
                }
            }
        }
    }

    for column in REVIEW_EVIDENCE_EXPORT_COLUMNS {
        output.ensure_column(column);
        for row in &mut output.rows {
            row.entry((*column).to_string())
                .or_insert(Value::String(String::new()));
        }
    }

    // Reorder columns: alias targets first, then remaining review columns, then anything else.
    let preferred_order: Vec<String> = alias_target_names
        .iter()
        .cloned()
        .chain(
            REVIEW_EVIDENCE_EXPORT_COLUMNS
                .iter()
                .filter(|c| !alias_target_names.iter().any(|t| t == *c))
                .map(|s| (*s).to_string()),
        )
        .collect();
    let mut final_order: Vec<String> = preferred_order.clone();
    for col in &output.columns {
        if !final_order.contains(col) {
            final_order.push(col.clone());
        }
    }
    output.columns = final_order;
    output
}

// ---------------------------------------------------------------------------
// Excel sanitization
// ---------------------------------------------------------------------------

fn strip_excel_illegal(value: &mut Value) {
    if let Value::String(s) = value {
        if EXCEL_ILLEGAL_RE.is_match(s) {
            *s = EXCEL_ILLEGAL_RE.replace_all(s, "").to_string();
        }
    }
}

fn truncate_excel_oversized(value: &mut Value) {
    if let Value::String(s) = value {
        if s.chars().count() > EXCEL_CELL_CHAR_LIMIT {
            const TRAIL: &str = "\n[TRUNCATED FOR EXCEL]";
            let take_chars = EXCEL_CELL_CHAR_LIMIT.saturating_sub(40);
            let mut truncated: String = s.chars().take(take_chars).collect();
            truncated.push_str(TRAIL);
            *s = truncated;
        }
    }
}

fn sanitize_and_truncate(table: &Table) -> Table {
    let mut output = table.clone();
    for row in &mut output.rows {
        for col in &output.columns {
            if let Some(value) = row.get_mut(col) {
                strip_excel_illegal(value);
                truncate_excel_oversized(value);
            }
        }
    }
    output
}

pub fn serialize_excel_table(table: &Table) -> AppResult<Vec<u8>> {
    let sanitized = sanitize_and_truncate(table);
    let mut workbook = Workbook::new();
    let worksheet = workbook
        .add_worksheet()
        .set_name("results")
        .map_err(|e| AppError::Excel(e.to_string()))?;
    for (col_idx, name) in sanitized.columns.iter().enumerate() {
        worksheet
            .write_string(0, col_idx as u16, name)
            .map_err(|e| AppError::Excel(e.to_string()))?;
    }
    for (row_idx, row) in sanitized.rows.iter().enumerate() {
        let excel_row = (row_idx + 1) as u32;
        for (col_idx, col_name) in sanitized.columns.iter().enumerate() {
            let col_u16 = col_idx as u16;
            match row.get(col_name) {
                Some(Value::String(s)) => {
                    worksheet
                        .write_string(excel_row, col_u16, s)
                        .map_err(|e| AppError::Excel(e.to_string()))?;
                }
                Some(Value::Number(n)) => {
                    if let Some(f) = n.as_f64() {
                        worksheet
                            .write_number(excel_row, col_u16, f)
                            .map_err(|e| AppError::Excel(e.to_string()))?;
                    } else {
                        worksheet
                            .write_string(excel_row, col_u16, n.to_string())
                            .map_err(|e| AppError::Excel(e.to_string()))?;
                    }
                }
                Some(Value::Bool(b)) => {
                    worksheet
                        .write_boolean(excel_row, col_u16, *b)
                        .map_err(|e| AppError::Excel(e.to_string()))?;
                }
                Some(Value::Null) | None => {
                    worksheet
                        .write_string(excel_row, col_u16, "")
                        .map_err(|e| AppError::Excel(e.to_string()))?;
                }
                Some(other) => {
                    worksheet
                        .write_string(excel_row, col_u16, other.to_string())
                        .map_err(|e| AppError::Excel(e.to_string()))?;
                }
            }
        }
    }
    workbook
        .save_to_buffer()
        .map_err(|e| AppError::Excel(e.to_string()))
}

// ---------------------------------------------------------------------------
// Fulltext markdown
// ---------------------------------------------------------------------------

fn clean_filename(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    let cleaned = FILENAME_SANITIZE_RE.replace_all(trimmed, "_").to_string();
    let cleaned = cleaned
        .trim_matches(|c: char| c == '.' || c == '_')
        .to_string();
    let pick = if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    };
    pick.chars().take(120).collect()
}

fn first_nonblank(row: &HashMap<String, Value>, columns: &[&str]) -> String {
    for column in columns {
        if let Some(value) = row.get(*column) {
            let text = cell_as_string(Some(value));
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
    }
    String::new()
}

fn safe_fulltext_stem(
    row: &HashMap<String, Value>,
    ordinal: usize,
    used: &mut HashSet<String>,
) -> String {
    let ut = first_nonblank(row, UT_FILENAME_COLUMNS);
    let fallback = format!("paper_{ordinal:04}");
    let mut stem = clean_filename(&ut, &fallback);
    let base = stem.clone();
    let mut suffix = 2;
    while used.contains(&stem.to_ascii_lowercase()) {
        stem = format!("{base}_{suffix}");
        suffix += 1;
    }
    used.insert(stem.to_ascii_lowercase());
    stem
}

fn canonical_section_heading(line: &str) -> Option<&'static str> {
    let lowered = line.trim().to_ascii_lowercase();
    let stripped = ALPHANUM_SPACE_RE.replace_all(&lowered, " ");
    let normalized = WS_RE.replace_all(&stripped, " ").trim().to_string();
    if normalized.is_empty() || normalized.len() > 80 {
        return None;
    }
    for (canonical, aliases) in SECTION_ALIASES {
        if aliases.contains(&normalized.as_str()) {
            return Some(canonical);
        }
    }
    None
}

fn format_paragraph(lines: &[String]) -> String {
    let joined = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    WS_RE.replace_all(&joined, " ").trim().to_string()
}

pub fn build_structured_fulltext_markdown(
    content_text: &str,
    metadata: Option<&Map<String, Value>>,
) -> (String, String) {
    let text = content_text.replace('\r', "\n");
    let lines: Vec<String> = text
        .split('\n')
        .map(|l| WS_RE.replace_all(l, " ").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let title = metadata
        .and_then(|m| m.get("article_title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let mut output: Vec<String> = Vec::new();
    if !title.is_empty() {
        output.push("#title".into());
        output.push(title.clone());
        output.push(String::new());
    }

    let mut paragraph_lines: Vec<String> = Vec::new();
    let mut seen_sections: HashSet<&'static str> = HashSet::new();
    let mut section_hits: usize = 0;

    let flush = |paragraph_lines: &mut Vec<String>, output: &mut Vec<String>| {
        if paragraph_lines.is_empty() {
            return;
        }
        let paragraph = format_paragraph(paragraph_lines);
        paragraph_lines.clear();
        if !paragraph.is_empty() {
            output.push(paragraph);
            output.push(String::new());
        }
    };

    for line in &lines {
        if let Some(heading) = canonical_section_heading(line) {
            flush(&mut paragraph_lines, &mut output);
            if heading == "title" && !title.is_empty() {
                continue;
            }
            output.push(format!("#{heading}"));
            output.push(String::new());
            if seen_sections.insert(heading) {
                section_hits += 1;
            }
            continue;
        }
        paragraph_lines.push(line.clone());
    }
    flush(&mut paragraph_lines, &mut output);

    let status: &str;
    if section_hits == 0 {
        let body = lines.join("\n").trim().to_string();
        output.clear();
        if !title.is_empty() {
            output.push("#title".into());
            output.push(title);
            output.push(String::new());
        }
        output.push("#full_text".into());
        output.push(body);
        status = "unstructured_fallback";
    } else if section_hits == 1 {
        status = "partially_structured";
    } else {
        status = "structured";
    }

    let joined = output.join("\n").trim().to_string();
    let collapsed = MULTI_NEWLINE_RE.replace_all(&joined, "\n\n").to_string();
    (collapsed, status.to_string())
}

// ---------------------------------------------------------------------------
// Externalize content_text + final serialization
// ---------------------------------------------------------------------------

fn looks_like_fulltext_artifact_path(value: &str) -> bool {
    let text = value.replace('\\', "/");
    let lower = text.to_ascii_lowercase();
    text.starts_with(&format!("{FULLTEXT_DIR}/"))
        && (lower.ends_with(".txt") || lower.ends_with(".md"))
}

fn externalize_content_text(table: &Table) -> (Table, Vec<(String, String)>) {
    let mut output = table.clone();
    let mut artifacts: Vec<(String, String)> = Vec::new();
    if !output.has_column("content_text") {
        return (output, artifacts);
    }
    for column in [
        "content_text_file",
        "content_text_excel_note",
        "content_text_structure_status",
    ] {
        output.ensure_column(column);
        for row in &mut output.rows {
            row.entry(column.to_string())
                .or_insert(Value::String(String::new()));
        }
    }

    let mut used: HashSet<String> = HashSet::new();
    let total = output.nrows();
    for ordinal in 1..=total {
        let row_idx = ordinal - 1;
        let value = output.cell(row_idx, "content_text").cloned();
        let text = match value {
            Some(Value::String(s)) => s,
            _ => continue,
        };
        if text.trim().is_empty() || looks_like_fulltext_artifact_path(&text) {
            continue;
        }
        let title = output
            .cell(row_idx, "Article Title")
            .map(|v| cell_as_string(Some(v)))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                output
                    .cell(row_idx, "article_title")
                    .map(|v| cell_as_string(Some(v)))
            })
            .or_else(|| {
                output
                    .cell(row_idx, "title")
                    .map(|v| cell_as_string(Some(v)))
            })
            .unwrap_or_default();

        let stem = {
            let row_ref = &output.rows[row_idx];
            safe_fulltext_stem(row_ref, ordinal, &mut used)
        };
        let filename = format!("{FULLTEXT_DIR}/{stem}.md");
        let mut metadata = Map::new();
        metadata.insert("article_title".into(), Value::String(title));
        let (markdown, status) = build_structured_fulltext_markdown(&text, Some(&metadata));
        artifacts.push((filename.clone(), markdown));
        output.set_cell(
            row_idx,
            "content_text_file",
            Value::String(filename.clone()),
        );
        output.set_cell(row_idx, "content_text", Value::String(filename));
        output.set_cell(
            row_idx,
            "content_text_structure_status",
            Value::String(status),
        );
        output.set_cell(
            row_idx,
            "content_text_excel_note",
            Value::String("全文已外置到 ZIP 内的 Markdown 文件，避免 Excel 单元格过长。".into()),
        );
    }
    (output, artifacts)
}

/// Serialize a result table. Returns `(bytes, packaged_table, format, fulltext_artifact_count)`.
/// `format` is `"xlsx"` if no fulltext artifacts were produced, otherwise `"zip"`.
pub fn serialize_result_table(table: &Table) -> AppResult<(Vec<u8>, Table, String, usize)> {
    let mut output = table.clone();
    if output.has_column("parsed_json") {
        flatten_json_column(&mut output, "parsed_json");
        output.columns.retain(|c| c != "parsed_json");
        for row in &mut output.rows {
            row.remove("parsed_json");
        }
    }
    let prepared = prepare_export_table(&output);
    let (packaged, artifacts) = externalize_content_text(&prepared);
    let excel_bytes = serialize_excel_table(&packaged)?;
    if artifacts.is_empty() {
        return Ok((excel_bytes, packaged, "xlsx".into(), 0));
    }
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut zip = ZipWriter::new(cursor);
        let options: SimpleFileOptions =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file("manifest.xlsx", options)
            .map_err(|e| AppError::Other(format!("zip start manifest.xlsx: {e}")))?;
        zip.write_all(&excel_bytes)?;
        zip.start_file("README.txt", options)
            .map_err(|e| AppError::Other(format!("zip start README.txt: {e}")))?;
        zip.write_all(
            b"Open manifest.xlsx for the table.\n\
              Rows with acquired full text store content_text as a relative Markdown path under fulltext/.\n\
              Uploading this ZIP back into wos-review restores content_text from those Markdown files automatically.\n",
        )?;
        for (filename, markdown) in &artifacts {
            zip.start_file(filename, options)
                .map_err(|e| AppError::Other(format!("zip start {filename}: {e}")))?;
            zip.write_all(markdown.as_bytes())?;
        }
        zip.finish()
            .map_err(|e| AppError::Other(format!("zip finalize: {e}")))?;
    }
    let count = artifacts.len();
    Ok((buf, packaged, "zip".into(), count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_table() -> Table {
        let mut t = Table::new();
        t.columns = vec!["Article Title".into(), "DOI".into(), "content_text".into()];
        t.rows.push(
            [
                ("Article Title".into(), json!("Sample Paper")),
                ("DOI".into(), json!("10.1234/abcd")),
                (
                    "content_text".into(),
                    json!("Abstract\nThis paper studies X.\nMethods\nWe used Y."),
                ),
            ]
            .into_iter()
            .collect(),
        );
        t
    }

    #[test]
    fn round_trip_xlsx() {
        let table = sample_table();
        let bytes = serialize_excel_table(&table).unwrap();
        assert!(bytes.len() > 100);
        let reloaded = read_xlsx_bytes(&bytes).unwrap();
        assert!(reloaded.has_column("Article Title"));
        assert_eq!(reloaded.nrows(), 1);
    }

    #[test]
    fn structured_fulltext_detects_sections() {
        let (md, status) = build_structured_fulltext_markdown(
            "Abstract\nThis is a study.\nMethods\nWe did stuff.\nResults\nIt worked.",
            None,
        );
        assert_eq!(status, "structured");
        assert!(md.contains("#abstract"));
        assert!(md.contains("#methods"));
        assert!(md.contains("#results"));
    }

    #[test]
    fn unstructured_fallback() {
        let (md, status) =
            build_structured_fulltext_markdown("Just some plain prose with no headings.", None);
        assert_eq!(status, "unstructured_fallback");
        assert!(md.contains("#full_text"));
    }

    #[test]
    fn serialize_result_table_emits_zip_when_fulltext_present() {
        let table = sample_table();
        let (bytes, _packaged, fmt, count) = serialize_result_table(&table).unwrap();
        assert_eq!(fmt, "zip");
        assert_eq!(count, 1);
        // ZIP local file header magic
        assert_eq!(&bytes[..2], b"PK");
    }

    #[test]
    fn prepare_export_table_adds_canonical_columns() {
        let table = sample_table();
        let prepared = prepare_export_table(&table);
        assert!(prepared.has_column("证据角色"));
        assert!(prepared.has_column("中文标题"));
        assert!(prepared.has_column("Article Title"));
    }

    #[test]
    fn csv_round_trip_utf8() {
        let csv = "Article Title,DOI\nFoo,10.1/abc\n";
        let table = parse_csv_text(csv).unwrap();
        assert_eq!(table.nrows(), 1);
        assert_eq!(cell_as_string(table.cell(0, "Article Title")), "Foo");
    }

    #[test]
    fn json_array_of_objects_preserves_first_seen_column_order() {
        let json = br#"[
            {"Article Title": "Paper A", "DOI": "10.1/a"},
            {"DOI": "10.1/b", "Article Title": "Paper B", "Authors": "Doe"}
        ]"#;
        let table = read_json_bytes(json).unwrap();
        assert_eq!(
            table.columns,
            vec![
                "Article Title".to_string(),
                "DOI".to_string(),
                "Authors".to_string(),
            ]
        );
        assert_eq!(table.nrows(), 2);
        assert_eq!(cell_as_string(table.cell(1, "Article Title")), "Paper B");
        assert_eq!(cell_as_string(table.cell(0, "Authors")), "");
    }

    #[test]
    fn json_object_with_explicit_columns_and_rows() {
        let json = br#"{
            "columns": ["DOI", "Article Title"],
            "rows": [{"Article Title": "X", "DOI": "10.1/x"}]
        }"#;
        let table = read_json_bytes(json).unwrap();
        // Explicit columns come first, in declared order.
        assert_eq!(table.columns[0], "DOI");
        assert_eq!(table.columns[1], "Article Title");
        assert_eq!(cell_as_string(table.cell(0, "DOI")), "10.1/x");
    }

    #[test]
    fn json_with_utf8_bom_is_accepted() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"[{"a": 1}]"#);
        let table = read_json_bytes(&bytes).unwrap();
        assert_eq!(table.nrows(), 1);
        assert_eq!(table.columns, vec!["a".to_string()]);
    }

    #[test]
    fn json_non_object_row_is_rejected() {
        let err = read_json_bytes(br#"[1, 2, 3]"#).unwrap_err();
        assert!(matches!(err, AppError::BadInput(_)));
    }

    #[test]
    fn read_table_unknown_extension_lists_json() {
        let err = read_table(b"whatever", "foo.txt").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains(".json"), "error should advertise .json: {msg}");
    }
}
