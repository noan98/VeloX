//! Application wiring: owns the event loop and connects the UI layer to the
//! browser logic.
//!
//! Everything UI-related happens on the main thread. The webview callbacks
//! (IPC, navigation, page load) forward their payloads into the event loop as
//! [`UserEvent`]s, so all state lives in one place and no locking is needed.

use std::error::Error;

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};

use crate::browser::{navigation, Tab};
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
    /// The content webview's devtools shortcut (F12 / Cmd+Opt+I) fired. Sent
    /// over a dedicated, tightly-restricted IPC channel, separate from the
    /// toolbar's — see docs/decisions.md D8.
    OpenDevtoolsRequested,
}

/// Build the window and run the event loop. Only returns on setup failure;
/// once running, the process exits with the event loop.
pub fn run(config: Config) -> Result<(), Box<dyn Error>> {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = BrowserWindow::new(&event_loop, &config, proxy)?;
    let mut tab = Tab::new(config.homepage.clone());

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
                handle_user_event(&window, &mut tab, user_event);
            }
            _ => {}
        }
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
        UserEvent::OpenDevtoolsRequested => window.open_devtools(),
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
        ToolbarCommand::OpenDevtools => window.open_devtools(),
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
