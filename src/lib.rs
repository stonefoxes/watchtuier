//! Library facade so integration tests (under `tests/`) can reach the modules.
//! The actual entry point lives in `src/main.rs`.

pub mod app;
pub mod cli;
pub mod network;
pub mod types;
pub mod ui;
pub mod usb;

/// System-tray front-end (StatusNotifierItem + freedesktop notifications).
#[cfg(target_os = "linux")]
pub mod tray;
