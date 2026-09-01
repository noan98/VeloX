//! Application wiring: owns the event loop and connects the UI layer to the
//! browser logic.
//!
//! Everything UI-related happens on the main thread. The webview callbacks
//! (IPC, navigation, page load) forward their payloads into the event loop as
//! [`UserEvent`]s, so all state lives in one place and no locking is needed.

use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};

use crate::browser::downloads;
use crate::browser::navigation::Intent;
use crate::browser::perf_log::PerfLog;
use crate::browser::{
    metrics, navigation, omnibox, persistence, ActivationEffect, BookmarkEntry, BookmarkStore,
    DownloadEntry, DownloadId, DownloadStore, Favicon, FilterList, HistoryEntry, HistoryStore,
    TabId, Tabs,
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
    /// `BrowserWindow::fetch_favicon`), for the history entry `history_id`
    /// (`0` when there is none — see `PageTitleResolved`'s doc comment,
    /// same sentinel, same reasoning, added for the history favicon in
    /// #18/D27). See docs/decisions.md D22: this is only ever a URL to try,
    /// never image bytes — the toolbar webview's own `<img>` tag performs
    /// the actual (async, non-blocking) fetch.
    FaviconResolved {
        tab_id: TabId,
        history_id: u64,
        url: String,
    },
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
    /// A content webview's `download_started_handler` accepted a download
    /// (see docs/decisions.md D28): `destination` is already the final,
    /// sanitized, collision-avoided path
    /// (`browser::downloads::prepare_destination`), chosen synchronously
    /// inside the wry callback before this event is sent. This only ever
    /// *registers* the download for the UI/`DownloadStore` — the decision
    /// to accept it already happened in `ui::window`.
    DownloadStarted {
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
        url: String,
        path: Option<PathBuf>,
        success: bool,
    },
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
}

/// What tab-latency logging needs: where to write records, and the epoch
/// (`process_start`) their `ts_ms` timestamps are relative to. Kept
/// separate from `PerfLog` itself so a `None` here (metrics off) costs
/// nothing beyond the `Option`.
struct PerfContext {
    process_start: Instant,
    log: Arc<PerfLog>,
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

    // Built once, shared with the RSS sampler thread and every perf-logging
    // call site in this file via `Arc::clone`; `None` when metrics are off,
    // matching `startup`'s `.then(...)` short-circuit above.
    let perf_log: Option<Arc<PerfLog>> = config.perf_metrics.then(|| build_perf_log(&config));

    if let (Some(interval), Some(log)) = (config.perf_rss_interval, perf_log.clone()) {
        spawn_rss_sampler(interval, log, process_start);
    }

    let homepage = config.homepage.clone();
    let auto_suspend_after = config.auto_suspend_after;
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
        perf: perf_log
            .clone()
            .map(|log| PerfContext { process_start, log }),
        downloads: DownloadStore::new(),
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
                if let Some(log) = perf_log.as_deref() {
                    record_perf_event(
                        &mut startup,
                        &mut page_load_timers,
                        log,
                        process_start,
                        &user_event,
                    );
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

/// Build the [`PerfLog`] performance events are written through: a file at
/// `config.perf_output_path` if one was requested and could be opened,
/// stderr otherwise. Only called when `config.perf_metrics` is on.
fn build_perf_log(config: &Config) -> Arc<PerfLog> {
    match &config.perf_output_path {
        Some(path) => match PerfLog::to_file(config.perf_format, Path::new(path)) {
            Ok(log) => Arc::new(log),
            Err(err) => {
                eprintln!(
                    "velox: failed to open perf output file {path:?}: {err}; \
                     falling back to stderr"
                );
                Arc::new(PerfLog::stderr(config.perf_format))
            }
        },
        None => Arc::new(PerfLog::stderr(config.perf_format)),
    }
}

/// Update startup/page-load metrics state for one [`UserEvent`], writing a
/// [`metrics::PerfRecord`] through `perf_log` whenever a measurement
/// completes. Only called when `config.perf_metrics` is on (see `run`'s
/// event loop), so every branch here is allowed a clock read — the off-path
/// never reaches this function at all.
fn record_perf_event(
    startup: &mut Option<metrics::StartupTimestamps>,
    page_load_timers: &mut HashMap<TabId, metrics::PageLoadTimer>,
    perf_log: &PerfLog,
    process_start: Instant,
    event: &UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(body) => {
            if matches!(toolbar::parse_command(body), Ok(ToolbarCommand::Ready)) {
                mark_startup(
                    startup,
                    perf_log,
                    process_start,
                    metrics::StartupTimestamps::mark_toolbar_ready,
                );
            }
        }
        UserEvent::NavigationStarted(id, _) => {
            page_load_timers
                .entry(*id)
                .or_default()
                .start(Instant::now());
        }
        UserEvent::LoadFinished(id, url) => {
            let now = Instant::now();
            if let Some(duration) = page_load_timers
                .get_mut(id)
                .and_then(|timer| timer.finish(now))
            {
                let elapsed = now.saturating_duration_since(process_start);
                perf_log.write(
                    &metrics::PerfRecord::page_load(url.as_str(), duration),
                    elapsed,
                );
            }
            // The first page to finish anywhere is time-to-first-page; a
            // background tab cannot beat the initial one to it, since it
            // can only be opened after the window is up.
            mark_startup(
                startup,
                perf_log,
                process_start,
                metrics::StartupTimestamps::mark_first_load_finished,
            );
        }
        UserEvent::LoadStarted(..)
        | UserEvent::NavigationBlocked(..)
        | UserEvent::PageTitleResolved { .. }
        | UserEvent::FaviconResolved { .. }
        | UserEvent::OpenDevtoolsRequested
        | UserEvent::ContentShortcut(_)
        | UserEvent::NewTabRequested(_)
        | UserEvent::DownloadStarted { .. }
        | UserEvent::DownloadCompleted { .. } => {}
    }
}

/// Apply one startup-checkpoint mark and, once the full report is
/// available, write it through `perf_log` and clear `startup` so it is only
/// reported once.
fn mark_startup(
    startup: &mut Option<metrics::StartupTimestamps>,
    perf_log: &PerfLog,
    process_start: Instant,
    mark: fn(&mut metrics::StartupTimestamps, Instant),
) {
    let Some(timestamps) = startup.as_mut() else {
        return;
    };
    let now = Instant::now();
    mark(timestamps, now);
    if let Some(report) = timestamps.report() {
        let elapsed = now.saturating_duration_since(process_start);
        perf_log.write(&metrics::PerfRecord::startup(report), elapsed);
        *startup = None;
    }
}

/// Log a tab-create/switch latency record if performance metrics are on
/// (`state.perf` is `Some`); a single `Option::is_none` check with no
/// `Instant::now()` call otherwise. `started` is the timestamp the caller
/// captured right before the operation it is timing.
fn record_tab_latency(
    state: &AppState,
    kind: metrics::TabLatencyKind,
    id: TabId,
    started: Instant,
) {
    let Some(perf) = &state.perf else {
        return;
    };
    let now = Instant::now();
    let duration = now.saturating_duration_since(started);
    let elapsed = now.saturating_duration_since(perf.process_start);
    perf.log.write(
        &metrics::PerfRecord::tab_latency(kind, id.get(), duration),
        elapsed,
    );
}

/// Spawn a background thread that periodically samples this process's
/// (and its descendants') RSS and writes it through `log`. Runs for the
/// lifetime of the process; only ever spawned when `config.perf_metrics`
/// and `config.perf_rss_interval` are both set, so it costs nothing
/// otherwise.
fn spawn_rss_sampler(interval: Duration, log: Arc<PerfLog>, process_start: Instant) {
    let pid = std::process::id();
    std::thread::spawn(move || loop {
        match metrics::sample_process_tree_rss(pid) {
            Ok(sample) => {
                let elapsed = Instant::now().saturating_duration_since(process_start);
                log.write(&metrics::PerfRecord::rss(sample), elapsed);
            }
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
                log_failure(
                    "fetch favicon",
                    window.fetch_favicon(id, history_id.unwrap_or(0)),
                );
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
        UserEvent::FaviconResolved {
            tab_id,
            history_id,
            url,
        } => {
            // A stale `tab_id` (the tab closed while the fetch was in
            // flight) is a safe no-op — mirrors `PageTitleResolved` above.
            if let Some(tab) = state.tabs.get_mut(tab_id) {
                tab.set_favicon_url(url.clone());
                sync_tab_strip(window, &state.tabs);
            }
            if state.history.update_favicon(history_id, url) {
                persist_history(state);
                refresh_history_panel(window, state, config);
            }
        }
        UserEvent::OpenDevtoolsRequested => window.open_devtools(),
        UserEvent::ContentShortcut(shortcut) => {
            handle_content_shortcut(window, state, homepage, shortcut)
        }
        UserEvent::NewTabRequested(url) => open_new_tab(window, state, &url),
        UserEvent::DownloadStarted {
            url,
            file_name,
            destination,
            started_at,
        } => {
            state
                .downloads
                .start(url, file_name, destination, started_at);
            refresh_downloads_panel(window, state);
        }
        UserEvent::DownloadCompleted { url, path, success } => {
            let now = now_unix();
            match state.downloads.resolve_completion(&url, path.as_deref()) {
                Some(id) if success => {
                    state.downloads.complete(id, now);
                }
                Some(id) => {
                    state
                        .downloads
                        .fail(id, "ダウンロードに失敗しました".to_owned(), now);
                }
                None => {
                    eprintln!(
                        "velox: could not correlate download completion for {url:?} \
                         (path={path:?}, success={success})"
                    );
                }
            }
            refresh_downloads_panel(window, state);
        }
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
        // Resolved via `navigation::classify_input` — the single place that
        // decides URL vs. search (see docs/decisions.md D26) — rather than
        // `navigation::normalize_input` directly, so a search query typed
        // and submitted with no candidate-dropdown interaction still goes
        // to the search engine instead of being rejected outright. `input`
        // here is also what a history/bookmark/candidate row click sends
        // (already a resolved URL, e.g. `Candidate::target_url`):
        // `classify_input` treats an already-absolute `https://…` URL as
        // `Intent::Url` unchanged, so this one path serves both cases.
        ToolbarCommand::Navigate { input } => match resolve_navigate_target(config, &input) {
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
            let started = Instant::now();
            if let Some(effect) = state.tabs.activate_at(id, started) {
                activate_and_refresh(window, state, id, effect);
                record_tab_latency(state, metrics::TabLatencyKind::Switch, id, started);
            }
        }
        ToolbarCommand::CloseActiveTab => {
            let id = state.tabs.active_id();
            close_tab(window, state, id);
        }
        ToolbarCommand::ReopenClosedTab => reopen_closed_tab(window, state),
        ToolbarCommand::NextTab => {
            let started = Instant::now();
            let effect = state.tabs.activate_relative(1, started);
            apply_activation(window, state, effect, started);
        }
        ToolbarCommand::PrevTab => {
            let started = Instant::now();
            let effect = state.tabs.activate_relative(-1, started);
            apply_activation(window, state, effect, started);
        }
        ToolbarCommand::ActivateTabByIndex { index } => {
            let started = Instant::now();
            let effect = state.tabs.activate_by_position(index as usize, started);
            apply_activation(window, state, effect, started);
        }
        ToolbarCommand::ActivateLastTab => {
            let started = Instant::now();
            let effect = state.tabs.activate_last(started);
            apply_activation(window, state, effect, started);
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
            refresh_downloads_panel(window, state);
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
                Some(Panel::Downloads) => refresh_downloads_panel(window, state),
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
            refresh_history_panel(window, state, config);
        }
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
                let url = state.tabs.active().current_url().to_owned();
                sync_bookmark_star(window, state, &url);
            }
        }
        ToolbarCommand::OpenDownload { id } => open_download(state, DownloadId::from(id)),
        ToolbarCommand::OpenDownloadsFolder => open_downloads_folder(),
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
        ToolbarCommand::FocusAddressBar => focus_address_bar(window, state),
        ToolbarCommand::OmniboxInput { input } => {
            // No `CandidateSource`s yet — #20 wires history/bookmark
            // sources in here (see `browser::omnibox::CandidateSource`'s
            // doc comment for exactly what it implements).
            let candidates = omnibox::build_candidates(
                &input,
                &config.search_engine.name,
                &config.search_engine.query_template,
                &[],
                omnibox::DEFAULT_CANDIDATE_LIMIT,
            );
            let open = if candidates.is_empty() {
                None
            } else {
                Some(Panel::Omnibox)
            };
            log_failure("toggle omnibox panel", window.set_panel(open));
            log_failure(
                "update omnibox candidates",
                window.set_candidates(&candidates),
            );
        }
        ToolbarCommand::OmniboxClose => {
            log_failure("close omnibox", window.set_panel(None));
            focus_address_bar(window, state);
        }
    }
}

/// Resolve what `ToolbarCommand::Navigate`'s raw `input` should actually
/// load: [`navigation::classify_input`] decides URL vs. search, and a
/// search intent is turned into the configured search engine's URL via
/// [`navigation::build_search_url`]. `None` covers every way this can fail
/// to resolve — empty input, a rejected scheme, or (in practice never, since
/// `Config`'s template always contains the required placeholder) a broken
/// search-engine template.
fn resolve_navigate_target(config: &Config, input: &str) -> Option<String> {
    match navigation::classify_input(input)? {
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
fn focus_address_bar(window: &mut BrowserWindow, state: &AppState) {
    log_failure(
        "focus address bar",
        window.focus_address_bar(state.tabs.active().current_url()),
    );
}

/// Open a new tab at `url` and make it active. The one path every "open a
/// new tab" trigger funnels through — `ToolbarCommand::NewTab` (homepage),
/// `ContentShortcut::NewTab` (homepage), and `UserEvent::NewTabRequested`
/// (a `target="_blank"`/`window.open()` URL, see docs/decisions.md D25) —
/// so the webview-build-then-activate sequence is written once.
fn open_new_tab(window: &mut BrowserWindow, state: &mut AppState, url: &str) {
    // Reuses the `Instant` `Tabs::open_at` needs anyway, so tab-create
    // latency costs no extra clock read when metrics are off (D19).
    let started = Instant::now();
    let id = state.tabs.open_at(url.to_owned(), started);
    log_failure("open tab", window.open_tab(id, url));
    // A brand new tab's webview was just built above; only its visibility
    // needs to change, never a resume.
    activate_and_refresh(window, state, id, ActivationEffect::Switch);
    record_tab_latency(state, metrics::TabLatencyKind::Create, id, started);
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
    let started = Instant::now();
    let Some(id) = state.tabs.reopen_closed(started) else {
        return;
    };
    let url = state
        .tabs
        .get(id)
        .map(|tab| tab.current_url().to_owned())
        .unwrap_or_default();
    log_failure("reopen tab", window.open_tab(id, &url));
    activate_and_refresh(window, state, id, ActivationEffect::Switch);
    // A reopened tab builds a fresh webview at the remembered URL, so it is
    // a tab creation as far as D19's latency metric is concerned.
    record_tab_latency(state, metrics::TabLatencyKind::Create, id, started);
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
    started: Instant,
) {
    if let Some(effect) = effect {
        let id = state.tabs.active_id();
        activate_and_refresh(window, state, id, effect);
        record_tab_latency(state, metrics::TabLatencyKind::Switch, id, started);
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
            let started = Instant::now();
            let effect = state.tabs.activate_relative(1, started);
            apply_activation(window, state, effect, started);
        }
        ContentShortcut::PrevTab => {
            let started = Instant::now();
            let effect = state.tabs.activate_relative(-1, started);
            apply_activation(window, state, effect, started);
        }
        ContentShortcut::ActivateTabAt(position) => {
            let started = Instant::now();
            let effect = state.tabs.activate_by_position(position as usize, started);
            apply_activation(window, state, effect, started);
        }
        ContentShortcut::ActivateLastTab => {
            let started = Instant::now();
            let effect = state.tabs.activate_last(started);
            apply_activation(window, state, effect, started);
        }
        ContentShortcut::FocusAddressBar => focus_address_bar(window, state),
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
/// toolbar, newest first, grouped into date sections (see
/// `browser::history::group_by_date` / docs/decisions.md D29) relative to
/// "now". Also the fallback the panel returns to when the search box is
/// cleared (see `ToolbarCommand::SearchHistory` below).
fn refresh_history_panel(window: &BrowserWindow, state: &AppState, config: &Config) {
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
fn search_history_panel(window: &BrowserWindow, state: &AppState, config: &Config, query: &str) {
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

/// Push every bookmark to the toolbar, newest first.
fn refresh_bookmarks_panel(window: &BrowserWindow, state: &AppState) {
    let entries: Vec<&BookmarkEntry> = state.bookmarks.entries_newest_first().collect();
    log_failure("update bookmarks panel", window.set_bookmarks(&entries));
}

/// Push the download list to the toolbar, most recently started first.
fn refresh_downloads_panel(window: &BrowserWindow, state: &AppState) {
    let entries: Vec<&DownloadEntry> = state.downloads.entries_newest_first().collect();
    log_failure("update downloads panel", window.set_downloads(&entries));
}

/// Open a completed download's file with the OS's default handler
/// (`ToolbarCommand::OpenDownload`). A no-op — logged, not an error — for
/// an unknown id or a download that has not reached
/// `DownloadState::Completed`; opening an in-progress/failed/cancelled
/// download's (possibly partial or nonexistent) file would be misleading.
fn open_download(state: &AppState, id: DownloadId) {
    match state.downloads.get(id) {
        Some(entry) if entry.state == crate::browser::DownloadState::Completed => {
            log_spawn_failure("open download", downloads::spawn_open(&entry.destination));
        }
        Some(_) => eprintln!("velox: open_download: {id:?} has not completed yet"),
        None => eprintln!("velox: open_download: unknown download {id:?}"),
    }
}

/// Open the downloads directory with the OS's default file manager
/// (`ToolbarCommand::OpenDownloadsFolder`). Creates the directory first
/// (best-effort) so opening it before anything has ever been downloaded
/// does not fail with a confusing "no such directory" error.
fn open_downloads_folder() {
    let Some(dir) = downloads::resolve_download_dir() else {
        eprintln!(
            "velox: open_downloads_folder: could not resolve a downloads directory \
             (no VELOX_DOWNLOAD_DIR/HOME/USERPROFILE)"
        );
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("velox: failed to create downloads directory {dir:?}: {err}");
    }
    log_spawn_failure("open downloads folder", downloads::spawn_open(&dir));
}

/// Best-effort cancel of an in-progress download (`ToolbarCommand::CancelDownload`):
/// marks it `DownloadState::Cancelled` and attempts to delete whatever
/// partial file exists at its destination. Does **not** stop the underlying
/// engine transfer — wry 0.56 exposes no API to do that; see
/// docs/decisions.md D28. A no-op for an unknown id or a download that has
/// already reached a terminal state.
fn cancel_download(state: &mut AppState, id: DownloadId) {
    let Some(destination) = state
        .downloads
        .get(id)
        .map(|entry| entry.destination.clone())
    else {
        return;
    };
    if state.downloads.cancel(id, now_unix()) {
        // Best-effort only: if the engine is still writing to this path, it
        // may recreate the file (or fail silently) after this runs — see
        // docs/decisions.md D28's "what's unverified"/limitation note.
        match std::fs::remove_file(&destination) {
            Ok(()) => {}
            // Already gone (never actually started writing, or the engine
            // had not created the file yet) — not an error worth logging.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!(
                "velox: failed to remove cancelled download's partial file {destination:?}: {err}"
            ),
        }
    }
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

/// Same as `log_failure`, for `downloads::spawn_open`'s launch-a-process
/// result. The spawned child is intentionally not waited on or otherwise
/// tracked — "open this file/folder in some other application" is a
/// fire-and-forget action, the same as a real desktop browser's own
/// "show in folder" / "open file" menu entries.
fn log_spawn_failure(action: &str, result: std::io::Result<std::process::Child>) {
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
            perf: None,
            downloads: DownloadStore::new(),
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
    fn record_tab_latency_is_a_noop_when_perf_metrics_are_off() {
        let state = state_with_history_enabled(true);
        assert!(state.perf.is_none());
        // Must not panic; there is nothing to assert on beyond that, since
        // "off" means no write happens at all.
        record_tab_latency(
            &state,
            metrics::TabLatencyKind::Create,
            state.tabs.active_id(),
            Instant::now(),
        );
    }

    #[test]
    fn record_tab_latency_writes_through_perf_log_when_metrics_are_on() {
        let mut state = state_with_history_enabled(true);
        state.perf = Some(PerfContext {
            process_start: Instant::now(),
            log: Arc::new(PerfLog::stderr(metrics::PerfFormat::Text)),
        });
        let id = state.tabs.active_id();
        let started = Instant::now();
        // Exercises the write path end-to-end (stderr sink); nothing to
        // assert on the output itself here, but this must not panic.
        record_tab_latency(&state, metrics::TabLatencyKind::Switch, id, started);
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
