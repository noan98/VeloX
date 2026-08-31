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

use std::cell::Cell;

use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder};

use crate::app::UserEvent;
use crate::browser::{BookmarkEntry, HistoryEntry};
use crate::config::Config;
use crate::ui::toolbar::{self, Panel};

/// A rectangle in logical pixels: `(x, y, width, height)`.
type LogicalRect = (u32, u32, u32, u32);

/// Split the window area into a toolbar strip and the content area below it.
fn split_layout(width: u32, height: u32, toolbar_height: u32) -> (LogicalRect, LogicalRect) {
    let toolbar_height = toolbar_height.min(height);
    let toolbar_rect = (0, 0, width, toolbar_height);
    let content_rect = (0, toolbar_height, width, height - toolbar_height);
    (toolbar_rect, content_rect)
}

/// How tall the toolbar webview needs to be: just the address bar row, or
/// that plus room for an open history/bookmarks panel.
///
/// The panel lives inside the toolbar webview (not the content webview), so
/// opening it means growing the toolbar webview's own native bounds rather
/// than drawing an overlay — see docs/decisions.md D9 and the "Visit history
/// and bookmarks" section of docs/architecture.md.
fn effective_toolbar_height(toolbar_height: u32, panel_height: u32, panel_open: bool) -> u32 {
    if panel_open {
        toolbar_height.saturating_add(panel_height)
    } else {
        toolbar_height
    }
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
    /// Kept so panel-driven UI updates (`fetch_page_title`) can send
    /// [`UserEvent`]s back into the event loop after `new` has returned.
    proxy: EventLoopProxy<UserEvent>,
    toolbar_height: u32,
    panel_height: u32,
    /// Which history/bookmarks panel is currently open, if any. Interior
    /// mutability is needed because `sync_layout` (called from the window
    /// resize handler, which only has `&BrowserWindow`) must account for it.
    open_panel: Cell<Option<Panel>>,
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
        let load_proxy = proxy.clone();
        let content_builder = WebViewBuilder::new()
            .with_bounds(to_bounds(content_rect))
            .with_url(&config.homepage)
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
            });

        let (toolbar, content) = build_webviews(&window, toolbar_builder, content_builder)?;

        Ok(Self {
            window,
            toolbar,
            content,
            proxy,
            toolbar_height: config.toolbar_height,
            panel_height: config.panel_height,
            open_panel: Cell::new(None),
        })
    }

    /// Recompute webview bounds after the window was resized (or a panel
    /// was opened/closed).
    pub fn sync_layout(&self) -> wry::Result<()> {
        let size = self
            .window
            .inner_size()
            .to_logical::<u32>(self.window.scale_factor());
        let toolbar_height = effective_toolbar_height(
            self.toolbar_height,
            self.panel_height,
            self.open_panel.get().is_some(),
        );
        let (toolbar_rect, content_rect) = split_layout(size.width, size.height, toolbar_height);
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

    /// Toggle the bookmark ("star") button's active state.
    pub fn set_bookmark_active(&self, active: bool) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_bookmark_active_script(active))
    }

    /// Which history/bookmarks panel is currently open, if any.
    pub fn open_panel(&self) -> Option<Panel> {
        self.open_panel.get()
    }

    /// Open the given panel, or close it if it is already open (pass
    /// `None` to unconditionally close). Resizes the toolbar webview to
    /// make room and updates its DOM to match.
    pub fn set_panel(&self, panel: Option<Panel>) -> wry::Result<()> {
        self.open_panel.set(panel);
        self.sync_layout()?;
        self.toolbar
            .evaluate_script(&toolbar::set_panel_script(panel))
    }

    /// Replace the history panel's contents.
    pub fn set_history(&self, entries: &[&HistoryEntry]) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_history_script(entries))
    }

    /// Replace the bookmarks panel's contents.
    pub fn set_bookmarks(&self, entries: &[&BookmarkEntry]) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_bookmarks_script(entries))
    }

    /// Asynchronously read `document.title` from the content webview and
    /// report it back as [`UserEvent::PageTitleResolved`] for the history
    /// entry `id`.
    ///
    /// Fire-and-forget by design (see docs/decisions.md D10): wry has no
    /// synchronous way to read a JS value, and the load that triggered this
    /// request may already be superseded by the time the title comes back —
    /// the callback still applies it to `id`, which is fine, it simply
    /// means an older history entry's title arrives late. A blank title is
    /// dropped rather than overwriting a previously known one.
    pub fn fetch_page_title(&self, id: u64) -> wry::Result<()> {
        let proxy = self.proxy.clone();
        self.content
            .evaluate_script_with_callback("document.title", move |raw| {
                if let Some(title) = extract_js_string_result(&raw) {
                    let title = title.trim();
                    if !title.is_empty() {
                        let _ = proxy.send_event(UserEvent::PageTitleResolved {
                            id,
                            title: title.to_owned(),
                        });
                    }
                }
            })
    }
}

/// `WebView::evaluate_script_with_callback` hands back the JS result
/// serialized as a JSON string (see wry's `eval`); unwrap that one layer to
/// get the actual string `document.title` evaluated to.
fn extract_js_string_result(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(raw).ok()
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
    fn effective_height_adds_panel_height_only_when_open() {
        assert_eq!(effective_toolbar_height(48, 320, false), 48);
        assert_eq!(effective_toolbar_height(48, 320, true), 368);
    }

    #[test]
    fn extracts_a_js_string_result() {
        assert_eq!(
            extract_js_string_result("\"Example Domain\""),
            Some("Example Domain".to_owned())
        );
    }

    #[test]
    fn extracting_non_string_js_results_yields_none() {
        assert_eq!(extract_js_string_result("null"), None);
        assert_eq!(extract_js_string_result(""), None);
        assert_eq!(extract_js_string_result("42"), None);
    }
}
