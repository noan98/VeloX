//! UI layer: the browser window and the toolbar (browser chrome).
//!
//! The toolbar itself is a small HTML page rendered in a dedicated webview;
//! it talks to Rust over the webview IPC channel (see [`toolbar`]).

#[cfg(windows)]
pub mod save_dialog_windows;
pub mod toolbar;
#[cfg(windows)]
pub mod webview2_blocking;
pub mod window;

pub use window::{BrowserWindow, ContentShortcut, SitePolicies};
