//! Application wiring: owns the event loop and connects the UI layer to the
//! browser logic.
//!
//! Everything UI-related happens on the main thread. The webview callbacks
//! (IPC, navigation, page load) forward their payloads into the event loop as
//! [`UserEvent`]s, so all state lives in one place and no locking is needed.

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{
    ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget,
};

use crate::browser::automation::{self, AutomationCommand};
use crate::browser::downloads;
use crate::browser::navigation::Intent;
use crate::browser::perf_log::{IpcLog, PerfLog};
use crate::browser::suspension::{
    self, MemoryBudgetInput, MemorySample, SuspendReason, SuspensionPolicy,
};
use crate::browser::{
    context_menu, find, input_history, metrics, navigation, omnibox, persistence, print,
    shortcut_reference, site_data, view_source, ActivationEffect, BookmarkStore, ClearOutcome,
    DownloadEntry, DownloadId, DownloadStore, Favicon, FilterList, HistoryBookmarkSource,
    HistoryEntry, HistoryStore, InputHistorySource, InputHistoryStore, SavedWindow,
    SessionSnapshot, Settings, SiteExceptions, SitePermissionStore, Tab, TabId, Tabs, WindowId,
    Windows,
};
use crate::config::Config;
use crate::ui::toolbar::{self, Panel, ToolbarCommand};
use crate::ui::{BrowserWindow, ContentShortcut, PdfExportRequest, SitePolicies};

mod automation_script;
mod download_actions;
mod find_bar;
mod page_actions;
mod panels;
mod perf;
mod persist;
mod tab_suspension;

use automation_script::{
    handle_automation_command, poll_automation_wait_timeout, resolve_automation_wait_for_startup,
    resolve_automation_wait_if_matching, spawn_automation, AutomationWaitState,
};
use download_actions::{
    cancel_download, open_download, open_downloads_folder, record_download_completion,
    record_save_page_completion,
};
use find_bar::{
    apply_find_matches, close_find_bar, open_find_bar, step_find, update_find_query, FindDirection,
};
use page_actions::{
    open_view_source_tab, print_active_tab, request_save_page, request_view_source,
    save_active_tab_as_pdf, show_print_status,
};
use panels::{
    apply_updated_settings, refresh_bookmarks_panel, refresh_downloads_panel,
    refresh_history_panel, refresh_history_panel_if_open, refresh_settings_panel,
    search_history_panel,
};
use perf::{
    build_perf_log, record_perf_event, record_state_write, record_tab_latency, record_tab_suspend,
    spawn_rss_sampler, PageLoadTimers, PerfContext,
};
use persist::{persist_bookmarks, persist_history, persist_input_history, persist_session};
use tab_suspension::{
    choose_memory_sample_window, handle_tab_freeze_finished, spawn_memory_pressure_sampler,
    suspend_tab, sweep_tabs,
};

/// Events forwarded from webview callbacks into the main event loop.
///
/// **Multi-window (Issue #29, docs/decisions.md D68)**: every variant below
/// that a specific window's webview(s) can send now carries a [`WindowId`]
/// alongside whatever it already carried. This is not redundant with
/// `TabId`: a `TabId` is only unique within the window that issued it (see
/// `browser::Windows`'s module doc comment), so a `TabId` alone cannot say
/// which window's `Tabs` an event belongs to once more than one window is
/// open. `ui::window::BrowserWindow` bakes its own id into every event its
/// webviews send (`BrowserWindow::id`), the same way it has always tagged
/// per-tab events with a `TabId`.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Reopen the windows a restored session had beyond its first (Issue
    /// #149). Sent once, by `run`, *before* `event_loop.run` — the proxy
    /// queues it, so it is delivered on the loop's first pass.
    ///
    /// **Why not just build them in `run` like the primary window**: a
    /// `BrowserWindow` needs an `EventLoopWindowTarget`, which does not
    /// exist until the loop is running. Going through the loop also means
    /// these windows are opened by the exact same `open_new_window` path
    /// Ctrl/Cmd+N uses, rather than a second construction site that could
    /// drift from it.
    ///
    /// Keeping them out of `run` has a second, deliberate effect: startup's
    /// `process_start` → `window_created` decomposition (#182) still
    /// measures **one** window, which is what every §21–§26 number was
    /// taken against.
    RestoreWindows(Vec<SavedWindow>),
    /// Raw IPC message from window `.0`'s toolbar webview (JSON, see
    /// [`toolbar::parse_command`]).
    ToolbarMessage(WindowId, String),
    /// Tab `.1`'s content webview, in window `.0`, is about to navigate to
    /// this URL.
    NavigationStarted(WindowId, TabId, String),
    /// Content blocking refused a main-frame navigation in tab `.1` (window
    /// `.0`) to this URL.
    NavigationBlocked(WindowId, TabId, String),
    /// Content blocking refused a subresource request (image/script/
    /// XHR/fetch/...) in tab `.1` (window `.0`) to this URL. Windows/WebView2
    /// only for now — see docs/decisions.md D59 — sent from
    /// `ui::webview2_blocking::attach`'s `WebResourceRequested` handler.
    SubresourceBlocked(WindowId, TabId, String),
    /// Tab `.1`'s content webview (window `.0`) started loading this URL.
    LoadStarted(WindowId, TabId, String),
    /// Tab `.1`'s content webview (window `.0`) finished loading this URL.
    LoadFinished(WindowId, TabId, String),
    /// `document.title` for tab `tab_id` in window `window_id` came back
    /// from its content webview (see `BrowserWindow::fetch_page_title`), for
    /// the history entry `history_id`. Carries `tab_id`/`window_id` — not
    /// just `history_id` — precisely so this event can be routed back to the
    /// right `Tab` as well as the right history entry; a `tab_id`/
    /// `window_id` for a tab or window that has since closed is simply
    /// ignored, not a panic.
    PageTitleResolved {
        window_id: WindowId,
        tab_id: TabId,
        history_id: u64,
        title: String,
    },
    /// Tab `tab_id` (window `window_id`)'s favicon URL came back from its
    /// content webview (see `BrowserWindow::fetch_favicon`), for the history
    /// entry `history_id` (`0` when there is none — see
    /// `PageTitleResolved`'s doc comment, same sentinel, same reasoning,
    /// added for the history favicon in #18/D27). See docs/decisions.md D22:
    /// this is only ever a URL to try, never image bytes — the toolbar
    /// webview's own `<img>` tag performs the actual (async, non-blocking)
    /// fetch.
    FaviconResolved {
        window_id: WindowId,
        tab_id: TabId,
        history_id: u64,
        /// The page this favicon belongs to, as of when the fetch was
        /// started (see `BrowserWindow::fetch_favicon`'s doc comment) — used
        /// to also update a bookmarked page's favicon
        /// (`BookmarkStore::update_favicon_by_url`, Issue #19, see
        /// docs/decisions.md D34), since a bookmark is keyed by URL, not by
        /// tab or history id.
        page_url: String,
        url: String,
    },
    /// Window `.0`'s active content webview's devtools shortcut (F12 /
    /// Cmd+Opt+I) fired. Sent over a dedicated, tightly-restricted IPC
    /// channel, separate from the toolbar's — see docs/decisions.md D18.
    /// Carries no `TabId`: `BrowserWindow::open_devtools` always resolves
    /// that window's currently active tab itself, matching how the shortcut
    /// is only ever wired into the webview the user is actually looking at.
    OpenDevtoolsRequested(WindowId),
    /// One of the tab-management keyboard shortcuts fired while window
    /// `.0`'s content webview had focus (see `ui::window::ContentShortcut`
    /// and docs/decisions.md D18/D23). Sent over the same kind of dedicated,
    /// untrusted IPC channel as `OpenDevtoolsRequested`, for the same reason.
    ContentShortcut(WindowId, ContentShortcut),
    /// Window `.0`'s content webview asked to open a new window for `.1` — a
    /// `target="_blank"` link or `window.open()` — which VeloX always
    /// answers by opening `.1` as a new tab *in that same window* instead
    /// (see docs/decisions.md D25). Carries no `TabId`: like the shortcuts
    /// above, this is a window-wide action ("open a new tab in this
    /// window"), not something that needs to be routed back to whichever tab
    /// asked.
    NewTabRequested(WindowId, String),
    /// A content webview's `download_started_handler` accepted a download
    /// (see docs/decisions.md D28): `destination` is already the final,
    /// sanitized, collision-avoided path
    /// (`browser::downloads::prepare_destination`), chosen synchronously
    /// inside the wry callback before this event is sent. This only ever
    /// *registers* the download for the UI/`DownloadStore` — the decision
    /// to accept it already happened in `ui::window`.
    DownloadStarted {
        /// The window whose content webview started this download (Issue
        /// #29/D68). `DownloadStore` itself stays global/shared across every
        /// window (see `AppState`'s doc comment) — this is only used to
        /// decide which window's downloads panel to refresh immediately;
        /// see `handle_user_event`'s doc comment on that limitation.
        window_id: WindowId,
        url: String,
        file_name: String,
        destination: PathBuf,
        started_at: u64,
    },
    /// A content webview's `download_completed_handler` fired. Carries no
    /// id of its own — wry does not hand one back — so the handler resolves
    /// which [`DownloadId`] this refers to via
    /// `DownloadStore::resolve_completion`. `path` is `Some` on
    /// Linux/Windows and always `None` on macOS (see docs/decisions.md
    /// D28); `success` is the authoritative signal either way.
    DownloadCompleted {
        /// See [`Self::DownloadStarted`]'s `window_id` doc comment.
        window_id: WindowId,
        url: String,
        path: Option<PathBuf>,
        success: bool,
    },
    /// One step of a `VELOX_AUTOMATION_SCRIPT` (Issue #112, see
    /// docs/decisions.md D44 and `browser::automation`). Sent by a
    /// dedicated background thread spawned once at startup
    /// (`spawn_automation`) that walks the parsed script and proxies each
    /// non-`Wait` command through here in order, sleeping locally between
    /// steps for `Wait` — `Wait` itself never becomes an event. This is a
    /// delivery mechanism only: every variant is resolved on the main
    /// thread in `handle_automation_command` by calling the exact same
    /// tab-management functions `ToolbarCommand`/`ContentShortcut` already
    /// use (`open_new_tab`, `close_tab`, `apply_activation`, ...), not a
    /// new state-mutation path. `AutomationCommand::Quit` is special-cased
    /// in `run`'s event loop, before dispatch, to set `ControlFlow::Exit`.
    /// `AutomationCommand::WaitLoad`/`WaitStartup` (Issue #169/#173) *do*
    /// become an event like any other non-`Wait` command, but the sending
    /// thread then also blocks on a separate channel (`AutomationWaitState`)
    /// until the main thread resolves it — see `spawn_automation`'s doc
    /// comment.
    Automation(AutomationCommand),
    /// A fresh process-tree memory sample from `spawn_memory_pressure_sampler`
    /// (Issue #63): the total in bytes (PSS where the platform can read it,
    /// RSS otherwise — see that function). Only ever sent while
    /// `Config::suspension.memory_budget_bytes` is set. Stored as
    /// `AppState::pending_memory_sample` and consumed by exactly one
    /// `sweep_tabs` pass, so the memory signal acts once per sample.
    MemorySampled(MemorySample),
    /// The in-page find bar's DOM search finished in tab `tab_id` of window
    /// `window_id`'s content webview (Issue #43, see
    /// `ui::window::BrowserWindow::search_in_page` and docs/decisions.md
    /// D69), reporting `total` matches. A `tab_id` that no longer matches
    /// `window_id`'s find session (`Windows::find`) — the find bar closed,
    /// or moved to a different tab, while the DOM search was still running
    /// — is a safe no-op, a stale result simply never applies. Carries
    /// `window_id` for the same reason every other per-tab event does
    /// (Issue #29/D68): a `tab_id` alone cannot say which window's find
    /// session this result belongs to, since two windows can share the same
    /// `TabId` value.
    FindMatchesUpdated {
        window_id: WindowId,
        tab_id: TabId,
        total: usize,
    },
    /// Issue #46 ("名前を付けて保存"):
    /// `ui::window::BrowserWindow::request_save_page` chose a destination —
    /// via the native Windows Save-As dialog, or (macOS/Linux, see
    /// docs/decisions.md D76) this window's resolved download directory —
    /// and is now registering the save so its progress/outcome shows up in
    /// the same Downloads panel a real download does, rather than growing a
    /// second, parallel piece of UI for it. Same shape as
    /// [`Self::DownloadStarted`] (down to the field names) other than not
    /// coming from a wry download-started callback.
    SavePageStarted {
        window_id: WindowId,
        url: String,
        file_name: String,
        destination: PathBuf,
        started_at: u64,
    },
    /// The save started by a `SavePageStarted` event for `destination`
    /// finished — successfully (`error: None`) or not (`error:
    /// Some(reason)`, an already-Japanese, human-readable message suitable
    /// to show as-is, unlike [`Self::DownloadCompleted`]'s fixed generic
    /// failure string — Issue #46's "エラー時に原因を表示できる" acceptance
    /// criterion). Resolved to a [`DownloadId`] via
    /// `DownloadStore::resolve_completion(&url, Some(&destination))` — the
    /// exact-destination-match path that function already has, see its doc
    /// comment — since a save's destination is always known exactly up
    /// front, unlike a wry download completion's ambiguous callback.
    SavePageFinished {
        window_id: WindowId,
        url: String,
        destination: PathBuf,
        error: Option<String>,
    },
    /// The Windows-only headless PDF export (Issue #40, see
    /// `ui::webview2_print::export_as_pdf` and docs/decisions.md D75)
    /// finished — `success`/`error` come from WebView2's own
    /// `PrintToPdfCompletedHandler`, so unlike `ContentShortcut::Print`'s
    /// `wry::WebView::print()` path this can report a *real* failure (disk
    /// full, permission denied, ...), not just "could the call be
    /// dispatched at all". Carries `window_id` for the same reason every
    /// other per-tab/per-window async result does (Issue #29/D68): the
    /// window that requested this export may have since closed, or may not
    /// be the one a stale `TabId` now resolves to in a different window.
    /// `tab_id` is not currently used to route the result anywhere more
    /// specific than "that window's shared print-status banner" (there is
    /// no per-tab export UI), but is kept for parity with every other
    /// per-tab event and for a future per-tab status display.
    PdfExportFinished {
        window_id: WindowId,
        tab_id: TabId,
        destination: PathBuf,
        success: bool,
        error: Option<String>,
    },
    /// `ICoreWebView2_3::TrySuspend` finished for tab `tab_id` in window
    /// `window_id` (Issue #243, `ui::webview2_suspend::try_suspend`).
    ///
    /// `TrySuspend` is a "try" API — it can decline (the page is playing
    /// media, a download is in flight) — and it is asynchronous, so the
    /// answer comes back here rather than at the call site, exactly like
    /// [`Self::PdfExportFinished`] (D75).
    ///
    /// **A `success: false` is not cosmetic.** By the time this arrives the
    /// tab is already marked suspended on the `browser::Tabs` side, so a
    /// refusal would leave a "suspended" tab holding a full, awake webview —
    /// the worst of both. The handler therefore falls back to discarding the
    /// webview for that tab, which is what the tab would have got under
    /// `SuspendMechanism::Discard` anyway.
    TabFreezeFinished {
        window_id: WindowId,
        tab_id: TabId,
        success: bool,
        error: Option<String>,
    },
    /// The active tab's page markup came back from
    /// `BrowserWindow::fetch_page_source` for View Source (Issue #45, see
    /// docs/decisions.md D72). `page_url` is the page it belongs to, as
    /// captured when the fetch was requested; `html` is the page's raw,
    /// **unescaped** `outerHTML` — `open_view_source_tab` is the only place
    /// that turns it into something safe to display
    /// (`browser::view_source::build_view_source_document`).
    ///
    /// Carries `window_id` (Issue #29/D68) so the resulting View Source tab
    /// opens in the window the request came from, for the same reason
    /// `FindMatchesUpdated` above does.
    ViewSourceReady {
        window_id: WindowId,
        page_url: String,
        html: String,
    },
    /// A right-click landed in tab `tab_id` (window `window_id`)'s content
    /// webview (Issue #39, see docs/decisions.md D78). `x`/`y` are already
    /// clamped to a finite, non-negative range; `raw` is otherwise
    /// completely untrusted (see `ui::window::parse_context_menu_open` and
    /// `browser::context_menu`'s module doc comment) — `handle_user_event`
    /// must run it through `context_menu::sanitize` before it is safe to
    /// act on or display. Carries `tab_id` explicitly (unlike
    /// `ContentShortcut`): unlike a keyboard shortcut, this arrives from a
    /// specific tab's own webview closure, so which tab it came from is
    /// always known precisely, never assumed to be "the active one".
    ContextMenuRequested {
        window_id: WindowId,
        tab_id: TabId,
        x: f64,
        y: f64,
        raw: context_menu::RawMenuContext,
    },
    /// The content webview reported that menu row `index` was clicked (see
    /// `ui::window::CONTEXT_MENU_ACTION_PREFIX`). `index` is resolved
    /// against `Windows::context_menu(window_id)`'s own
    /// [`context_menu::OpenContextMenu`] — the exact list that menu was
    /// rendered with — never re-derived, so a stale or out-of-range index
    /// (including one for an already-closed menu) simply resolves to
    /// nothing rather than acting on the wrong target.
    ContextMenuActionSelected {
        window_id: WindowId,
        tab_id: TabId,
        index: usize,
    },
    /// The context menu was dismissed with no selection (clicked outside
    /// it, or Esc) — see `ui::window::CONTEXT_MENU_CLOSE_MESSAGE`.
    ContextMenuClosed(WindowId, TabId),
    /// This tab's page reported form input (Issue #272, D142) — see
    /// `ui::window::FORM_INPUT_MESSAGE`. Fires on every `input` event, so
    /// the handler must be cheap and idempotent (`Tab::mark_form_input`
    /// is both).
    FormInputDetected(WindowId, TabId),
}

/// All mutable application state, gathered so the event handlers below take
/// one argument instead of a growing list of `&mut` parameters.
///
/// **Multi-window (Issue #29, D68)**: `windows` replaces what used to be a
/// single `tabs: Tabs` field — every other field here (`history`,
/// `bookmarks`, `input_history`, `downloads`, `perf`) stays whole-process/
/// shared across every window; only whether a given window's activity is
/// *recorded* into them is now per-window (Issue #27, D74 — see
/// [`window_is_private`], which replaced a single whole-process
/// `history_enabled` bool once two windows could disagree on privacy).
struct AppState {
    windows: Windows,
    /// What a newly opened window (Ctrl/Cmd+N, `ToolbarCommand::NewWindow`/
    /// `ContentShortcut::NewWindow`/`AutomationCommand::NewWindow`) is built
    /// with: the same site-scoped policies every other window shares
    /// (`Arc`s, cheap to clone). See docs/decisions.md D68.
    ///
    /// The `EventLoopProxy` a new `BrowserWindow` also needs is deliberately
    /// *not* a field here (unlike `site_policies`) — it is threaded through
    /// as a plain function argument instead (`open_new_window`), closed over
    /// by `run`'s event loop the same way `homepage`/`suspension_policy` are.
    /// Keeping `AppState` free of any `wry`/`tao` handle is what lets
    /// `state_with_history_enabled` build one in a unit test with no
    /// display — see docs/architecture.md's D20 layering rule, which this
    /// preserves even though `AppState` (unlike `browser::`) is allowed to
    /// depend on the UI layer.
    site_policies: SitePolicies,
    history: HistoryStore,
    bookmarks: BookmarkStore,
    /// Previously-submitted search queries (Issue #20) — see
    /// docs/decisions.md D38. Gated by [`window_is_private`] for recording
    /// the same way `history`/`bookmarks` are, but — like `history` — still
    /// read from for candidates regardless of privacy; see
    /// `record_input_history_if_enabled` and D39/D74.
    input_history: InputHistoryStore,
    /// Where `history`/`bookmarks`/`input_history` are persisted; `None`
    /// when no data directory could be resolved (see
    /// `persistence::default_data_dir`), in which case all three stores
    /// stay in-memory only for this run.
    data_dir: Option<PathBuf>,
    /// The [`SessionSnapshot`] most recently written to `session.json` by
    /// [`persist_session`] (Issue #67) — `None` until the first successful
    /// write. `sync_tab_strip` calls `persist_session` after nearly every
    /// tab-affecting event, but most of those events do not change what a
    /// restore needs (`SessionSnapshot` only carries url/title/favicon per
    /// tab, not the loading flag `NavigationStarted`/`LoadFinished` flip);
    /// caching the last-written value lets `persist_session` skip the
    /// `fs::create_dir_all`/`serde_json::to_string_pretty`/`fs::write`
    /// sequence entirely when the freshly-built snapshot is unchanged,
    /// instead of re-writing byte-identical content to disk. See
    /// docs/decisions.md D86 for the measurement that motivated this.
    last_persisted_session: Option<SessionSnapshot>,
    /// Tab-create/switch latency logging (Issue #13). `None` when
    /// `config.perf_metrics` is off, in which case `record_tab_latency`
    /// below is a single `Option::is_none` check — no extra `Instant::now()`
    /// call beyond the one `ToolbarCommand::NewTab`/`ActivateTab` already
    /// makes for `Tabs::open_at`/`activate_at`'s own bookkeeping. See
    /// docs/architecture.md, "Performance extension points".
    perf: Option<PerfContext>,
    /// Session-scoped download list (Issue #16). Not persisted to disk — see
    /// docs/decisions.md D28.
    downloads: DownloadStore,
    /// The most recent `UserEvent::MemorySampled` not yet acted on by
    /// `sweep_tabs` (Issue #63). Consumed exactly once per pass — by
    /// whichever window `next_memory_sample_window` names — so each sample
    /// drives the memory signal exactly once — re-using a stale sample on
    /// every loop pass would keep suspending tabs before the previous
    /// sweep's effect is even visible in the numbers.
    pending_memory_sample: Option<MemorySample>,
    /// Which window's `sweep_tabs` call should consume the next fresh
    /// memory sample (Issue #186, docs/decisions.md D90). A sample is
    /// consumed by exactly one window's sweep per event-loop pass — with
    /// more than one window open, always handing it to whichever window
    /// happens to be swept first (`state.windows.ids()`'s fixed,
    /// window-creation order) meant a window with nothing eligible to
    /// suspend (e.g. a single always-active tab) silently discarded every
    /// sample forever, starving every *other* window's memory signal no
    /// matter how far over budget the whole process tree was. Round-
    /// robining which window gets first claim on each fresh sample fixes
    /// this without suspending more than one window's worth of tabs per
    /// sample (unlike handing the same sample to every window, which would
    /// multiply a single over-budget reading into simultaneous over-
    /// reclaim across every open window — see D90 for the measurements
    /// behind this choice). `None` means "no preference yet" (defaults to
    /// the first window in `ids()` order); `run`'s sweep loop advances it
    /// after handing a sample out, to whichever window comes right after
    /// the served one in the *current* window order — self-healing if that
    /// window has since closed, since the lookup simply falls back to the
    /// front of the list.
    next_memory_sample_window: Option<WindowId>,
    /// Persisted, user-editable settings (Issue #30, see
    /// docs/decisions.md D67) — the settings screen's current, already-
    /// sanitized value. `Config` was already merged with this once at
    /// startup (`Config::apply_settings`, in `run` below, before
    /// `BrowserWindow::new`); this copy is what the settings screen itself
    /// reads from and writes back to (`refresh_settings_panel`,
    /// `apply_updated_settings`). Whole-process, unlike per-window privacy
    /// (D74) — every window's settings screen shows and edits the same
    /// value; see D68's multi-window/#30 integration for why appearance
    /// changes are pushed to every open window, not just the one whose
    /// settings screen made the change.
    settings: Settings,
    /// Per-origin permission decisions (Issue #24, docs/decisions.md D60),
    /// the same `Arc` handed to `BrowserWindow`'s `with_permission_handler`
    /// wiring — kept here too so the settings screen's Security tab can
    /// display them (read-only; see `browser::settings`'s module doc
    /// comment for why this issue does not add a write path from the
    /// settings screen).
    site_permissions: Arc<SitePermissionStore>,
}

/// `state.windows.tabs_mut(window_id)`, indexed the same way
/// `browser::tabs::Tabs::active()`/`active_mut()` index their own
/// `self.tabs[self.active]` — a documented, structural invariant, not a
/// fallible operation being shortcut (Issue #29/D68).
///
/// Every call site of this helper is reached only after the caller already
/// resolved `window_id` to a live `&mut BrowserWindow` in `ui_windows` (see
/// `handle_user_event`'s doc comment, and every dispatcher's own —
/// `handle_toolbar_command`/`handle_content_shortcut`/
/// `handle_automation_command`/`sweep_tabs` all receive `window: &mut
/// BrowserWindow` already resolved by their caller from that same
/// `window_id`). `ui_windows` and `state.windows` are only ever changed
/// together, both inside `open_new_window` (insert) and
/// `close_window_by_tao_id` (remove), on this single-threaded event loop —
/// there is no `await` point or other thread that could remove one without
/// the other between that resolution and this call — so a `window_id`
/// already known-good in `ui_windows` is known-good here too, exactly the
/// same guarantee `Tabs`' own `self.active` index relies on. This keeps
/// every leaf function below as close as possible to what it looked like
/// before multi-window (`state.tabs.foo()` becomes `tabs_of(state,
/// window_id).foo()`) instead of threading an `Option` through every one of
/// them for a case that cannot actually happen here.
fn tabs_of(state: &mut AppState, window_id: WindowId) -> &mut Tabs {
    state
        .windows
        .tabs_mut(window_id)
        .expect("window_id was just resolved against the same ui_windows/state.windows pair")
}

/// Build the window and run the event loop. Only returns on setup failure;
/// once running, the process exits with the event loop.
///
/// `process_start` is the earliest timestamp the caller could capture
/// (ideally the top of `main`); it only feeds the startup-timing report and
/// is otherwise unused when `config.perf_metrics` is off.
pub fn run(mut config: Config, process_start: Instant) -> Result<(), Box<dyn Error>> {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    // Issue #182 (D92). Captured unconditionally, unlike every other
    // checkpoint in this function: whether metrics are on at all is not
    // settled until `apply_settings` below has read `settings.json` (the
    // Advanced tab can flip `perf_metrics`), and this timestamp has to be
    // taken before that. That costs one `Instant::now()` per process on the
    // metrics-off path — the D19/D43 "no clock reads when metrics are off"
    // convention is about per-event hot paths and sampler threads, not a
    // single read on a path that runs once. It is folded into
    // `StartupTimestamps` a few statements below, once one exists.
    let event_loop_built = Instant::now();
    let proxy = event_loop.create_proxy();
    // Cloned before `proxy` is moved into `BrowserWindow::new` below — see
    // `spawn_automation`'s call site further down, once `AppState` exists.
    let automation_proxy = proxy.clone();
    // Same story for the memory sampler (Issue #63), spawned further down
    // once the config's suspension policy has been read.
    let memory_sampler_proxy = proxy.clone();

    // Issue #30 (D67): the settings screen's persisted, user-editable
    // settings are loaded and merged onto `config` before anything below
    // reads it — `blocklist`/`site_exceptions` (Privacy tab),
    // `startup`/`perf_metrics` (Advanced tab), and `Tabs`/`BrowserWindow`
    // (General/Performance/Downloads tabs) all consult `config` only once,
    // at construction time.
    //
    // Critically, `apply_settings` only ever runs when a `settings.json`
    // *actually exists* on disk — a missing one (first run, or the data
    // directory could not be resolved) or a corrupt one (see
    // `persistence::load_settings`'s contract) must never be treated as "the
    // user wants every setting reset to its blind default": that would
    // silently discard whatever `VELOX_*` environment variable/CLI flag
    // `Config::from_env_and_args` above just resolved, on every single
    // launch that has never touched the settings screen. Instead,
    // `Config::to_settings` seeds `AppState::settings` from `config` as it
    // already stands (env/CLI included) — see its doc comment, and
    // docs/decisions.md D67 for the bug this fixed during development
    // (VELOX_* env vars going silently inert the moment this feature
    // landed, caught by this project's own integration test suite).
    // 環境変数を読むだけの純粋な解決なので一度だけ行い、`settings.json`・
    // サイト権限・セッション復元・履歴類のすべてで共有する。
    let data_dir = persistence::default_data_dir();
    let settings = match data_dir.as_deref().and_then(persistence::load_settings) {
        Some(loaded) => {
            let sanitized = loaded.sanitize();
            config.apply_settings(&sanitized);
            sanitized
        }
        None => config.to_settings(),
    };

    // `.then(...)` short-circuits: when metrics are off, no `Instant` is
    // captured here and `startup` stays `None`, so every checkpoint below
    // becomes a single cheap `Option` check with no clock read.
    let mut startup = config
        .perf_metrics
        .then(|| metrics::StartupTimestamps::new(process_start));
    if let Some(startup) = startup.as_mut() {
        startup.mark_event_loop_built(event_loop_built);
    }

    // Built once, shared with the RSS sampler thread, every perf-logging
    // call site in this file, and (Issue #66) every `BrowserWindow`'s
    // Rust → JS IPC instrumentation, via `Arc::clone`/`IpcLog::clone`.
    // `None` when metrics are off, matching `startup`'s `.then(...)`
    // short-circuit above. Built here — before `BrowserWindow::new` below —
    // rather than where it used to sit (right before the RSS sampler is
    // spawned) specifically so the primary window's own construction can
    // already pass its `ipc_log` in.
    let perf_log: Option<Arc<PerfLog>> = config.perf_metrics.then(|| build_perf_log(&config));
    let ipc_log = perf_log.clone().map(|log| IpcLog::new(log, process_start));

    let blocklist = Arc::new(build_blocklist(&config));
    let site_exceptions = Arc::new(build_site_exceptions(&config));

    // Loaded here (rather than down with `history`/`bookmarks`/
    // `input_history` below) because `site_permissions` — unlike those
    // three — must exist before `BrowserWindow::new` builds the first
    // tab's content webview: its `with_permission_handler` wiring
    // (docs/decisions.md D60) needs the store from the very first
    // permission request, not just from whenever `AppState` gets around to
    // loading it. Session restore (Issue #25, see docs/decisions.md D65)
    // likewise needs `data_dir` before `Tabs`/`BrowserWindow` are built,
    // since it decides what the *first* `Tabs` looks like.
    //
    // 以前は用途ごとに別々に解決して 1 行ずつ警告していたため、ログの
    // 互換性を保つよう 2 行とも出す。
    if data_dir.is_none() {
        eprintln!(
            "velox: could not resolve a data directory (no VELOX_DATA_DIR/HOME/APPDATA); \
             history, bookmarks, and site permissions will not be saved this session"
        );
        eprintln!(
            "velox: could not resolve a data directory (no VELOX_DATA_DIR/HOME/APPDATA); \
             history, bookmarks, and session restore will not be saved this session"
        );
    }
    let site_permissions = Arc::new(
        data_dir
            .as_deref()
            .map(persistence::load_site_permissions)
            .unwrap_or_default(),
    );
    // Kept for `AppState` too (Issue #30's settings screen, Security tab —
    // see docs/decisions.md D67): a read-only view, cloned before the
    // original moves into `SitePolicies` below.
    let site_permissions_for_state = Arc::clone(&site_permissions);

    // Issue #25 (D65): restore the previous session's tabs when the setting
    // is on and a usable snapshot exists; otherwise (setting off, no data
    // directory, no file yet, or a corrupt/empty one —
    // `SessionSnapshot::sanitize` returns `None` for both) fall back to the
    // pre-#25 behavior of a single tab at the homepage. A corrupt or
    // truncated `session.json` must never stop VeloX from starting, so
    // every step here degrades to `None` instead of propagating an error.
    // Private mode never restores, matching D14's "a private launch leaves
    // no trace of — and inherits no trace from — any session" rule.
    let restored_session = if config.restore_previous_session && !config.private {
        data_dir
            .as_deref()
            .and_then(persistence::load_session)
            .and_then(SessionSnapshot::sanitize)
    } else {
        None
    };
    // Issue #29 (D68): `Windows` starts with exactly one window — restored
    // from the previous session's snapshot when one applies, a fresh single
    // tab at the homepage otherwise. A window the user opens later
    // (Ctrl/Cmd+N) always starts fresh at the homepage; only the ones the
    // snapshot carries are restored (Issue #149/D141 — see just below).
    let mut windows = Windows::new_with_privacy(config.homepage.clone(), config.private);
    // Issue #149: the windows after the first cannot be built here — a
    // `BrowserWindow` needs an `EventLoopWindowTarget`, which only exists
    // once `event_loop.run` is going. They are carried into the loop and
    // opened on its first pass (`UserEvent::RestoreWindows`), so startup's
    // `process_start` → `window_created` decomposition (#182) still measures
    // exactly one window, as every §21-§26 number was taken against.
    let mut restored_secondary: Vec<SavedWindow> = Vec::new();
    let primary_id = match restored_session {
        Some(snapshot) => {
            // Replace the placeholder window `Windows::new` just made above
            // with the actually-restored one, so there is still exactly one
            // window (never zero, never two) at this point.
            let placeholder = windows.ids().next().expect("Windows::new opens one window");
            windows.close_window(placeholder);
            restored_secondary = snapshot.secondary().to_vec();
            let primary = snapshot
                .primary()
                .expect("sanitize returns None rather than an empty window list");
            windows.open_restored_window(&primary.tabs, primary.active_index)
        }
        None => windows.ids().next().expect("Windows::new opens one window"),
    };
    // Issue #25/D65: the active tab's own `current_url` — not necessarily
    // `config.homepage` once session restore is in play — is what the
    // first real webview must load.
    let primary_tabs = windows
        .tabs(primary_id)
        .expect("primary window just opened");
    let initial_url = primary_tabs.active().current_url().to_owned();
    let initial_tab = primary_tabs.active_id();
    // Kept around in `AppState` (Issue #29/D68): every window opened later
    // (Ctrl/Cmd+N) needs its own `EventLoopProxy` clone and the exact same
    // site-scoped policies the first window was built with — see
    // `SitePolicies`'s doc comment for why cloning it is cheap.
    let site_policies = SitePolicies {
        blocklist,
        site_exceptions,
        site_permissions,
    };
    let window_event_proxy = proxy.clone();
    // Issue #182 (D92): everything from `event_loop_built` to here is
    // VeloX's own Rust work — `settings.json`, the blocklist and
    // site-exception lists, site permissions, session restore, `Windows`/
    // `Tabs`. It is the only span of `process_start` → `window_created`
    // that VeloX can shorten without touching tao or the web engine, so it
    // gets its own checkpoint rather than being lumped in with them.
    if let Some(startup) = startup.as_mut() {
        startup.mark_pre_window_setup_done(Instant::now());
    }
    // `Some` only while metrics are on; `BrowserWindow::new` fills in the
    // two checkpoints inside window construction (see its `build_timings`
    // parameter) and they are folded into `startup` right after it returns.
    let mut window_build_timings = startup.is_some().then(metrics::WindowBuildTimings::default);
    let primary_window = BrowserWindow::new(
        &event_loop,
        primary_id,
        &config,
        proxy,
        initial_tab,
        &initial_url,
        site_policies.clone(),
        config.private,
        ipc_log.clone(),
        window_build_timings.as_mut(),
    )?;
    // Every open native window, keyed by the same `browser::WindowId`
    // `windows: Windows` above uses for its logical (tab-owning) half — see
    // docs/decisions.md D68. Kept as a separate map (rather than folding
    // `BrowserWindow` into `AppState`) because `AppState` is meant to be the
    // UI/engine-independent half of this file's state; `BrowserWindow` is
    // the one type that actually owns `wry`/`tao` handles.
    let mut ui_windows: HashMap<WindowId, BrowserWindow> = HashMap::new();
    ui_windows.insert(primary_id, primary_window);
    // Which window `AutomationCommand`s currently target (Issue #112).
    // Automation predates multi-window and addresses tabs by their position
    // in "the" tab strip; extending it to address a specific window by name
    // is left to a follow-up (see the PR description) — for now every script
    // starts out targeting the primary window and can point itself at a
    // freshly opened one with a `new_window` step (`AutomationCommand::NewWindow`).
    let mut automation_window = primary_id;
    if let Some(startup) = startup.as_mut() {
        // Issue #182: fold in what `BrowserWindow::new` measured from the
        // inside. Both timestamps were taken during the call that just
        // returned, so they belong before `mark_window_created` below —
        // which is what closes the span they sit in.
        if let Some(timings) = window_build_timings {
            if let Some(at) = timings.native_window_built {
                startup.mark_native_window_built(at);
            }
            if let Some(at) = timings.toolbar_webview_built {
                startup.mark_toolbar_webview_built(at);
            }
        }
        startup.mark_window_created(Instant::now());
    }

    // Issue #30 (D67): the bookmark bar's shown/hidden state is now
    // persisted (previously session-only, resetting on every restart — see
    // the old note in docs/architecture.md this issue resolves). Applied
    // once, right after the primary window exists; the toolbar's `ready`
    // handler later echoes this same value back via
    // `window.bookmark_bar_visible()`. Every window opened later
    // (Ctrl/Cmd+N) gets the same treatment in `open_new_window` (Issue
    // #29/D68) — this setting is whole-process, not per-window.
    if let Some(primary_window_ui) = ui_windows.get_mut(&primary_id) {
        log_failure(
            "apply initial bookmark bar visibility",
            primary_window_ui.set_bookmark_bar_visible(settings.appearance.show_bookmark_bar),
        );
    }

    if let (Some(interval), Some(log)) = (config.perf_rss_interval, perf_log.clone()) {
        spawn_rss_sampler(interval, log, process_start);
    }

    let homepage = config.homepage.clone();
    let suspension_policy = config.suspension;
    // Issue #63: the memory signal needs a sampler; the other two signals
    // (idle time, live-tab cap) are evaluated from `Tabs` alone on every
    // loop pass and need no thread. Only spawned when a budget is set, so
    // the default configuration walks `/proc` exactly never.
    if suspension_policy.memory_budget_bytes.is_some() {
        spawn_memory_pressure_sampler(
            suspension_policy.memory_check_interval,
            config.memory_budget_input,
            memory_sampler_proxy,
        );
    }
    // One timer per tab: background tabs load concurrently with the active
    // one, so a single shared timer would have their loads overwrite each
    // other's start times. Keyed by `(WindowId, TabId)`, not `TabId` alone
    // (Issue #29/D68): a `TabId` is only unique within its own window, so
    // two windows loading a page at the same moment could otherwise share
    // one timer entry and corrupt each other's duration.
    let mut page_load_timers: PageLoadTimers = HashMap::new();
    // Issue #169: the automation thread's other half of `AutomationWaitState`
    // — `automation_wait_rx` is moved into `spawn_automation` below (when a
    // script is actually running), `automation_wait` (holding the `Sender`
    // half) stays on the main thread and is threaded through the event loop
    // like `page_load_timers`.
    let (automation_wait_tx, automation_wait_rx) = mpsc::channel::<()>();
    let mut automation_wait = AutomationWaitState {
        pending: None,
        notify: automation_wait_tx,
        startup_reported: false,
    };

    let history = data_dir
        .as_deref()
        .map(persistence::load_history)
        .unwrap_or_default();
    let bookmarks = data_dir
        .as_deref()
        .map(persistence::load_bookmarks)
        .unwrap_or_default();
    let input_history = data_dir
        .as_deref()
        .map(persistence::load_input_history)
        .unwrap_or_default();

    let mut state = AppState {
        windows,
        site_policies,
        history,
        bookmarks,
        input_history,
        data_dir,
        last_persisted_session: None,
        perf: perf_log
            .clone()
            .map(|log| PerfContext { process_start, log }),
        downloads: DownloadStore::new(),
        pending_memory_sample: None,
        next_memory_sample_window: None,
        settings,
        site_permissions: site_permissions_for_state,
    };

    // Everything above (history/bookmarks/input-history load, `AppState`
    // build) is synchronous Rust code that runs before the GTK/webview
    // event loop even starts pumping — see D43. Marking it here isolates
    // that cost from whatever happens inside the toolbar webview itself.
    if let Some(startup) = startup.as_mut() {
        startup.mark_rust_setup_done(Instant::now());
    }

    // Issue #112: only when `VELOX_AUTOMATION_SCRIPT` names a file, read
    // and parse it once, right here at startup, and hand it to a
    // background thread that drives it. See docs/decisions.md D44 and
    // `browser::automation`'s module doc comment for why this is a
    // read-once opt-in file instead of any kind of listening
    // socket/RPC server. A missing/unreadable file or a parse error is
    // logged and otherwise ignored — never fatal, matching every other
    // `log_failure`-style guard in this file.
    if let Some(script_path) = std::env::var_os("VELOX_AUTOMATION_SCRIPT") {
        match std::fs::read_to_string(&script_path) {
            Ok(text) => match automation::parse_script(&text) {
                Ok(commands) => spawn_automation(automation_proxy, commands, automation_wait_rx),
                Err(err) => eprintln!(
                    "velox: VELOX_AUTOMATION_SCRIPT {script_path:?} は解析できません: {err}"
                ),
            },
            Err(err) => {
                eprintln!("velox: VELOX_AUTOMATION_SCRIPT {script_path:?} を読み込めません: {err}")
            }
        }
    }

    // Issue #149: hand the previous session's extra windows to the loop.
    // The proxy queues this, so it arrives on the loop's first pass — the
    // earliest point at which an `EventLoopWindowTarget` exists to build a
    // `BrowserWindow` against. Sending nothing when there is nothing to
    // restore keeps the common path (no session, or one window) byte for
    // byte what it was.
    if !restored_secondary.is_empty()
        && window_event_proxy
            .send_event(UserEvent::RestoreWindows(restored_secondary))
            .is_err()
    {
        // The loop is already gone, so there is nothing to restore into.
        // Never fatal — the primary window is up either way.
        eprintln!("velox: 前回のセッションの 2 枚目以降のウィンドウを復元できませんでした");
    }

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            // Issue #29 (D68): `window_id` here is `tao`'s own id for the
            // native window the event happened in — distinct from
            // `browser::WindowId` — so every window-scoped `WindowEvent` is
            // first resolved to *which* `BrowserWindow` it belongs to via
            // `BrowserWindow::tao_id`, rather than assuming there is only
            // one.
            Event::WindowEvent {
                window_id: tao_id,
                event: WindowEvent::CloseRequested,
                ..
            } => close_window_by_tao_id(
                &mut ui_windows,
                &mut state,
                &mut page_load_timers,
                tao_id,
                control_flow,
            ),
            Event::WindowEvent {
                window_id: tao_id,
                event: WindowEvent::Resized(_),
                ..
            } => {
                if let Some(window) = window_by_tao_id_mut(&mut ui_windows, tao_id) {
                    log_failure("resize layout", window.sync_layout());
                }
            }
            Event::UserEvent(user_event) => {
                if debug_logging_enabled() {
                    eprintln!("velox[debug]: {user_event:?}");
                }
                if let Some(log) = perf_log.as_deref() {
                    // Issue #173: detect the exact moment `record_perf_event`
                    // (via `mark_startup`) writes the `startup` perf record —
                    // a `startup: Some -> None` transition, the same signal
                    // `mark_startup` itself uses to write the record exactly
                    // once (see its own doc comment) — so a pending
                    // `wait_startup` can be resolved right here without
                    // threading `automation_wait` through
                    // `record_perf_event`/`mark_startup` themselves (which
                    // would otherwise need to know about the automation
                    // machinery at all, unlike every other perf event they
                    // handle).
                    let startup_was_pending = startup.is_some();
                    record_perf_event(
                        &mut startup,
                        &mut page_load_timers,
                        log,
                        process_start,
                        &user_event,
                    );
                    if startup_was_pending && startup.is_none() {
                        resolve_automation_wait_for_startup(&mut automation_wait);
                    }
                }
                // `quit` (Issue #112) is handled here, before dispatch,
                // exactly like `WindowEvent::CloseRequested` above — it
                // needs `control_flow`, which `handle_user_event` does not
                // have access to.
                if matches!(user_event, UserEvent::Automation(AutomationCommand::Quit)) {
                    *control_flow = ControlFlow::Exit;
                } else {
                    handle_user_event(
                        target,
                        &window_event_proxy,
                        &mut ui_windows,
                        &mut state,
                        &config,
                        &homepage,
                        &mut automation_window,
                        &mut page_load_timers,
                        &mut automation_wait,
                        user_event,
                    );
                }
            }
            _ => {}
        }

        // Automatic tab suspension (Issue #63, `browser::suspension`): on
        // every pass through the loop (an actual event, or the timer below
        // waking us up), suspend whatever background tabs the policy picks
        // — idle too long, over the live-tab cap, or (when a fresh memory
        // sample just arrived) over the memory budget — then schedule the
        // next wake-up for whichever background tab will go idle soonest.
        // `suspension::plan`/`Tabs::next_idle_deadline` are pure and
        // clock-injected, so all the policy logic this loop needs is
        // unit-tested without a window. Issue #29/D68: run once per open
        // window rather than pooling every window's tabs into one policy
        // decision — each window's live-tab cap/idle timer is evaluated
        // independently of every other window's.
        //
        // Issue #186: exactly one window's sweep this pass gets the fresh
        // memory sample (if any) — `pending_memory_sample` is taken once,
        // up front, not per window — and it goes to whichever window
        // `next_memory_sample_window` names (round-robin, advanced below),
        // not always the first window `ids()` happens to list. See that
        // field's doc comment and docs/decisions.md D90 for why round-robin
        // was chosen over handing the same sample to every window.
        if *control_flow != ControlFlow::Exit {
            let mut next_wake: Option<Instant> = None;
            let window_ids: Vec<WindowId> = state.windows.ids().collect();
            let memory_sample = state.pending_memory_sample.take();
            let served_window = if memory_sample.is_some() {
                let (served, next_cursor) =
                    choose_memory_sample_window(&window_ids, state.next_memory_sample_window);
                state.next_memory_sample_window = next_cursor;
                served
            } else {
                None
            };
            for window_id in window_ids {
                let memory = memory_sample.filter(|_| Some(window_id) == served_window);
                if let Some(wake) = sweep_tabs(
                    &mut ui_windows,
                    &mut state,
                    window_id,
                    &suspension_policy,
                    memory,
                    Instant::now(),
                ) {
                    next_wake = earliest(next_wake, wake);
                }
            }
            // Issue #169: give `wait_load`'s deadline (if one is pending) a
            // say in when the loop next wakes up too — otherwise a script
            // waiting on a load that never finishes would only be noticed
            // whenever some *other* event or the suspension sweep happened
            // to wake the loop next, rather than promptly at its own
            // timeout.
            if let Some(wake) =
                poll_automation_wait_timeout(&mut automation_wait, &state, Instant::now())
            {
                next_wake = earliest(next_wake, wake);
            }
            if let Some(next_wake) = next_wake {
                *control_flow = ControlFlow::WaitUntil(next_wake);
            }
        }
    });
}

/// `current` (まだ無ければ `None`) と `candidate` のうち早い方の起床時刻。
fn earliest(current: Option<Instant>, candidate: Instant) -> Option<Instant> {
    Some(current.map_or(candidate, |existing| existing.min(candidate)))
}

/// Find the `BrowserWindow` whose native window is `tao_id`, if any is still
/// open (Issue #29/D68) — the event-loop closure's one lookup from a `tao`
/// `WindowEvent` to the `BrowserWindow` it belongs to.
fn window_by_tao_id_mut(
    ui_windows: &mut HashMap<WindowId, BrowserWindow>,
    tao_id: tao::window::WindowId,
) -> Option<&mut BrowserWindow> {
    ui_windows
        .values_mut()
        .find(|window| window.tao_id() == tao_id)
}

/// Handle a native `WindowEvent::CloseRequested` for the window whose `tao`
/// id is `tao_id` (Issue #29/D68): drop that window's `BrowserWindow` (which
/// drops its `tao::window::Window` and every webview it held — the resource
/// release the issue's acceptance criteria asks for) and its `Tabs`, then
/// end the process once no window is left open — matching how closing the
/// last window ends every mainstream desktop browser on Windows/Linux (see
/// docs/decisions.md D68 for why this, not macOS's "keep running with zero
/// windows" convention, given CLAUDE.md's Windows-first priority). An
/// unknown `tao_id` (should not happen — every native window this process
/// creates is tracked here) is a silent no-op rather than a panic.
///
/// Also drops every `page_load_timers` entry that belonged to this window
/// (Issue #62/D79): a tab whose page never finished loading before the
/// window closed would otherwise leave its timer entry behind forever —
/// `WindowId`s are never reused, so nothing would ever overwrite it. See
/// [`PageLoadTimers`]'s doc comment.
fn close_window_by_tao_id(
    ui_windows: &mut HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    page_load_timers: &mut PageLoadTimers,
    tao_id: tao::window::WindowId,
    control_flow: &mut ControlFlow,
) {
    let Some(id) = window_by_tao_id_mut(ui_windows, tao_id).map(|window| window.id()) else {
        return;
    };
    ui_windows.remove(&id);
    state.windows.close_window(id);
    page_load_timers.retain(|(window_id, _), _| *window_id != id);
    if state.windows.is_empty() {
        *control_flow = ControlFlow::Exit;
    }
}

/// Build the content-blocking filter list: VeloX's built-in list, plus an
/// optional user-supplied list merged on top. A missing/unreadable extra
/// list is logged and skipped rather than treated as fatal (see the
/// `log_failure` pattern used for UI calls below).
fn build_blocklist(config: &Config) -> FilterList {
    let mut list = FilterList::built_in();
    if let Some(path) = &config.extra_blocklist_path {
        match std::fs::read_to_string(path) {
            Ok(text) => list.merge(&text),
            Err(err) => eprintln!("velox: failed to read extra blocklist {path:?}: {err}"),
        }
    }
    list
}

/// Build the per-site content-blocking exception set (Issue #22) from
/// `Config::content_blocking_site_exceptions`.
fn build_site_exceptions(config: &Config) -> SiteExceptions {
    SiteExceptions::from_hosts(&config.content_blocking_site_exceptions)
}

/// Dispatch one [`UserEvent`]. UI failures are logged, never fatal.
///
/// **Multi-window (Issue #29/D68)**: most arms first resolve the
/// [`WindowId`] the event names to a live `&mut BrowserWindow` in
/// `ui_windows` — an id with no entry (the window closed while the event was
/// in flight, e.g. a title fetch that lands after its tab's window closed)
/// is a silent no-op, the same "stale id resolves to nothing" convention
/// `TabId` already documents. `ToolbarCommand::NewWindow`/
/// `ContentShortcut::NewWindow`/`AutomationCommand::NewWindow` are the one
/// family of exceptions: they are intercepted *before* that resolution,
/// since opening a window needs `&mut HashMap<WindowId, BrowserWindow>` as a
/// whole (to insert the new entry) at the same time another entry
/// (`window`, the sender) would otherwise be mutably borrowed — see
/// `open_new_window`'s call sites below.
///
/// One known gap this issue accepts (see the PR description): a
/// `DownloadStarted`/`DownloadCompleted` refreshes only the *originating*
/// window's downloads panel, even though `DownloadStore` itself is shared —
/// a window not otherwise interacted with will not see its downloads panel
/// update until it next reopens that panel. Refreshing every open window's
/// panel immediately would need the same "insert vs. one entry" borrow this
/// doc comment already flags for `NewWindow`, applied to a hot path; left
/// for a follow-up.
#[allow(clippy::too_many_arguments)]
fn handle_user_event(
    target: &EventLoopWindowTarget<UserEvent>,
    window_event_proxy: &EventLoopProxy<UserEvent>,
    ui_windows: &mut HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    config: &Config,
    homepage: &str,
    automation_window: &mut WindowId,
    page_load_timers: &mut PageLoadTimers,
    automation_wait: &mut AutomationWaitState,
    event: UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(window_id, body) => match toolbar::parse_command(&body) {
            // See this function's doc comment: intercepted before resolving
            // `window_id` to a `&mut BrowserWindow`.
            //
            // Issue #27/D74: `NewPrivateWindow` passes `true` instead of
            // `config.private` — a private window is opened regardless of
            // whether the process itself was launched private.
            Ok(command @ (ToolbarCommand::NewWindow | ToolbarCommand::NewPrivateWindow)) => {
                let private = config.private || matches!(command, ToolbarCommand::NewPrivateWindow);
                open_new_window(
                    target,
                    window_event_proxy,
                    ui_windows,
                    state,
                    config,
                    homepage,
                    private,
                );
            }
            // See this function's doc comment, and `apply_updated_settings`'s:
            // settings are whole-process (D67), so applying one touches
            // every open window's chrome, which needs `ui_windows` as a
            // whole — the same reason `NewWindow` is intercepted here.
            Ok(ToolbarCommand::UpdateSettings { settings }) => {
                apply_updated_settings(ui_windows, state, *settings);
            }
            Ok(ToolbarCommand::ResetSettings) => {
                apply_updated_settings(ui_windows, state, Settings::default());
            }
            Ok(command) => {
                if let Some(window) = ui_windows.get_mut(&window_id) {
                    handle_toolbar_command(
                        window,
                        window_id,
                        state,
                        config,
                        homepage,
                        page_load_timers,
                        command,
                    );
                }
            }
            Err(err) => eprintln!(
                "velox: ignoring malformed toolbar message ({} bytes, preview {:?}): {err}",
                body.len(),
                log_preview(&body)
            ),
        },
        UserEvent::NavigationStarted(window_id, id, url)
        | UserEvent::LoadStarted(window_id, id, url) => {
            let Some(window) = ui_windows.get_mut(&window_id) else {
                return;
            };
            let is_active = state.windows.tabs_mut(window_id).is_some_and(|tabs| {
                if let Some(tab) = tabs.get_mut(id) {
                    tab.on_navigation_started(&url);
                }
                tabs.active_id() == id
            });
            // Issue #43/D69, integrated with multi-window in D68: the page
            // under an open find session is about to change, so its
            // matches/highlights are about to become stale — close it
            // rather than keep showing a count (or an active-match
            // highlight) for content that no longer exists. Scoped to
            // *this* window's own find session (`Windows::find`) — a
            // search open in a different window, even one whose active tab
            // happens to share this `TabId` value, is never touched.
            if state
                .windows
                .find(window_id)
                .is_some_and(|session| session.tab_id() == id)
            {
                close_find_bar(window, window_id, state);
            }
            // Issue #39/D78: same reasoning as the find bar above — a menu
            // opened against the page that is about to be replaced would
            // otherwise keep offering actions (a link URL, a selection)
            // that no longer make sense once navigation completes.
            // `hide_context_menu` also removes the overlay from the DOM —
            // the new page will eventually replace it anyway, but a slow
            // load could otherwise leave the stale menu visible in the
            // meantime.
            if context_menu_is_for(state, window_id, id) {
                state.windows.take_context_menu(window_id);
                log_failure("hide context menu", window.hide_context_menu(id));
            }
            if is_active {
                log_failure("update address bar", window.set_url_display(&url));
                log_failure("show loading state", window.set_loading(true));
                sync_bookmark_star(window, state, &url);
            }
            sync_tab_strip(window, window_id, state);
        }
        UserEvent::NavigationBlocked(window_id, id, url) => {
            eprintln!("velox: blocked navigation to {url} in tab {id:?} (window {window_id:?})");
            record_blocked_request(ui_windows, state, window_id, id, |tab| {
                tab.on_navigation_blocked(&url)
            });
        }
        UserEvent::SubresourceBlocked(window_id, id, url) => {
            // Deliberately no `eprintln!` here unlike `NavigationBlocked`
            // above: a busy page can trigger this dozens of times per
            // second (every blocked ad/tracker image, script, XHR...), and
            // spamming stderr at that rate would drown out every other
            // `log_failure` line this file relies on for diagnostics.
            record_blocked_request(ui_windows, state, window_id, id, |tab| {
                tab.on_subresource_blocked(&url)
            });
        }
        UserEvent::LoadFinished(window_id, id, url) => {
            // Issue #169: resolve a pending `wait_load` before anything
            // else below can `return` early (e.g. the window already
            // closed) — a `wait_load` targeting a tab whose window is gone
            // would otherwise sit unresolved until its own timeout instead
            // of clearing right away.
            resolve_automation_wait_if_matching(automation_wait, window_id, id);
            let Some(window) = ui_windows.get_mut(&window_id) else {
                return;
            };
            let is_active = state.windows.tabs_mut(window_id).is_some_and(|tabs| {
                // A failed load reports an empty URL; keep showing the URL
                // the tab tried to reach instead of blanking it out.
                if let Some(tab) = tabs.get_mut(id) {
                    if url.is_empty() {
                        tab.on_load_failed();
                    } else {
                        tab.on_load_finished(&url);
                    }
                }
                tabs.active_id() == id
            });
            if !url.is_empty() {
                // Recorded for whichever tab just finished loading, not only
                // the active one: a background tab finishing a load is a
                // real visit too (see docs/decisions.md D13 and the "Visit
                // history and bookmarks" section of docs/architecture.md).
                //
                // Exception: a `data:` URL — today, only ever a View Source
                // tab (Issue #45, D72) — is never recorded. It is a
                // synthetic, address-bar-unfriendly base64 blob with no real
                // "site" behind it to revisit; recording it would only
                // pollute history/the omnibox with a huge unreadable string.
                //
                // `record_visit_if_enabled` itself skips a private window
                // (Issue #27, D74), so both exclusions compose here.
                let history_id = if url.starts_with("data:") {
                    None
                } else {
                    record_visit_if_enabled(state, window_id, &url, config.history_max_entries)
                };
                if history_id.is_some() {
                    persist_history(state);
                    refresh_history_panel_if_open(window, state, config);
                }
                // Title/favicon are tab-strip state, independent of whether
                // this visit was recorded to history — private mode (no
                // history recording) still wants a readable tab strip (see
                // docs/decisions.md D22). `0` is a safe sentinel
                // `history_id` when there is none: `HistoryStore` ids start
                // at 1, so `HistoryStore::update_title` simply finds nothing
                // to update rather than touching an unrelated entry.
                let history_id = history_id.unwrap_or(0);
                log_failure("fetch page title", window.fetch_page_title(id, history_id));
                log_failure(
                    "fetch favicon",
                    window.fetch_favicon(id, history_id, url.clone()),
                );
            }
            if is_active {
                if !url.is_empty() {
                    log_failure("update address bar", window.set_url_display(&url));
                    sync_bookmark_star(window, state, &url);
                }
                log_failure("hide loading state", window.set_loading(false));
            }
            sync_tab_strip(window, window_id, state);
        }
        UserEvent::PageTitleResolved {
            window_id,
            tab_id,
            history_id,
            title,
        } => {
            // A stale `tab_id`/`window_id` (the tab or window closed while
            // the title fetch was in flight) is a safe no-op here — only the
            // history entry still gets its title.
            if let Some(tabs) = state.windows.tabs_mut(window_id) {
                if let Some(tab) = tabs.get_mut(tab_id) {
                    tab.set_title(title.clone());
                    // Unlike `FaviconResolved` below, nothing else here
                    // already calls `persist_session` (which
                    // `sync_tab_strip` would also cover) — persist
                    // explicitly so a title that arrives just before a
                    // crash is not lost from the next restore (Issue
                    // #25/D65).
                    persist_session(state);
                }
            }
            if state.history.update_title(history_id, title) {
                persist_history(state);
                if let Some(window) = ui_windows.get(&window_id) {
                    refresh_history_panel_if_open(window, state, config);
                }
            }
        }
        UserEvent::FaviconResolved {
            window_id,
            tab_id,
            history_id,
            page_url,
            url,
        } => {
            // A stale `tab_id`/`window_id` (the tab or window closed while
            // the fetch was in flight) is a safe no-op — mirrors
            // `PageTitleResolved` above.
            if let Some(tabs) = state.windows.tabs_mut(window_id) {
                if let Some(tab) = tabs.get_mut(tab_id) {
                    tab.set_favicon_url(url.clone());
                    if let Some(window) = ui_windows.get(&window_id) {
                        sync_tab_strip(window, window_id, state);
                    }
                }
            }
            if state.history.update_favicon(history_id, url.clone()) {
                persist_history(state);
                if let Some(window) = ui_windows.get(&window_id) {
                    refresh_history_panel_if_open(window, state, config);
                }
            }
            // Issue #19/D34: a bookmarked page's favicon updates the same
            // way, keyed by URL (a bookmark has no history/tab id of its
            // own to correlate against) — a global store, so every open
            // window's bookmarks panel is refreshed, not just the
            // originating one.
            if state.bookmarks.update_favicon_by_url(&page_url, url) {
                persist_bookmarks(state);
                for window in ui_windows.values() {
                    refresh_bookmarks_panel(window, state);
                }
            }
        }
        UserEvent::OpenDevtoolsRequested(window_id) => {
            if let Some(window) = ui_windows.get_mut(&window_id) {
                window.open_devtools();
            }
        }
        // See this function's doc comment. The source window is unused:
        // opening a window needs no source tab. Issue #27/D74:
        // `NewPrivateWindow` passes `true` instead of `config.private`.
        UserEvent::ContentShortcut(
            _,
            shortcut @ (ContentShortcut::NewWindow | ContentShortcut::NewPrivateWindow),
        ) => {
            let private = config.private || matches!(shortcut, ContentShortcut::NewPrivateWindow);
            open_new_window(
                target,
                window_event_proxy,
                ui_windows,
                state,
                config,
                homepage,
                private,
            );
        }
        UserEvent::ContentShortcut(window_id, shortcut) => {
            if let Some(window) = ui_windows.get_mut(&window_id) {
                handle_content_shortcut(
                    window,
                    window_id,
                    state,
                    config,
                    homepage,
                    page_load_timers,
                    shortcut,
                );
            }
        }
        UserEvent::NewTabRequested(window_id, url) => {
            if let Some(window) = ui_windows.get_mut(&window_id) {
                open_new_tab(window, window_id, state, &url);
            }
        }
        // Issue #46: a page save registers exactly like a real download,
        // so both show up in the same Downloads panel.
        UserEvent::DownloadStarted {
            window_id,
            url,
            file_name,
            destination,
            started_at,
        }
        | UserEvent::SavePageStarted {
            window_id,
            url,
            file_name,
            destination,
            started_at,
        } => {
            state
                .downloads
                .start(url, file_name, destination, started_at);
            if let Some(window) = ui_windows.get(&window_id) {
                refresh_downloads_panel(window, state);
            }
        }
        UserEvent::RestoreWindows(saved) => {
            for window in saved {
                restore_window(
                    target,
                    window_event_proxy,
                    ui_windows,
                    state,
                    config,
                    &window,
                );
            }
        }
        UserEvent::DownloadCompleted {
            window_id,
            url,
            path,
            success,
        } => {
            record_download_completion(state, &url, path.as_deref(), success);
            if let Some(window) = ui_windows.get(&window_id) {
                refresh_downloads_panel(window, state);
            }
        }
        UserEvent::SavePageFinished {
            window_id,
            url,
            destination,
            error,
        } => {
            record_save_page_completion(state, &url, &destination, error);
            if let Some(window) = ui_windows.get(&window_id) {
                refresh_downloads_panel(window, state);
            }
        }
        // Issue #27/D74: `NewPrivateWindow` passes `true` instead of
        // `config.private` — lets an automation script (and this crate's
        // integration tests) drive the private-window path.
        UserEvent::Automation(
            command @ (AutomationCommand::NewWindow | AutomationCommand::NewPrivateWindow),
        ) => {
            let private = config.private || matches!(command, AutomationCommand::NewPrivateWindow);
            if let Some(new_id) = open_new_window(
                target,
                window_event_proxy,
                ui_windows,
                state,
                config,
                homepage,
                private,
            ) {
                *automation_window = new_id;
            }
        }
        UserEvent::Automation(command) => {
            if let Some(window) = ui_windows.get_mut(automation_window) {
                handle_automation_command(
                    window,
                    *automation_window,
                    state,
                    page_load_timers,
                    automation_wait,
                    command,
                );
            } else if matches!(
                command,
                AutomationCommand::WaitLoad { .. } | AutomationCommand::WaitStartup { .. }
            ) {
                // The targeted window is already gone (should not happen in
                // practice — see the doc comment on `tabs_of`'s `.expect`)
                // but a `wait_load`/`wait_startup` must still never hang the
                // automation thread waiting for a notification nothing will
                // ever send. (`wait_startup` does not actually need a window
                // at all, unlike `wait_load` — but it is dispatched through
                // this same `ui_windows.get_mut(automation_window)` gate as
                // every other automation command, so it needs the same
                // guard here.)
                automation_wait.wake();
            }
        }
        UserEvent::MemorySampled(sample) => {
            // Acted on by `sweep_tabs` at the end of this loop pass (it
            // runs after every event), not here: the sweep is the one
            // place that combines all three signals.
            state.pending_memory_sample = Some(sample);
        }
        UserEvent::FindMatchesUpdated {
            window_id,
            tab_id,
            total,
        } => {
            if let Some(window) = ui_windows.get(&window_id) {
                apply_find_matches(window, window_id, state, tab_id, total);
            }
        }
        UserEvent::PdfExportFinished {
            window_id,
            tab_id: _,
            destination,
            success,
            error,
        } => {
            let Some(window) = ui_windows.get_mut(&window_id) else {
                return;
            };
            let message = match (success, error) {
                (true, _) => format!("PDFとして保存しました: {}", destination.display()),
                (false, Some(reason)) => format!("PDFの書き出しに失敗しました: {reason}"),
                (false, None) => "PDFの書き出しに失敗しました".to_owned(),
            };
            show_print_status(window, &message);
        }
        UserEvent::TabFreezeFinished {
            window_id,
            tab_id,
            success,
            error,
        } => {
            // A window closed while its freeze was in flight is a safe
            // no-op, like every other window-addressed async result here.
            //
            // **This check must stay first.** `tabs_of` below resolves
            // `window_id` with `.expect()`, on the documented assumption that
            // the caller already established the window exists — and closing
            // a window drops it from `ui_windows` and `state.windows`
            // together (`close_window_by_tao_id`). Doing the lookup first
            // would turn this very case, the one the paragraph above calls a
            // safe no-op, into a panic.
            let Some(window) = ui_windows.get_mut(&window_id) else {
                return;
            };
            handle_tab_freeze_finished(window, window_id, state, tab_id, success, error);
        }
        UserEvent::ViewSourceReady {
            window_id,
            page_url,
            html,
        } => {
            // Issue #29/D68: the View Source tab belongs in the window the
            // request came from. A window closed while its source fetch was
            // still in flight is a safe no-op, same as every other
            // window-addressed event here.
            let Some(window) = ui_windows.get_mut(&window_id) else {
                return;
            };
            open_view_source_tab(window, window_id, state, &page_url, &html);
        }
        UserEvent::ContextMenuRequested {
            window_id,
            tab_id,
            x,
            y,
            raw,
        } => {
            let Some(window) = ui_windows.get_mut(&window_id) else {
                return;
            };
            // The one and only place `raw` — completely untrusted content-
            // webview input, see `context_menu::RawMenuContext`'s doc
            // comment — is turned into something safe to act on or display.
            // `build_menu` (pure, `browser::context_menu`) is the single
            // table deciding which items apply and whether each is enabled.
            let context = context_menu::sanitize(raw);
            let entries = context_menu::build_menu(&context);
            state.windows.set_context_menu(
                window_id,
                context_menu::OpenContextMenu::new(tab_id, entries.clone()),
            );
            log_failure(
                "show context menu",
                window.show_context_menu(tab_id, &entries, x, y),
            );
        }
        UserEvent::ContextMenuActionSelected {
            window_id,
            tab_id,
            index,
        } => {
            // Only resolve against *this* window's open menu, and only if
            // it is still the one opened for `tab_id` — a stale click for a
            // menu already replaced by a different tab's (in the same
            // window) must never touch the new one. Checked before
            // `take_context_menu` so a mismatch never discards a menu that
            // is not actually the one this click belongs to.
            if !context_menu_is_for(state, window_id, tab_id) {
                return;
            }
            // `resolve` also re-checks `enabled` — a hostile page dispatching
            // a fake click on a greyed-out row cannot run it anyway.
            let Some(action) = state
                .windows
                .take_context_menu(window_id)
                .and_then(|menu| menu.resolve(index))
            else {
                return;
            };
            match action {
                // Needs `&mut ui_windows` as a whole (a brand new window),
                // same reason `ToolbarCommand::NewWindow`/
                // `ContentShortcut::NewWindow` are intercepted before
                // narrowing to one `&mut BrowserWindow` — see this
                // function's doc comment.
                context_menu::MenuAction::OpenLinkInNewWindow(url) => {
                    // Issue #27/D74: inherit the *source* window's privacy
                    // rather than `config.private` — a link opened from a
                    // private window's context menu must stay private, the
                    // same way a real browser's "開くリンクを新しいプライベート
                    // ウィンドウで開く" would, without needing a second,
                    // dedicated menu item for it. Falls back to
                    // `config.private` only if `window_id` is somehow
                    // already gone (should not happen — the menu was just
                    // resolved against that same window above).
                    let private = state
                        .windows
                        .is_private(window_id)
                        .unwrap_or(config.private);
                    open_new_window(
                        target,
                        window_event_proxy,
                        ui_windows,
                        state,
                        config,
                        &url,
                        private,
                    );
                }
                other => {
                    if let Some(window) = ui_windows.get_mut(&window_id) {
                        handle_context_menu_action(window, window_id, state, config, tab_id, other);
                    }
                }
            }
        }
        UserEvent::FormInputDetected(window_id, tab_id) => {
            // Issue #272 / D142. The whole policy switch lives here: with
            // the protection off the flag is simply never set, so the
            // measurement's two arms differ in `Candidate::has_form_input`
            // while the injected script keeps running identically in both
            // (see `Config::protect_form_input`).
            if config.protect_form_input {
                if let Some(tab) = state
                    .windows
                    .tabs_mut(window_id)
                    .and_then(|tabs| tabs.get_mut(tab_id))
                {
                    tab.mark_form_input();
                }
            }
        }
        UserEvent::ContextMenuClosed(window_id, tab_id) => {
            if context_menu_is_for(state, window_id, tab_id) {
                state.windows.take_context_menu(window_id);
            }
        }
    }
}

/// ブロックしたリクエストを `window_id` のタブ `id` に記録する
/// (`NavigationBlocked`/`SubresourceBlocked` 共通)。`record` がタブ側の
/// カウンタを更新し、そのタブがアクティブならバッジも更新する。閉じた
/// ウィンドウ宛ての遅れて届いたイベントは何もしない。
fn record_blocked_request(
    ui_windows: &HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    window_id: WindowId,
    id: TabId,
    record: impl FnOnce(&mut Tab),
) {
    let Some(window) = ui_windows.get(&window_id) else {
        return;
    };
    let Some(tabs) = state.windows.tabs_mut(window_id) else {
        return;
    };
    if let Some(tab) = tabs.get_mut(id) {
        record(tab);
    }
    // Only the active tab's badge is visible right now; a blocked request in
    // a background tab still updates its own `Tab::blocked_count` above and
    // is picked up the moment that tab becomes active (see
    // `activate_and_refresh`).
    if id == tabs.active_id() {
        sync_block_count(window, tabs);
    }
}

/// `window_id` で開いているコンテキストメニューが、タブ `tab_id` に対して
/// 開かれたものか (Issue #39/D78)。別タブ向けのメニューや、既に閉じた
/// メニューに対する遅れたイベントを弾くための共通チェック。
fn context_menu_is_for(state: &AppState, window_id: WindowId, tab_id: TabId) -> bool {
    state
        .windows
        .context_menu(window_id)
        .is_some_and(|menu| menu.tab_id() == tab_id)
}

/// Open a brand new window (Ctrl/Cmd+N) with a single tab at `url`. The one
/// path every "open a new window" trigger funnels through —
/// `ToolbarCommand::NewWindow`, `ContentShortcut::NewWindow`, and
/// `AutomationCommand::NewWindow` — mirroring how [`open_new_tab`] is the
/// one path for "open a new tab" (Issue #29, see docs/decisions.md D68).
///
/// Returns the new window's id on success. A `BrowserWindow::new` failure —
/// the same construction the very first window already went through, so
/// this should not happen in practice — is logged to stderr and rolled back
/// (the `Windows` entry `open_window` just created is removed again) rather
/// than taking the whole process down or leaving an orphaned logical window
/// with no `BrowserWindow` behind it.
///
/// `private` decides the new window's own privacy (Issue #27, D74) —
/// `ToolbarCommand::NewWindow`/`ContentShortcut::NewWindow`/
/// `AutomationCommand::NewWindow` (Ctrl/Cmd+N) pass `config.private` through
/// unchanged (so a `--private`-launched process keeps opening private
/// windows, exactly as before this issue), while their
/// `NewPrivateWindow`/`new_private_window` counterparts (Ctrl/Cmd+Shift+N)
/// always pass `true` regardless of `config.private`. Threaded into both
/// `browser::Windows::open_window_with_privacy` (the logical half —
/// gates history/input-history recording and session persistence, see
/// `window_is_private`) and `ui::window::BrowserWindow::new` (the engine
/// half — `.with_incognito`/ephemeral `WebContext`, see docs/decisions.md
/// D15) with the same value, since the two must always agree.
fn open_new_window(
    target: &EventLoopWindowTarget<UserEvent>,
    window_event_proxy: &EventLoopProxy<UserEvent>,
    ui_windows: &mut HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    config: &Config,
    url: &str,
    private: bool,
) -> Option<WindowId> {
    let window_id = state
        .windows
        .open_window_with_privacy(url.to_owned(), private);
    attach_browser_window(
        target,
        window_event_proxy,
        ui_windows,
        state,
        config,
        window_id,
        private,
    )
}

/// Reopen one window from a restored session (Issue #149), tabs and all.
///
/// The only difference from [`open_new_window`] is *which* `Windows` call
/// creates the logical window — `open_restored_window` (the same one
/// `app::run` uses for the primary window) instead of a fresh single tab.
/// Everything after that is shared, so a restored window cannot drift from
/// a Ctrl/Cmd+N one in theme, bookmark-bar state or failure handling.
///
/// **Always non-private.** A private window's tabs never reach
/// `session.json` in the first place (`persist_session` filters them out),
/// so nothing here can have come from one — and restoring *into* a private
/// window would be the same D14 violation from the other direction.
fn restore_window(
    target: &EventLoopWindowTarget<UserEvent>,
    window_event_proxy: &EventLoopProxy<UserEvent>,
    ui_windows: &mut HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    config: &Config,
    saved: &SavedWindow,
) -> Option<WindowId> {
    let window_id = state
        .windows
        .open_restored_window(&saved.tabs, saved.active_index);
    attach_browser_window(
        target,
        window_event_proxy,
        ui_windows,
        state,
        config,
        window_id,
        false,
    )
}

/// Build the `BrowserWindow` for a logical window that `Windows` has just
/// created, and register it. Shared by [`open_new_window`] and
/// [`restore_window`] so the two cannot diverge.
///
/// The window is loaded at its *active tab's* current URL, which is what
/// makes this work for both callers: a fresh window's single tab is at the
/// requested URL, and a restored window's active tab is at whatever it was
/// showing last.
fn attach_browser_window(
    target: &EventLoopWindowTarget<UserEvent>,
    window_event_proxy: &EventLoopProxy<UserEvent>,
    ui_windows: &mut HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    config: &Config,
    window_id: WindowId,
    private: bool,
) -> Option<WindowId> {
    let tabs = state
        .windows
        .tabs(window_id)
        .expect("the caller just opened this window");
    let tab_id = tabs.active_id();
    let url = tabs.active().current_url().to_owned();
    let url = url.as_str();
    match BrowserWindow::new(
        target,
        window_id,
        config,
        window_event_proxy.clone(),
        tab_id,
        url,
        state.site_policies.clone(),
        private,
        state.perf.as_ref().map(PerfContext::to_ipc_log),
        // Issue #182: startup's `process_start` → `window_created`
        // decomposition is about the *first* window only. A window opened
        // later (Ctrl/Cmd+N) does not pay the engine's one-time
        // initialization cost and is not part of any startup measurement,
        // so it records nothing here.
        None,
    ) {
        Ok(window) => {
            // Issue #30/D67, integrated with multi-window in D68: settings
            // are whole-process (`AppState::settings`), so a window opened
            // after startup must reflect the *current* saved Appearance
            // settings from the moment it exists — the same push
            // `app::run` does once for the primary window, and
            // `apply_updated_settings` repeats for every open window
            // whenever the settings screen saves a change.
            log_failure(
                "apply theme",
                window.set_theme(state.settings.appearance.theme),
            );
            log_failure(
                "apply initial bookmark bar visibility",
                window.set_bookmark_bar_visible(state.settings.appearance.show_bookmark_bar),
            );
            ui_windows.insert(window_id, window);
            Some(window_id)
        }
        Err(err) => {
            eprintln!("velox: failed to open a new window: {err}");
            state.windows.close_window(window_id);
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_toolbar_command(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    config: &Config,
    homepage: &str,
    page_load_timers: &mut PageLoadTimers,
    command: ToolbarCommand,
) {
    match command {
        // Resolved via `navigation::classify_input` — the single place that
        // decides URL vs. search (see docs/decisions.md D26) — rather than
        // `navigation::normalize_input` directly, so a search query typed
        // and submitted with no candidate-dropdown interaction still goes
        // to the search engine instead of being rejected outright. `input`
        // here is also what a history/bookmark/candidate row click sends
        // (already a resolved URL, e.g. `Candidate::target_url`):
        // `classify_input` treats an already-absolute `https://…` URL as
        // `Intent::Url` unchanged, so this one path serves both cases.
        ToolbarCommand::Navigate { input } => {
            // Issue #20: remember a search query the user actually typed
            // and submitted (not a candidate-row click already resolved to
            // a URL — `classify_input` on an already-absolute `https://…`
            // target_url yields `Intent::Url`, never `Intent::Search`, so
            // this naturally only fires for raw typed text — see
            // docs/decisions.md D38).
            let intent = navigation::classify_input(&input);
            if let Some(Intent::Search(query)) = &intent {
                record_input_history_if_enabled(
                    state,
                    window_id,
                    query,
                    input_history::DEFAULT_MAX_ENTRIES,
                );
                persist_input_history(state);
            }
            match resolve_intent(config, intent) {
                Some(url) => {
                    navigate_active_tab(window, window_id, state, &url);
                    // A panel entry click drives this same command; close
                    // whichever panel was open now that the user has acted
                    // on it.
                    log_failure("close panel", window.set_panel(None));
                }
                None => {
                    eprintln!("velox: cannot navigate to {input:?}");
                    // Snap the address bar back to the page we are actually
                    // on.
                    log_failure(
                        "restore address bar",
                        window.set_url_display(tabs_of(state, window_id).active().current_url()),
                    );
                }
            }
        }
        ToolbarCommand::Back => log_failure("go back", window.go_back()),
        ToolbarCommand::Forward => log_failure("go forward", window.go_forward()),
        ToolbarCommand::Reload => log_failure("reload", window.reload()),
        ToolbarCommand::OpenDevtools => window.open_devtools(),
        ToolbarCommand::NewTab => open_new_tab(window, window_id, state, homepage),
        ToolbarCommand::CloseTab { id } => {
            close_tab(window, window_id, state, page_load_timers, TabId::from(id))
        }
        ToolbarCommand::ActivateTab { id } => {
            activate_tab(window, window_id, state, TabId::from(id))
        }
        ToolbarCommand::CloseActiveTab => {
            close_active_tab(window, window_id, state, page_load_timers)
        }
        ToolbarCommand::ReopenClosedTab => reopen_closed_tab(window, window_id, state),
        ToolbarCommand::NextTab => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_relative(1, now)
            });
        }
        ToolbarCommand::PrevTab => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_relative(-1, now)
            });
        }
        ToolbarCommand::ActivateTabByIndex { index } => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_by_position(index as usize, now)
            });
        }
        ToolbarCommand::ActivateLastTab => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_last(now)
            });
        }
        ToolbarCommand::SuspendTab { id } => {
            if suspend_tab(window, window_id, state, TabId::from(id)) {
                sync_tab_strip(window, window_id, state);
            }
            // Otherwise: unknown id, the active tab (never suspended), or
            // already suspended — a no-op, mirroring `CloseTab`'s guards.
        }
        ToolbarCommand::TogglePinTab { id } => {
            if tabs_of(state, window_id)
                .toggle_pinned(TabId::from(id))
                .is_some()
            {
                sync_tab_strip(window, window_id, state);
            }
            // Otherwise: unknown id — a no-op, mirroring `SuspendTab`.
        }
        // A pure startup-timing probe (Issue #59/D43) — `record_perf_event`
        // already consumed it above; nothing to do here.
        ToolbarCommand::ScriptStarted => {}
        ToolbarCommand::Ready => {
            log_failure(
                "initialize address bar",
                window.set_url_display(tabs_of(state, window_id).active().current_url()),
            );
            log_failure(
                "initialize loading state",
                window.set_loading(tabs_of(state, window_id).active().is_loading()),
            );
            // `window.is_private()` — this window's own flag (D74) — not
            // `config.private`: the latter is process-wide and would show
            // every window's badge according to whichever window happened
            // to launch the process, wrong the moment a private and a
            // normal window coexist.
            log_failure(
                "show private indicator",
                window.set_private(window.is_private()),
            );
            log_failure(
                "initialize bookmark bar visibility",
                window.set_bookmark_bar_visible(window.bookmark_bar_visible()),
            );
            // Issue #30 (D67): the chrome theme override, applied here the
            // same way `set_private` is above — a one-time push on `ready`,
            // since it never changes except through the settings screen
            // (which pushes it again itself via `apply_updated_settings`).
            // Whole-process (D67/D68): every window's toolbar applies the
            // same `state.settings`, since the settings screen is not
            // per-window state.
            log_failure(
                "apply theme",
                window.set_theme(state.settings.appearance.theme),
            );
            sync_block_count(window, tabs_of(state, window_id));
            sync_active_bookmark_star(window, window_id, state);
            refresh_history_panel(window, state, config);
            refresh_bookmarks_panel(window, state);
            refresh_downloads_panel(window, state);
            refresh_settings_panel(window, state);
            sync_tab_strip(window, window_id, state);
        }
        ToolbarCommand::ToggleBookmark => toggle_current_bookmark(window, window_id, state),
        ToolbarCommand::TogglePanel { panel } => {
            let next = (window.open_panel() != Some(panel)).then_some(panel);
            log_failure("toggle panel", window.set_panel(next));
            match next {
                Some(Panel::History) => refresh_history_panel(window, state, config),
                Some(Panel::Bookmarks) => refresh_bookmarks_panel(window, state),
                Some(Panel::Downloads) => refresh_downloads_panel(window, state),
                Some(Panel::Settings) => refresh_settings_panel(window, state),
                // The toolbar's own UI never sends `toggle_panel` for this
                // variant (it is opened/closed only via
                // `OmniboxInput`/`OmniboxClose`, which push their own
                // content); nothing to refresh here even if it somehow was.
                Some(Panel::Omnibox) | None => {}
            }
        }
        ToolbarCommand::DeleteHistoryEntry { id } => {
            if state.history.remove(id) {
                persist_history(state);
                refresh_history_panel(window, state, config);
            }
        }
        ToolbarCommand::ClearHistory => {
            state.history.clear();
            persist_history(state);
            // Issue #20 (D38): typed search-query history is part of the
            // same "what have I been doing" privacy surface as page-visit
            // history, so clearing one clears both.
            state.input_history.clear();
            persist_input_history(state);
            refresh_history_panel(window, state, config);
        }
        ToolbarCommand::ClearSiteData => clear_all_site_data(window),
        ToolbarCommand::SearchHistory { query } => {
            // An empty query means "search cleared" (see
            // `browser::history::search`'s doc comment and D30) — go back
            // to the normal recency-ordered panel rather than rendering an
            // intentionally-empty search result list.
            if query.trim().is_empty() {
                refresh_history_panel(window, state, config);
            } else {
                search_history_panel(window, state, config, &query);
            }
        }
        ToolbarCommand::RemoveBookmark { id } => {
            if state.bookmarks.remove(id) {
                persist_bookmarks(state);
                refresh_bookmarks_panel(window, state);
                sync_active_bookmark_star(window, window_id, state);
            }
        }
        ToolbarCommand::OpenDownload { id } => open_download(state, DownloadId::from(id)),
        ToolbarCommand::OpenDownloadsFolder => {
            open_downloads_folder(config.download_dir_override.as_deref())
        }
        ToolbarCommand::CancelDownload { id } => {
            cancel_download(state, DownloadId::from(id));
            refresh_downloads_panel(window, state);
        }
        ToolbarCommand::RemoveDownloadEntry { id } => {
            if state.downloads.remove(DownloadId::from(id)) {
                refresh_downloads_panel(window, state);
            }
        }
        // --- Omnibox (Issue #15) ---
        ToolbarCommand::FocusAddressBar => focus_address_bar(window, window_id, state),
        ToolbarCommand::OmniboxInput { input } => {
            // Issue #20: history/bookmark matches, then previously-typed
            // search queries, ranked by `browser::ranking` — see
            // docs/decisions.md D36-D39.
            //
            // **A private window reads neither history nor typed queries**
            // (Issue #157, D136). The three stores are shared by every
            // window in the process (see `AppState`'s doc comment), so
            // without this a private window's omnibox surfaces what a
            // *normal* window recorded moments earlier — while the user is
            // typing, on screen, which is exactly the situation private
            // browsing exists for. D39's premise ("reads of what was
            // recorded before private mode started are fine") held when
            // private browsing was whole-app; it stopped holding when #27
            // made the two coexist.
            //
            // Bookmarks stay: real browsers show them in an incognito
            // omnibox, and a bookmark is something the user saved on
            // purpose, not a trace of where they have been.
            //
            // `window_is_private` answers `true` for a window it does not
            // know, so an unknown id shows *less*, never more.
            let private = window_is_private(state, window_id);
            let now = now_unix();
            let history_bookmark_source = HistoryBookmarkSource {
                history: (!private).then_some(&state.history),
                bookmarks: &state.bookmarks,
                now,
            };
            let input_history_source = InputHistorySource {
                store: &state.input_history,
                search_engine_name: &config.search_engine.name,
                search_query_template: &config.search_engine.query_template,
                now,
            };
            // The typed-query source is dropped wholesale rather than
            // emptied: there is no "bookmarks half" of it worth keeping.
            let sources: Vec<&dyn omnibox::CandidateSource> = if private {
                vec![&history_bookmark_source]
            } else {
                vec![&history_bookmark_source, &input_history_source]
            };
            let candidates = omnibox::build_candidates(
                &input,
                &config.search_engine.name,
                &config.search_engine.query_template,
                &sources,
                omnibox::DEFAULT_CANDIDATE_LIMIT,
            );
            let open = (!candidates.is_empty()).then_some(Panel::Omnibox);
            log_failure("toggle omnibox panel", window.set_panel(open));
            log_failure(
                "update omnibox candidates",
                window.set_candidates(&candidates),
            );
        }
        ToolbarCommand::OmniboxClose => {
            log_failure("close omnibox", window.set_panel(None));
            focus_address_bar(window, window_id, state);
        }
        // --- Bookmark folders, editing, reordering, and the bookmark bar
        //     (Issue #19, see docs/decisions.md D32/D33/D34/D35) ---
        ToolbarCommand::EditBookmark {
            id,
            title,
            url,
            folder_id,
        } => {
            let title = {
                let trimmed = title.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_owned())
            };
            // D33: the one place a bookmark's URL is (re)validated — the
            // exact same `navigate::normalize_input` every other URL in
            // VeloX goes through, so an edit can never smuggle in a
            // rejected scheme (e.g. `javascript:`) that `Navigate` itself
            // would refuse.
            match navigation::normalize_input(&url) {
                Some(normalized) => {
                    if let Err(err) = state.bookmarks.edit(id, title, normalized, folder_id) {
                        eprintln!("velox: rejected bookmark edit for id {id}: {err:?}");
                    } else {
                        persist_bookmarks(state);
                    }
                }
                None => {
                    eprintln!("velox: rejected bookmark edit for id {id}: invalid URL {url:?}");
                }
            }
            // Always refreshed, success or failure, so the panel's inline
            // edit form closes and shows the entry's actual (possibly
            // unchanged) state either way.
            refresh_bookmarks_panel(window, state);
            sync_active_bookmark_star(window, window_id, state);
        }
        ToolbarCommand::CreateBookmarkFolder { name } => {
            let name = name.trim();
            if !name.is_empty() {
                state.bookmarks.create_folder(name.to_owned(), now_unix());
                persist_bookmarks(state);
            }
            refresh_bookmarks_panel(window, state);
        }
        ToolbarCommand::RenameBookmarkFolder { id, name } => {
            let name = name.trim();
            if !name.is_empty() && state.bookmarks.rename_folder(id, name.to_owned()) {
                persist_bookmarks(state);
            }
            refresh_bookmarks_panel(window, state);
        }
        ToolbarCommand::RemoveBookmarkFolder { id } => {
            if state.bookmarks.remove_folder(id) {
                persist_bookmarks(state);
            }
            refresh_bookmarks_panel(window, state);
        }
        ToolbarCommand::MoveBookmarkUp { id } => {
            if state.bookmarks.move_up(id) {
                persist_bookmarks(state);
                refresh_bookmarks_panel(window, state);
            }
        }
        ToolbarCommand::MoveBookmarkDown { id } => {
            if state.bookmarks.move_down(id) {
                persist_bookmarks(state);
                refresh_bookmarks_panel(window, state);
            }
        }
        ToolbarCommand::ToggleBookmarkBar => toggle_bookmark_bar(window),
        // Intercepted in `handle_user_event` before it ever reaches here —
        // see that function's doc comment for why (opening a window, or
        // applying a settings change to every window, needs `&mut
        // HashMap<WindowId, BrowserWindow>` as a whole, which conflicts with
        // the `window: &mut BrowserWindow` entry already borrowed to get
        // here). Listed explicitly, not folded into a wildcard, so a future
        // `ToolbarCommand` variant still fails to compile here instead of
        // silently doing nothing.
        ToolbarCommand::NewWindow | ToolbarCommand::NewPrivateWindow => {}
        ToolbarCommand::UpdateSettings { .. } | ToolbarCommand::ResetSettings => {}

        // --- In-page find (Issue #43), see docs/decisions.md D69, and D68's
        //     multi-window integration: every one of these now targets
        //     `window_id`'s own find session (`Windows::find`/`set_find`/
        //     `take_find`), never a global one shared by every window. ---
        ToolbarCommand::OpenFindBar => open_find_bar(window, window_id, state),
        ToolbarCommand::FindQuery {
            query,
            case_sensitive,
        } => update_find_query(window, window_id, state, query, case_sensitive),
        ToolbarCommand::FindNext => step_find(window, window_id, state, FindDirection::Next),
        ToolbarCommand::FindPrevious => {
            step_find(window, window_id, state, FindDirection::Previous)
        }
        ToolbarCommand::FindClose => close_find_bar(window, window_id, state),

        // --- Save page (Issue #46, "名前を付けて保存"), see
        //     docs/decisions.md D76 ---
        ToolbarCommand::SavePage => request_save_page(window, window_id, state, config),
        // --- Print / PDF export (Issue #40), see docs/decisions.md D75 ---
        ToolbarCommand::Print => print_active_tab(window, window_id, state),
        ToolbarCommand::SaveAsPdf => save_active_tab_as_pdf(window, window_id, state, config),
        // --- View Source (Issue #45), see docs/decisions.md D72 ---
        ToolbarCommand::ViewSource => request_view_source(window, window_id, state),
    }
}

/// Bookmark/unbookmark the active tab's current page (the star button,
/// `ToolbarCommand::ToggleBookmark`, and Ctrl/Cmd+D from either the toolbar
/// or a content webview — `ContentShortcut::ToggleBookmark` — all funnel
/// through here).
fn toggle_current_bookmark(window: &BrowserWindow, window_id: WindowId, state: &mut AppState) {
    let url = tabs_of(state, window_id).active().current_url().to_owned();
    let title = known_title_for(&state.history, &url);
    let now = now_unix();
    let active = state.bookmarks.toggle(&url, title, now);
    persist_bookmarks(state);
    log_failure("update bookmark star", window.set_bookmark_active(active));
    refresh_bookmarks_panel(window, state);
}

/// Show/hide the bookmark bar (Ctrl/Cmd+Shift+B from either the toolbar or a
/// content webview, and the toolbar's own bar-toggle button all funnel
/// through here).
fn toggle_bookmark_bar(window: &BrowserWindow) {
    let next = !window.bookmark_bar_visible();
    log_failure("toggle bookmark bar", window.set_bookmark_bar_visible(next));
}

/// Resolve an already-classified [`Intent`] to a loadable URL: a URL intent
/// passes through unchanged, a search intent is turned into the configured
/// search engine's URL via [`navigation::build_search_url`]. `None` covers
/// every way this can fail to resolve — empty/refused input, or (in
/// practice never, since `Config`'s template always contains the required
/// placeholder) a broken search-engine template.
///
/// Split out from `classify_input` (rather than folding the classification
/// in here, as a single `resolve_navigate_target(config, input)` used to)
/// so `ToolbarCommand::Navigate`'s handler can classify `input` once and
/// reuse the same [`Intent`] both to resolve the destination and — new in
/// Issue #20 — to decide whether the raw input was a search query worth
/// remembering in `InputHistoryStore` (see docs/decisions.md D38).
fn resolve_intent(config: &Config, intent: Option<Intent>) -> Option<String> {
    match intent? {
        Intent::Url(url) => Some(url),
        Intent::Search(query) => {
            navigation::build_search_url(&config.search_engine.query_template, &query)
        }
    }
}

/// Focus the toolbar's address bar and select the active tab's current URL
/// — shared by `ToolbarCommand::FocusAddressBar` (Ctrl/Cmd+L from the
/// toolbar), `ContentShortcut::FocusAddressBar` (Ctrl/Cmd+L from a content
/// webview), and `ToolbarCommand::OmniboxClose` (Esc, which also needs the
/// address bar restored to the real current URL).
fn focus_address_bar(window: &BrowserWindow, window_id: WindowId, state: &mut AppState) {
    log_failure(
        "focus address bar",
        window.focus_address_bar(tabs_of(state, window_id).active().current_url()),
    );
}

/// Navigate the active tab to `url` (already normalized/resolved by the
/// caller). Shared by `ToolbarCommand::Navigate` (address bar submit, a
/// history/bookmark/candidate row click) and `AutomationCommand::Navigate`
/// (Issue #112, `handle_automation_command`) — both already have a
/// ready-to-load URL by the time they get here, just via different
/// resolution paths (`resolve_intent`'s search/URL classification vs.
/// `browser::automation::parse_script`'s `navigation::normalize_input`
/// call).
fn navigate_active_tab(
    window: &BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    url: &str,
) {
    tabs_of(state, window_id)
        .active_mut()
        .on_navigation_started(url);
    log_failure("navigate", window.navigate(url));
}

/// The `is_loading` probe `BrowserWindow::open_tab`/`resume_tab` take (see
/// docs/decisions.md D54): whether tab `id`'s page is still loading, read
/// from the `Tabs` state that `LoadStarted`/`LoadFinished` keep current, so
/// the window never puts a new tab into a web process busy loading another
/// tab's page. Unknown ids (never the case in practice) count as idle.
fn loading_probe(tabs: &Tabs) -> impl Fn(TabId) -> bool + '_ {
    move |id| tabs.get(id).is_some_and(|tab| tab.is_loading())
}

/// Open a new tab at `url` and make it active. The one path every "open a
/// new tab" trigger funnels through — `ToolbarCommand::NewTab` (homepage),
/// `ContentShortcut::NewTab` (homepage), `UserEvent::NewTabRequested`
/// (a `target="_blank"`/`window.open()` URL, see docs/decisions.md D25),
/// and `AutomationCommand::Open` (Issue #112) — so the
/// webview-build-then-activate sequence is written once.
fn open_new_tab(window: &mut BrowserWindow, window_id: WindowId, state: &mut AppState, url: &str) {
    // Reuses the `Instant` `Tabs::open_at` needs anyway, so tab-create
    // latency costs no extra clock read when metrics are off (D19).
    let started = Instant::now();
    let id = tabs_of(state, window_id).open_at(url.to_owned(), started);
    log_failure(
        "open tab",
        window.open_tab(id, url, loading_probe(tabs_of(state, window_id))),
    );
    // A brand new tab's webview was just built above; only its visibility
    // needs to change, never a resume.
    activate_and_refresh(window, window_id, state, id, ActivationEffect::Switch);
    record_tab_latency(state, metrics::TabLatencyKind::Create, id, started);
}

/// Close tab `id` — the shared implementation behind the toolbar's own
/// close button (`ToolbarCommand::CloseTab`), Ctrl/Cmd+W from either the
/// toolbar or the content webview (`CloseActiveTab`/
/// `ContentShortcut::CloseTab`, both of which resolve `id` to the active
/// tab before calling this). A no-op — matching `Tabs::close` — for an
/// unknown id or the last remaining tab.
///
/// Also drops `id`'s `page_load_timers` entry, if any (Issue #62/D79): a
/// page that never finished loading before its tab was closed would
/// otherwise leave that entry behind forever — see [`PageLoadTimers`]'s doc
/// comment.
fn close_tab(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    page_load_timers: &mut PageLoadTimers,
    id: TabId,
) {
    if let Some((new_active, effect)) = tabs_of(state, window_id).close(id) {
        window.close_tab(id);
        page_load_timers.remove(&(window_id, id));
        // The tab that replaces the one just closed may itself have been
        // suspended (a background tab can be suspended while the tab in
        // front of it is closed); `effect` already reflects that
        // (`Tabs::close`), so `activate_and_refresh` resumes it if needed
        // without re-deriving it here.
        activate_and_refresh(window, window_id, state, new_active, effect);
    }
    // Otherwise: unknown id, or `id` was the only remaining tab — VeloX
    // always keeps at least one tab open.
}

/// Reopen the most recently closed tab (Ctrl/Cmd+Shift+T, from either the
/// toolbar or the content webview). A no-op if nothing has been closed yet
/// (see `browser::tabs::Tabs::reopen_closed`).
fn reopen_closed_tab(window: &mut BrowserWindow, window_id: WindowId, state: &mut AppState) {
    let started = Instant::now();
    let Some(id) = tabs_of(state, window_id).reopen_closed(started) else {
        return;
    };
    let url = tabs_of(state, window_id)
        .get(id)
        .map(|tab| tab.current_url().to_owned())
        .unwrap_or_default();
    log_failure(
        "reopen tab",
        window.open_tab(id, &url, loading_probe(tabs_of(state, window_id))),
    );
    activate_and_refresh(window, window_id, state, id, ActivationEffect::Switch);
    // A reopened tab builds a fresh webview at the remembered URL, so it is
    // a tab creation as far as D19's latency metric is concerned.
    record_tab_latency(state, metrics::TabLatencyKind::Create, id, started);
}

/// Close `window_id`'s active tab (Ctrl/Cmd+W from either the toolbar —
/// `ToolbarCommand::CloseActiveTab` — or a content webview —
/// `ContentShortcut::CloseTab`) via [`close_tab`].
fn close_active_tab(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    page_load_timers: &mut PageLoadTimers,
) {
    let id = tabs_of(state, window_id).active_id();
    close_tab(window, window_id, state, page_load_timers, id);
}

/// Make tab `id` active (a tab-strip click, `ToolbarCommand::ActivateTab`,
/// or the `switch <index>` automation command) and log the switch latency.
/// A no-op for an unknown id (`Tabs::activate_at` returns `None`).
fn activate_tab(window: &mut BrowserWindow, window_id: WindowId, state: &mut AppState, id: TabId) {
    let started = Instant::now();
    if let Some(effect) = tabs_of(state, window_id).activate_at(id, started) {
        activate_and_refresh(window, window_id, state, id, effect);
        record_tab_latency(state, switch_latency_kind(effect), id, started);
    }
}

/// `activate` (`Tabs` の相対/位置指定アクティベーションのいずれか) を
/// 計測開始時刻つきで実行し、結果を [`apply_activation`] で反映する —
/// ツールバーとコンテンツ側ショートカットの Next/Prev/位置指定/最後のタブ
/// がすべて通る共通経路。
fn activate_with(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    activate: impl FnOnce(&mut Tabs, Instant) -> Option<ActivationEffect>,
) {
    let started = Instant::now();
    let effect = activate(tabs_of(state, window_id), started);
    apply_activation(window, window_id, state, effect, started);
}

/// Apply an activation `effect` already resolved by one of `Tabs`'
/// relative/positional activation methods (`activate_relative`,
/// `activate_by_position`, `activate_last`) against the tab that is now
/// active. `None` (nothing to apply — e.g. `ActivateTabByIndex` for a
/// position with no tab) is a silent no-op.
fn apply_activation(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    effect: Option<ActivationEffect>,
    started: Instant,
) {
    if let Some(effect) = effect {
        let id = tabs_of(state, window_id).active_id();
        activate_and_refresh(window, window_id, state, id, effect);
        record_tab_latency(state, switch_latency_kind(effect), id, started);
    }
}

/// Which latency event a tab switch is logged as: `tab_switch` for a tab
/// that already had a live webview, `tab_resume` when the switch had to
/// rebuild a suspended tab's webview first (Issue #63) — see
/// `metrics::TabLatencyKind::Resume` for why the two are kept apart.
fn switch_latency_kind(effect: ActivationEffect) -> metrics::TabLatencyKind {
    match effect {
        ActivationEffect::Switch => metrics::TabLatencyKind::Switch,
        ActivationEffect::Resume => metrics::TabLatencyKind::Resume,
    }
}

/// Dispatch one content-webview keyboard shortcut (see
/// `ui::window::ContentShortcut` and docs/decisions.md D18/D23) to the same
/// tab operations the toolbar's own equivalent commands use — every branch
/// here mirrors one `ToolbarCommand` arm in `handle_toolbar_command`.
#[allow(clippy::too_many_arguments)]
fn handle_content_shortcut(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    config: &Config,
    homepage: &str,
    page_load_timers: &mut PageLoadTimers,
    shortcut: ContentShortcut,
) {
    match shortcut {
        ContentShortcut::NewTab => open_new_tab(window, window_id, state, homepage),
        ContentShortcut::CloseTab => close_active_tab(window, window_id, state, page_load_timers),
        ContentShortcut::ReopenClosedTab => reopen_closed_tab(window, window_id, state),
        ContentShortcut::NextTab => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_relative(1, now)
            });
        }
        ContentShortcut::PrevTab => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_relative(-1, now)
            });
        }
        ContentShortcut::ActivateTabAt(position) => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_by_position(position as usize, now)
            });
        }
        ContentShortcut::ActivateLastTab => {
            activate_with(window, window_id, state, |tabs, now| {
                tabs.activate_last(now)
            });
        }
        ContentShortcut::FocusAddressBar => focus_address_bar(window, window_id, state),
        ContentShortcut::ToggleBookmark => toggle_current_bookmark(window, window_id, state),
        ContentShortcut::ToggleBookmarkBar => toggle_bookmark_bar(window),
        // Intercepted in `handle_user_event` before it ever reaches here —
        // see that function's doc comment (same reason as
        // `ToolbarCommand::NewWindow` in `handle_toolbar_command`).
        ContentShortcut::NewWindow | ContentShortcut::NewPrivateWindow => {}
        ContentShortcut::OpenFindBar => open_find_bar(window, window_id, state),
        ContentShortcut::SavePage => request_save_page(window, window_id, state, config),
        ContentShortcut::Print => print_active_tab(window, window_id, state),
        ContentShortcut::ViewSource => request_view_source(window, window_id, state),
    }
}

/// Dispatch one resolved, already-sanitized context-menu action (Issue #39,
/// see docs/decisions.md D78) — every branch here reuses an existing shared
/// tab-management function, exactly like [`handle_content_shortcut`] mirrors
/// `handle_toolbar_command`. `action` has already been resolved against the
/// exact menu list that was rendered (`context_menu::OpenContextMenu::
/// resolve`), so nothing here re-checks `enabled` or re-validates a URL —
/// that already happened in `browser::context_menu::sanitize`/`build_menu`.
///
/// `tab_id` is the tab the menu was opened on (not necessarily the active
/// tab any more by the time the user clicks a row, though in practice it
/// almost always still is — see `context_menu::RawMenuContext`'s doc
/// comment) — actions that read/write a specific tab's webview (`Copy`/
/// `Paste`) act on *that* tab; actions that open something new
/// (`SearchSelection`/`OpenLinkInNewTab`/`OpenImageInNewTab`) still open it
/// as a new tab in this window, matching every other "open a new tab" path.
///
/// [`context_menu::MenuAction::OpenLinkInNewWindow`] is intercepted in
/// `handle_user_event` before this function is ever called (needs
/// `&mut ui_windows` as a whole, same reason `ContentShortcut::NewWindow`
/// is) — its arm here is unreachable in practice but kept so this match
/// stays exhaustive over the whole enum.
fn handle_context_menu_action(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    config: &Config,
    tab_id: TabId,
    action: context_menu::MenuAction,
) {
    match action {
        context_menu::MenuAction::Back => log_failure("context menu back", window.go_back()),
        context_menu::MenuAction::Forward => {
            log_failure("context menu forward", window.go_forward())
        }
        context_menu::MenuAction::Reload => log_failure("context menu reload", window.reload()),
        context_menu::MenuAction::Copy => {
            log_failure("context menu copy", window.copy_selection(tab_id))
        }
        context_menu::MenuAction::Paste => {
            log_failure("context menu paste", window.paste_into(tab_id))
        }
        context_menu::MenuAction::SearchSelection(query) => {
            if let Some(url) =
                navigation::build_search_url(&config.search_engine.query_template, &query)
            {
                open_new_tab(window, window_id, state, &url);
            }
        }
        context_menu::MenuAction::OpenLinkInNewTab(url) => {
            open_new_tab(window, window_id, state, &url)
        }
        context_menu::MenuAction::OpenImageInNewTab(url) => {
            open_new_tab(window, window_id, state, &url)
        }
        // Issue #161: the same helpers Ctrl/Cmd+S and Ctrl/Cmd+P go through
        // (`ToolbarCommand::SavePage`/`Print` and their `ContentShortcut`
        // twins), not a second implementation — so the context menu cannot
        // drift away from the shortcut in format, destination or error
        // handling. Both act on the active tab, which is the tab the menu
        // was opened on: a background tab's webview is hidden, so it never
        // receives the right-click that opens one.
        context_menu::MenuAction::SavePage => request_save_page(window, window_id, state, config),
        context_menu::MenuAction::Print => print_active_tab(window, window_id, state),
        context_menu::MenuAction::Inspect => window.open_devtools(),
        // See this function's doc comment.
        context_menu::MenuAction::OpenLinkInNewWindow(_) => {}
    }
}

/// Show `id` in the window, then bring the toolbar (address bar, loading
/// indicator, bookmark star, block-count badge, tab strip) up to date with
/// the now-active tab. The caller must have already made `id` the active
/// tab in `window_id`'s `Tabs` (`activate`/`activate_at`, `open`/`open_at`,
/// or the replacement tab returned by `close`) and pass along the
/// [`ActivationEffect`] that call reported.
///
/// `effect` decides which `BrowserWindow` call applies `id` on the webview
/// side: [`ActivationEffect::Resume`] rebuilds a suspended tab's dropped
/// webview (`resume_tab`, loading its last known URL); `Switch` just changes
/// which already-live webview is visible (`activate_tab`). `Tabs` (not this
/// function) is what already resolved the tab's state transition — see
/// `browser::tabs::Tabs::resolve_activation` — so this only has to act on
/// the answer.
fn activate_and_refresh(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    id: TabId,
    effect: ActivationEffect,
) {
    // Issue #43/D69, integrated with multi-window in D68: the find bar is
    // tied to whichever tab was active *in this window* when it opened (see
    // `browser::find::FindState`'s doc comment) — any tab switch in this
    // window invalidates that, so close this window's own find session
    // (`Windows::find`) rather than let it keep showing stale
    // highlights/counts for a tab that is no longer on screen. Never
    // touches another window's independent find session.
    if state.windows.find(window_id).is_some() {
        close_find_bar(window, window_id, state);
    }
    // Issue #39/D78: a context menu belongs to the specific tab it was
    // opened against; switching away from that tab in this window makes it
    // stale the same way a tab switch invalidates the find bar above.
    // Unlike a navigation (which replaces the DOM the overlay lives in), a
    // tab switch just hides the previous tab's still-live webview (D8/D9),
    // so the overlay would otherwise still be sitting in that tab's DOM the
    // next time it is switched back to — `hide_context_menu` removes it
    // from *that* tab specifically, not whichever tab ends up active.
    if let Some(menu) = state.windows.take_context_menu(window_id) {
        log_failure("hide context menu", window.hide_context_menu(menu.tab_id()));
    }
    let result = match effect {
        ActivationEffect::Resume => {
            let tabs = tabs_of(state, window_id);
            window.resume_tab(id, tabs.active().current_url(), loading_probe(tabs))
        }
        ActivationEffect::Switch => window.activate_tab(id),
    };
    log_failure(
        match effect {
            ActivationEffect::Resume => "resume tab",
            ActivationEffect::Switch => "activate tab",
        },
        result,
    );
    if let Some(tab) = tabs_of(state, window_id).get(id) {
        let url = tab.current_url().to_owned();
        log_failure("update address bar", window.set_url_display(&url));
        log_failure("update loading state", window.set_loading(tab.is_loading()));
        sync_bookmark_star(window, state, &url);
    }
    sync_block_count(window, tabs_of(state, window_id));
    sync_tab_strip(window, window_id, state);
}

/// Push the full tab list to the toolbar's tab strip, and persist a fresh
/// session snapshot (Issue #25, D65) — piggybacking on this function rather
/// than adding a parallel call at each of its call sites, since every
/// tab-affecting change already routes through here to keep the tab strip
/// current. `sync_tab_strip` runs unconditionally (`window.set_tabs` doesn't
/// care about private mode); `persist_session` below is what actually gates
/// writing to disk on this window's own privacy/`data_dir`.
fn sync_tab_strip(window: &BrowserWindow, window_id: WindowId, state: &mut AppState) {
    let tabs = tabs_of(state, window_id);
    let active_id = tabs.active_id();
    let summaries: Vec<toolbar::TabSummary> = tabs
        .iter()
        .map(|tab| toolbar::TabSummary {
            id: tab.id().get(),
            url: tab.current_url().to_owned(),
            title: tab.title().map(str::to_owned),
            favicon: match tab.favicon() {
                Favicon::Url(url) => Some(url.clone()),
                Favicon::Unknown => None,
            },
            loading: tab.is_loading(),
            active: tab.id() == active_id,
            suspended: tab.is_suspended(),
            pinned: tab.is_pinned(),
        })
        .collect();
    log_failure("update tab strip", window.set_tabs(&summaries));
    persist_session(state);
}

/// Push the active tab's blocked-navigation count to the toolbar badge.
/// Background tabs keep accumulating their own `Tab::blocked_count` (see
/// `UserEvent::NavigationBlocked`) without touching the badge until they
/// become active, the same active-tab-only pattern `sync_bookmark_star`
/// uses for the bookmark star.
fn sync_block_count(window: &BrowserWindow, tabs: &Tabs) {
    log_failure(
        "update block counter",
        window.set_block_count(tabs.active().blocked_count()),
    );
}

/// Whether window `window_id` is private (Issue #27, D74) — the single
/// choke point [`record_visit_if_enabled`]/[`record_input_history_if_enabled`]/
/// [`persist_session`] all gate on, replacing what used to be one
/// process-wide `AppState::history_enabled` bool (see docs/decisions.md
/// D13/D14/D68). An unknown `window_id` (the window closed before this
/// check ran, or was never opened — should not happen for a `window_id` a
/// caller just resolved a live `BrowserWindow`/`Tabs` from, but there is no
/// need to `expect` it here) defaults to `true` — the safe-by-default
/// direction, matching `TabId`'s own "a stale id is a no-op" convention:
/// treating a vanished window as private only ever means "record one fewer
/// visit than strictly necessary", never the other, much worse way around.
///
/// Issue #157 added a *read*-side consumer (which omnibox candidate sources
/// a window may draw from). The same direction is safe there for the same
/// reason: an unknown window shows one fewer suggestion, never one more.
fn window_is_private(state: &AppState, window_id: WindowId) -> bool {
    state.windows.is_private(window_id).unwrap_or(true)
}

/// Record a page visit in window `window_id` if that window's history
/// recording is currently enabled (Issue #27, D74: private browsing is now
/// per-window, not whole-app — see [`window_is_private`]).
///
/// This is the single call site page loads flow through on their way into
/// `state.history`.
fn record_visit_if_enabled(
    state: &mut AppState,
    window_id: WindowId,
    url: &str,
    max_entries: usize,
) -> Option<u64> {
    if window_is_private(state, window_id) {
        return None;
    }
    Some(
        state
            .history
            .record_visit(url, None, now_unix(), max_entries),
    )
}

/// Record a submitted search query to `state.input_history`, gated by the
/// exact same per-window privacy check [`record_visit_if_enabled`] uses
/// (Issue #20/#27 — see docs/decisions.md D38/D39/D74: input history is
/// part of the same privacy surface as page-visit history, so it follows
/// the same rule — recording is skipped for a private window, but entries
/// recorded before that window opened (by any window) stay readable for
/// candidates, same as `HistoryStore`).
fn record_input_history_if_enabled(
    state: &mut AppState,
    window_id: WindowId,
    text: &str,
    max_entries: usize,
) {
    if window_is_private(state, window_id) {
        return;
    }
    state.input_history.record(text, now_unix(), max_entries);
}

/// Best-effort title for `url` from what history already knows, used when
/// bookmarking a page so the bookmark shows a name instead of a bare URL.
fn known_title_for(history: &HistoryStore, url: &str) -> Option<String> {
    history
        .entries_newest_first()
        .find(|entry| entry.url == url)
        .and_then(|entry| entry.title.clone())
}

/// Push the bookmark ("star") button's active state for `url`.
fn sync_bookmark_star(window: &BrowserWindow, state: &AppState, url: &str) {
    log_failure(
        "update bookmark star",
        window.set_bookmark_active(state.bookmarks.is_bookmarked(url)),
    );
}

/// [`sync_bookmark_star`] for `window_id`'s active tab's current URL.
fn sync_active_bookmark_star(window: &BrowserWindow, window_id: WindowId, state: &mut AppState) {
    let url = tabs_of(state, window_id).active().current_url().to_owned();
    sync_bookmark_star(window, state, &url);
}

/// Handle `ToolbarCommand::ClearSiteData` (Issue #26, docs/decisions.md
/// D66): delegate to `BrowserWindow::clear_all_site_data` and log the
/// outcome. Never returns an error to the caller — a failed clear is not
/// fatal (the acceptance condition "削除失敗時に安全にエラー処理される") —
/// and stays silent on full success the same way `persist_*`'s
/// `log_failure` calls do, only speaking up when there is something the
/// user might need to know about.
fn clear_all_site_data(window: &BrowserWindow) {
    let result = window.clear_all_site_data();
    let first_error = || {
        result
            .first_error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    match site_data::summarize(result.attempted, result.failed) {
        ClearOutcome::Success | ClearOutcome::Nothing => {}
        ClearOutcome::Partial => eprintln!(
            "velox: cleared site data on {}/{} webviews; first error: {}",
            result.attempted - result.failed,
            result.attempted,
            first_error()
        ),
        ClearOutcome::AllFailed => eprintln!(
            "velox: failed to clear site data on all {} webview(s): {}",
            result.attempted,
            first_error()
        ),
    }
}

/// `VELOX_DEBUG` が設定されていれば、診断用の詳細ログを stderr に出す。
fn debug_logging_enabled() -> bool {
    std::env::var_os("VELOX_DEBUG").is_some()
}

/// Current time as a unix timestamp (seconds). Falls back to `0` on a clock
/// set before 1970, which should never happen in practice; kept infallible
/// so callers never need to thread a `Result` through for it.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Maximum number of `char`s of a malformed/oversized IPC body ever printed
/// to stderr by [`handle_user_event`]'s `ToolbarMessage` arm.
const LOG_PREVIEW_MAX_CHARS: usize = 200;

/// Truncate `text` to at most [`LOG_PREVIEW_MAX_CHARS`] characters for a log
/// line, appending `…` when something was cut (Issue #35: a malformed IPC
/// message can legitimately be many megabytes — e.g. a giant clipboard
/// paste rejected by `toolbar::MAX_IPC_PAYLOAD_BYTES` — and dumping the
/// whole thing into stderr on every rejection would itself be an unbounded
/// sink, working against the very size cap that rejected it). Truncates on
/// a `char` boundary (via `chars()`), never a byte boundary, so this can
/// never panic on multi-byte UTF-8 input.
fn log_preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(LOG_PREVIEW_MAX_CHARS).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

/// A failed UI call (e.g. a script that could not be evaluated) should not
/// crash the browser; surface it on stderr instead.
///
/// 結果型についてジェネリックにしてあり、永続化が返す IO エラーや
/// `downloads::spawn_open` のプロセス起動結果にも同じヘルパーを使う。
/// 後者で起動した子プロセス (`Ok` 側の値) を待たず追跡もしないのは意図的 —
/// 「このファイル/フォルダを別アプリで開く」は投げっぱなしの操作で、実際の
/// デスクトップブラウザの「フォルダに表示」「ファイルを開く」と同じ扱い。
fn log_failure<T, E: std::fmt::Display>(action: &str, result: Result<T, E>) {
    if let Err(err) = result {
        eprintln!("velox: failed to {action}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // --- log_preview (Issue #35): a malformed/oversized IPC body must
    // never be dumped to stderr in full. ---

    #[test]
    fn log_preview_leaves_short_text_unchanged() {
        assert_eq!(log_preview(""), "");
        assert_eq!(log_preview("short message"), "short message");
    }

    #[test]
    fn log_preview_truncates_long_text_with_an_ellipsis() {
        let huge = "a".repeat(5_000_000);
        let preview = log_preview(&huge);
        assert_eq!(preview.chars().count(), LOG_PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn log_preview_does_not_panic_on_multibyte_utf8_near_the_cut_point() {
        // Every character here is multi-byte; truncation must happen on a
        // `char` boundary, never mid-codepoint (which would panic on a
        // naive byte-index slice).
        let text = "あ".repeat(LOG_PREVIEW_MAX_CHARS + 50);
        let preview = log_preview(&text);
        assert_eq!(preview.chars().count(), LOG_PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
        // Re-parsing as UTF-8 must succeed (proves no boundary was cut).
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
    }

    #[test]
    fn log_preview_of_exactly_the_cap_has_no_ellipsis() {
        let text = "a".repeat(LOG_PREVIEW_MAX_CHARS);
        assert_eq!(log_preview(&text), text);
    }

    /// The [`WindowId`] `state_with_history_enabled` always opens — every
    /// test that needs one calls this rather than re-deriving it from
    /// `state.windows.ids().next()`, since `Windows::new` never issues
    /// anything else as the very first id.
    fn test_window_id() -> WindowId {
        WindowId::from(0)
    }

    /// Build an `AppState` the way `run()` would for a fresh tab, with its
    /// one window's history recording set to `history_enabled` (i.e. its
    /// privacy is `!history_enabled` — what `Config::private` drove at
    /// startup pre-#27, now a per-window flag on `Windows` itself, see
    /// docs/decisions.md D13/D14/D74). Single-window (Issue #29's
    /// multi-window-specific behavior — `Windows`, `open_new_window` — is
    /// covered by `browser::windows`'s own unit tests instead; this helper
    /// only needs *a* window to exist for every pre-#29 test below to keep
    /// working unchanged).
    fn state_with_history_enabled(history_enabled: bool) -> AppState {
        state_with_windows(Windows::new_with_privacy(
            "https://example.com/",
            !history_enabled,
        ))
    }

    /// `windows` を持つ、それ以外は空の (データディレクトリ・metrics なし)
    /// `AppState`。
    fn state_with_windows(windows: Windows) -> AppState {
        AppState {
            windows,
            site_policies: SitePolicies {
                blocklist: Arc::new(FilterList::built_in()),
                site_exceptions: Arc::new(SiteExceptions::from_hosts(Vec::<String>::new())),
                site_permissions: Arc::new(SitePermissionStore::new()),
            },
            history: HistoryStore::new(),
            bookmarks: BookmarkStore::new(),
            input_history: InputHistoryStore::new(),
            data_dir: None,
            last_persisted_session: None,
            perf: None,
            downloads: DownloadStore::new(),
            pending_memory_sample: None,
            next_memory_sample_window: None,
            settings: Settings::default(),
            site_permissions: Arc::new(SitePermissionStore::new()),
        }
    }

    #[test]
    fn records_a_visit_when_history_is_enabled() {
        let mut state = state_with_history_enabled(true);
        let id = record_visit_if_enabled(&mut state, test_window_id(), "https://example.com/", 0);
        assert!(id.is_some());
        assert_eq!(state.history.entries().len(), 1);
    }

    #[test]
    fn private_mode_records_no_visit() {
        // This is the private-browsing invariant from docs/decisions.md
        // D13/D14/D74: with history recording disabled (as it is for a
        // private window), a page load must never reach
        // `HistoryStore::record_visit`.
        let mut state = state_with_history_enabled(false);
        let id = record_visit_if_enabled(&mut state, test_window_id(), "https://example.com/", 0);
        assert!(id.is_none());
        assert!(state.history.entries().is_empty());
    }

    #[test]
    fn an_unknown_window_is_treated_as_private() {
        // The direction this defaults in is a safety property, not an
        // implementation detail: `record_visit_if_enabled` and (since
        // Issue #157) the omnibox's candidate sources both branch on it.
        // Defaulting the other way would record a visit, or surface another
        // window's browsing, for a window nobody can account for.
        let state = state_with_history_enabled(true);
        assert!(!window_is_private(&state, test_window_id()));
        assert!(
            window_is_private(&state, WindowId::from(9999)),
            "未知のウィンドウが private 扱いになっていない — 記録も候補表示も安全側に倒れない"
        );
    }

    #[test]
    fn private_mode_history_store_stays_empty_across_multiple_loads() {
        let mut state = state_with_history_enabled(false);
        for url in [
            "https://a.example/",
            "https://b.example/",
            "https://a.example/",
        ] {
            assert!(record_visit_if_enabled(&mut state, test_window_id(), url, 0).is_none());
        }
        assert!(state.history.entries().is_empty());
    }

    #[test]
    fn a_private_window_records_no_visit_while_a_normal_window_with_the_same_tab_id_still_does() {
        // The critical multi-window scenario Issue #27/D74 exists to get
        // right: a normal window and a private window opened side by side
        // end up with the same `TabId` (see `browser::windows`'s
        // `each_window_has_its_own_independent_tab_id_space`), so gating
        // recording off anything keyed by `TabId` alone (instead of
        // `WindowId`) would either leak a private visit into `state.history`
        // or wrongly suppress the normal window's own recording. The #29
        // PR description names three real bugs of exactly this shape found
        // in search/settings — this test exists so this issue does not add
        // a fourth for history/input-history recording.
        let mut windows = Windows::new("https://normal.example/");
        let normal_window = windows.ids().next().unwrap();
        let private_window = windows.open_window_with_privacy("https://private.example/", true);
        assert_eq!(
            windows.tabs(normal_window).unwrap().active_id(),
            windows.tabs(private_window).unwrap().active_id(),
            "test assumes both windows share a TabId value"
        );

        let mut state = state_with_windows(windows);

        let private_id = record_visit_if_enabled(
            &mut state,
            private_window,
            "https://private.example/visited",
            0,
        );
        assert!(
            private_id.is_none(),
            "the private window must not record a visit"
        );
        let normal_id = record_visit_if_enabled(
            &mut state,
            normal_window,
            "https://normal.example/visited",
            0,
        );
        assert!(
            normal_id.is_some(),
            "the normal window sharing the same TabId must still record its visit"
        );
        assert_eq!(
            state
                .history
                .entries()
                .iter()
                .map(|entry| entry.url.as_str())
                .collect::<Vec<_>>(),
            vec!["https://normal.example/visited"],
            "only the normal window's visit should have reached HistoryStore"
        );

        // Same story for input history (Issue #20/#27): the private
        // window's typed query must not reach `input_history`, and the
        // normal window's must.
        record_input_history_if_enabled(&mut state, private_window, "private query", 0);
        record_input_history_if_enabled(&mut state, normal_window, "normal query", 0);
        let recorded: Vec<&str> = state
            .input_history
            .entries()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(recorded, vec!["normal query"]);
    }

    #[test]
    fn persist_history_is_a_noop_without_a_data_dir() {
        // Exercises the same "no write happens" path a private session with
        // no data_dir override would take; a data_dir is only ever set from
        // `persistence::default_data_dir()` in `run()`, unaffected by a
        // window's own privacy, so the write-suppression for private mode
        // has to come entirely from never producing history entries to
        // persist in the first place (checked above) rather than from
        // `persist_history` deciding not to write.
        let state = state_with_history_enabled(false);
        assert!(state.data_dir.is_none());
        // Should not panic and should not touch the filesystem.
        persist_history(&state);
    }

    /// A fresh, unique temp directory for a `persist_session` test to write
    /// into — mirrors `unique_temp_file` below (added for the download
    /// tests) but for a directory `persistence::save_session` can create
    /// `session.json` under.
    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir = unique_temp_file(&format!("velox-app-test-{label}"));
        std::fs::create_dir_all(&dir).expect("create a temp dir for a persist_session test");
        dir
    }

    #[test]
    fn persist_session_skips_an_all_private_process_but_writes_a_normal_one() {
        // Issue #27/D74's other acceptance criterion for session
        // persistence: a process launched with `--private` (so its only
        // window is private) must never write `session.json`, exactly like
        // `record_visit_if_enabled` must never write to `state.history`.
        //
        // Since Issue #149 this no longer rides on "only the primary window
        // is saved" — every non-private window is saved, and the guarantee
        // is that a private one is not among them.
        let dir = unique_temp_dir("private-session");
        let mut state = state_with_history_enabled(false); // private == true
        state.data_dir = Some(dir.clone());

        persist_session(&mut state);
        assert!(
            !dir.join("session.json").exists(),
            "an all-private process must never write session.json"
        );

        // Flipping the same window to non-private (a fresh, non-private
        // `AppState`, same shape otherwise) must write it.
        let mut normal_state = state_with_history_enabled(true);
        normal_state.data_dir = Some(dir.clone());
        persist_session(&mut normal_state);
        assert!(
            dir.join("session.json").exists(),
            "a normal window must still write session.json"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persist_session_saves_every_window_and_leaves_private_ones_out() {
        // Issue #149's core: a second window's tabs must reach disk too —
        // and a private one's must not, even when a normal window in the
        // same process is being saved alongside it.
        let dir = unique_temp_dir("multi-window-session");
        let mut state = state_with_history_enabled(true);
        state.data_dir = Some(dir.clone());
        state
            .windows
            .open_window_with_privacy("https://second.example/".to_owned(), false);
        let private = state
            .windows
            .open_window_with_privacy("https://secret.example/".to_owned(), true);
        assert!(state.windows.is_private(private).unwrap());

        persist_session(&mut state);

        let saved = persistence::load_session(&dir)
            .expect("session.json")
            .sanitize()
            .expect("a usable snapshot");
        assert_eq!(saved.windows.len(), 2, "{saved:?}");
        let urls: Vec<&str> = saved
            .windows
            .iter()
            .map(|window| window.tabs[0].url.as_str())
            .collect();
        assert!(urls.contains(&"https://second.example/"), "{urls:?}");
        assert!(
            !urls.iter().any(|url| url.contains("secret")),
            "プライベートウィンドウのタブがディスクに出ている: {urls:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persist_session_skips_a_redundant_write_when_the_snapshot_is_unchanged() {
        // Issue #67/D86: `sync_tab_strip` calls `persist_session`
        // unconditionally after nearly every tab-affecting event, but most
        // of those do not change what `SessionSnapshot` actually captures
        // (url/title/favicon) — this must not re-write identical content.
        let dir = unique_temp_dir("redundant-session");
        let mut state = state_with_history_enabled(true);
        state.data_dir = Some(dir.clone());
        let window_id = state.windows.ids().next().unwrap();

        persist_session(&mut state);
        let path = dir.join("session.json");
        assert!(path.exists(), "the first call must write session.json");
        assert!(state.last_persisted_session.is_some());

        // Overwrite the file on disk out from under `persist_session` with
        // a sentinel — if a second call with an unchanged in-memory
        // snapshot actually re-serializes and re-writes (rather than being
        // skipped), this sentinel is what gets clobbered.
        std::fs::write(&path, "sentinel").expect("overwrite with sentinel");
        persist_session(&mut state);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "sentinel",
            "a snapshot identical to the last write must not touch the file"
        );

        // A real change (a new tab) must still be written.
        state
            .windows
            .tabs_mut(window_id)
            .unwrap()
            .open("https://second.example/");
        persist_session(&mut state);
        assert_ne!(
            std::fs::read_to_string(&path).unwrap(),
            "sentinel",
            "an actual snapshot change must still be written"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_tab_has_no_blocked_navigations_in_app_state() {
        let mut state = state_with_history_enabled(true);
        assert_eq!(
            tabs_of(&mut state, test_window_id())
                .active()
                .blocked_count(),
            0
        );
    }

    #[test]
    fn record_tab_latency_is_a_noop_when_perf_metrics_are_off() {
        let mut state = state_with_history_enabled(true);
        assert!(state.perf.is_none());
        let id = tabs_of(&mut state, test_window_id()).active_id();
        // Must not panic; there is nothing to assert on beyond that, since
        // "off" means no write happens at all.
        record_tab_latency(&state, metrics::TabLatencyKind::Create, id, Instant::now());
    }

    #[test]
    fn record_tab_latency_writes_through_perf_log_when_metrics_are_on() {
        let mut state = state_with_history_enabled(true);
        state.perf = Some(PerfContext {
            process_start: Instant::now(),
            log: Arc::new(PerfLog::stderr(metrics::PerfFormat::Text)),
        });
        let id = tabs_of(&mut state, test_window_id()).active_id();
        let started = Instant::now();
        // Exercises the write path end-to-end (stderr sink); nothing to
        // assert on the output itself here, but this must not panic.
        record_tab_latency(&state, metrics::TabLatencyKind::Switch, id, started);
    }

    // --- page_load_timers lifetime (Issue #62/D79): a `NavigationStarted`/
    // `LoadFinished` pair must not leave a permanent entry behind — see
    // `PageLoadTimers`'s doc comment for why an unbounded map here is a real
    // leak (`WindowId`/`TabId` are never reused). ---

    #[test]
    fn load_finished_removes_the_page_load_timer_entry() {
        let mut page_load_timers: PageLoadTimers = HashMap::new();
        let mut startup = None;
        let log = PerfLog::stderr(metrics::PerfFormat::Text);
        let process_start = Instant::now();
        let window_id = test_window_id();
        let id = TabId::from(0);

        record_perf_event(
            &mut startup,
            &mut page_load_timers,
            &log,
            process_start,
            &UserEvent::NavigationStarted(window_id, id, "https://example.com/".to_owned()),
        );
        assert_eq!(page_load_timers.len(), 1, "start must record an entry");

        record_perf_event(
            &mut startup,
            &mut page_load_timers,
            &log,
            process_start,
            &UserEvent::LoadFinished(window_id, id, "https://example.com/".to_owned()),
        );
        assert!(
            page_load_timers.is_empty(),
            "finish must remove the entry, not just consume its start mark, or every tab a \
             session ever navigates leaks one entry for the life of the process (TabId/WindowId \
             are never reused)"
        );
    }

    #[test]
    fn load_started_between_navigation_started_and_load_finished_splits_the_page_load_record() {
        // Issue #69: `LoadStarted` marks the mid-checkpoint that splits
        // `page_load`'s `duration_ms` into `engine_duration_ms` (the
        // black-box portion, Epic #57 rule 3) and `dispatch_duration_ms`
        // (VeloX's own event handling). End-to-end through
        // `record_perf_event`, asserting on the actual JSON line a
        // `velox-bench` trial would parse.
        let mut page_load_timers: PageLoadTimers = HashMap::new();
        let mut startup = None;
        let path = unique_temp_file("velox-app-page-load-stages");
        let _ = std::fs::remove_file(&path);
        let log = PerfLog::to_file(metrics::PerfFormat::Json, &path).expect("open perf log file");
        let process_start = Instant::now();
        let window_id = test_window_id();
        let id = TabId::from(0);

        record_perf_event(
            &mut startup,
            &mut page_load_timers,
            &log,
            process_start,
            &UserEvent::NavigationStarted(window_id, id, "https://example.com/".to_owned()),
        );
        record_perf_event(
            &mut startup,
            &mut page_load_timers,
            &log,
            process_start,
            &UserEvent::LoadStarted(window_id, id, "https://example.com/".to_owned()),
        );
        record_perf_event(
            &mut startup,
            &mut page_load_timers,
            &log,
            process_start,
            &UserEvent::LoadFinished(window_id, id, "https://example.com/".to_owned()),
        );
        assert!(
            page_load_timers.is_empty(),
            "finish must still remove the entry"
        );
        drop(log);

        let contents = std::fs::read_to_string(&path).expect("read perf log file");
        let _ = std::fs::remove_file(&path);
        let page_load_line = contents
            .lines()
            .find(|line| line.contains("\"event\":\"page_load\""))
            .expect("a page_load record must have been written");
        let value: serde_json::Value = serde_json::from_str(page_load_line).expect("valid JSON");
        assert!(
            !value["engine_duration_ms"].is_null(),
            "LoadStarted fired before LoadFinished, so engine_duration_ms must be a number, got {value}"
        );
        assert!(!value["dispatch_duration_ms"].is_null(), "got {value}");
    }

    #[test]
    fn load_finished_without_a_matching_start_leaves_no_entry() {
        // A stray `LoadFinished` (e.g. this event arrived for a tab whose
        // `NavigationStarted` was never recorded) must not create or leave
        // an entry either.
        let mut page_load_timers: PageLoadTimers = HashMap::new();
        let mut startup = None;
        let log = PerfLog::stderr(metrics::PerfFormat::Text);
        record_perf_event(
            &mut startup,
            &mut page_load_timers,
            &log,
            Instant::now(),
            &UserEvent::LoadFinished(
                test_window_id(),
                TabId::from(0),
                "https://example.com/".to_owned(),
            ),
        );
        assert!(page_load_timers.is_empty());
    }

    #[test]
    fn blocked_navigation_increments_only_the_target_tabs_counter() {
        let mut state = state_with_history_enabled(true);
        let window_id = test_window_id();
        let active_id = tabs_of(&mut state, window_id).active_id();
        // Opening a tab makes it active; activate the original tab again so
        // the new one is a real background tab for this test.
        let background_id =
            tabs_of(&mut state, window_id).open_at("https://example.com/", Instant::now());
        tabs_of(&mut state, window_id).activate_at(active_id, Instant::now());

        if let Some(tab) = tabs_of(&mut state, window_id).get_mut(background_id) {
            tab.on_navigation_blocked("https://doubleclick.net/");
        }

        assert_eq!(tabs_of(&mut state, window_id).active().blocked_count(), 0);
        assert_eq!(
            tabs_of(&mut state, window_id)
                .get(background_id)
                .unwrap()
                .blocked_count(),
            1
        );
        assert_eq!(tabs_of(&mut state, window_id).active_id(), active_id);
    }

    // --- Downloads (Issue #16, see docs/decisions.md D28) ---

    fn unique_temp_file(label: &str) -> PathBuf {
        let unique = format!(
            "{label}-{:?}-{}",
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn cancel_download_marks_the_entry_cancelled_and_removes_the_partial_file() {
        let mut state = state_with_history_enabled(true);
        let path = unique_temp_file("velox-app-cancel");
        std::fs::write(&path, b"partial").unwrap();

        let id = state.downloads.start(
            "https://example.com/f".to_owned(),
            "f".to_owned(),
            path.clone(),
            1,
        );
        cancel_download(&mut state, id);

        assert_eq!(
            state.downloads.get(id).unwrap().state,
            crate::browser::DownloadState::Cancelled
        );
        assert!(!path.exists());
    }

    #[test]
    fn cancel_download_on_an_unknown_id_does_not_panic() {
        let mut state = state_with_history_enabled(true);
        // Must not panic; there is no entry to cancel or file to remove.
        cancel_download(&mut state, DownloadId::from(9999));
    }

    #[test]
    fn cancel_download_on_an_already_completed_entry_is_a_noop() {
        let mut state = state_with_history_enabled(true);
        let path = unique_temp_file("velox-app-cancel-completed");
        std::fs::write(&path, b"done").unwrap();

        let id = state.downloads.start(
            "https://example.com/f".to_owned(),
            "f".to_owned(),
            path.clone(),
            1,
        );
        state.downloads.complete(id, 2);

        cancel_download(&mut state, id);

        // Still Completed, not Cancelled — a terminal state must not be
        // overwritten — and the (already "downloaded") file is left alone.
        assert_eq!(
            state.downloads.get(id).unwrap().state,
            crate::browser::DownloadState::Completed
        );
        assert!(path.exists());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn download_started_event_registers_an_entry_and_completed_event_resolves_it() {
        let mut window_state = state_with_history_enabled(true);
        window_state.downloads.start(
            "https://example.com/report.pdf".to_owned(),
            "report.pdf".to_owned(),
            PathBuf::from("/tmp/report.pdf"),
            100,
        );
        assert_eq!(window_state.downloads.entries().len(), 1);

        let id = window_state.downloads.entries()[0].id;
        let resolved = window_state.downloads.resolve_completion(
            "https://example.com/report.pdf",
            Some(Path::new("/tmp/report.pdf")),
        );
        assert_eq!(resolved, Some(id));
        assert!(window_state.downloads.complete(id, 200));
        assert_eq!(
            window_state.downloads.get(id).unwrap().state,
            crate::browser::DownloadState::Completed
        );
    }
}
