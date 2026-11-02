//! push-agent-tray: a tray icon that supervises the push-agent-bridge instances on this
//! machine. The logic lives in `push-tray-core`; this is the Tauri shell around it.
//!
//! `--autostart` (how the Run key or the XDG entry starts it) changes nothing: the tray
//! always starts the instances marked autostart.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

fn main() {
    app::run();
}
