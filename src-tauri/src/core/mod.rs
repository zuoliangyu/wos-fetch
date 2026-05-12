//! Core utilities — pure logic + IO helpers shared across skills.
//!
//! Mirrors the Python `core/` package; each submodule is a port of the
//! correspondingly-named `.py` file. See `../../../core/<name>.py` for the
//! original reference implementation during the migration.

#![allow(dead_code)]

pub mod checkpoint;
pub mod json_protocol;
pub mod json_repair;
pub mod llm_client;
pub mod table_io;
pub mod text_normalize;
