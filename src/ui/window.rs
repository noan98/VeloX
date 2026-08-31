//! The browser window: one native window hosting two webviews.
//!
//! ```text
//! +--------------------------------------+
//! |  toolbar webview (browser chrome)    |  <- fixed height strip
//! +--------------------------------------+
//! |  content webview (the web page)      |  <- fills the rest
//! +--------------------------------------+
//! ```
//!
//! The toolbar is our own HTML (see [`crate::ui::toolbar`]); the content view
//! renders whatever page the user navigates to. Keeping the chrome in a
//! separate webview means untrusted page content can never touch the UI.

use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder};

use crate::app::UserEvent;
use crate::config::Config;
use crate::ui::toolbar;

/// A rectangle in logical pixels: `(x, y, width, height)`.
type LogicalRect = (u32, u32, u32, u32);

/// The only message the content webview's devtools IPC channel accepts.
///
/// The content webview renders untrusted page content, so unlike the
/// toolbar's IPC channel (which parses a structured, trusted [`ToolbarCommand`]),
/// this handler does not deserialize anything a page sends it. It only ever
/// compares the raw body against this fixed string and otherwise ignores the
/// message. See docs/decisions.md D8 for the trust-boundary reasoning.
///
/// [`ToolbarCommand`]: crate::ui::toolbar::ToolbarCommand
const OPEN_DEVTOOLS_MESSAGE: &str = "velox:open-devtools";

/// Initialization script injected into the content webview to capture the
/// devtools shortcut (F12, or Cmd+Opt+I on macOS) even while the page has
/// focus, and forward it to Rust over [`OPEN_DEVTOOLS_MESSAGE`].
///
/// Registered via `with_initialization_script`, so it runs before any page
/// script on every navigation, and listens in the capture phase so it gets
/// first refusal against pages that try to swallow the keydown themselves.
/// See docs/decisions.md D8 for why this approach was chosen over a
/// tao-level accelerator / `WindowEvent::KeyboardInput`.
fn devtools_shortcut_script() -> String {
    format!(
        r#"(() => {{
  "use strict";
  window.addEventListener("keydown", (event) => {{
    const isF12 = event.key === "F12";
    const isMacToggle = event.metaKey && event.altKey && (event.key === "i" || event.key === "I");
    if (!isF12 && !isMacToggle) {{
      return;
    }}
    event.preventDefault();
    if (window.ipc) {{
      window.ipc.postMessage("{OPEN_DEVTOOLS_MESSAGE}");
    }}
  }}, true);
}})();"#
    )
}

/// Split the window area into a toolbar strip and the content area below it.
fn split_layout(width: u32, height: u32, toolbar_height: u32) -> (LogicalRect, LogicalRect) {
    let toolbar_height = toolbar_height.min(height);
    let toolbar_rect = (0, 0, width, toolbar_height);
    let content_rect = (0, toolbar_height, width, height - toolbar_height);
    (toolbar_rect, content_rect)
}

fn to_bounds((x, y, width, height): LogicalRect) -> Rect {
    Rect {
        position: LogicalPosition::new(x, y).into(),
        size: LogicalSize::new(width, height).into(),
    }
}

/// The main browser window and its two webviews.
pub struct BrowserWindow {
    window: Window,
    toolbar: WebView,
    content: WebView,
    toolbar_height: u32,
}

impl BrowserWindow {
    /// Create the window, the toolbar webview and the content webview, and
    /// start loading `config.homepage`.
    pub fn new(
        event_loop: &EventLoopWindowTarget<UserEvent>,
        config: &Config,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let window = WindowBuilder::new()
            .with_title(&config.window_title)
            .with_inner_size(tao::dpi::LogicalSize::new(
                config.window_width,
                config.window_height,
            ))
            .build(event_loop)?;

        let size = window.inner_size().to_logical::<u32>(window.scale_factor());
        let (toolbar_rect, content_rect) =
            split_layout(size.width, size.height, config.toolbar_height);

        let ipc_proxy = proxy.clone();
        let toolbar_builder = WebViewBuilder::new()
            .with_bounds(to_bounds(toolbar_rect))
            .with_html(toolbar::TOOLBAR_HTML)
            .with_ipc_handler(move |request| {
                let _ = ipc_proxy.send_event(UserEvent::ToolbarMessage(request.into_body()));
            });

        let nav_proxy = proxy.clone();
        let devtools_proxy = proxy.clone();
        let load_proxy = proxy;
        let content_builder = WebViewBuilder::new()
            .with_bounds(to_bounds(content_rect))
            .with_url(&config.homepage)
            .with_devtools(true)
            .with_initialization_script(devtools_shortcut_script())
            .with_navigation_handler(move |url| {
                let _ = nav_proxy.send_event(UserEvent::NavigationStarted(url));
                true
            })
            .with_on_page_load_handler(move |event, url| {
                let event = match event {
                    PageLoadEvent::Started => UserEvent::LoadStarted(url),
                    PageLoadEvent::Finished => UserEvent::LoadFinished(url),
                };
                let _ = load_proxy.send_event(event);
            })
            .with_ipc_handler(move |request| {
                // See OPEN_DEVTOOLS_MESSAGE: this content-webview IPC channel
                // is untrusted and deliberately does nothing but this one
                // exact-match check.
                if request.body().as_str() == OPEN_DEVTOOLS_MESSAGE {
                    let _ = devtools_proxy.send_event(UserEvent::OpenDevtoolsRequested);
                }
            });

        let (toolbar, content) = build_webviews(&window, toolbar_builder, content_builder)?;

        Ok(Self {
            window,
            toolbar,
            content,
            toolbar_height: config.toolbar_height,
        })
    }

    /// Recompute webview bounds after the window was resized.
    pub fn sync_layout(&self) -> wry::Result<()> {
        let size = self
            .window
            .inner_size()
            .to_logical::<u32>(self.window.scale_factor());
        let (toolbar_rect, content_rect) =
            split_layout(size.width, size.height, self.toolbar_height);
        self.toolbar.set_bounds(to_bounds(toolbar_rect))?;
        self.content.set_bounds(to_bounds(content_rect))
    }

    /// Load `url` in the content webview.
    pub fn navigate(&self, url: &str) -> wry::Result<()> {
        self.content.load_url(url)
    }

    /// Go back in the engine's session history (no-op at the oldest entry).
    pub fn go_back(&self) -> wry::Result<()> {
        self.content.evaluate_script("history.back();")
    }

    /// Go forward in the engine's session history (no-op at the newest entry).
    pub fn go_forward(&self) -> wry::Result<()> {
        self.content.evaluate_script("history.forward();")
    }

    /// Reload the current page.
    pub fn reload(&self) -> wry::Result<()> {
        self.content.reload()
    }

    /// Show `url` in the toolbar's address bar.
    pub fn set_url_display(&self, url: &str) -> wry::Result<()> {
        self.toolbar.evaluate_script(&toolbar::set_url_script(url))
    }

    /// Toggle the toolbar's loading indicator.
    pub fn set_loading(&self, loading: bool) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_loading_script(loading))
    }

    /// Open DevTools (Web Inspector) for the active content webview.
    ///
    /// Kept as this one method so that #2's move to `Vec<WebView>` only has
    /// to change what "active" resolves to, not every devtools call site.
    ///
    /// Compiled in whenever wry's `open_devtools` API exists: unconditionally
    /// on Linux/Windows, debug-only on macOS. See docs/decisions.md D8.
    #[cfg(any(debug_assertions, not(target_os = "macos")))]
    pub fn open_devtools(&self) {
        self.content.open_devtools();
    }

    /// macOS release builds do not compile wry's devtools API (see
    /// docs/decisions.md D8); log instead of silently doing nothing.
    #[cfg(not(any(debug_assertions, not(target_os = "macos"))))]
    pub fn open_devtools(&self) {
        eprintln!(
            "velox: devtools is unavailable in macOS release builds (see docs/decisions.md D8)"
        );
    }
}

/// Attach both webviews to the window.
///
/// On Linux/BSD, tao windows are gtk windows and wry webviews are gtk
/// widgets, so both are placed in a `gtk::Fixed` container (positioned via
/// `with_bounds`/`set_bounds`). Everywhere else wry supports true child
/// webviews directly.
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
))]
fn build_webviews(
    window: &Window,
    toolbar_builder: WebViewBuilder<'_>,
    content_builder: WebViewBuilder<'_>,
) -> Result<(WebView, WebView), Box<dyn std::error::Error>> {
    use gtk::prelude::{BoxExt, WidgetExt};
    use tao::platform::unix::WindowExtUnix;
    use wry::WebViewBuilderExtUnix;

    let vbox = window
        .default_vbox()
        .ok_or("tao window was created without its default gtk vbox")?;
    let fixed = gtk::Fixed::new();
    vbox.pack_start(&fixed, true, true, 0);
    fixed.show_all();

    let toolbar = toolbar_builder.build_gtk(&fixed)?;
    let content = content_builder.build_gtk(&fixed)?;
    Ok((toolbar, content))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
)))]
fn build_webviews(
    window: &Window,
    toolbar_builder: WebViewBuilder<'_>,
    content_builder: WebViewBuilder<'_>,
) -> Result<(WebView, WebView), Box<dyn std::error::Error>> {
    let toolbar = toolbar_builder.build_as_child(window)?;
    let content = content_builder.build_as_child(window)?;
    Ok((toolbar, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_splits_toolbar_and_content() {
        let (toolbar, content) = split_layout(1024, 768, 48);
        assert_eq!(toolbar, (0, 0, 1024, 48));
        assert_eq!(content, (0, 48, 1024, 720));
    }

    #[test]
    fn layout_survives_tiny_windows() {
        let (toolbar, content) = split_layout(200, 30, 48);
        assert_eq!(toolbar, (0, 0, 200, 30));
        assert_eq!(content, (0, 30, 200, 0));
    }

    #[test]
    fn devtools_script_captures_f12_and_mac_toggle_and_reports_the_trigger_message() {
        let script = devtools_shortcut_script();
        assert!(script.contains("F12"));
        assert!(script.contains("metaKey && event.altKey"));
        assert!(script.contains(&format!(
            "window.ipc.postMessage(\"{OPEN_DEVTOOLS_MESSAGE}\")"
        )));
        // Registered in the capture phase (the trailing `true` to addEventListener).
        assert!(script.contains("}, true);"));
    }
}
