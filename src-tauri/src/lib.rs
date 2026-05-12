//! wos-fetch — Tauri 2 backend
//!
//! Entry point invoked from `main.rs` (desktop) or the mobile entry attribute.

mod commands;
mod core;
mod error;
mod schemas;
mod skills;

pub use error::{AppError, AppResult};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wos_fetch_lib=info,warn".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::new_app_state())
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::upload_file,
            commands::wos_targets,
            commands::wos_launch,
            commands::wos_search,
            commands::run_screening,
            commands::browser_open,
            commands::run_fulltext_publishers,
            commands::run_fulltext,
            commands::task_status,
            commands::task_export_meta,
            commands::task_save_to,
            commands::generate_query,
            commands::generate_plan,
            commands::validate_llm,
            commands::scan_models,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
