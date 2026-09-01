//! The browser window: one native window hosting the toolbar webview and one
//! content webview per open tab.
//!
//! ```text
//! +--------------------------------------+
//! |  toolbar webview (browser chrome)    |  <- fixed height strip, shared;
//! +--------------------------------------+     grows to make room for an
//! |  content webview (the active tab)    |  <- fills the rest       open
//! +--------------------------------------+     history/bookmarks panel
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
//! [`ContentTab`], which is what lets tab suspension ([`Self::suspend_tab`])
//! drop a background tab's webview to reclaim memory without reshaping this
//! struct, and [`Self::resume_tab`] rebuild it later — see [`ContentTab`].
//!
//! **Ownership boundary** (see `browser::tab` module doc comment and
//! docs/decisions.md D19): `BrowserWindow` is the sole owner of every
//! content `WebView`, keyed by [`crate::browser::TabId`] in `contents`
//! below. `browser::Tab`/`Tabs` hold the *logical* lifecycle state
//! (`browser::TabState`: active/background/suspended/restoring) that this
//! type's `Option<WebView>` per tab must be kept consistent with — but
//! `browser::` itself never references a `WebView` or any other
//! `wry`/`tao`/`gtk` type. `app.rs` is what keeps the two in sync, always
//! updating `Tabs` first and pushing the result here.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder};

use crate::app::UserEvent;
use crate::browser::{BookmarkEntry, FilterList, HistoryEntry, TabId};
use crate::config::Config;
use crate::ui::toolbar::{self, Panel};

/// A rectangle in logical pixels: `(x, y, width, height)`.
type LogicalRect = (u32, u32, u32, u32);

/// The only message the content webview's devtools IPC channel accepts.
///
/// The content webview renders untrusted page content, so unlike the
/// toolbar's IPC channel (which parses a structured, trusted [`ToolbarCommand`]),
/// this handler does not deserialize anything a page sends it. It only ever
/// compares the raw body against this fixed string and otherwise ignores the
/// message. See docs/decisions.md D18 for the trust-boundary reasoning.
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
/// See docs/decisions.md D18 for why this approach was chosen over a
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

/// How tall the toolbar webview needs to be: just the tab strip + address
/// bar rows, or that plus room for an open history/bookmarks panel.
///
/// The panel lives inside the toolbar webview (not a content webview), so
/// opening it means growing the toolbar webview's own native bounds rather
/// than drawing an overlay — see docs/decisions.md D11 and the "Visit
/// history and bookmarks" section of docs/architecture.md.
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

/// One tab's content webview.
struct ContentTab {
    /// The tab's webview.
    ///
    /// `Some` for every open, awake tab. `None` while the tab is suspended
    /// ([`BrowserWindow::suspend_tab`] `take()`s and drops it to reclaim
    /// memory); the tab's `Tab` state (URL, loading flag — kept by
    /// `browser::Tabs`, outside this struct) is enough to rebuild it on
    /// reactivation via [`BrowserWindow::resume_tab`].
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
    panel_height: u32,
    /// Which history/bookmarks panel is currently open, if any. Interior
    /// mutability is needed because `sync_layout` (called from the window
    /// resize handler, which only has `&BrowserWindow`) must account for it.
    open_panel: Cell<Option<Panel>>,
    /// Kept so panel-driven UI updates (`fetch_page_title`) can send
    /// [`UserEvent`]s back into the event loop after `new` has returned.
    proxy: EventLoopProxy<UserEvent>,
    contents: HashMap<TabId, ContentTab>,
    active: Option<TabId>,
    /// Whole-app private browsing (see docs/decisions.md D14). Kept so tabs
    /// opened after startup are built with the same ephemeral data store.
    private: bool,
    /// Ad/tracker filter rules (see docs/decisions.md D17). Kept — alongside
    /// `content_blocking_enabled` — so every tab's content webview, however
    /// and whenever it comes into existence (a newly opened tab, a tab
    /// rebuilt on resume from suspension), is built through
    /// [`content_webview_builder`] with the same blocking behavior as the
    /// first tab.
    blocklist: Arc<FilterList>,
    /// Whether content blocking is currently active; threaded into every
    /// [`content_webview_builder`] call alongside `blocklist`.
    content_blocking_enabled: bool,
}

impl BrowserWindow {
    /// Create the window, the toolbar webview, and the first tab's content
    /// webview (bound to `initial_tab`, loading `config.homepage`).
    ///
    /// `blocklist` is the ad/tracker filter list content blocking matches
    /// against (see docs/decisions.md D17); it is stored on `self` so every
    /// tab opened later — or rebuilt on resume from suspension — is built
    /// through the same [`content_webview_builder`] with blocking applied.
    pub fn new(
        event_loop: &EventLoopWindowTarget<UserEvent>,
        config: &Config,
        proxy: EventLoopProxy<UserEvent>,
        initial_tab: TabId,
        blocklist: Arc<FilterList>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // A window-title suffix is a second, independent tell for private
        // mode (docs/decisions.md D14): unlike the toolbar badge it survives
        // being covered by another window in a taskbar/alt-tab switcher.
        let window_title = if config.private {
            format!("{} — プライベート", config.window_title)
        } else {
            config.window_title.clone()
        };
        let window = WindowBuilder::new()
            .with_title(&window_title)
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

        let content_blocking_enabled = config.content_blocking_enabled;
        let content_builder = content_webview_builder(
            initial_tab,
            &config.homepage,
            content_rect,
            &proxy,
            config.private,
            Arc::clone(&blocklist),
            content_blocking_enabled,
        );
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
            panel_height: config.panel_height,
            open_panel: Cell::new(None),
            proxy,
            contents,
            active: Some(initial_tab),
            private: config.private,
            blocklist,
            content_blocking_enabled,
        })
    }

    /// Current toolbar/content rectangles for the window's present size,
    /// accounting for whether a history/bookmarks panel is currently open
    /// (it grows the toolbar webview and shrinks the content area).
    fn layout(&self) -> (LogicalRect, LogicalRect) {
        let size = self
            .window
            .inner_size()
            .to_logical::<u32>(self.window.scale_factor());
        let toolbar_height = effective_toolbar_height(
            self.toolbar_height,
            self.panel_height,
            self.open_panel.get().is_some(),
        );
        split_layout(size.width, size.height, toolbar_height)
    }

    /// Recompute webview bounds after the window was resized (or a panel was
    /// opened/closed). Every open tab's content webview is resized, not just
    /// the active one, so a background tab is laid out correctly the moment
    /// it becomes visible instead of only on its own activation.
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
        let builder = content_webview_builder(
            id,
            url,
            content_rect,
            &self.proxy,
            self.private,
            Arc::clone(&self.blocklist),
            self.content_blocking_enabled,
        )
        .with_visible(false);
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

    /// Suspend tab `id`: drop its content webview to reclaim memory,
    /// keeping the `contents` entry (now `webview: None`) so the tab can be
    /// rebuilt later via [`Self::resume_tab`]. Refuses — logging instead of
    /// panicking, like every other UI call here — to suspend the currently
    /// active tab (it must stay visible) or an unknown tab id. The caller
    /// (`app.rs`) is expected to have already checked
    /// `browser::Tabs::suspend` succeeded, which enforces the same rule on
    /// the state side; this is a defensive second check on the webview
    /// side, not the source of truth for whether suspension is allowed.
    pub fn suspend_tab(&mut self, id: TabId) -> wry::Result<()> {
        if self.active == Some(id) {
            eprintln!("velox: suspend_tab: refusing to suspend the active tab {id:?}");
            return Ok(());
        }
        match self.contents.get_mut(&id) {
            Some(tab) => {
                // Dropped here: this is the memory reclaim.
                tab.webview.take();
                Ok(())
            }
            None => {
                eprintln!("velox: suspend_tab: unknown tab {id:?}");
                Ok(())
            }
        }
    }

    /// Rebuild a suspended tab's content webview, loading `url` (its last
    /// known address — everything else, scroll position, in-progress form
    /// input, and JS-side session history, was lost when the webview was
    /// dropped by [`Self::suspend_tab`]), and make it the visible tab.
    ///
    /// This is exactly [`Self::open_tab`] followed by [`Self::activate_tab`]
    /// — rebuilding a dropped webview for a `TabId` that `contents` already
    /// tracks is the same operation as building the first one for a new
    /// tab, so there is nothing suspension-specific to do here beyond
    /// reusing that path.
    pub fn resume_tab(&mut self, id: TabId, url: &str) -> wry::Result<()> {
        self.open_tab(id, url)?;
        self.activate_tab(id)
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

    /// Update the toolbar's blocked-request counter badge. The caller
    /// (`app.rs`) passes the *active* tab's `blocked_count` — background
    /// tabs accumulate their own counts but do not touch this badge until
    /// they become active (see `Tab::on_navigation_blocked` and
    /// `UserEvent::NavigationBlocked`'s `TabId`).
    pub fn set_block_count(&self, count: u32) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_block_count_script(count))
    }

    /// Open DevTools (Web Inspector) for the active tab's content webview.
    ///
    /// Kept as this one method (rather than inlining at each call site) so
    /// that a future change to how "active" is resolved only has to change
    /// here. A no-op — logged, not an error — when there is no active tab or
    /// the active tab is currently suspended (`webview: None`): there is
    /// nothing to open an inspector on until the tab is resumed.
    ///
    /// Compiled in whenever wry's `open_devtools` API exists: unconditionally
    /// on Linux/Windows, debug-only on macOS. See docs/decisions.md D18.
    #[cfg(any(debug_assertions, not(target_os = "macos")))]
    pub fn open_devtools(&self) {
        match self.active_webview() {
            Some(webview) => webview.open_devtools(),
            None => eprintln!(
                "velox: open_devtools: no active (or suspended) content webview to open devtools for"
            ),
        }
    }

    /// macOS release builds do not compile wry's devtools API (see
    /// docs/decisions.md D18); log instead of silently doing nothing.
    #[cfg(not(any(debug_assertions, not(target_os = "macos"))))]
    pub fn open_devtools(&self) {
        eprintln!(
            "velox: devtools is unavailable in macOS release builds (see docs/decisions.md D18)"
        );
    }

    /// Re-render the tab strip from `tabs`.
    pub fn set_tabs(&self, tabs: &[toolbar::TabSummary]) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_tabs_script(tabs))
    }

    /// Toggle the bookmark ("star") button's active state.
    pub fn set_bookmark_active(&self, active: bool) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_bookmark_active_script(active))
    }

    /// Show or hide the toolbar's always-visible private-browsing indicator.
    pub fn set_private(&self, private: bool) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_private_script(private))
    }

    /// Which history/bookmarks panel is currently open, if any.
    pub fn open_panel(&self) -> Option<Panel> {
        self.open_panel.get()
    }

    /// Open the given panel, or close it if it is already open (pass
    /// `None` to unconditionally close). Resizes the toolbar webview to
    /// make room — shrinking every open tab's content webview, not just the
    /// active one, via [`Self::sync_layout`] — and updates the toolbar's DOM
    /// to match.
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

    /// Asynchronously read `document.title` from tab `tab_id`'s content
    /// webview and report it back as [`UserEvent::PageTitleResolved`] for
    /// the history entry `history_id`.
    ///
    /// A no-op — not an error — when `tab_id` is unknown or currently
    /// suspended (no webview to read from): the history entry simply keeps
    /// showing its URL as the interim title until the tab is next resumed
    /// and reloads, at which point a fresh `LoadFinished` records a new
    /// entry and requests its title the normal way.
    ///
    /// Fire-and-forget by design (see docs/decisions.md D12): wry has no
    /// synchronous way to read a JS value, and the load that triggered this
    /// request may already be superseded by the time the title comes back —
    /// the callback still applies it to `history_id`, which is fine, it
    /// simply means an older history entry's title arrives late. A blank
    /// title is dropped rather than overwriting a previously known one.
    pub fn fetch_page_title(&self, tab_id: TabId, history_id: u64) -> wry::Result<()> {
        let webview = match self
            .contents
            .get(&tab_id)
            .and_then(|tab| tab.webview.as_ref())
        {
            Some(webview) => webview,
            None => return Ok(()),
        };
        let proxy = self.proxy.clone();
        webview.evaluate_script_with_callback("document.title", move |raw| {
            if let Some(title) = extract_js_string_result(&raw) {
                let title = title.trim();
                if !title.is_empty() {
                    let _ = proxy.send_event(UserEvent::PageTitleResolved {
                        tab_id,
                        history_id,
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

/// Build the `WebViewBuilder` for a tab's content webview: bounds, initial
/// URL, and navigation/page-load handlers that tag their `UserEvent`s with
/// `id` so `app.rs` knows which tab they belong to.
///
/// Every content webview — the first tab built in [`BrowserWindow::new`], a
/// tab opened later via [`BrowserWindow::open_tab`], and a tab rebuilt on
/// resume from suspension (which reuses `open_tab`) — is built through this
/// one function, which is what makes content blocking apply uniformly
/// regardless of when or how a tab's webview comes into existence: the
/// navigation handler checks `blocklist` before firing
/// [`UserEvent::NavigationStarted`], refusing the navigation (returning
/// `false`) and reporting [`UserEvent::NavigationBlocked`] instead when
/// `content_blocking_enabled` is set and the URL matches (see
/// docs/decisions.md D17).
fn content_webview_builder<'a>(
    id: TabId,
    url: &str,
    content_rect: LogicalRect,
    proxy: &EventLoopProxy<UserEvent>,
    private: bool,
    blocklist: Arc<FilterList>,
    content_blocking_enabled: bool,
) -> WebViewBuilder<'a> {
    let nav_proxy = proxy.clone();
    let block_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let devtools_proxy = proxy.clone();
    WebViewBuilder::new()
        .with_bounds(to_bounds(content_rect))
        .with_url(url)
        // Ephemeral (non-persistent) cookies/storage/cache for the page
        // content itself; see docs/decisions.md D15 for the per-platform
        // backing (WebKitGTK ephemeral WebContext / WKWebsiteDataStore
        // nonPersistentDataStore / WebView2 private-mode controller option)
        // and why the toolbar webview does not need this (it only ever loads
        // our own embedded HTML, never site content). Every tab's webview
        // goes through this builder, so tabs opened later - and suspended
        // tabs rebuilt on resume - stay ephemeral too.
        .with_incognito(private)
        // DevTools (see docs/decisions.md D18): every content webview built
        // through this one function — the initial tab, a newly opened tab,
        // and a suspended tab rebuilt on resume — gets the inspector enabled,
        // the F12/Cmd+Opt+I capture script injected, and its own untrusted
        // devtools IPC channel, so the shortcut keeps working no matter when
        // or how the webview came to exist.
        .with_devtools(true)
        .with_initialization_script(devtools_shortcut_script())
        .with_navigation_handler(move |url| {
            if content_blocking_enabled && blocklist.is_blocked(&url) {
                let _ = block_proxy.send_event(UserEvent::NavigationBlocked(id, url));
                return false;
            }
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
        .with_ipc_handler(move |request| {
            // See OPEN_DEVTOOLS_MESSAGE: this content-webview IPC channel is
            // untrusted and deliberately does nothing but this one
            // exact-match check — it never deserializes page-supplied data.
            if request.body().as_str() == OPEN_DEVTOOLS_MESSAGE {
                let _ = devtools_proxy.send_event(UserEvent::OpenDevtoolsRequested);
            }
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
