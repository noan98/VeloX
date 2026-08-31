//! Browser logic that is independent of any UI toolkit or web engine.
//!
//! Most of this module is plain Rust state and pure functions, which keeps
//! it unit-testable without spawning a window: `navigation`, `tab`,
//! `history` and `bookmarks` hold no filesystem or UI dependency.
//! `persistence` is the one exception — a thin, deliberately "dumb" IO layer
//! that reads/writes `history`/`bookmarks` as JSON files; see its module
//! doc comment.

pub mod bookmarks;
pub mod history;
pub mod navigation;
pub mod persistence;
pub mod tab;

pub use bookmarks::{BookmarkEntry, BookmarkStore};
pub use history::{HistoryEntry, HistoryStore};
pub use tab::Tab;
