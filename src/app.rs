//! Application wiring: owns the event loop and connects the UI layer to the
//! browser logic.
//!
//! Everything UI-related happens on the main thread. The webview callbacks
//! (IPC, navigation, page load) forward their payloads into the event loop as
//! [`UserEvent`]s, so all state lives in one place and no locking is needed.

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};

use crate::browser::{
    metrics, navigation, persistence, ActivationEffect, BookmarkEntry, BookmarkStore, Favicon,
    FilterList, HistoryEntry, HistoryStore, TabId, Tabs,
};
use crate::config::Config;
use crate::ui::toolbar::{self, Panel, ToolbarCommand};
use crate::ui::{BrowserWindow, ContentShortcut};

/// Events forwarded from webview callbacks into the main event loop.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Raw IPC message from the toolbar webview (JSON, see
    /// [`toolbar::parse_command`]).
    ToolbarMessage(String),
    /// Tab `.0`'s content webview is about to navigate to this URL.
    NavigationStarted(TabId, String),
    /// Content blocking refused a main-frame navigation in tab `.0` to this
    /// URL.
    NavigationBlocked(TabId, String),
    /// Tab `.0`'s content webview started loading this URL.
    LoadStarted(TabId, String),
    /// Tab `.0`'s content webview finished loading this URL.
    LoadFinished(TabId, String),
    /// `document.title` for tab `tab_id` came back from its content webview
    /// (see `BrowserWindow::fetch_page_title`), for the history entry
    /// `history_id`. Carries `tab_id` — not just `history_id` — precisely so
    /// this event can be routed back to the right `Tab` as well as the
    /// right history entry; a `tab_id` for a tab that has since closed is
    /// simply ignored (see `Tabs::get_mut`), not a panic.
    PageTitleResolved {
        tab_id: TabId,
        history_id: u64,
        title: String,
    },
    /// Tab `tab_id`'s favicon URL came back from its content webview (see
    /// `BrowserWindow::fetch_favicon`). See docs/decisions.md D22: this is
    /// only ever a URL to try, never image bytes — the toolbar webview's own
    /// `<img>` tag performs the actual (async, non-blocking) fetch.
    FaviconResolved { tab_id: TabId, url: String },
    /// The active content webview's devtools shortcut (F12 / Cmd+Opt+I)
    /// fired. Sent over a dedicated, tightly-restricted IPC channel, separate
    /// from the toolbar's — see docs/decisions.md D18. Carries no `TabId`:
    /// `BrowserWindow::open_devtools` always resolves the currently active
    /// tab itself, matching how the shortcut is only ever wired into the
    /// webview the user is actually looking at.
    OpenDevtoolsRequested,
    /// One of the tab-management keyboard shortcuts fired while a content
    /// webview had focus (see `ui::window::ContentShortcut` and
    /// docs/decisions.md D18/D23). Sent over the same kind of dedicated,
    /// untrusted IPC channel as `OpenDevtoolsRequested`, for the same reason.
    ContentShortcut(ContentShortcut),
    /// A content webview asked to open a new window for `url` — a
    /// `target="_blank"` link or `window.open()` — which VeloX always
    /// answers by opening `url` as a new tab instead (see
    /// docs/decisions.md D25). Carries no `TabId`: like the shortcuts above,
    /// this is a browser-wide action ("open a new tab"), not something that
    /// needs to be routed back to whichever tab asked.
    NewTabRequested(String),
}

/// All mutable application state, gathered so the event handlers below take
/// one argument instead of a growing list of `&mut` parameters.
struct AppState {
    tabs: Tabs,
    history: HistoryStore,
    bookmarks: BookmarkStore,
    /// Where `history`/`bookmarks` are persisted; `None` when no data
    /// directory could be resolved (see `persistence::default_data_dir`),
    /// in which case both stores stay in-memory only for this run.
    data_dir: Option<PathBuf>,
    /// Single choke point for whether page visits are written to
    /// `history`. Mirrors `Config::private` for the life of the process
    /// (whole-app private browsing, see docs/decisions.md D14); a
    /// per-window/per-tab notion can set it dynamically once that concept
    /// exists — see `record_visit_if_enabled` below and docs/decisions.md
    /// D13.
    history_enabled: bool,
}

/// Build the window and run the event loop. Only returns on setup failure;
/// once running, the process exits with the event loop.
///
/// `process_start` is the earliest timestamp the caller could capture
/// (ideally the top of `main`); it only feeds the startup-timing report and
/// is otherwise unused when `config.perf_metrics` is off.
pub fn run(config: Config, process_start: Instant) -> Result<(), Box<dyn Error>> {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // `.then(...)` short-circuits: when metrics are off, no `Instant` is
    // captured here and `startup` stays `None`, so every checkpoint below
    // becomes a single cheap `Option` check with no clock read.
    let mut startup = config
        .perf_metrics
        .then(|| metrics::StartupTimestamps::new(process_start));

    let blocklist = Arc::new(build_blocklist(&config));

    let tabs = Tabs::new(config.homepage.clone());
    let mut window = BrowserWindow::new(&event_loop, &config, proxy, tabs.active_id(), blocklist)?;
    if let Some(startup) = startup.as_mut() {
        startup.mark_window_created(Instant::now());
    }

    if config.perf_metrics {
        if let Some(interval) = config.perf_rss_interval {
            spawn_rss_sampler(interval);
        }
    }

    let homepage = config.homepage.clone();
    let auto_suspend_after = config.auto_suspend_after;
    let perf_metrics_enabled = config.perf_metrics;
    // One timer per tab: background tabs load concurrently with the active
    // one, so a single shared timer would have their loads overwrite each
    // other's start times.
    let mut page_load_timers: HashMap<TabId, metrics::PageLoadTimer> = HashMap::new();

    let data_dir = persistence::default_data_dir();
    if data_dir.is_none() {
        eprintln!(
            "velox: could not resolve a data directory (no VELOX_DATA_DIR/HOME/APPDATA); \
             history and bookmarks will not be saved this session"
        );
    }
    let history = data_dir
        .as_deref()
        .map(persistence::load_history)
        .unwrap_or_default();
    let bookmarks = data_dir
        .as_deref()
        .map(persistence::load_bookmarks)
        .unwrap_or_default();

    let mut state = AppState {
        tabs,
        history,
        bookmarks,
        data_dir,
        history_enabled: !config.private,
    };

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => log_failure("resize layout", window.sync_layout()),
            Event::UserEvent(user_event) => {
                if std::env::var_os("VELOX_DEBUG").is_some() {
                    eprintln!("velox[debug]: {user_event:?}");
                }
                if perf_metrics_enabled {
                    record_perf_event(&mut startup, &mut page_load_timers, &user_event);
                }
                handle_user_event(&mut window, &mut state, &config, &homepage, user_event);
            }
            _ => {}
        }

        // Automatic tab suspension: on every pass through the loop (an
        // actual event, or the timer below waking us up), suspend whatever
        // background tabs have gone idle long enough, then schedule the
        // next wake-up for whichever background tab will go idle soonest.
        // `Tabs::idle_background_tabs`/`next_idle_deadline` are pure and
        // clock-injected (see `browser::tabs`), so all the policy logic
        // this loop needs is already unit-tested without a window.
        if *control_flow != ControlFlow::Exit {
            if let Some(next_wake) = sweep_idle_tabs(
                &mut window,
                &mut state.tabs,
                auto_suspend_after,
                Instant::now(),
            ) {
                *control_flow = ControlFlow::WaitUntil(next_wake);
            }
        }
    });
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

/// Update startup/page-load metrics state for one [`UserEvent`], logging to
/// stderr whenever a measurement completes. Only called when
/// `config.perf_metrics` is on, so every branch here is allowed a clock read
/// (the off-path never reaches this function at all).
fn record_perf_event(
    startup: &mut Option<metrics::StartupTimestamps>,
    page_load_timers: &mut HashMap<TabId, metrics::PageLoadTimer>,
    event: &UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(body) => {
            if matches!(toolbar::parse_command(body), Ok(ToolbarCommand::Ready)) {
                mark_startup(startup, metrics::StartupTimestamps::mark_toolbar_ready);
            }
        }
        UserEvent::NavigationStarted(id, _) => {
            page_load_timers
                .entry(*id)
                .or_default()
                .start(Instant::now());
        }
        UserEvent::LoadFinished(id, url) => {
            if let Some(duration) = page_load_timers
                .get_mut(id)
                .and_then(|timer| timer.finish(Instant::now()))
            {
                eprintln!("velox[perf] {}", metrics::format_page_load(url, duration));
            }
            // The first page to finish anywhere is time-to-first-page; a
            // background tab cannot beat the initial one to it, since it
            // can only be opened after the window is up.
            mark_startup(
                startup,
                metrics::StartupTimestamps::mark_first_load_finished,
            );
        }
        UserEvent::LoadStarted(..)
        | UserEvent::NavigationBlocked(..)
        | UserEvent::PageTitleResolved { .. }
        | UserEvent::FaviconResolved { .. }
        | UserEvent::OpenDevtoolsRequested
        | UserEvent::ContentShortcut(_)
        | UserEvent::NewTabRequested(_) => {}
    }
}

/// Apply one startup-checkpoint mark and, once the full report is
/// available, print it and clear `startup` so it is only reported once.
fn mark_startup(
    startup: &mut Option<metrics::StartupTimestamps>,
    mark: fn(&mut metrics::StartupTimestamps, Instant),
) {
    let Some(timestamps) = startup.as_mut() else {
        return;
    };
    mark(timestamps, Instant::now());
    if let Some(report) = timestamps.report() {
        eprintln!("velox[perf] {report}");
        *startup = None;
    }
}

/// Spawn a background thread that periodically samples this process's
/// (and its descendants') RSS and logs it to stderr. Runs for the lifetime
/// of the process; only ever spawned when `config.perf_metrics` and
/// `config.perf_rss_interval` are both set, so it costs nothing otherwise.
fn spawn_rss_sampler(interval: Duration) {
    let pid = std::process::id();
    std::thread::spawn(move || loop {
        match metrics::sample_process_tree_rss(pid) {
            Ok(sample) => eprintln!("velox[perf] {sample}"),
            Err(err) => eprintln!("velox: rss sampling failed: {err}"),
        }
        std::thread::sleep(interval);
    });
}

/// Suspend every background tab that has been idle for at least
/// `auto_suspend_after` as of `now`, then return when the loop should next
/// check again (the soonest a still-awake background tab would become
/// eligible). Returns `None` when automatic suspension is disabled
/// (`auto_suspend_after` is `None`) or there is no background tab to watch,
/// in which case the caller should leave `control_flow` as `Wait`.
fn sweep_idle_tabs(
    window: &mut BrowserWindow,
    tabs: &mut Tabs,
    auto_suspend_after: Option<std::time::Duration>,
    now: Instant,
) -> Option<Instant> {
    let idle_after = auto_suspend_after?;
    let candidates = tabs.idle_background_tabs(now, idle_after);
    if !candidates.is_empty() {
        for id in candidates {
            if tabs.suspend(id) {
                log_failure("auto-suspend tab", window.suspend_tab(id));
            }
        }
        sync_tab_strip(window, tabs);
    }
    tabs.next_idle_deadline(idle_after)
}

/// Dispatch one [`UserEvent`]. UI failures are logged, never fatal.
fn handle_user_event(
    window: &mut BrowserWindow,
    state: &mut AppState,
    config: &Config,
    homepage: &str,
    event: UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(body) => match toolbar::parse_command(&body) {
            Ok(command) => handle_toolbar_command(window, state, config, homepage, command),
            Err(err) => eprintln!("velox: ignoring malformed toolbar message {body:?}: {err}"),
        },
        UserEvent::NavigationStarted(id, url) | UserEvent::LoadStarted(id, url) => {
            if let Some(tab) = state.tabs.get_mut(id) {
                tab.on_navigation_started(&url);
            }
            if id == state.tabs.active_id() {
                log_failure("update address bar", window.set_url_display(&url));
                log_failure("show loading state", window.set_loading(true));
                sync_bookmark_star(window, state, &url);
            }
            sync_tab_strip(window, &state.tabs);
        }
        UserEvent::NavigationBlocked(id, url) => {
            eprintln!("velox: blocked navigation to {url} in tab {id:?}");
            if let Some(tab) = state.tabs.get_mut(id) {
                tab.on_navigation_blocked(&url);
            }
            // Only the active tab's badge is visible right now; a blocked
            // navigation in a background tab still updates its own
            // `Tab::blocked_count` above and is picked up the moment that
            // tab becomes active (see `activate_and_refresh`).
            if id == state.tabs.active_id() {
                sync_block_count(window, &state.tabs);
            }
        }
        UserEvent::LoadFinished(id, url) => {
            // A failed load reports an empty URL; keep showing the URL the
            // tab tried to reach instead of blanking it out.
            if let Some(tab) = state.tabs.get_mut(id) {
                if url.is_empty() {
                    tab.on_load_failed();
                } else {
                    tab.on_load_finished(&url);
                }
            }
            if !url.is_empty() {
                // Recorded for whichever tab just finished loading, not only
                // the active one: a background tab finishing a load is a
                // real visit too (see docs/decisions.md D13 and the "Visit
                // history and bookmarks" section of docs/architecture.md).
                let history_id = record_visit_if_enabled(state, &url, config.history_max_entries);
                if history_id.is_some() {
                    persist_history(state);
                    refresh_history_panel(window, state, config);
                }
                // Title/favicon are tab-strip state, independent of whether
                // this visit was recorded to history — private mode (no
                // history recording) still wants a readable tab strip (see
                // docs/decisions.md D22). `0` is a safe sentinel
                // `history_id` when there is none: `HistoryStore` ids start
                // at 1, so `HistoryStore::update_title` simply finds nothing
                // to update rather than touching an unrelated entry.
                log_failure(
                    "fetch page title",
                    window.fetch_page_title(id, history_id.unwrap_or(0)),
                );
                log_failure("fetch favicon", window.fetch_favicon(id));
            }
            if id == state.tabs.active_id() {
                if !url.is_empty() {
                    log_failure("update address bar", window.set_url_display(&url));
                    sync_bookmark_star(window, state, &url);
                }
                log_failure("hide loading state", window.set_loading(false));
            }
            sync_tab_strip(window, &state.tabs);
        }
        UserEvent::PageTitleResolved {
            tab_id,
            history_id,
            title,
        } => {
            // A stale `tab_id` (the tab closed while the title fetch was in
            // flight) is a safe no-op here — only the history entry still
            // gets its title.
            if let Some(tab) = state.tabs.get_mut(tab_id) {
                tab.set_title(title.clone());
            }
            if state.history.update_title(history_id, title) {
                persist_history(state);
                refresh_history_panel(window, state, config);
            }
        }
        UserEvent::FaviconResolved { tab_id, url } => {
            // A stale `tab_id` (the tab closed while the fetch was in
            // flight) is a safe no-op — mirrors `PageTitleResolved` above.
            if let Some(tab) = state.tabs.get_mut(tab_id) {
                tab.set_favicon_url(url);
                sync_tab_strip(window, &state.tabs);
            }
        }
        UserEvent::OpenDevtoolsRequested => window.open_devtools(),
        UserEvent::ContentShortcut(shortcut) => {
            handle_content_shortcut(window, state, homepage, shortcut)
        }
        UserEvent::NewTabRequested(url) => open_new_tab(window, state, &url),
    }
}

fn handle_toolbar_command(
    window: &mut BrowserWindow,
    state: &mut AppState,
    config: &Config,
    homepage: &str,
    command: ToolbarCommand,
) {
    match command {
        ToolbarCommand::Navigate { input } => match navigation::normalize_input(&input) {
            Some(url) => {
                state.tabs.active_mut().on_navigation_started(&url);
                log_failure("navigate", window.navigate(&url));
                // A panel entry click drives this same command; close
                // whichever panel was open now that the user has acted on it.
                log_failure("close panel", window.set_panel(None));
            }
            None => {
                eprintln!("velox: cannot navigate to {input:?}");
                // Snap the address bar back to the page we are actually on.
                log_failure(
                    "restore address bar",
                    window.set_url_display(state.tabs.active().current_url()),
                );
            }
        },
        ToolbarCommand::Back => log_failure("go back", window.go_back()),
        ToolbarCommand::Forward => log_failure("go forward", window.go_forward()),
        ToolbarCommand::Reload => log_failure("reload", window.reload()),
        ToolbarCommand::OpenDevtools => window.open_devtools(),
        ToolbarCommand::NewTab => open_new_tab(window, state, homepage),
        ToolbarCommand::CloseTab { id } => close_tab(window, state, TabId::from(id)),
        ToolbarCommand::ActivateTab { id } => {
            let id = TabId::from(id);
            if let Some(effect) = state.tabs.activate_at(id, Instant::now()) {
                activate_and_refresh(window, state, id, effect);
            }
        }
        ToolbarCommand::CloseActiveTab => {
            let id = state.tabs.active_id();
            close_tab(window, state, id);
        }
        ToolbarCommand::ReopenClosedTab => reopen_closed_tab(window, state),
        ToolbarCommand::NextTab => {
            let effect = state.tabs.activate_relative(1, Instant::now());
            apply_activation(window, state, effect);
        }
        ToolbarCommand::PrevTab => {
            let effect = state.tabs.activate_relative(-1, Instant::now());
            apply_activation(window, state, effect);
        }
        ToolbarCommand::ActivateTabByIndex { index } => {
            let effect = state
                .tabs
                .activate_by_position(index as usize, Instant::now());
            apply_activation(window, state, effect);
        }
        ToolbarCommand::ActivateLastTab => {
            let effect = state.tabs.activate_last(Instant::now());
            apply_activation(window, state, effect);
        }
        ToolbarCommand::SuspendTab { id } => {
            let id = TabId::from(id);
            if state.tabs.suspend(id) {
                log_failure("suspend tab", window.suspend_tab(id));
                sync_tab_strip(window, &state.tabs);
            }
            // Otherwise: unknown id, the active tab (never suspended), or
            // already suspended — a no-op, mirroring `CloseTab`'s guards.
        }
        ToolbarCommand::Ready => {
            log_failure(
                "initialize address bar",
                window.set_url_display(state.tabs.active().current_url()),
            );
            log_failure(
                "initialize loading state",
                window.set_loading(state.tabs.active().is_loading()),
            );
            log_failure("show private indicator", window.set_private(config.private));
            sync_block_count(window, &state.tabs);
            let url = state.tabs.active().current_url().to_owned();
            sync_bookmark_star(window, state, &url);
            refresh_history_panel(window, state, config);
            refresh_bookmarks_panel(window, state);
            sync_tab_strip(window, &state.tabs);
        }
        ToolbarCommand::ToggleBookmark => {
            let url = state.tabs.active().current_url().to_owned();
            let title = known_title_for(&state.history, &url);
            let now = now_unix();
            let active = state.bookmarks.toggle(&url, title, now);
            persist_bookmarks(state);
            log_failure("update bookmark star", window.set_bookmark_active(active));
            refresh_bookmarks_panel(window, state);
        }
        ToolbarCommand::TogglePanel { panel } => {
            let next = if window.open_panel() == Some(panel) {
                None
            } else {
                Some(panel)
            };
            log_failure("toggle panel", window.set_panel(next));
            match next {
                Some(Panel::History) => refresh_history_panel(window, state, config),
                Some(Panel::Bookmarks) => refresh_bookmarks_panel(window, state),
                None => {}
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
            refresh_history_panel(window, state, config);
        }
        ToolbarCommand::RemoveBookmark { id } => {
            if state.bookmarks.remove(id) {
                persist_bookmarks(state);
                refresh_bookmarks_panel(window, state);
                let url = state.tabs.active().current_url().to_owned();
                sync_bookmark_star(window, state, &url);
            }
        }
    }
}

/// Open a new tab at `url` and make it active. The one path every "open a
/// new tab" trigger funnels through — `ToolbarCommand::NewTab` (homepage),
/// `ContentShortcut::NewTab` (homepage), and `UserEvent::NewTabRequested`
/// (a `target="_blank"`/`window.open()` URL, see docs/decisions.md D25) —
/// so the webview-build-then-activate sequence is written once.
fn open_new_tab(window: &mut BrowserWindow, state: &mut AppState, url: &str) {
    let id = state.tabs.open_at(url.to_owned(), Instant::now());
    log_failure("open tab", window.open_tab(id, url));
    // A brand new tab's webview was just built above; only its visibility
    // needs to change, never a resume.
    activate_and_refresh(window, state, id, ActivationEffect::Switch);
}

/// Close tab `id` — the shared implementation behind the toolbar's own
/// close button (`ToolbarCommand::CloseTab`), Ctrl/Cmd+W from either the
/// toolbar or the content webview (`CloseActiveTab`/
/// `ContentShortcut::CloseTab`, both of which resolve `id` to the active
/// tab before calling this). A no-op — matching `Tabs::close` — for an
/// unknown id or the last remaining tab.
fn close_tab(window: &mut BrowserWindow, state: &mut AppState, id: TabId) {
    if let Some((new_active, effect)) = state.tabs.close(id) {
        window.close_tab(id);
        // The tab that replaces the one just closed may itself have been
        // suspended (a background tab can be suspended while the tab in
        // front of it is closed); `effect` already reflects that
        // (`Tabs::close`), so `activate_and_refresh` resumes it if needed
        // without re-deriving it here.
        activate_and_refresh(window, state, new_active, effect);
    }
    // Otherwise: unknown id, or `id` was the only remaining tab — VeloX
    // always keeps at least one tab open.
}

/// Reopen the most recently closed tab (Ctrl/Cmd+Shift+T, from either the
/// toolbar or the content webview). A no-op if nothing has been closed yet
/// (see `browser::tabs::Tabs::reopen_closed`).
fn reopen_closed_tab(window: &mut BrowserWindow, state: &mut AppState) {
    let Some(id) = state.tabs.reopen_closed(Instant::now()) else {
        return;
    };
    let url = state
        .tabs
        .get(id)
        .map(|tab| tab.current_url().to_owned())
        .unwrap_or_default();
    log_failure("reopen tab", window.open_tab(id, &url));
    activate_and_refresh(window, state, id, ActivationEffect::Switch);
}

/// Apply an activation `effect` already resolved by one of `Tabs`'
/// relative/positional activation methods (`activate_relative`,
/// `activate_by_position`, `activate_last`) against the tab that is now
/// active. `None` (nothing to apply — e.g. `ActivateTabByIndex` for a
/// position with no tab) is a silent no-op.
fn apply_activation(
    window: &mut BrowserWindow,
    state: &mut AppState,
    effect: Option<ActivationEffect>,
) {
    if let Some(effect) = effect {
        let id = state.tabs.active_id();
        activate_and_refresh(window, state, id, effect);
    }
}

/// Dispatch one content-webview keyboard shortcut (see
/// `ui::window::ContentShortcut` and docs/decisions.md D18/D23) to the same
/// tab operations the toolbar's own equivalent commands use — every branch
/// here mirrors one `ToolbarCommand` arm in `handle_toolbar_command`.
fn handle_content_shortcut(
    window: &mut BrowserWindow,
    state: &mut AppState,
    homepage: &str,
    shortcut: ContentShortcut,
) {
    match shortcut {
        ContentShortcut::NewTab => open_new_tab(window, state, homepage),
        ContentShortcut::CloseTab => {
            let id = state.tabs.active_id();
            close_tab(window, state, id);
        }
        ContentShortcut::ReopenClosedTab => reopen_closed_tab(window, state),
        ContentShortcut::NextTab => {
            let effect = state.tabs.activate_relative(1, Instant::now());
            apply_activation(window, state, effect);
        }
        ContentShortcut::PrevTab => {
            let effect = state.tabs.activate_relative(-1, Instant::now());
            apply_activation(window, state, effect);
        }
        ContentShortcut::ActivateTabAt(position) => {
            let effect = state
                .tabs
                .activate_by_position(position as usize, Instant::now());
            apply_activation(window, state, effect);
        }
        ContentShortcut::ActivateLastTab => {
            let effect = state.tabs.activate_last(Instant::now());
            apply_activation(window, state, effect);
        }
    }
}

/// Show `id` in the window, then bring the toolbar (address bar, loading
/// indicator, bookmark star, block-count badge, tab strip) up to date with
/// the now-active tab. The caller must have already made `id` the active
/// tab in `state.tabs` (`activate`/`activate_at`, `open`/`open_at`, or the
/// replacement tab returned by `close`) and pass along the
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
    state: &mut AppState,
    id: TabId,
    effect: ActivationEffect,
) {
    let result = match effect {
        ActivationEffect::Resume => window.resume_tab(id, state.tabs.active().current_url()),
        ActivationEffect::Switch => window.activate_tab(id),
    };
    log_failure(
        match effect {
            ActivationEffect::Resume => "resume tab",
            ActivationEffect::Switch => "activate tab",
        },
        result,
    );
    if let Some(tab) = state.tabs.get(id) {
        let url = tab.current_url().to_owned();
        log_failure("update address bar", window.set_url_display(&url));
        log_failure("update loading state", window.set_loading(tab.is_loading()));
        sync_bookmark_star(window, state, &url);
    }
    sync_block_count(window, &state.tabs);
    sync_tab_strip(window, &state.tabs);
}

/// Push the full tab list to the toolbar's tab strip.
fn sync_tab_strip(window: &BrowserWindow, tabs: &Tabs) {
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
        })
        .collect();
    log_failure("update tab strip", window.set_tabs(&summaries));
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

/// Record a page visit if history recording is currently enabled.
///
/// This is the single call site page loads flow through on their way into
/// `state.history` — see the doc comment on [`AppState::history_enabled`].
/// Adding private browsing later only needs to make `history_enabled`
/// reflect the active tab/window's private state before this runs; nothing
/// else in the recording path needs to change.
fn record_visit_if_enabled(state: &mut AppState, url: &str, max_entries: usize) -> Option<u64> {
    if !state.history_enabled {
        return None;
    }
    Some(
        state
            .history
            .record_visit(url, None, now_unix(), max_entries),
    )
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

/// Push the most recent `config.history_panel_limit` history entries to the
/// toolbar, newest first.
fn refresh_history_panel(window: &BrowserWindow, state: &AppState, config: &Config) {
    let entries: Vec<&HistoryEntry> = state
        .history
        .entries_newest_first()
        .take(config.history_panel_limit)
        .collect();
    log_failure("update history panel", window.set_history(&entries));
}

/// Push every bookmark to the toolbar, newest first.
fn refresh_bookmarks_panel(window: &BrowserWindow, state: &AppState) {
    let entries: Vec<&BookmarkEntry> = state.bookmarks.entries_newest_first().collect();
    log_failure("update bookmarks panel", window.set_bookmarks(&entries));
}

fn persist_history(state: &AppState) {
    if let Some(dir) = &state.data_dir {
        log_io_failure(
            "save history",
            persistence::save_history(dir, &state.history),
        );
    }
}

fn persist_bookmarks(state: &AppState) {
    if let Some(dir) = &state.data_dir {
        log_io_failure(
            "save bookmarks",
            persistence::save_bookmarks(dir, &state.bookmarks),
        );
    }
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

/// A failed UI call (e.g. a script that could not be evaluated) should not
/// crash the browser; surface it on stderr instead.
fn log_failure(action: &str, result: wry::Result<()>) {
    if let Err(err) = result {
        eprintln!("velox: failed to {action}: {err}");
    }
}

/// Same as `log_failure`, for the IO errors persistence returns.
fn log_io_failure(action: &str, result: std::io::Result<()>) {
    if let Err(err) = result {
        eprintln!("velox: failed to {action}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `AppState` the way `run()` would for a fresh tab, with a
    /// given `history_enabled` (what `Config::private` drives at startup —
    /// see docs/decisions.md D13/D14).
    fn state_with_history_enabled(history_enabled: bool) -> AppState {
        AppState {
            tabs: Tabs::new("https://example.com/"),
            history: HistoryStore::new(),
            bookmarks: BookmarkStore::new(),
            data_dir: None,
            history_enabled,
        }
    }

    #[test]
    fn records_a_visit_when_history_is_enabled() {
        let mut state = state_with_history_enabled(true);
        let id = record_visit_if_enabled(&mut state, "https://example.com/", 0);
        assert!(id.is_some());
        assert_eq!(state.history.entries().len(), 1);
    }

    #[test]
    fn private_mode_records_no_visit() {
        // This is the private-browsing invariant from docs/decisions.md
        // D13/D14: with history recording disabled (as it is for the whole
        // app when `Config::private` is set), a page load must never reach
        // `HistoryStore::record_visit`.
        let mut state = state_with_history_enabled(false);
        let id = record_visit_if_enabled(&mut state, "https://example.com/", 0);
        assert!(id.is_none());
        assert!(state.history.entries().is_empty());
    }

    #[test]
    fn private_mode_history_store_stays_empty_across_multiple_loads() {
        let mut state = state_with_history_enabled(false);
        for url in [
            "https://a.example/",
            "https://b.example/",
            "https://a.example/",
        ] {
            assert!(record_visit_if_enabled(&mut state, url, 0).is_none());
        }
        assert!(state.history.entries().is_empty());
    }

    #[test]
    fn persist_history_is_a_noop_without_a_data_dir() {
        // Exercises the same "no write happens" path a private session with
        // no data_dir override would take; a data_dir is only ever set from
        // `persistence::default_data_dir()` in `run()`, unaffected by
        // `history_enabled` itself, so the write-suppression for private
        // mode has to come entirely from never producing history entries to
        // persist in the first place (checked above) rather than from
        // `persist_history` deciding not to write.
        let state = state_with_history_enabled(false);
        assert!(state.data_dir.is_none());
        // Should not panic and should not touch the filesystem.
        persist_history(&state);
    }

    #[test]
    fn new_tab_has_no_blocked_navigations_in_app_state() {
        let state = state_with_history_enabled(true);
        assert_eq!(state.tabs.active().blocked_count(), 0);
    }

    #[test]
    fn blocked_navigation_increments_only_the_target_tabs_counter() {
        let mut state = state_with_history_enabled(true);
        let active_id = state.tabs.active_id();
        // Opening a tab makes it active; activate the original tab again so
        // the new one is a real background tab for this test.
        let background_id = state.tabs.open_at("https://example.com/", Instant::now());
        state.tabs.activate_at(active_id, Instant::now());

        if let Some(tab) = state.tabs.get_mut(background_id) {
            tab.on_navigation_blocked("https://doubleclick.net/");
        }

        assert_eq!(state.tabs.active().blocked_count(), 0);
        assert_eq!(state.tabs.get(background_id).unwrap().blocked_count(), 1);
        assert_eq!(state.tabs.active_id(), active_id);
    }
}
