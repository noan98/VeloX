//! Application wiring: owns the event loop and connects the UI layer to the
//! browser logic.
//!
//! Everything UI-related happens on the main thread. The webview callbacks
//! (IPC, navigation, page load) forward their payloads into the event loop as
//! [`UserEvent`]s, so all state lives in one place and no locking is needed.

use std::error::Error;
use std::time::Instant;

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};

use crate::browser::{navigation, TabId, Tabs};
use crate::config::Config;
use crate::ui::toolbar::{self, TabSummary, ToolbarCommand};
use crate::ui::BrowserWindow;

/// Events forwarded from webview callbacks into the main event loop.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Raw IPC message from the toolbar webview (JSON, see
    /// [`toolbar::parse_command`]).
    ToolbarMessage(String),
    /// Tab `.0`'s content webview is about to navigate to this URL.
    NavigationStarted(TabId, String),
    /// Tab `.0`'s content webview started loading this URL.
    LoadStarted(TabId, String),
    /// Tab `.0`'s content webview finished loading this URL.
    LoadFinished(TabId, String),
}

/// Build the window and run the event loop. Only returns on setup failure;
/// once running, the process exits with the event loop.
pub fn run(config: Config) -> Result<(), Box<dyn Error>> {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let mut tabs = Tabs::new(config.homepage.clone());
    let mut window = BrowserWindow::new(&event_loop, &config, proxy, tabs.active_id())?;
    let homepage = config.homepage.clone();
    let auto_suspend_after = config.auto_suspend_after;

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
                handle_user_event(&mut window, &mut tabs, &homepage, user_event);
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
            if let Some(next_wake) =
                sweep_idle_tabs(&mut window, &mut tabs, auto_suspend_after, Instant::now())
            {
                *control_flow = ControlFlow::WaitUntil(next_wake);
            }
        }
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
    tabs: &mut Tabs,
    homepage: &str,
    event: UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(body) => match toolbar::parse_command(&body) {
            Ok(command) => handle_toolbar_command(window, tabs, homepage, command),
            Err(err) => eprintln!("velox: ignoring malformed toolbar message {body:?}: {err}"),
        },
        UserEvent::NavigationStarted(id, url) | UserEvent::LoadStarted(id, url) => {
            if let Some(tab) = tabs.get_mut(id) {
                tab.on_navigation_started(&url);
            }
            if id == tabs.active_id() {
                log_failure("update address bar", window.set_url_display(&url));
                log_failure("show loading state", window.set_loading(true));
            }
            sync_tab_strip(window, tabs);
        }
        UserEvent::LoadFinished(id, url) => {
            // A failed load reports an empty URL; keep showing the URL the
            // tab tried to reach instead of blanking it out.
            if let Some(tab) = tabs.get_mut(id) {
                if url.is_empty() {
                    tab.on_load_failed();
                } else {
                    tab.on_load_finished(&url);
                }
            }
            if id == tabs.active_id() {
                if !url.is_empty() {
                    log_failure("update address bar", window.set_url_display(&url));
                }
                log_failure("hide loading state", window.set_loading(false));
            }
            sync_tab_strip(window, tabs);
        }
    }
}

fn handle_toolbar_command(
    window: &mut BrowserWindow,
    tabs: &mut Tabs,
    homepage: &str,
    command: ToolbarCommand,
) {
    match command {
        ToolbarCommand::Navigate { input } => match navigation::normalize_input(&input) {
            Some(url) => {
                tabs.active_mut().on_navigation_started(&url);
                log_failure("navigate", window.navigate(&url));
            }
            None => {
                eprintln!("velox: cannot navigate to {input:?}");
                // Snap the address bar back to the page we are actually on.
                log_failure(
                    "restore address bar",
                    window.set_url_display(tabs.active().current_url()),
                );
            }
        },
        ToolbarCommand::Back => log_failure("go back", window.go_back()),
        ToolbarCommand::Forward => log_failure("go forward", window.go_forward()),
        ToolbarCommand::Reload => log_failure("reload", window.reload()),
        ToolbarCommand::NewTab => {
            let id = tabs.open_at(homepage.to_owned(), Instant::now());
            log_failure("open tab", window.open_tab(id, homepage));
            activate_and_refresh(window, tabs, id);
        }
        ToolbarCommand::CloseTab { id } => {
            let id = TabId::from(id);
            if let Some(new_active) = tabs.close(id) {
                window.close_tab(id);
                // The tab that replaces the one just closed may itself have
                // been suspended (a background tab can be suspended while
                // the tab in front of it is closed); `activate_and_refresh`
                // resumes it if so.
                activate_and_refresh(window, tabs, new_active);
            }
            // Otherwise: unknown id, or `id` was the only remaining tab —
            // VeloX always keeps at least one tab open.
        }
        ToolbarCommand::ActivateTab { id } => {
            let id = TabId::from(id);
            if tabs.activate_at(id, Instant::now()) {
                activate_and_refresh(window, tabs, id);
            }
        }
        ToolbarCommand::SuspendTab { id } => {
            let id = TabId::from(id);
            if tabs.suspend(id) {
                log_failure("suspend tab", window.suspend_tab(id));
                sync_tab_strip(window, tabs);
            }
            // Otherwise: unknown id, the active tab (never suspended), or
            // already suspended — a no-op, mirroring `CloseTab`'s guards.
        }
        ToolbarCommand::Ready => {
            log_failure(
                "initialize address bar",
                window.set_url_display(tabs.active().current_url()),
            );
            log_failure(
                "initialize loading state",
                window.set_loading(tabs.active().is_loading()),
            );
            sync_tab_strip(window, tabs);
        }
    }
}

/// Show `id` in the window, then bring the toolbar (address bar, loading
/// indicator, tab strip) up to date with the now-active tab. The caller must
/// have already made `id` the active tab in `tabs` (`activate`/`activate_at`,
/// `open`/`open_at`, or the replacement tab returned by `close`).
///
/// If `id` was suspended, this also resumes it: clears the suspended flag
/// on the `Tabs` side and rebuilds its content webview (loading its last
/// known URL) on the `BrowserWindow` side, instead of the plain visibility
/// toggle used for an already-awake tab.
fn activate_and_refresh(window: &mut BrowserWindow, tabs: &mut Tabs, id: TabId) {
    let was_suspended = tabs.active().is_suspended();
    let result = if was_suspended {
        tabs.active_mut().resume();
        window.resume_tab(id, tabs.active().current_url())
    } else {
        window.activate_tab(id)
    };
    log_failure(
        if was_suspended {
            "resume tab"
        } else {
            "activate tab"
        },
        result,
    );
    if let Some(tab) = tabs.get(id) {
        log_failure(
            "update address bar",
            window.set_url_display(tab.current_url()),
        );
        log_failure("update loading state", window.set_loading(tab.is_loading()));
    }
    sync_tab_strip(window, tabs);
}

/// Push the full tab list to the toolbar's tab strip.
fn sync_tab_strip(window: &BrowserWindow, tabs: &Tabs) {
    let active_id = tabs.active_id();
    let summaries: Vec<TabSummary> = tabs
        .iter()
        .map(|tab| TabSummary {
            id: tab.id().get(),
            url: tab.current_url().to_owned(),
            loading: tab.is_loading(),
            active: tab.id() == active_id,
            suspended: tab.is_suspended(),
        })
        .collect();
    log_failure("update tab strip", window.set_tabs(&summaries));
}

/// A failed UI call (e.g. a script that could not be evaluated) should not
/// crash the browser; surface it on stderr instead.
fn log_failure(action: &str, result: wry::Result<()>) {
    if let Err(err) = result {
        eprintln!("velox: failed to {action}: {err}");
    }
}
