//! ツールバーのパネル (履歴・ブックマーク・ダウンロード・設定) への
//! 表示内容のプッシュと、設定変更の反映。

use super::*;

/// [`refresh_history_panel`], but only when the history panel is actually
/// open (Issue #66).
///
/// `refresh_history_panel` always builds and pushes the full
/// (`config.history_panel_limit`-capped, up to 200 entries by default)
/// history list — exactly right while the panel is open, where it is what
/// makes a newly recorded visit show up live, but the panel is closed the
/// overwhelming majority of a browsing session, and every one of
/// `LoadFinished`/`PageTitleResolved`/`FaviconResolved` (each firing at
/// least once per page visit, sometimes all three for one visit) called it
/// unconditionally regardless — real IPC traffic measured with `velox-bench
/// ipc-summary` showed `set_history` among the largest Rust → JS payloads
/// in an ordinary session even though the panel was never opened (see
/// docs/performance-targets.md §18). This is the one place this issue found
/// real, safe-to-cut redundant traffic (the tab strip's equally frequent
/// `set_tabs` push is *not* gated this way — see §18 for why that one is
/// intentional: the tab strip, unlike this panel, is always visible).
///
/// Opening the panel (`ToolbarCommand::TogglePanel`) already refreshes it
/// immediately on its own (see that handler below), so skipping the push
/// while closed changes no visible behavior: a closed panel was never
/// rendering these pushes to begin with, and the moment it opens it gets
/// current data regardless of how long it had been closed.
pub(super) fn refresh_history_panel_if_open(
    window: &BrowserWindow,
    state: &AppState,
    config: &Config,
) {
    if window.open_panel() == Some(Panel::History) {
        refresh_history_panel(window, state, config);
    }
}

/// Push the most recent `config.history_panel_limit` history entries to the
/// toolbar, newest first, grouped into date sections (see
/// `browser::history::group_by_date` / docs/decisions.md D29) relative to
/// "now". Also the fallback the panel returns to when the search box is
/// cleared (see `ToolbarCommand::SearchHistory` below). Unconditional —
/// callers on the "closed the overwhelming majority of the time" path
/// (`LoadFinished`/`PageTitleResolved`/`FaviconResolved`) go through
/// [`refresh_history_panel_if_open`] instead; every other caller here
/// (`Ready`, `TogglePanel` opening the panel, an edit made *through* the
/// open panel itself) is a point where a push is always correct.
pub(super) fn refresh_history_panel(window: &BrowserWindow, state: &AppState, config: &Config) {
    let entries: Vec<&HistoryEntry> = state
        .history
        .entries_newest_first()
        .take(config.history_panel_limit)
        .collect();
    log_failure(
        "update history panel",
        window.set_history(&entries, now_unix()),
    );
}

/// Push the history entries matching `query` (see
/// `browser::history::HistoryStore::search` / docs/decisions.md D30) to the
/// toolbar, capped to `config.history_panel_limit` like the normal panel.
pub(super) fn search_history_panel(
    window: &BrowserWindow,
    state: &AppState,
    config: &Config,
    query: &str,
) {
    let entries: Vec<&HistoryEntry> = state
        .history
        .search(query)
        .into_iter()
        .take(config.history_panel_limit)
        .collect();
    log_failure(
        "update history panel (search)",
        window.set_history(&entries, now_unix()),
    );
}

/// Push the current bookmark tree (root entries + folders, each in manual
/// display order — see docs/decisions.md D32/D34) to both the bookmarks
/// panel and the always-visible bookmark bar. The two surfaces render the
/// exact same [`toolbar::BookmarksView`], built once here, so they can never
/// show a different bookmark set from each other.
pub(super) fn refresh_bookmarks_panel(window: &BrowserWindow, state: &AppState) {
    let view = toolbar::BookmarksView::from_store(&state.bookmarks);
    log_failure("update bookmarks panel", window.set_bookmarks(&view));
    log_failure("update bookmark bar", window.set_bookmark_bar(&view));
}

/// Push the download list to the toolbar, most recently started first.
pub(super) fn refresh_downloads_panel(window: &BrowserWindow, state: &AppState) {
    let entries: Vec<&DownloadEntry> = state.downloads.entries_newest_first().collect();
    log_failure("update downloads panel", window.set_downloads(&entries));
}

/// Push the settings screen's full contents (Issue #30, see
/// docs/decisions.md D67): the persisted, editable `Settings` document plus
/// the two read-only reference views (Shortcuts, Security) — see
/// [`toolbar::SettingsView`]. Called on `ready` (so the screen has data the
/// moment it is first opened) and again after every successful
/// `update_settings`/`reset_settings`, so the form always echoes back what
/// was actually persisted.
pub(super) fn refresh_settings_panel(window: &BrowserWindow, state: &AppState) {
    // `shortcut_reference` now renders each row's key label for the current
    // platform (Issue #38, docs/decisions.md D77) and so returns an owned
    // `Vec` rather than a `&'static` slice — kept alive in this local for
    // `SettingsView` to borrow.
    let shortcuts = shortcut_reference();
    let view = toolbar::SettingsView {
        settings: &state.settings,
        shortcuts: &shortcuts,
        site_permissions: state.site_permissions.records(),
    };
    log_failure("update settings panel", window.set_settings(&view));
}

/// Apply a new settings document from the settings screen
/// (`ToolbarCommand::UpdateSettings`/`ResetSettings`, Issue #30): sanitize
/// it (the same hardening `persistence::load_settings` + `sanitize` already
/// give a `settings.json` loaded at startup — an IPC payload is external
/// input the same way, see docs/decisions.md D67), persist it, replace
/// `state.settings`, and apply the two fields that take effect immediately
/// (`ui::window::BrowserWindow::set_theme`/`set_bookmark_bar_visible` —
/// Appearance) before re-rendering the panel. Every other field only takes
/// effect on the next restart, via `Config::apply_settings` in `run`.
///
/// **Multi-window (Issue #29/D68)**: `state.settings` is whole-process, the
/// same way `history`/`bookmarks` are (see `AppState::settings`'s doc
/// comment) — a change made from *any* window's settings screen must be
/// reflected in the chrome of *every* open window immediately, not just the
/// one that made it, and every open window's own settings screen (if it
/// happens to be open there too) must echo the same saved value back. This
/// is why the function takes `ui_windows: &HashMap<WindowId,
/// BrowserWindow>` as a whole and loops over every entry, rather than the
/// single already-resolved `window: &mut BrowserWindow` most other
/// `ToolbarCommand` handlers take — see `handle_user_event`'s doc comment
/// for why `UpdateSettings`/`ResetSettings` are intercepted there, before a
/// single window is resolved, the same way `NewWindow` is.
pub(super) fn apply_updated_settings(
    ui_windows: &HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    settings: Settings,
) {
    state.settings = settings.sanitize();
    let state = &*state;
    if let Some(dir) = &state.data_dir {
        log_failure(
            "save settings",
            persistence::save_settings(dir, &state.settings),
        );
    }
    let appearance = &state.settings.appearance;
    for window in ui_windows.values() {
        log_failure("apply theme", window.set_theme(appearance.theme));
        log_failure(
            "apply bookmark bar visibility",
            window.set_bookmark_bar_visible(appearance.show_bookmark_bar),
        );
        refresh_settings_panel(window, state);
    }
}
