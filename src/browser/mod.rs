//! Browser logic that is independent of any UI toolkit or web engine.
//!
//! Everything in this module is plain Rust state and pure functions, which
//! keeps it unit-testable without spawning a window.

pub mod navigation;
pub mod tab;
pub mod tabs;

pub use tab::{Tab, TabId};
pub use tabs::Tabs;
