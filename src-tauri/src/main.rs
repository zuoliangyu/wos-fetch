// Prevents additional console window on Windows in release, do NOT remove
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    wos_fetch_lib::run()
}
