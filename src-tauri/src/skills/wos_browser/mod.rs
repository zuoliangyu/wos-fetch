//! Chromium / WoS automation via chromiumoxide (CDP over WebSocket).
//!
//! Port target: `skills/wos_browser/` (browser.py, cdp.py, scraper.py,
//! search.py, tools.py — ~1500 lines combined). This is the largest and most
//! brittle subsystem in the project; see task #5.

#![allow(dead_code)]

use std::path::PathBuf;

pub mod browser;
pub mod cdp;
pub mod scraper;
pub mod search;
pub mod tools;

/// Where the dedicated WoS browser profile is stored. Equivalent of
/// `DEFAULT_BROWSER_PROFILE_ROOT` in the Python `tools.py`.
pub fn default_profile_root() -> PathBuf {
    dirs_home().join("wos-fetch-profile")
}

pub fn default_profile_dir() -> PathBuf {
    default_profile_root().join("browser-wos-profile")
}

fn dirs_home() -> PathBuf {
    // std lib lacks home_dir; rather than pull in `dirs`, use platform env vars.
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}
