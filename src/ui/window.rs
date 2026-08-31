//! The browser window: one native window hosting the toolbar webview and one
//! content webview per open tab.
//!
//! ```text
//! +--------------------------------------+
//! |  toolbar webview (browser chrome)    |  <- fixed height strip, shared
//! +--------------------------------------+
//! |  content webview (the active tab)    |  <- fills the rest
//! +--------------------------------------+
//! ```
//!
//! The toolbar is our own HTML (see [`crate::ui::toolbar`]); each tab's
//! content view renders whatever page the user navigated it to. Keeping the
//! chrome in a separate webview means untrusted page content can never touch
//! the UI.
//!
//! Only the active tab's content webview is visible at a time; switching
//! tabs toggles visibility/bounds rather than destroying and recreating a
//! webview, so an inactive tab's scroll position and in-progress form input
//! survive the switch. Every content webview lives behind an `Option` inside
//! [`ContentTab`] so a future tab-suspension feature (dropping a background
//! tab's webview to save memory) can drop it without reshaping this struct —
//! see [`ContentTab`].

use std::collections::HashMap;

use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder};

use crate::app::UserEvent;
use crate::browser::TabId;
use crate::config::Config;
use crate::ui::toolbar;

/// A rectangle in logical pixels: `(x, y, width, height)`.
type LogicalRect = (u32, u32, u32, u32);

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

/// One tab's content webview.
struct ContentTab {
    /// The tab's webview.
    ///
    /// Wrapped in `Option` so a later tab-suspension feature can `take()`
    /// and drop it for a backgrounded tab to reclaim memory, while the
    /// tab's `Tab` state (URL, loading flag — kept by `app.rs`, outside this
    /// struct) is enough to rebuild it on reactivation. VeloX does not
    /// suspend tabs yet, so this is always `Some` today.
    webview: Option<WebView>,
}

/// The main browser window: the toolbar webview and one content webview per
/// tab.
pub struct BrowserWindow {
    window: Window,
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
    ))]
    host: gtk::Fixed,
    toolbar: WebView,
    toolbar_height: u32,
    proxy: EventLoopProxy<UserEvent>,
    contents: HashMap<TabId, ContentTab>,
    active: Option<TabId>,
}

impl BrowserWindow {
    /// Create the window, the toolbar webview, and the first tab's content
    /// webview (bound to `initial_tab`, loading `config.homepage`).
    pub fn new(
        event_loop: &EventLoopWindowTarget<UserEvent>,
        config: &Config,
        proxy: EventLoopProxy<UserEvent>,
        initial_tab: TabId,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let window = WindowBuilder::new()
            .with_title(&config.window_title)
            .with_inner_size(tao::dpi::LogicalSize::new(
                config.window_width,
                config.window_height,
            ))
            .build(event_loop)?;

        // On Linux/BSD, tao windows are gtk windows and wry webviews are gtk
        // widgets, so every webview goes in this `gtk::Fixed` container
        // (positioned via `with_bounds`/`set_bounds`), created once and
        // reused for every tab opened afterwards. Everywhere else wry
        // supports true child webviews directly (see `attach` below).
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
        ))]
        let host = {
            use gtk::prelude::{BoxExt, WidgetExt};
            use tao::platform::unix::WindowExtUnix;

            let vbox = window
                .default_vbox()
                .ok_or("tao window was created without its default gtk vbox")?;
            let fixed = gtk::Fixed::new();
            vbox.pack_start(&fixed, true, true, 0);
            fixed.show_all();
            fixed
        };

        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
        ))]
        let attach = |builder: WebViewBuilder<'_>| -> wry::Result<WebView> {
            use wry::WebViewBuilderExtUnix;
            builder.build_gtk(&host)
        };
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
        )))]
        let attach = |builder: WebViewBuilder<'_>| -> wry::Result<WebView> {
            builder.build_as_child(&window)
        };

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
        let toolbar = attach(toolbar_builder)?;

        let content_builder =
            content_webview_builder(initial_tab, &config.homepage, content_rect, &proxy);
        let content = attach(content_builder)?;

        let mut contents = HashMap::new();
        contents.insert(
            initial_tab,
            ContentTab {
                webview: Some(content),
            },
        );

        Ok(Self {
            window,
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
            ))]
            host,
            toolbar,
            toolbar_height: config.toolbar_height,
            proxy,
            contents,
            active: Some(initial_tab),
        })
    }

    /// Current toolbar/content rectangles for the window's present size.
    fn layout(&self) -> (LogicalRect, LogicalRect) {
        let size = self
            .window
            .inner_size()
            .to_logical::<u32>(self.window.scale_factor());
        split_layout(size.width, size.height, self.toolbar_height)
    }

    /// Recompute webview bounds after the window was resized.
    pub fn sync_layout(&self) -> wry::Result<()> {
        let (toolbar_rect, content_rect) = self.layout();
        self.toolbar.set_bounds(to_bounds(toolbar_rect))?;
        let bounds = to_bounds(content_rect);
        for tab in self.contents.values() {
            if let Some(webview) = &tab.webview {
                webview.set_bounds(bounds)?;
            }
        }
        Ok(())
    }

    /// Open a new tab: build its content webview (loading `url`), bound to
    /// `id`. The new webview starts hidden; the caller (`app.rs`) always
    /// follows up with [`Self::activate_tab`], since a newly opened tab is
    /// also the newly active one.
    pub fn open_tab(&mut self, id: TabId, url: &str) -> wry::Result<()> {
        let (_, content_rect) = self.layout();
        let builder =
            content_webview_builder(id, url, content_rect, &self.proxy).with_visible(false);
        let webview = self.attach_webview(builder)?;
        self.contents.insert(
            id,
            ContentTab {
                webview: Some(webview),
            },
        );
        Ok(())
    }

    /// Close a tab: drop its content webview. Does not change which tab is
    /// active — the caller activates the tab that should replace it (see
    /// `browser::Tabs::close`) via [`Self::activate_tab`].
    pub fn close_tab(&mut self, id: TabId) {
        self.contents.remove(&id);
        if self.active == Some(id) {
            self.active = None;
        }
    }

    /// Make `id` the visible tab: hide the previously active webview and
    /// show `id`'s. A no-op if `id` is already active.
    pub fn activate_tab(&mut self, id: TabId) -> wry::Result<()> {
        if self.active == Some(id) {
            return Ok(());
        }
        if let Some(previous) = self.active.and_then(|prev| self.contents.get(&prev)) {
            if let Some(webview) = &previous.webview {
                webview.set_visible(false)?;
            }
        }
        match self.contents.get(&id) {
            Some(tab) => {
                if let Some(webview) = &tab.webview {
                    let (_, content_rect) = self.layout();
                    webview.set_bounds(to_bounds(content_rect))?;
                    webview.set_visible(true)?;
                }
                self.active = Some(id);
                Ok(())
            }
            None => {
                // Should not happen: `app.rs` only ever activates a tab it
                // just opened or that is already tracked here.
                eprintln!("velox: activate_tab: unknown tab {id:?}");
                Ok(())
            }
        }
    }

    /// The active tab's content webview, if any is currently active.
    fn active_webview(&self) -> Option<&WebView> {
        self.active
            .and_then(|id| self.contents.get(&id))
            .and_then(|tab| tab.webview.as_ref())
    }

    /// Load `url` in the active tab's content webview.
    pub fn navigate(&self, url: &str) -> wry::Result<()> {
        match self.active_webview() {
            Some(webview) => webview.load_url(url),
            None => Ok(()),
        }
    }

    /// Go back in the active tab's session history (no-op at the oldest
    /// entry).
    pub fn go_back(&self) -> wry::Result<()> {
        match self.active_webview() {
            Some(webview) => webview.evaluate_script("history.back();"),
            None => Ok(()),
        }
    }

    /// Go forward in the active tab's session history (no-op at the newest
    /// entry).
    pub fn go_forward(&self) -> wry::Result<()> {
        match self.active_webview() {
            Some(webview) => webview.evaluate_script("history.forward();"),
            None => Ok(()),
        }
    }

    /// Reload the active tab's current page.
    pub fn reload(&self) -> wry::Result<()> {
        match self.active_webview() {
            Some(webview) => webview.reload(),
            None => Ok(()),
        }
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

    /// Re-render the tab strip from `tabs`.
    pub fn set_tabs(&self, tabs: &[toolbar::TabSummary]) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_tabs_script(tabs))
    }
}

/// Build the `WebViewBuilder` for a tab's content webview: bounds, initial
/// URL, and navigation/page-load handlers that tag their `UserEvent`s with
/// `id` so `app.rs` knows which tab they belong to.
fn content_webview_builder<'a>(
    id: TabId,
    url: &str,
    content_rect: LogicalRect,
    proxy: &EventLoopProxy<UserEvent>,
) -> WebViewBuilder<'a> {
    let nav_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    WebViewBuilder::new()
        .with_bounds(to_bounds(content_rect))
        .with_url(url)
        .with_navigation_handler(move |url| {
            let _ = nav_proxy.send_event(UserEvent::NavigationStarted(id, url));
            true
        })
        .with_on_page_load_handler(move |event, url| {
            let event = match event {
                PageLoadEvent::Started => UserEvent::LoadStarted(id, url),
                PageLoadEvent::Finished => UserEvent::LoadFinished(id, url),
            };
            let _ = load_proxy.send_event(event);
        })
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
))]
impl BrowserWindow {
    /// Attach a webview built for a tab opened after startup. Startup's own
    /// toolbar + first tab attach via the local `attach` closure in `new`
    /// (no `self` exists yet at that point).
    fn attach_webview(&self, builder: WebViewBuilder<'_>) -> wry::Result<WebView> {
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(&self.host)
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
)))]
impl BrowserWindow {
    fn attach_webview(&self, builder: WebViewBuilder<'_>) -> wry::Result<WebView> {
        builder.build_as_child(&self.window)
    }
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
}
