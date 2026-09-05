//! Browser logic that is independent of any UI toolkit or web engine.
//!
//! Most of this module is plain Rust state and pure functions, which keeps
//! it unit-testable without spawning a window: `navigation`, `tab`,
//! `history`, `bookmarks`, `benchmark`, `automation` and `suspension` hold no filesystem
//! or UI dependency. `persistence` and `perf_log` are the exceptions — thin,
//! deliberately "dumb" IO layers (JSON files for `persistence`; stderr/a
//! file for `perf_log`'s [`metrics::PerfRecord`] lines); see their module
//! doc comments.

pub mod automation;
pub mod benchmark;
pub mod blocklist;
pub mod bookmarks;
pub mod downloads;
pub mod gui_probe;
pub mod history;
pub mod input_history;
pub mod metrics;
pub mod navigation;
pub mod omnibox;
pub mod omnibox_candidates;
pub mod perf_log;
pub mod persistence;
pub mod ranking;
pub mod site_data;
pub mod suspension;
pub mod tab;
pub mod tabs;

pub use automation::{AutomationCommand, AutomationError};
pub use blocklist::FilterList;
pub use bookmarks::{BookmarkEditError, BookmarkEntry, BookmarkFolder, BookmarkStore};
pub use downloads::{DownloadEntry, DownloadId, DownloadState, DownloadStore};
pub use gui_probe::gui_probe_reason;
pub use history::{
    date_bucket, group_by_date, search as search_history, HistoryDateBucket, HistoryEntry,
    HistoryGroup, HistoryStore,
};
pub use input_history::{InputHistoryEntry, InputHistoryStore};
pub use omnibox::{Candidate, CandidateKind, CandidateSource};
pub use omnibox_candidates::{HistoryBookmarkSource, InputHistorySource};
pub use site_data::ClearOutcome;
pub use suspension::{SuspendReason, SuspensionPolicy};
pub use tab::{Favicon, InvalidTabTransition, Tab, TabId, TabState};
pub use tabs::{ActivationEffect, Tabs};
