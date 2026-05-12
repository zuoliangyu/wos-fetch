//! Resumable checkpoint storage for long-running pipelines.
//!
//! Port of `core/checkpoint.py`. Each "job" is identified by a `resume_key`
//! (sanitized to a filesystem-safe slug) and stored under
//! `<project_root>/.checkpoints/<safe_key>/`. Files written:
//!
//! - `meta.json`       — status, progress counters, optional summary
//! - `batch_NNNNN.json` — per-batch row append-log (rebuilds full row list)
//! - `final.xlsx` / `final.zip` — terminal serialized result (if any)
//!
//! Writes go through a `.tmp` sibling + rename for atomicity.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};

use crate::{AppError, AppResult};

static UNSAFE_CHARS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^A-Za-z0-9_.\-]+").unwrap());

/// Returns `<cwd>/.checkpoints`. The Python version anchored on the source
/// file's parent.parent; here we anchor on the current working directory at
/// startup, which the Tauri app sets to the project root.
pub fn checkpoint_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".checkpoints")
}

pub fn checkpoint_job_dir(resume_key: &str) -> PathBuf {
    checkpoint_root().join(resume_key)
}

pub fn checkpoint_meta_path(resume_key: &str) -> PathBuf {
    checkpoint_job_dir(resume_key).join("meta.json")
}

pub fn checkpoint_result_path(resume_key: &str) -> PathBuf {
    checkpoint_job_dir(resume_key).join("final.xlsx")
}

pub fn checkpoint_result_zip_path(resume_key: &str) -> PathBuf {
    checkpoint_job_dir(resume_key).join("final.zip")
}

fn safe_resume_key(resume_key: &str) -> AppResult<String> {
    let key = resume_key.trim();
    if key.is_empty() {
        return Err(AppError::BadInput("resume_key is required.".into()));
    }
    let safe: String = UNSAFE_CHARS_RE
        .replace_all(key, "_")
        .trim_matches(|c: char| c == '.' || c == '_')
        .chars()
        .take(160)
        .collect();
    if safe.is_empty() {
        return Err(AppError::BadInput(
            "resume_key must contain at least one safe character.".into(),
        ));
    }
    Ok(safe)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn atomic_write_json(path: &Path, payload: &Value) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temp_path = path.to_path_buf();
    let new_ext = match path.extension() {
        Some(ext) => format!("{}.tmp", ext.to_string_lossy()),
        None => "tmp".to_string(),
    };
    temp_path.set_extension(new_ext);
    let serialized = serde_json::to_vec_pretty(payload)?;
    fs::write(&temp_path, serialized)?;
    fs::rename(&temp_path, path)?;
    Ok(())
}

/// Read `meta.json` for a job, returning an empty map if missing or unreadable.
pub fn load_checkpoint_meta(resume_key: &str) -> Map<String, Value> {
    let path = checkpoint_meta_path(resume_key);
    if !path.exists() {
        return Map::new();
    }
    match fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Upsert the meta record for a job (status, counters, optional summary).
pub fn save_checkpoint_meta(
    resume_key: &str,
    source_name: &str,
    status: &str,
    completed_rows: u64,
    total_rows: u64,
    summary: Option<Value>,
) -> AppResult<Value> {
    let safe_key = safe_resume_key(resume_key)?;
    let mut payload = load_checkpoint_meta(&safe_key);
    payload.insert("source_name".into(), Value::String(source_name.into()));
    payload.insert("status".into(), Value::String(status.into()));
    payload.insert("completed_rows".into(), Value::from(completed_rows));
    payload.insert("total_rows".into(), Value::from(total_rows));
    payload.insert("updated_at".into(), Value::from(unix_now()));
    if let Some(summary) = summary {
        payload.insert("summary".into(), summary);
    }
    let value = Value::Object(payload);
    atomic_write_json(&checkpoint_meta_path(&safe_key), &value)?;
    Ok(value)
}

/// Set the cancel flag on a running job's meta record.
pub fn request_checkpoint_cancel(resume_key: &str, reason: &str) -> AppResult<Value> {
    let safe_key = safe_resume_key(resume_key)?;
    let mut payload = load_checkpoint_meta(&safe_key);
    let trimmed_reason: String = reason.trim().chars().take(300).collect();
    payload.insert("cancel_requested".into(), Value::Bool(true));
    payload.insert("cancel_reason".into(), Value::String(trimmed_reason));
    payload.insert("cancel_requested_at".into(), Value::from(unix_now()));
    payload.insert("updated_at".into(), Value::from(unix_now()));
    if let Some(Value::String(status)) = payload.get("status") {
        if status.trim().eq_ignore_ascii_case("running") {
            payload.insert("status".into(), Value::String("cancelling".into()));
        }
    }
    let value = Value::Object(payload);
    atomic_write_json(&checkpoint_meta_path(&safe_key), &value)?;
    Ok(value)
}

pub fn checkpoint_cancel_requested(resume_key: &str) -> bool {
    load_checkpoint_meta(resume_key)
        .get("cancel_requested")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Append a batch of rows to the job's checkpoint. Returns the written path.
pub fn append_checkpoint_batch(
    resume_key: &str,
    batch_index: u32,
    rows: &[Value],
) -> AppResult<PathBuf> {
    let safe_key = safe_resume_key(resume_key)?;
    let dir = checkpoint_job_dir(&safe_key);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("batch_{:05}.json", batch_index));
    let mut temp_path = path.clone();
    temp_path.set_extension("json.tmp");
    // Only keep object-shaped rows (matches the Python isinstance(row, dict) filter).
    let objects: Vec<&Value> = rows.iter().filter(|v| v.is_object()).collect();
    let serialized = serde_json::to_vec(&objects)?;
    fs::write(&temp_path, serialized)?;
    fs::rename(&temp_path, &path)?;
    Ok(path)
}

/// Store the final serialized result file (xlsx or zip) for a job.
pub fn save_checkpoint_result_file(
    resume_key: &str,
    payload: &[u8],
    result_format: &str,
) -> AppResult<PathBuf> {
    let safe_key = safe_resume_key(resume_key)?;
    let suffix = if result_format.eq_ignore_ascii_case("zip") {
        "zip"
    } else {
        "xlsx"
    };
    let path = checkpoint_job_dir(&safe_key).join(format!("final.{suffix}"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temp_path = path.clone();
    temp_path.set_extension(format!("{suffix}.tmp"));
    fs::write(&temp_path, payload)?;
    fs::rename(&temp_path, &path)?;
    Ok(path)
}

/// Reload all rows that have been checkpointed for `resume_key`, in batch order.
pub fn load_checkpoint_rows(resume_key: &str) -> Vec<Value> {
    let dir = checkpoint_job_dir(resume_key);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut batch_paths: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("batch_") && n.ends_with(".json"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    batch_paths.sort();

    let mut rows: Vec<Value> = Vec::new();
    for path in batch_paths {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Value::Array(items) = parsed {
            rows.extend(items.into_iter().filter(|v| v.is_object()));
        }
    }
    rows
}

#[derive(Debug, serde::Serialize)]
pub struct CheckpointListItem {
    pub resume_key: String,
    pub source_name: String,
    pub status: String,
    pub completed_rows: u64,
    pub total_rows: u64,
    pub batch_count: u32,
    pub has_final_result: bool,
    pub updated_at: i64,
}

/// Enumerate all known checkpoints, newest first by mtime.
pub fn list_checkpoints() -> Vec<CheckpointListItem> {
    let root = checkpoint_root();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = match fs::read_dir(&root) {
        Ok(it) => it
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if !path.is_dir() {
                    return None;
                }
                let mtime = e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(UNIX_EPOCH);
                Some((path, mtime))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    entries
        .into_iter()
        .map(|(dir, _)| {
            let resume_key = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let meta = load_checkpoint_meta(&resume_key);
            let batch_count = fs::read_dir(&dir)
                .map(|it| {
                    it.flatten()
                        .filter(|e| {
                            e.file_name()
                                .to_str()
                                .map(|n| n.starts_with("batch_") && n.ends_with(".json"))
                                .unwrap_or(false)
                        })
                        .count() as u32
                })
                .unwrap_or(0);
            let has_final = checkpoint_result_path(&resume_key).exists()
                || checkpoint_result_zip_path(&resume_key).exists();
            CheckpointListItem {
                resume_key: resume_key.clone(),
                source_name: meta
                    .get("source_name")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| resume_key.clone()),
                status: meta
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                completed_rows: meta
                    .get("completed_rows")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                total_rows: meta.get("total_rows").and_then(Value::as_u64).unwrap_or(0),
                batch_count,
                has_final_result: has_final,
                updated_at: meta.get("updated_at").and_then(Value::as_i64).unwrap_or(0),
            }
        })
        .collect()
}

// NOTE: `export_checkpoint_result` in the Python version depends on
// `table_io::serialize_result_table`. That function is implemented under task
// #3; this module will gain its companion `export_checkpoint_result` once
// table_io lands. For now callers can use `load_checkpoint_rows` directly.
