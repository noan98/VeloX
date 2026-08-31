//! Browser logic that is independent of any UI toolkit or web engine.
//!
//! Everything in this module is plain Rust state and pure functions, which
//! keeps it unit-testable without spawning a window.

pub mod blocklist;
pub mod navigation;
pub mod tab;

pub use blocklist::FilterList;
pub use tab::Tab;
