//! Browser launch & debug-target discovery; orchestration glue.
//!
//! Port target: `skills/wos_browser/tools.py` (~770 lines, the largest single
//! file in the project). Responsibilities include:
//!
//! - Detect default Chromium-family browser (Edge/Chrome/Brave) from the
//!   Windows registry / candidate paths.
//! - Launch the browser with `--remote-debugging-port` + isolated user-data-dir.
//! - Reuse an existing debug session if one is already listening.
//! - List / open / close CDP targets via the `http://127.0.0.1:<port>/json/*`
//!   HTTP endpoints.
//! - Wrap each found target in a `CDPSession` for JS evaluation.

#![allow(dead_code)]

pub const DEFAULT_DEBUG_PORT: u16 = 9222;
pub const WOS_START_URL: &str = "https://www.webofscience.com/wos/woscc/advanced-search";

#[derive(Debug)]
pub struct LaunchResult {
    pub browser_name: String,
    pub executable_path: String,
    pub user_data_dir: String,
    pub debug_port: u16,
}

pub async fn launch_wos_browser(
    _chrome_path: Option<&str>,
    _start_url: Option<&str>,
    _port: u16,
) -> crate::AppResult<LaunchResult> {
    // TODO(task-5): port from skills/wos_browser/tools.py
    Err(crate::AppError::Browser(
        "launch_wos_browser not yet implemented".into(),
    ))
}
