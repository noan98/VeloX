//! UI layer: the browser window and the toolbar (browser chrome).
//!
//! The toolbar itself is a small HTML page rendered in a dedicated webview;
//! it talks to Rust over the webview IPC channel (see [`toolbar`]).

pub mod toolbar;
#[cfg(windows)]
pub mod webview2_blocking;
#[cfg(windows)]
pub mod webview2_print;
pub mod window;

pub use window::{BrowserWindow, ContentShortcut, PdfExportRequest, SitePolicies};
