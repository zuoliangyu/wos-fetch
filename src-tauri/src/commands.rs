//! Tauri command handlers — the IPC surface exposed to the React frontend.
//!
//! Each `#[tauri::command]` corresponds to one frontend `invoke(...)` call.
//! Commands here should be thin orchestrators that delegate to modules in
//! `core/`, `skills/`, etc.

/// Minimal handshake used during scaffolding to verify frontend <-> backend wiring.
#[tauri::command]
pub async fn ping() -> Result<String, String> {
    Ok("pong from rust".to_string())
}
