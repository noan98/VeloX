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
//! docs/decisions.md D20): `BrowserWindow` is the sole owner of every
//! content `WebView`, keyed by [`crate::browser::TabId`] in `contents`
//! below. `browser::Tab`/`Tabs` hold the *logical* lifecycle state
//! (`browser::TabState`: active/background/suspended/restoring) that this
//! type's `Option<WebView>` per tab must be kept consistent with — but
//! `browser::` itself never references a `WebView` or any other
//! `wry`/`tao`/`gtk` type. `app.rs` is what keeps the two in sync, always
//! updating `Tabs` first and pushing the result here.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder};

use crate::app::UserEvent;
use crate::browser::downloads;
use crate::browser::{group_by_date, Candidate, DownloadEntry, FilterList, HistoryEntry, TabId};
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

// --- Tab-management keyboard shortcuts (see docs/decisions.md D23) ---
//
// Fixed sentinel strings for the content webview's shortcut IPC channel,
// alongside `OPEN_DEVTOOLS_MESSAGE` above. As with devtools, this channel
// exists because the content webview is untrusted page content: it can
// never grow into a second `ToolbarCommand`-style structured-command parser
// (see docs/decisions.md D18), so every message here is compared by exact
// string equality only, never deserialized.
const NEW_TAB_MESSAGE: &str = "velox:new-tab";
const CLOSE_TAB_MESSAGE: &str = "velox:close-tab";
const REOPEN_CLOSED_TAB_MESSAGE: &str = "velox:reopen-closed-tab";
const NEXT_TAB_MESSAGE: &str = "velox:next-tab";
const PREV_TAB_MESSAGE: &str = "velox:prev-tab";
const ACTIVATE_LAST_TAB_MESSAGE: &str = "velox:activate-tab-last";
/// Ctrl/Cmd+L (Issue #15): focus the address bar. See
/// `ContentShortcut::FocusAddressBar` and docs/decisions.md D26.
const FOCUS_ADDRESS_BAR_MESSAGE: &str = "velox:focus-address-bar";
/// Ctrl/Cmd+D (Issue #19): bookmark/unbookmark the current page. See
/// `ContentShortcut::ToggleBookmark` and docs/decisions.md D35.
const TOGGLE_BOOKMARK_MESSAGE: &str = "velox:toggle-bookmark";
/// Ctrl/Cmd+Shift+B (Issue #19): show/hide the bookmark bar. See
/// `ContentShortcut::ToggleBookmarkBar` and docs/decisions.md D35.
const TOGGLE_BOOKMARK_BAR_MESSAGE: &str = "velox:toggle-bookmark-bar";
/// Prefix shared by the eight `velox:activate-tab-1` .. `velox:activate-tab-8`
/// messages (Ctrl/Cmd+1..8); see [`tab_shortcut_script`] and
/// [`parse_content_shortcut`].
const ACTIVATE_TAB_MESSAGE_PREFIX: &str = "velox:activate-tab-";

/// A tab-management keyboard shortcut reported by the content webview's
/// shortcut IPC channel (see [`parse_content_shortcut`]).
///
/// Deliberately carries no [`TabId`]: every variant here acts on "the
/// active tab" (`browser::Tabs::active_id()` and friends), resolved
/// server-side in `app.rs` — mirroring how [`UserEvent::OpenDevtoolsRequested`]
/// already always resolves the active tab rather than trusting the sending
/// webview's identity (see docs/decisions.md D18). This also sidesteps a
/// subtlety: only the visible, focused webview can realistically be the
/// source of a real keypress, so there is no meaningful "which tab sent
/// this" question to answer in the first place.
///
/// [`UserEvent::OpenDevtoolsRequested`]: crate::app::UserEvent::OpenDevtoolsRequested
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentShortcut {
    /// Ctrl/Cmd+T.
    NewTab,
    /// Ctrl/Cmd+W.
    CloseTab,
    /// Ctrl/Cmd+Shift+T.
    ReopenClosedTab,
    /// Ctrl/Cmd+Tab.
    NextTab,
    /// Ctrl/Cmd+Shift+Tab.
    PrevTab,
    /// Ctrl/Cmd+1..8: activate the tab at this 1-based display position.
    ActivateTabAt(u8),
    /// Ctrl/Cmd+9: activate the last tab.
    ActivateLastTab,
    /// Ctrl/Cmd+L (Issue #15): focus the address bar and select its
    /// contents. The content-webview half of
    /// `ui::toolbar::ToolbarCommand::FocusAddressBar` — both are handled by
    /// the same shared function in `app.rs`.
    FocusAddressBar,
    /// Ctrl/Cmd+D (Issue #19): bookmark/unbookmark the current page. The
    /// content-webview half of `ui::toolbar::ToolbarCommand::ToggleBookmark`
    /// — both are handled by the same shared function in `app.rs`. See
    /// docs/decisions.md D35.
    ToggleBookmark,
    /// Ctrl/Cmd+Shift+B (Issue #19): show/hide the bookmark bar. The
    /// content-webview half of
    /// `ui::toolbar::ToolbarCommand::ToggleBookmarkBar`. See
    /// docs/decisions.md D35.
    ToggleBookmarkBar,
}

/// Parse one content-webview shortcut IPC message body. `None` for anything
/// that is not an exact match for one of the fixed sentinel strings above —
/// including, deliberately, any attempt at parsing it as JSON or otherwise
/// treating it as structured data (see [`ContentShortcut`]'s doc comment and
/// docs/decisions.md D18/D23).
fn parse_content_shortcut(body: &str) -> Option<ContentShortcut> {
    match body {
        NEW_TAB_MESSAGE => Some(ContentShortcut::NewTab),
        CLOSE_TAB_MESSAGE => Some(ContentShortcut::CloseTab),
        REOPEN_CLOSED_TAB_MESSAGE => Some(ContentShortcut::ReopenClosedTab),
        NEXT_TAB_MESSAGE => Some(ContentShortcut::NextTab),
        PREV_TAB_MESSAGE => Some(ContentShortcut::PrevTab),
        ACTIVATE_LAST_TAB_MESSAGE => Some(ContentShortcut::ActivateLastTab),
        FOCUS_ADDRESS_BAR_MESSAGE => Some(ContentShortcut::FocusAddressBar),
        TOGGLE_BOOKMARK_MESSAGE => Some(ContentShortcut::ToggleBookmark),
        TOGGLE_BOOKMARK_BAR_MESSAGE => Some(ContentShortcut::ToggleBookmarkBar),
        "velox:activate-tab-1" => Some(ContentShortcut::ActivateTabAt(1)),
        "velox:activate-tab-2" => Some(ContentShortcut::ActivateTabAt(2)),
        "velox:activate-tab-3" => Some(ContentShortcut::ActivateTabAt(3)),
        "velox:activate-tab-4" => Some(ContentShortcut::ActivateTabAt(4)),
        "velox:activate-tab-5" => Some(ContentShortcut::ActivateTabAt(5)),
        "velox:activate-tab-6" => Some(ContentShortcut::ActivateTabAt(6)),
        "velox:activate-tab-7" => Some(ContentShortcut::ActivateTabAt(7)),
        "velox:activate-tab-8" => Some(ContentShortcut::ActivateTabAt(8)),
        _ => None,
    }
}

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

/// Initialization script that captures the tab-management keyboard
/// shortcuts (Ctrl/Cmd+T/W/Shift+T/Tab/Shift+Tab/1-9) while the content
/// webview has focus, forwarding a fixed sentinel string per shortcut over
/// the same untrusted IPC channel devtools uses (see [`ContentShortcut`] and
/// docs/decisions.md D18/D23 for why this is a separate injected script
/// rather than a tao-level accelerator).
///
/// `event.ctrlKey || event.metaKey` accepts both modifiers on every
/// platform instead of branching on OS (macOS is Cmd, Linux/Windows is
/// Ctrl) — the simplest way to "handle both", and harmless since Cmd simply
/// never fires outside macOS and vice versa.
fn tab_shortcut_script() -> String {
    format!(
        r#"(() => {{
  "use strict";
  window.addEventListener("keydown", (event) => {{
    const mod = event.ctrlKey || event.metaKey;
    if (!mod) {{
      return;
    }}
    let message = null;
    if (!event.altKey && !event.shiftKey) {{
      if (event.key === "t" || event.key === "T") {{
        message = "{NEW_TAB_MESSAGE}";
      }} else if (event.key === "w" || event.key === "W") {{
        message = "{CLOSE_TAB_MESSAGE}";
      }} else if (event.key === "Tab") {{
        message = "{NEXT_TAB_MESSAGE}";
      }} else if (event.key === "9") {{
        message = "{ACTIVATE_LAST_TAB_MESSAGE}";
      }} else if (event.key >= "1" && event.key <= "8") {{
        message = "{ACTIVATE_TAB_MESSAGE_PREFIX}" + event.key;
      }} else if (event.key === "l" || event.key === "L") {{
        message = "{FOCUS_ADDRESS_BAR_MESSAGE}";
      }} else if (event.key === "d" || event.key === "D") {{
        message = "{TOGGLE_BOOKMARK_MESSAGE}";
      }}
    }} else if (event.shiftKey && !event.altKey) {{
      if (event.key === "t" || event.key === "T") {{
        message = "{REOPEN_CLOSED_TAB_MESSAGE}";
      }} else if (event.key === "Tab") {{
        message = "{PREV_TAB_MESSAGE}";
      }} else if (event.key === "b" || event.key === "B") {{
        message = "{TOGGLE_BOOKMARK_BAR_MESSAGE}";
      }}
    }}
    if (message === null) {{
      return;
    }}
    event.preventDefault();
    if (window.ipc) {{
      window.ipc.postMessage(message);
    }}
  }}, true);
}})();"#
    )
}

/// Initialization script that resolves this page's favicon URL on demand:
/// its `<link rel="icon">` (or the closest relative, `rel~="icon"`, which
/// also matches `shortcut icon`/`apple-touch-icon` etc.) if the page
/// declares one, otherwise a same-origin `/favicon.ico` guess. Only ever
/// invoked via [`BrowserWindow::fetch_favicon`]'s
/// `evaluate_script_with_callback` — not injected as a standing listener —
/// so this returns a value rather than posting a message. See
/// docs/decisions.md D22 for why resolving *a URL* is all this does: the
/// actual image fetch is left entirely to the toolbar webview's own `<img>`
/// tag, never performed here or anywhere else in Rust.
const RESOLVE_FAVICON_SCRIPT: &str = r#"(() => {
  try {
    const link = document.querySelector('link[rel~="icon"][href]');
    if (link && link.href) {
      return link.href;
    }
    return new URL("/favicon.ico", location.href).href;
  } catch (err) {
    return "";
  }
})();"#;

/// Split the window area into a toolbar strip and the content area below it.
fn split_layout(width: u32, height: u32, toolbar_height: u32) -> (LogicalRect, LogicalRect) {
    let toolbar_height = toolbar_height.min(height);
    let toolbar_rect = (0, 0, width, toolbar_height);
    let content_rect = (0, toolbar_height, width, height - toolbar_height);
    (toolbar_rect, content_rect)
}

/// How tall the toolbar webview needs to be: the tab strip + address bar
/// rows, plus room for the bookmark bar when it is showing, plus room for
/// an open history/bookmarks/downloads/omnibox panel when one is open.
///
/// Both the panel and the bookmark bar live inside the toolbar webview (not
/// a content webview), so showing either means growing the toolbar
/// webview's own native bounds rather than drawing an overlay — see
/// docs/decisions.md D11 for the panel and D35 for why the bookmark bar is
/// a *third*, independent, always-can-be-on component here rather than
/// reusing the panel machinery: unlike a panel, the bar is meant to stay
/// visible while a panel is also open (both are simple additional rows in
/// the toolbar webview's own flex column), so their heights are summed, not
/// treated as alternatives.
fn effective_toolbar_height(
    toolbar_height: u32,
    panel_height: u32,
    panel_open: bool,
    bookmark_bar_height: u32,
    bookmark_bar_visible: bool,
) -> u32 {
    let mut height = toolbar_height;
    if bookmark_bar_visible {
        height = height.saturating_add(bookmark_bar_height);
    }
    if panel_open {
        height = height.saturating_add(panel_height);
    }
    height
}

fn to_bounds((x, y, width, height): LogicalRect) -> Rect {
    Rect {
        position: LogicalPosition::new(x, y).into(),
        size: LogicalSize::new(width, height).into(),
    }
}

/// Start a [`WebViewBuilder`], sharing `context` when one is given.
///
/// See docs/decisions.md D49: in non-private mode `BrowserWindow` holds one
/// `WebContext` shared by the toolbar and every tab's content webview, so
/// `wry` stops creating a fresh `WebKitWebProcess`/`WebKitNetworkProcess`
/// pair per webview. `context` is `None` in private mode (see
/// `BrowserWindow::context`'s doc comment for why sharing is skipped there
/// rather than attempted and ignored) — `WebViewBuilder::new()` reproduces
/// today's behavior in that case.
fn new_webview_builder(context: Option<&mut WebContext>) -> WebViewBuilder<'_> {
    match context {
        Some(context) => WebViewBuilder::new_with_web_context(context),
        None => WebViewBuilder::new(),
    }
}

/// Ask WebKitGTK to put the webview `builder` is about to create into the
/// same `WebKitWebProcess` as `related` (docs/decisions.md D52).
///
/// D49's shared `WebContext` merged the per-webview `WebKitNetworkProcess`
/// but left one `WebKitWebProcess` per webview (`docs/memory-analysis.md`
/// §9.3): WebKitGTK only shares a web process between views that are
/// explicitly *related*, which wry exposes as
/// `WebViewBuilderExtUnix::with_related_view`. A content webview built
/// after the first (a newly opened tab, a suspended tab rebuilt on resume)
/// is therefore related to one that is already alive — which one, and
/// whether at all, is decided by [`pick_process_group`] — so a window's
/// tabs end up in a handful of shared `WebKitWebProcess`es instead of one
/// each.
///
/// The toolbar is deliberately *never* passed here: it is VeloX's trusted
/// UI (docs/decisions.md D18/D23 draw the IPC trust boundary around it),
/// and sharing a renderer process with page content would put untrusted
/// pages on the same side of that boundary as the toolbar's own DOM.
///
/// `webkit2gtk::WebView` (the type `with_related_view` wants) is obtained
/// from wry's own `WebViewExtUnix::webview` accessor on the existing
/// `wry::WebView`, so no new dependency crate is needed and nothing outside
/// this function ever names a WebKitGTK type — the layering rule in D20
/// (`browser::` never sees `wry`/`gtk`) is untouched, and `src/ui/` already
/// depends on the platform backend.
///
/// On every other platform this is the identity: wry has no equivalent
/// there (WKWebView shares its content process pool per
/// `WKProcessPool`/configuration automatically, WebView2 per environment),
/// and D48/D49 measured this problem on WebKitGTK only.
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
))]
fn with_related_content_view<'a>(
    builder: WebViewBuilder<'a>,
    related: Option<&WebView>,
) -> WebViewBuilder<'a> {
    use wry::{WebViewBuilderExtUnix, WebViewExtUnix};
    match related {
        Some(related) => builder.with_related_view(related.webview()),
        None => builder,
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
)))]
fn with_related_content_view<'a>(
    builder: WebViewBuilder<'a>,
    _related: Option<&WebView>,
) -> WebViewBuilder<'a> {
    builder
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
    /// Which `WebKitWebProcess` this tab's webview lives in, as an opaque
    /// id handed out by [`BrowserWindow::next_process_group`] (docs/
    /// decisions.md D52). Tabs built with `related` pointing at a tab in
    /// group `g` join group `g`; a tab built with no related view starts a
    /// new group. Only meaningful on WebKitGTK (elsewhere the id is
    /// assigned but never influences anything) and only while `webview`
    /// is `Some` — a suspended tab has left its process, so it does not
    /// count towards the group's size.
    process_group: u64,
}

/// Upper bound on how many content webviews are put into one
/// `WebKitWebProcess` (docs/decisions.md D52).
///
/// Sharing *every* tab through one process (the first cut of D52) cut PSS
/// by up to a third at 20 tabs, but a web process has a single main
/// thread: opening several tabs back-to-back (`velox-bench`'s `tab_switch`
/// scenario opens 4 at once) serialized their page loads, `page_load_ms`
/// going from 18ms to 136ms (median) — a regression Epic #57's rule 4
/// ("memory savings that slow loading are not an improvement") does not
/// allow. The fix is two-fold: [`pick_process_group`] never puts a tab
/// into a process that is still loading another tab's page (that alone
/// brought `tab_switch` back to +15〜18%), and this cap bounds how much a
/// single renderer crash or main-thread stall can take down — 20 tabs
/// collapse into 5 processes instead of 20. The value is a tuning knob,
/// not a measured optimum: it matches the core count of the fixed
/// benchmark environment (`docs/performance-targets.md` §1).
const MAX_TABS_PER_WEB_PROCESS: usize = 4;

/// Pick the process group a new tab should join, given `(group, loading)`
/// for every tab that currently has a live webview: the fullest group that
/// still has room under [`MAX_TABS_PER_WEB_PROCESS`] *and* has no tab
/// currently loading a page (so processes fill up before a new one is
/// started, but a burst of tabs opened back-to-back — each still loading
/// when the next one is opened — fans out over fresh processes and loads
/// in parallel, exactly as it did before D52). `None` when no such group
/// exists, meaning the tab should start a fresh process/group.
///
/// Pure so it can be unit-tested without a display; the caller maps the
/// chosen group back to one of its live webviews.
fn pick_process_group(live_tabs: impl IntoIterator<Item = (u64, bool)>) -> Option<u64> {
    // (size, has a loading tab) per group.
    let mut groups: HashMap<u64, (usize, bool)> = HashMap::new();
    for (group, loading) in live_tabs {
        let entry = groups.entry(group).or_insert((0, false));
        entry.0 += 1;
        entry.1 |= loading;
    }
    groups
        .into_iter()
        .filter(|(_, (size, busy))| *size < MAX_TABS_PER_WEB_PROCESS && !busy)
        // Ties broken by the lower group id so the choice is deterministic
        // regardless of `HashMap` iteration order.
        .max_by_key(|(group, (size, _))| (*size, std::cmp::Reverse(*group)))
        .map(|(group, _)| group)
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
    /// Height of the bookmark bar row when it is visible (Issue #19, see
    /// docs/decisions.md D35) — added to `toolbar_height` the same way
    /// `panel_height` is, but independently of whether a panel is also
    /// open.
    bookmark_bar_height: u32,
    /// Which history/bookmarks panel is currently open, if any. Interior
    /// mutability is needed because `sync_layout` (called from the window
    /// resize handler, which only has `&BrowserWindow`) must account for it.
    open_panel: Cell<Option<Panel>>,
    /// Whether the bookmark bar is currently showing. Session-only — not
    /// persisted across restarts (see docs/decisions.md D35) — starting
    /// `false` so a fresh window never grows past `toolbar_height` before
    /// the user has asked for the bar. Interior mutability for the same
    /// `sync_layout` reason as `open_panel`.
    bookmark_bar_visible: Cell<bool>,
    /// Kept so panel-driven UI updates (`fetch_page_title`) can send
    /// [`UserEvent`]s back into the event loop after `new` has returned.
    proxy: EventLoopProxy<UserEvent>,
    contents: HashMap<TabId, ContentTab>,
    active: Option<TabId>,
    /// Next unused [`ContentTab::process_group`] id (docs/decisions.md
    /// D52). Only ever incremented; group ids are never reused.
    next_process_group: u64,
    /// The `WebContext` shared by the toolbar and every tab's content
    /// webview (see docs/decisions.md D49). `Some` only in non-private mode:
    /// `wry`'s WebKitGTK backend ignores any custom context passed via
    /// `attributes.context` once `.with_incognito(true)` is set — it always
    /// builds a fresh `WebContext::new_ephemeral()` per webview instead (see
    /// docs/decisions.md D15) — so there is nothing to share in private mode
    /// and this stays `None` there, leaving every private webview exactly as
    /// isolated as before this change. Every webview built after startup
    /// (`open_tab`, and `resume_tab` through it) borrows this mutably via
    /// [`new_webview_builder`], which is why it lives on `self` rather than
    /// only inside `new`.
    context: Option<WebContext>,
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

        // Shared across the toolbar and every content webview in non-private
        // mode only — see docs/decisions.md D49 and `BrowserWindow::context`'s
        // doc comment for why private mode gets `None` instead of a context
        // that `.with_incognito(true)` would just ignore anyway.
        let mut context = if config.private {
            None
        } else {
            Some(WebContext::new(None))
        };

        let ipc_proxy = proxy.clone();
        let toolbar_builder = new_webview_builder(context.as_mut())
            .with_bounds(to_bounds(toolbar_rect))
            .with_html(toolbar::TOOLBAR_HTML)
            // The toolbar now loads more than our own embedded HTML: a
            // tab's favicon is rendered as a plain `<img>` pointed at a
            // page-controlled URL (see docs/decisions.md D22), so in
            // private mode this webview must be just as ephemeral as every
            // content webview (docs/decisions.md D14/D15) — otherwise a
            // favicon fetch could persist cookies/cache private browsing is
            // supposed to leave no trace of.
            .with_incognito(config.private)
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
            WebviewIsolation {
                private: config.private,
                context: context.as_mut(),
                // The first content webview: nothing to relate to yet. It
                // starts process group 0, the first group later tabs can
                // join (D52).
                related: None,
            },
            Arc::clone(&blocklist),
            content_blocking_enabled,
        );
        let content = attach(content_builder)?;

        let mut contents = HashMap::new();
        contents.insert(
            initial_tab,
            ContentTab {
                webview: Some(content),
                process_group: 0,
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
            bookmark_bar_height: config.bookmark_bar_height,
            open_panel: Cell::new(None),
            bookmark_bar_visible: Cell::new(false),
            proxy,
            contents,
            active: Some(initial_tab),
            next_process_group: 1,
            context,
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
            self.bookmark_bar_height,
            self.bookmark_bar_visible.get(),
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
    ///
    /// `is_loading` answers, for an already-open tab, whether its page is
    /// still loading (`browser::Tab::is_loading`, owned by `app.rs`'s
    /// `Tabs`, which is why it is passed in rather than looked up here):
    /// this tab's webview is not put into a `WebKitWebProcess` that is busy
    /// loading another tab's page — see [`pick_process_group`] and
    /// docs/decisions.md D52.
    pub fn open_tab(
        &mut self,
        id: TabId,
        url: &str,
        is_loading: impl Fn(TabId) -> bool,
    ) -> wry::Result<()> {
        let (_, content_rect) = self.layout();
        // Which `WebKitWebProcess` to put this tab in (D52): join the
        // fullest idle group that still has room, through any live webview
        // of that group (they are all in the same process, so which one
        // does not matter); otherwise start a new group, which makes
        // WebKitGTK start a fresh web process for this tab.
        let live = self
            .contents
            .iter()
            .filter(|(_, tab)| tab.webview.is_some())
            .map(|(tab_id, tab)| (tab.process_group, is_loading(*tab_id)));
        let (process_group, related) = match pick_process_group(live) {
            Some(group) => {
                let related = self
                    .contents
                    .values()
                    .find(|tab| tab.process_group == group)
                    .and_then(|tab| tab.webview.as_ref());
                (group, related)
            }
            None => {
                let group = self.next_process_group;
                self.next_process_group += 1;
                (group, None)
            }
        };
        let builder = content_webview_builder(
            id,
            url,
            content_rect,
            &self.proxy,
            WebviewIsolation {
                private: self.private,
                context: self.context.as_mut(),
                related,
            },
            Arc::clone(&self.blocklist),
            self.content_blocking_enabled,
        )
        .with_visible(false);
        // Not `self.attach_webview(builder)`: `builder` may already hold a
        // `&mut` borrow of `self.context` (docs/decisions.md D49), and a
        // `&self` method call would borrow all of `self`, conflicting with
        // it. Passing the target field directly keeps the two borrows
        // disjoint — see `Self::attach_webview`'s doc comment.
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
        ))]
        let webview = Self::attach_webview(&self.host, builder)?;
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
        )))]
        let webview = Self::attach_webview(&self.window, builder)?;
        self.contents.insert(
            id,
            ContentTab {
                webview: Some(webview),
                process_group,
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
    ///
    /// `is_loading` is passed through to [`Self::open_tab`].
    pub fn resume_tab(
        &mut self,
        id: TabId,
        url: &str,
        is_loading: impl Fn(TabId) -> bool,
    ) -> wry::Result<()> {
        self.open_tab(id, url, is_loading)?;
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

    /// Focus the toolbar webview and force the address bar to show `url`,
    /// selected — Ctrl/Cmd+L (`FocusAddressBar`) and the Esc-restore step
    /// (`OmniboxClose`) in `app.rs` both call this. `self.toolbar.focus()`
    /// moves native/OS keyboard focus to the toolbar webview (needed when a
    /// content webview had it, e.g. Ctrl+L pressed while looking at a
    /// page); the address bar already having focus makes this a harmless
    /// no-op re-focus.
    pub fn focus_address_bar(&self, url: &str) -> wry::Result<()> {
        self.toolbar.focus()?;
        self.toolbar
            .evaluate_script(&toolbar::set_focus_address_bar_script(url))
    }

    /// Replace the omnibox candidate dropdown's contents (Issue #15). Does
    /// not itself open/close `Panel::Omnibox` — the caller (`app.rs`)
    /// decides that from whether `candidates` is empty, via `set_panel`.
    pub fn set_candidates(&self, candidates: &[Candidate]) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_candidates_script(candidates))
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

    /// Replace the history panel's contents, grouped into date sections
    /// (今日/昨日/過去7日/それ以前 — see docs/decisions.md D29) relative to
    /// `now` (unix seconds). `entries` is expected newest-first, e.g. from
    /// `HistoryStore::entries_newest_first` or `HistoryStore::search`.
    pub fn set_history(&self, entries: &[&HistoryEntry], now: u64) -> wry::Result<()> {
        let groups = group_by_date(entries.iter().copied(), now);
        self.toolbar
            .evaluate_script(&toolbar::set_history_script(&groups))
    }

    /// Replace the bookmarks panel's contents (folders and their entries).
    pub fn set_bookmarks(&self, view: &toolbar::BookmarksView<'_>) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_bookmarks_script(view))
    }

    /// Replace the always-visible bookmark bar's contents (Issue #19, see
    /// docs/decisions.md D35). Same underlying data as [`Self::set_bookmarks`]
    /// — `app.rs` builds one [`toolbar::BookmarksView`] per refresh and
    /// pushes it to both.
    pub fn set_bookmark_bar(&self, view: &toolbar::BookmarksView<'_>) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_bookmark_bar_script(view))
    }

    /// Whether the bookmark bar is currently showing.
    pub fn bookmark_bar_visible(&self) -> bool {
        self.bookmark_bar_visible.get()
    }

    /// Show or hide the bookmark bar: resizes the toolbar webview to make
    /// (or reclaim) room, the same way [`Self::set_panel`] does for a
    /// dropdown panel, and updates the toolbar's DOM to match. See
    /// docs/decisions.md D35 for why this is independent of `set_panel`
    /// rather than another [`Panel`] variant.
    pub fn set_bookmark_bar_visible(&self, visible: bool) -> wry::Result<()> {
        self.bookmark_bar_visible.set(visible);
        self.sync_layout()?;
        self.toolbar
            .evaluate_script(&toolbar::set_bookmark_bar_visible_script(visible))
    }

    /// Replace the downloads panel's contents (Issue #16, see
    /// docs/decisions.md D28).
    pub fn set_downloads(&self, entries: &[&DownloadEntry]) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_downloads_script(entries))
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

    /// Asynchronously resolve tab `tab_id`'s favicon URL (see
    /// [`RESOLVE_FAVICON_SCRIPT`]) and report it back as
    /// [`UserEvent::FaviconResolved`] for the history entry `history_id` (as
    /// well as the tab strip) — `0` is a safe sentinel for "no history
    /// entry" the same way [`Self::fetch_page_title`] uses it, since
    /// `HistoryStore` ids start at 1. `page_url` is the page this favicon
    /// belongs to (its URL as of *this* load, captured by the caller before
    /// the fetch starts) — carried through to
    /// [`UserEvent::FaviconResolved`] so `app.rs` can also update a
    /// bookmark's favicon (`BookmarkStore::update_favicon_by_url`, keyed by
    /// URL, not id — see docs/decisions.md D34) without needing to re-read
    /// the tab's (possibly by-then-stale, if the tab navigated again in the
    /// meantime) current URL.
    ///
    /// Same shape and same reasoning as [`Self::fetch_page_title`] (see
    /// docs/decisions.md D12/D22/D27): a no-op for an unknown or suspended
    /// tab, fire-and-forget (a superseded navigation just means a stale
    /// answer gets applied late), and this only ever resolves a URL string —
    /// the actual favicon image fetch happens later, asynchronously, as a
    /// plain `<img src>` load in the toolbar webview, never here.
    pub fn fetch_favicon(
        &self,
        tab_id: TabId,
        history_id: u64,
        page_url: String,
    ) -> wry::Result<()> {
        let webview = match self
            .contents
            .get(&tab_id)
            .and_then(|tab| tab.webview.as_ref())
        {
            Some(webview) => webview,
            None => return Ok(()),
        };
        let proxy = self.proxy.clone();
        webview.evaluate_script_with_callback(RESOLVE_FAVICON_SCRIPT, move |raw| {
            if let Some(url) = extract_js_string_result(&raw) {
                if !url.is_empty() {
                    let _ = proxy.send_event(UserEvent::FaviconResolved {
                        tab_id,
                        history_id,
                        page_url: page_url.clone(),
                        url,
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

/// A webview's private-browsing/`WebContext` isolation settings, bundled so
/// [`content_webview_builder`] stays under clippy's argument-count lint
/// (docs/decisions.md D49 added the `context` field; `private` moved in
/// alongside it since the two are directly related — see this struct's
/// field docs).
struct WebviewIsolation<'a> {
    /// Whole-app private browsing (see docs/decisions.md D14).
    private: bool,
    /// The `WebContext` to build this webview against when `private` is
    /// `false` (docs/decisions.md D49). Ignored — not even read — when
    /// `private` is `true`: `.with_incognito(true)` makes `wry`'s WebKitGTK
    /// backend build a fresh ephemeral context per webview regardless of
    /// what is passed here (docs/decisions.md D15), so a private webview's
    /// builder is constructed with `context: None` in the first place (see
    /// `BrowserWindow::context`'s doc comment) rather than relying on that
    /// downstream behavior to discard a real one.
    context: Option<&'a mut WebContext>,
    /// An already-alive content webview whose `WebKitWebProcess` this one
    /// should join (docs/decisions.md D52, see [`with_related_content_view`]).
    /// `None` for the very first content webview (there is nothing to join
    /// yet) and always `None` when `private` is `true`: a private webview
    /// goes through wry's `.with_incognito(true)` path, which builds its
    /// own ephemeral `WebContext` per webview (D15) — relating it to
    /// another view would make WebKitGTK take the *related* view's context
    /// instead, silently changing what "private" isolates, so the private
    /// process layout stays exactly as it was before D52 (see
    /// `docs/memory-analysis.md` §9.4/§10).
    related: Option<&'a WebView>,
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
    isolation: WebviewIsolation<'a>,
    blocklist: Arc<FilterList>,
    content_blocking_enabled: bool,
) -> WebViewBuilder<'a> {
    let WebviewIsolation {
        private,
        context,
        related,
    } = isolation;
    // Never relate a private webview (see `WebviewIsolation::related`).
    let related = if private { None } else { related };
    let nav_proxy = proxy.clone();
    let block_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let devtools_proxy = proxy.clone();
    let new_window_proxy = proxy.clone();
    let download_started_proxy = proxy.clone();
    let download_completed_proxy = proxy.clone();
    with_related_content_view(new_webview_builder(context), related)
        .with_bounds(to_bounds(content_rect))
        .with_url(url)
        // Ephemeral (non-persistent) cookies/storage/cache for the page
        // content itself; see docs/decisions.md D15 for the per-platform
        // backing (WebKitGTK ephemeral WebContext / WKWebsiteDataStore
        // nonPersistentDataStore / WebView2 private-mode controller option).
        // The toolbar webview now gets the same treatment for the same
        // reason (see the `with_incognito` call on `toolbar_builder` in
        // `BrowserWindow::new`) since #11's favicon rendering gave it its
        // own path to page-controlled URLs. Every tab's webview goes through
        // this builder, so tabs opened later - and suspended tabs rebuilt on
        // resume - stay ephemeral too.
        .with_incognito(private)
        // DevTools (see docs/decisions.md D18): every content webview built
        // through this one function — the initial tab, a newly opened tab,
        // and a suspended tab rebuilt on resume — gets the inspector enabled,
        // the F12/Cmd+Opt+I capture script injected, and its own untrusted
        // devtools IPC channel, so the shortcut keeps working no matter when
        // or how the webview came to exist.
        .with_devtools(true)
        .with_initialization_script(devtools_shortcut_script())
        // Tab-management keyboard shortcuts (see docs/decisions.md D23):
        // same treatment, same trust boundary, same untrusted IPC channel
        // below — just a second injected script and a second fixed set of
        // sentinel strings, rather than growing the devtools one to mean two
        // different things.
        .with_initialization_script(tab_shortcut_script())
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
            // Untrusted content-webview IPC channel (see OPEN_DEVTOOLS_MESSAGE
            // and ContentShortcut's doc comment): every branch here is either
            // one fixed exact-match string comparison or a lookup into a
            // fixed, closed set of them — never JSON parsing, never anything
            // page-supplied treated as structured data.
            let body = request.body().as_str();
            if body == OPEN_DEVTOOLS_MESSAGE {
                let _ = devtools_proxy.send_event(UserEvent::OpenDevtoolsRequested);
            } else if let Some(shortcut) = parse_content_shortcut(body) {
                let _ = devtools_proxy.send_event(UserEvent::ContentShortcut(shortcut));
            }
        })
        // `target="_blank"` links and `window.open()` (see docs/decisions.md
        // D25): wry's `with_new_window_req_handler` fires synchronously with
        // the requested URL on every backend (WebKitGTK's `create` signal,
        // WebView2's `NewWindowRequested`, WKWebView's
        // `createWebViewWithConfiguration:...`). We always `Deny` — never
        // `Allow` (a bare native window outside VeloX's tab model) or
        // `Create` (would need a platform-specific webview sharing the
        // opener's configuration; see D25) — and instead open the URL as a
        // new VeloX tab ourselves, the same way `ToolbarCommand::NewTab`
        // does but at the requested URL instead of the homepage.
        .with_new_window_req_handler(move |url, _features| {
            let _ = new_window_proxy.send_event(UserEvent::NewTabRequested(url));
            wry::NewWindowResponse::Deny
        })
        // Downloads (Issue #16, see docs/decisions.md D28): both handlers
        // fire on every backend VeloX ships on (confirmed from wry 0.56.1's
        // source — see D28). `with_download_started_handler` must decide
        // synchronously (return `bool`) and may rewrite the destination
        // `PathBuf` in place, so the destination resolution itself
        // (`browser::downloads::prepare_destination`, which sanitizes the
        // suggested file name and avoids same-name collisions) has to run
        // right here, not after a round trip through the event loop — the
        // same synchronous-decision-then-async-notify shape
        // `with_navigation_handler` above already uses for content
        // blocking. VeloX always accepts every download (`true`, matching
        // wry's own default), so this only ever *redirects* a download,
        // never blocks one.
        .with_download_started_handler(move |url, destination| {
            let suggested_name = destination
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Prefer VeloX's own directory resolution (respects
            // `VELOX_DOWNLOAD_DIR`, see docs/decisions.md D28) over
            // whatever default wry already picked; fall back to wry's own
            // suggested directory only if no environment variable could be
            // resolved at all, rather than failing the download outright.
            let dir = downloads::resolve_download_dir()
                .or_else(|| destination.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."));
            let final_path = match downloads::prepare_destination(&dir, &suggested_name) {
                Ok(path) => path,
                Err(err) => {
                    eprintln!(
                        "velox: failed to prepare download destination in {dir:?}: {err}; \
                         refusing this download"
                    );
                    return false;
                }
            };
            let file_name = final_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(suggested_name);
            *destination = final_path.clone();
            let _ = download_started_proxy.send_event(UserEvent::DownloadStarted {
                url,
                file_name,
                destination: final_path,
                started_at: unix_now(),
            });
            true
        })
        .with_download_completed_handler(move |url, path, success| {
            let _ = download_completed_proxy.send_event(UserEvent::DownloadCompleted {
                url,
                path,
                success,
            });
        })
}

/// Current time as a unix timestamp (seconds); `0` on a clock set before
/// 1970, which should never happen in practice. A separate copy of
/// `app::now_unix` (private there) — `ui::window` needs a timestamp at the
/// moment a download is accepted, inside a wry callback that has no access
/// to `app.rs`'s state, so duplicating this trivial conversion is simpler
/// than threading a clock dependency through.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    ///
    /// Takes `host` explicitly rather than `&self` — see the call site in
    /// [`Self::open_tab`] — because `builder` may already hold a `&mut`
    /// borrow of `self.context` (docs/decisions.md D49); a `&self` method
    /// here would borrow the whole struct and conflict with that, whereas a
    /// disjoint `&self.host` argument does not.
    fn attach_webview(host: &gtk::Fixed, builder: WebViewBuilder<'_>) -> wry::Result<WebView> {
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(host)
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
    /// See the Linux/BSD `attach_webview` above for why this takes `window`
    /// explicitly instead of `&self`.
    fn attach_webview(window: &Window, builder: WebViewBuilder<'_>) -> wry::Result<WebView> {
        builder.build_as_child(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_group_starts_fresh_when_nothing_is_live() {
        assert_eq!(pick_process_group([]), None);
    }

    #[test]
    fn process_group_joins_the_fullest_idle_group_with_room() {
        // Group 1 has 2 live tabs, group 0 has 1: fill group 1 first.
        assert_eq!(
            pick_process_group([(0, false), (1, false), (1, false)]),
            Some(1)
        );
        // Ties go to the lower id, deterministically.
        assert_eq!(pick_process_group([(3, false), (2, false)]), Some(2));
    }

    #[test]
    fn process_group_starts_fresh_once_every_group_is_full() {
        let full: Vec<(u64, bool)> = vec![(0, false); MAX_TABS_PER_WEB_PROCESS];
        assert_eq!(pick_process_group(full.iter().copied()), None);
        // A full group is skipped in favor of one with room, however small.
        let mut mixed = full;
        mixed.push((7, false));
        assert_eq!(pick_process_group(mixed), Some(7));
    }

    #[test]
    fn process_group_never_joins_a_group_that_is_still_loading() {
        // One loading tab makes its whole group busy, however much room
        // it has; with no other group, start fresh (parallel loads).
        assert_eq!(pick_process_group([(0, true), (0, false)]), None);
        // A smaller idle group beats a bigger busy one.
        assert_eq!(
            pick_process_group([(0, true), (0, false), (1, false)]),
            Some(1)
        );
    }

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
        assert_eq!(effective_toolbar_height(48, 320, false, 30, false), 48);
        assert_eq!(effective_toolbar_height(48, 320, true, 30, false), 368);
    }

    #[test]
    fn effective_height_adds_bookmark_bar_height_only_when_visible() {
        assert_eq!(effective_toolbar_height(48, 320, false, 30, true), 78);
        assert_eq!(effective_toolbar_height(48, 320, false, 30, false), 48);
    }

    #[test]
    fn effective_height_sums_bar_and_panel_when_both_are_showing() {
        // The bar and a panel are independent, additive components (see
        // docs/decisions.md D35) — not alternatives like `set_panel`'s own
        // variants are.
        assert_eq!(effective_toolbar_height(48, 320, true, 30, true), 398);
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

    #[test]
    fn tab_shortcut_script_captures_expected_combos_in_capture_phase() {
        let script = tab_shortcut_script();
        for message in [
            NEW_TAB_MESSAGE,
            CLOSE_TAB_MESSAGE,
            REOPEN_CLOSED_TAB_MESSAGE,
            NEXT_TAB_MESSAGE,
            PREV_TAB_MESSAGE,
            ACTIVATE_LAST_TAB_MESSAGE,
            FOCUS_ADDRESS_BAR_MESSAGE,
            TOGGLE_BOOKMARK_MESSAGE,
            TOGGLE_BOOKMARK_BAR_MESSAGE,
        ] {
            assert!(
                script.contains(message),
                "script is missing sentinel {message:?}"
            );
        }
        assert!(script.contains(ACTIVATE_TAB_MESSAGE_PREFIX));
        assert!(script.contains("event.ctrlKey || event.metaKey"));
        assert!(script.contains("}, true);"));
    }

    #[test]
    fn parse_content_shortcut_matches_every_sentinel_exactly() {
        assert_eq!(
            parse_content_shortcut(NEW_TAB_MESSAGE),
            Some(ContentShortcut::NewTab)
        );
        assert_eq!(
            parse_content_shortcut(CLOSE_TAB_MESSAGE),
            Some(ContentShortcut::CloseTab)
        );
        assert_eq!(
            parse_content_shortcut(REOPEN_CLOSED_TAB_MESSAGE),
            Some(ContentShortcut::ReopenClosedTab)
        );
        assert_eq!(
            parse_content_shortcut(NEXT_TAB_MESSAGE),
            Some(ContentShortcut::NextTab)
        );
        assert_eq!(
            parse_content_shortcut(PREV_TAB_MESSAGE),
            Some(ContentShortcut::PrevTab)
        );
        assert_eq!(
            parse_content_shortcut(ACTIVATE_LAST_TAB_MESSAGE),
            Some(ContentShortcut::ActivateLastTab)
        );
        assert_eq!(
            parse_content_shortcut(FOCUS_ADDRESS_BAR_MESSAGE),
            Some(ContentShortcut::FocusAddressBar)
        );
        assert_eq!(
            parse_content_shortcut(TOGGLE_BOOKMARK_MESSAGE),
            Some(ContentShortcut::ToggleBookmark)
        );
        assert_eq!(
            parse_content_shortcut(TOGGLE_BOOKMARK_BAR_MESSAGE),
            Some(ContentShortcut::ToggleBookmarkBar)
        );
        for n in 1u8..=8 {
            assert_eq!(
                parse_content_shortcut(&format!("velox:activate-tab-{n}")),
                Some(ContentShortcut::ActivateTabAt(n))
            );
        }
    }

    #[test]
    fn parse_content_shortcut_rejects_anything_not_an_exact_known_sentinel() {
        // Never treated as JSON/structured data, and never a prefix/fuzzy
        // match — see docs/decisions.md D18/D23.
        for body in [
            "",
            "velox:new-tab ",
            "VELOX:NEW-TAB",
            "velox:activate-tab-0",
            "velox:activate-tab-9",
            "velox:activate-tab-99",
            "velox:activate-tab-",
            r#"{"cmd":"new_tab"}"#,
            "velox:open-devtools",
        ] {
            assert_eq!(parse_content_shortcut(body), None, "body was {body:?}");
        }
    }

    #[test]
    fn favicon_script_falls_back_to_a_same_origin_guess() {
        assert!(RESOLVE_FAVICON_SCRIPT.contains("link[rel~=\"icon\"]"));
        assert!(RESOLVE_FAVICON_SCRIPT.contains("/favicon.ico"));
    }
}
