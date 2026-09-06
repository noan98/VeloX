//! Browser logic that is independent of any UI toolkit or web engine.
//!
//! Most of this module is plain Rust state and pure functions, which keeps
//! it unit-testable without spawning a window: `navigation`, `tab`,
//! `history`, `bookmarks`, `site_permissions`, `session`, `settings`,
//! `benchmark`, `automation`, `suspension` and `find` hold no filesystem
//! or UI dependency.
//! `persistence` and `perf_log` are the exceptions — thin, deliberately
//! "dumb" IO layers (JSON files for `persistence`; stderr/a file for
//! `perf_log`'s [`metrics::PerfRecord`] lines); see their module doc
//! comments.

pub mod automation;
pub mod benchmark;
pub mod blocklist;
pub mod bookmarks;
pub mod context_menu;
pub mod downloads;
pub mod find;
pub mod gui_probe;
pub mod history;
pub mod input_history;
pub mod metrics;
pub mod navigation;
pub mod omnibox;
pub mod omnibox_candidates;
pub mod perf_log;
pub mod persistence;
pub mod print;
pub mod ranking;
pub mod session;
pub mod settings;
pub mod site_data;
pub mod site_permissions;
pub mod subresource;
pub mod suspension;
pub mod tab;
pub mod tabs;
pub mod view_source;
pub mod window_id;
pub mod windows;

pub use automation::{AutomationCommand, AutomationError};
pub use blocklist::{FilterList, MatchContext, RuleResourceType};
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
pub use session::{SavedTab, SessionSnapshot};
pub use settings::{
    native_window_theme, shortcut_reference, AdvancedSettings, AppearanceSettings,
    DownloadsSettings, GeneralSettings, PerformanceSettings, PrivacySettings, ResolvedTheme,
    SearchSettings, Settings, ShortcutInfo, Theme, SETTINGS_SCHEMA_VERSION,
};
pub use site_data::ClearOutcome;
pub use site_permissions::{
    origin_of, PermissionDecision, PermissionKind, PermissionRecord, Resolution,
    SitePermissionStore,
};
pub use subresource::{is_blocked_resource, ResourceType, SiteExceptions};
pub use suspension::{SuspendReason, SuspensionPolicy};
pub use tab::{Favicon, InvalidTabTransition, Tab, TabId, TabState};
pub use tabs::{ActivationEffect, Tabs};
pub use view_source::{build_view_source_document, to_data_url, MAX_SOURCE_BYTES};
pub use window_id::WindowId;
pub use windows::Windows;
