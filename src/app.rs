//! Application wiring: owns the event loop and connects the UI layer to the
//! browser logic.
//!
//! Everything UI-related happens on the main thread. The webview callbacks
//! (IPC, navigation, page load) forward their payloads into the event loop as
//! [`UserEvent`]s, so all state lives in one place and no locking is needed.

use std::error::Error;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};

use crate::browser::{
    navigation, persistence, BookmarkEntry, BookmarkStore, HistoryEntry, HistoryStore, Tab,
};
use crate::config::Config;
use crate::ui::toolbar::{self, Panel, ToolbarCommand};
use crate::ui::BrowserWindow;

/// Events forwarded from webview callbacks into the main event loop.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Raw IPC message from the toolbar webview (JSON, see
    /// [`toolbar::parse_command`]).
    ToolbarMessage(String),
    /// The content webview is about to navigate to this URL.
    NavigationStarted(String),
    /// The content webview started loading this URL.
    LoadStarted(String),
    /// The content webview finished loading this URL.
    LoadFinished(String),
    /// `document.title` for the history entry `id` came back from the
    /// content webview (see `BrowserWindow::fetch_page_title`).
    PageTitleResolved { id: u64, title: String },
}

/// All mutable application state, gathered so the event handlers below take
/// one argument instead of a growing list of `&mut` parameters.
struct AppState {
    tab: Tab,
    history: HistoryStore,
    bookmarks: BookmarkStore,
    /// Where `history`/`bookmarks` are persisted; `None` when no data
    /// directory could be resolved (see `persistence::default_data_dir`),
    /// in which case both stores stay in-memory only for this run.
    data_dir: Option<PathBuf>,
    /// Single choke point for whether page visits are written to
    /// `history`. Always `true` today; private browsing (#7) is the
    /// intended reason to ever set this to `false` (per-window/per-tab, once
    /// that concept exists) — see `record_visit_if_enabled` below and
    /// docs/decisions.md D11.
    history_enabled: bool,
}

/// Build the window and run the event loop. Only returns on setup failure;
/// once running, the process exits with the event loop.
pub fn run(config: Config) -> Result<(), Box<dyn Error>> {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = BrowserWindow::new(&event_loop, &config, proxy)?;

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
        tab: Tab::new(config.homepage.clone()),
        history,
        bookmarks,
        data_dir,
        history_enabled: true,
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
                handle_user_event(&window, &mut state, &config, user_event);
            }
            _ => {}
        }
    });
}

/// Dispatch one [`UserEvent`]. UI failures are logged, never fatal.
fn handle_user_event(
    window: &BrowserWindow,
    state: &mut AppState,
    config: &Config,
    event: UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(body) => match toolbar::parse_command(&body) {
            Ok(command) => handle_toolbar_command(window, state, config, command),
            Err(err) => eprintln!("velox: ignoring malformed toolbar message {body:?}: {err}"),
        },
        UserEvent::NavigationStarted(url) | UserEvent::LoadStarted(url) => {
            state.tab.on_navigation_started(&url);
            log_failure("update address bar", window.set_url_display(&url));
            log_failure("show loading state", window.set_loading(true));
            sync_bookmark_star(window, state, &url);
        }
        UserEvent::LoadFinished(url) => {
            // A failed load reports an empty URL; keep showing the URL the
            // user tried to reach instead of blanking the address bar.
            if url.is_empty() {
                state.tab.on_load_failed();
            } else {
                state.tab.on_load_finished(&url);
                log_failure("update address bar", window.set_url_display(&url));

                if let Some(id) = record_visit_if_enabled(state, &url, config.history_max_entries) {
                    persist_history(state);
                    refresh_history_panel(window, state, config);
                    log_failure("fetch page title", window.fetch_page_title(id));
                }
                sync_bookmark_star(window, state, &url);
            }
            log_failure("hide loading state", window.set_loading(false));
        }
        UserEvent::PageTitleResolved { id, title } => {
            if state.history.update_title(id, title) {
                persist_history(state);
                refresh_history_panel(window, state, config);
            }
        }
    }
}

fn handle_toolbar_command(
    window: &BrowserWindow,
    state: &mut AppState,
    config: &Config,
    command: ToolbarCommand,
) {
    match command {
        ToolbarCommand::Navigate { input } => match navigation::normalize_input(&input) {
            Some(url) => {
                state.tab.on_navigation_started(&url);
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
                    window.set_url_display(state.tab.current_url()),
                );
            }
        },
        ToolbarCommand::Back => log_failure("go back", window.go_back()),
        ToolbarCommand::Forward => log_failure("go forward", window.go_forward()),
        ToolbarCommand::Reload => log_failure("reload", window.reload()),
        ToolbarCommand::Ready => {
            log_failure(
                "initialize address bar",
                window.set_url_display(state.tab.current_url()),
            );
            log_failure(
                "initialize loading state",
                window.set_loading(state.tab.is_loading()),
            );
            let url = state.tab.current_url().to_owned();
            sync_bookmark_star(window, state, &url);
            refresh_history_panel(window, state, config);
            refresh_bookmarks_panel(window, state);
        }
        ToolbarCommand::ToggleBookmark => {
            let url = state.tab.current_url().to_owned();
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
                let url = state.tab.current_url().to_owned();
                sync_bookmark_star(window, state, &url);
            }
        }
    }
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
