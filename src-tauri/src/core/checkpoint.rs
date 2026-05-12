//! Resumable checkpoint storage for long-running pipelines.
//!
//! Port target: `core/checkpoint.py`. Persists per-row state to disk so a
//! relevance-screening or full-text-fetching run can be interrupted and
//! resumed without redoing completed work.

#![allow(dead_code)]

use std::path::Path;

pub struct Checkpoint {
    // TODO(task-2): mirror the fields in core/checkpoint.py
}

impl Checkpoint {
    pub fn load(_path: &Path) -> crate::AppResult<Self> {
        // TODO(task-2): port from core/checkpoint.py
        Err(crate::AppError::Other("checkpoint not yet implemented".into()))
    }

    pub fn save(&self, _path: &Path) -> crate::AppResult<()> {
        // TODO(task-2)
        Err(crate::AppError::Other("checkpoint not yet implemented".into()))
    }
}
