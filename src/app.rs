//! Application wiring: owns the event loop and connects the UI layer to the
//! browser logic.
//!
//! Everything UI-related happens on the main thread. The webview callbacks
//! (IPC, navigation, page load) forward their payloads into the event loop as
//! [`UserEvent`]s, so all state lives in one place and no locking is needed.

use std::error::Error;
use std::time::{Duration, Instant};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};

use crate::browser::{metrics, navigation, Tab};
use crate::config::Config;
use crate::ui::toolbar::{self, ToolbarCommand};
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

    let window = BrowserWindow::new(&event_loop, &config, proxy)?;
    if let Some(startup) = startup.as_mut() {
        startup.mark_window_created(Instant::now());
    }

    if config.perf_metrics {
        if let Some(interval) = config.perf_rss_interval {
            spawn_rss_sampler(interval);
        }
    }

    let mut tab = Tab::new(config.homepage.clone());
    let mut page_load_timer = metrics::PageLoadTimer::new();
    let perf_metrics_enabled = config.perf_metrics;

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
                    record_perf_event(&mut startup, &mut page_load_timer, &user_event);
                }
                handle_user_event(&window, &mut tab, user_event);
            }
            _ => {}
        }
    });
}

/// Update startup/page-load metrics state for one [`UserEvent`], logging to
/// stderr whenever a measurement completes. Only called when
/// `config.perf_metrics` is on, so every branch here is allowed a clock read
/// (the off-path never reaches this function at all).
fn record_perf_event(
    startup: &mut Option<metrics::StartupTimestamps>,
    page_load_timer: &mut metrics::PageLoadTimer,
    event: &UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(body) => {
            if matches!(toolbar::parse_command(body), Ok(ToolbarCommand::Ready)) {
                mark_startup(startup, metrics::StartupTimestamps::mark_toolbar_ready);
            }
        }
        UserEvent::NavigationStarted(_) => {
            page_load_timer.start(Instant::now());
        }
        UserEvent::LoadFinished(url) => {
            if let Some(duration) = page_load_timer.finish(Instant::now()) {
                eprintln!("velox[perf] {}", metrics::format_page_load(url, duration));
            }
            mark_startup(
                startup,
                metrics::StartupTimestamps::mark_first_load_finished,
            );
        }
        UserEvent::LoadStarted(_) => {}
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

/// Dispatch one [`UserEvent`]. UI failures are logged, never fatal.
fn handle_user_event(window: &BrowserWindow, tab: &mut Tab, event: UserEvent) {
    match event {
        UserEvent::ToolbarMessage(body) => match toolbar::parse_command(&body) {
            Ok(command) => handle_toolbar_command(window, tab, command),
            Err(err) => eprintln!("velox: ignoring malformed toolbar message {body:?}: {err}"),
        },
        UserEvent::NavigationStarted(url) | UserEvent::LoadStarted(url) => {
            tab.on_navigation_started(&url);
            log_failure("update address bar", window.set_url_display(&url));
            log_failure("show loading state", window.set_loading(true));
        }
        UserEvent::LoadFinished(url) => {
            // A failed load reports an empty URL; keep showing the URL the
            // user tried to reach instead of blanking the address bar.
            if url.is_empty() {
                tab.on_load_failed();
            } else {
                tab.on_load_finished(&url);
                log_failure("update address bar", window.set_url_display(&url));
            }
            log_failure("hide loading state", window.set_loading(false));
        }
    }
}

fn handle_toolbar_command(window: &BrowserWindow, tab: &mut Tab, command: ToolbarCommand) {
    match command {
        ToolbarCommand::Navigate { input } => match navigation::normalize_input(&input) {
            Some(url) => {
                tab.on_navigation_started(&url);
                log_failure("navigate", window.navigate(&url));
            }
            None => {
                eprintln!("velox: cannot navigate to {input:?}");
                // Snap the address bar back to the page we are actually on.
                log_failure(
                    "restore address bar",
                    window.set_url_display(tab.current_url()),
                );
            }
        },
        ToolbarCommand::Back => log_failure("go back", window.go_back()),
        ToolbarCommand::Forward => log_failure("go forward", window.go_forward()),
        ToolbarCommand::Reload => log_failure("reload", window.reload()),
        ToolbarCommand::Ready => {
            log_failure(
                "initialize address bar",
                window.set_url_display(tab.current_url()),
            );
            log_failure(
                "initialize loading state",
                window.set_loading(tab.is_loading()),
            );
        }
    }
}

/// A failed UI call (e.g. a script that could not be evaluated) should not
/// crash the browser; surface it on stderr instead.
fn log_failure(action: &str, result: wry::Result<()>) {
    if let Err(err) = result {
        eprintln!("velox: failed to {action}: {err}");
    }
}
