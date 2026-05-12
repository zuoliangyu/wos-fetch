//! Thin CDP helpers layered on top of chromiumoxide.
//!
//! Port target: `skills/wos_browser/cdp.py` plus the `CDPSession` class in
//! `tools.py`. chromiumoxide already gives us a typed CDP client, so this
//! module mostly contains convenience wrappers (poll, wait_for_condition,
//! safe_evaluate_js) that match the Python helpers' signatures.

#![allow(dead_code)]

// TODO(task-5): re-export chromiumoxide types and add helpers
