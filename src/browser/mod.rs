//! Browser logic that is independent of any UI toolkit or web engine.
//!
//! Most of this module is plain Rust state and pure functions, which keeps
//! it unit-testable without spawning a window: `navigation`, `tab`,
//! `history`, `bookmarks` and `benchmark` hold no filesystem or UI
//! dependency. `persistence` and `perf_log` are the exceptions — thin,
//! deliberately "dumb" IO layers (JSON files for `persistence`; stderr/a
//! file for `perf_log`'s [`metrics::PerfRecord`] lines); see their module
//! doc comments.

pub mod benchmark;
pub mod blocklist;
pub mod bookmarks;
pub mod history;
pub mod metrics;
pub mod navigation;
pub mod omnibox;
pub mod perf_log;
pub mod persistence;
pub mod tab;
pub mod tabs;

pub use blocklist::FilterList;
pub use bookmarks::{BookmarkEntry, BookmarkStore};
pub use history::{
    date_bucket, group_by_date, search as search_history, HistoryDateBucket, HistoryEntry,
    HistoryGroup, HistoryStore,
};
pub use omnibox::{Candidate, CandidateKind, CandidateSource};
pub use tab::{Favicon, InvalidTabTransition, Tab, TabId, TabState};
pub use tabs::{ActivationEffect, Tabs};
