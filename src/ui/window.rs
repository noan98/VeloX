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
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Icon, Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{
    PageLoadEvent, PermissionKind as WryPermissionKind, PermissionResponse, Rect, WebContext,
    WebView, WebViewBuilder,
};

use crate::app::UserEvent;
use crate::browser::downloads;
use crate::browser::site_permissions::{self, PermissionKind, Resolution, SitePermissionStore};
use crate::browser::{
    group_by_date, Candidate, DownloadEntry, FilterList, HistoryEntry, SiteExceptions, TabId,
};
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
/// Ctrl/Cmd+F (Issue #43): open the in-page find bar. See
/// `ContentShortcut::OpenFindBar` and docs/decisions.md D69.
const OPEN_FIND_BAR_MESSAGE: &str = "velox:open-find-bar";
/// Ctrl/Cmd+U (Issue #45): view the active tab's page source. See
/// `ContentShortcut::ViewSource` and docs/decisions.md D72.
const VIEW_SOURCE_MESSAGE: &str = "velox:view-source";

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
    /// Ctrl/Cmd+F (Issue #43): open the in-page find bar. The
    /// content-webview half of `ui::toolbar::ToolbarCommand::OpenFindBar` —
    /// both are handled by the same shared function in `app.rs`. See
    /// docs/decisions.md D69.
    OpenFindBar,
    /// Ctrl/Cmd+U (Issue #45): view the active tab's page source. The
    /// content-webview half of `ui::toolbar::ToolbarCommand::ViewSource` —
    /// both are handled by the same shared function in `app.rs`. See
    /// docs/decisions.md D72.
    ViewSource,
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
        OPEN_FIND_BAR_MESSAGE => Some(ContentShortcut::OpenFindBar),
        VIEW_SOURCE_MESSAGE => Some(ContentShortcut::ViewSource),
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
      }} else if (event.key === "f" || event.key === "F") {{
        message = "{OPEN_FIND_BAR_MESSAGE}";
      }} else if (event.key === "u" || event.key === "U") {{
        message = "{VIEW_SOURCE_MESSAGE}";
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

/// The VeloX logo, embedded at build time so the binary needs no asset
/// directory next to it at runtime. 128x128 is plenty: every platform scales
/// the window icon down (title bar / taskbar / alt-tab), never up.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../assets/icon/velox-128.png");

/// Decodes [`WINDOW_ICON_PNG`] into the RGBA buffer `tao` wants for the
/// title-bar/taskbar icon. Returns `None` (and logs to stderr) instead of
/// failing window creation if the embedded PNG cannot be decoded: a missing
/// icon is cosmetic and must never keep the browser from starting. See
/// docs/decisions.md D52 for why the window icon is set here at runtime
/// while the `.exe` resource icon is embedded by `build.rs`.
fn load_window_icon() -> Option<Icon> {
    match decode_window_icon() {
        Ok(icon) => Some(icon),
        Err(err) => {
            eprintln!("velox: failed to load the window icon: {err}");
            None
        }
    }
}

fn decode_window_icon() -> Result<Icon, Box<dyn std::error::Error>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(WINDOW_ICON_PNG));
    // Normalize whatever the asset happens to be (palette, 16-bit, no alpha)
    // to the 8-bit RGBA layout `Icon::from_rgba` requires.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let size = reader
        .output_buffer_size()
        .ok_or("icon dimensions overflow the output buffer size")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf)?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "unexpected pixel format {:?}/{:?} (want 8-bit RGBA)",
            info.color_type, info.bit_depth
        )
        .into());
    }
    buf.truncate(info.buffer_size());
    Ok(Icon::from_rgba(buf, info.width, info.height)?)
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
///
/// The find bar (Issue #43, docs/decisions.md D69) is a *fourth* such
/// independent, additive row, for the same reason the bookmark bar is: it
/// is a single compact strip, not a `Panel`-sized dropdown, and there is no
/// reason it could not stay open while a panel or the bookmark bar is also
/// showing.
fn effective_toolbar_height(
    toolbar_height: u32,
    panel_height: u32,
    panel_open: bool,
    bookmark_bar_height: u32,
    bookmark_bar_visible: bool,
    find_bar_height: u32,
    find_bar_visible: bool,
) -> u32 {
    let mut height = toolbar_height;
    if bookmark_bar_visible {
        height = height.saturating_add(bookmark_bar_height);
    }
    if panel_open {
        height = height.saturating_add(panel_height);
    }
    if find_bar_visible {
        height = height.saturating_add(find_bar_height);
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
/// same `WebKitWebProcess` as `related` (docs/decisions.md D54).
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

/// WebKitGTK's `is-playing-audio` for `webview` — see
/// [`BrowserWindow::is_playing_audio`]. Guarded by `has_property` so a
/// WebKitGTK build without the property (it has existed since 2.8, so this
/// is purely defensive) reads as "not playing" instead of a GLib panic.
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
))]
fn webview_is_playing_audio(webview: &WebView) -> bool {
    use gtk::glib::prelude::*;
    use wry::WebViewExtUnix;
    let inner = webview.webview();
    inner.has_property("is-playing-audio", Some(bool::static_type()))
        && inner.property::<bool>("is-playing-audio")
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
)))]
fn webview_is_playing_audio(_webview: &WebView) -> bool {
    false
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
    /// decisions.md D54). Tabs built with `related` pointing at a tab in
    /// group `g` join group `g`; a tab built with no related view starts a
    /// new group. Only meaningful on WebKitGTK (elsewhere the id is
    /// assigned but never influences anything) and only while `webview`
    /// is `Some` — a suspended tab has left its process, so it does not
    /// count towards the group's size.
    process_group: u64,
}

/// Upper bound on how many content webviews are put into one
/// `WebKitWebProcess` (docs/decisions.md D54).
///
/// Sharing *every* tab through one process (the first cut of D54) cut PSS
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
/// Default cap, kept as a named constant for the unit tests below; the
/// running value comes from `Config::max_tabs_per_web_process`
/// (`VELOX_MAX_TABS_PER_PROCESS`, Issue #60 / D57) so the trade-off can be
/// measured without rebuilding.
#[cfg(test)]
const MAX_TABS_PER_WEB_PROCESS: usize = 4;

/// Pick the process group a new tab should join, given `(group, loading)`
/// for every tab that currently has a live webview: the fullest group that
/// still has room under [`MAX_TABS_PER_WEB_PROCESS`] *and* has no tab
/// currently loading a page (so processes fill up before a new one is
/// started, but a burst of tabs opened back-to-back — each still loading
/// when the next one is opened — fans out over fresh processes and loads
/// in parallel, exactly as it did before D54). `None` when no such group
/// exists, meaning the tab should start a fresh process/group.
///
/// Pure so it can be unit-tested without a display; the caller maps the
/// chosen group back to one of its live webviews.
fn pick_process_group(
    live_tabs: impl IntoIterator<Item = (u64, bool)>,
    max_tabs_per_process: usize,
) -> Option<u64> {
    // (size, has a loading tab) per group.
    let mut groups: HashMap<u64, (usize, bool)> = HashMap::new();
    for (group, loading) in live_tabs {
        let entry = groups.entry(group).or_insert((0, false));
        entry.0 += 1;
        entry.1 |= loading;
    }
    groups
        .into_iter()
        .filter(|(_, (size, busy))| *size < max_tabs_per_process.max(1) && !busy)
        // Ties broken by the lower group id so the choice is deterministic
        // regardless of `HashMap` iteration order.
        .max_by_key(|(group, (size, _))| (*size, std::cmp::Reverse(*group)))
        .map(|(group, _)| group)
}

/// The site-scoped stores [`BrowserWindow::new`] needs, bundled into one
/// argument.
///
/// These three arrived from three separate issues (#17/#21 の `blocklist`、
/// #22 の `site_exceptions`、#24 の `site_permissions`) and were originally
/// three separate parameters. Together with `initial_url` (#25) that pushed
/// `new` past clippy's `too_many_arguments` limit, and the three are always
/// passed together anyway — every one of them is a policy that must apply
/// identically to every tab, whenever and however its webview comes into
/// existence. Bundling them is the same move [`ContentPolicy`] makes one
/// layer down for [`content_webview_builder`]; this type is the public,
/// `app.rs`-facing half of it (it deliberately does *not* carry
/// `content_blocking_enabled`, which `new` derives from `Config` itself).
pub struct SitePolicies {
    /// Ad/tracker filter rules content blocking matches against
    /// (docs/decisions.md D17).
    pub blocklist: Arc<FilterList>,
    /// Per-site content-blocking exceptions (Issue #22, D59).
    pub site_exceptions: Arc<SiteExceptions>,
    /// Per-origin permission decisions (Issue #24, D60).
    pub site_permissions: Arc<SitePermissionStore>,
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
    /// Height of the find bar row when it is visible (Issue #43, see
    /// docs/decisions.md D69) — added to `toolbar_height` the same
    /// independent, additive way `bookmark_bar_height` is.
    find_bar_height: u32,
    /// Whether the in-page find bar is currently showing. Session-only, the
    /// same reasoning as `bookmark_bar_visible`: starts `false` so a fresh
    /// window never grows past `toolbar_height` before Ctrl/Cmd+F is
    /// pressed. Interior mutability for the same `sync_layout` reason as
    /// `bookmark_bar_visible`/`open_panel`.
    find_bar_visible: Cell<bool>,
    /// Kept so panel-driven UI updates (`fetch_page_title`) can send
    /// [`UserEvent`]s back into the event loop after `new` has returned.
    proxy: EventLoopProxy<UserEvent>,
    contents: HashMap<TabId, ContentTab>,
    active: Option<TabId>,
    /// Next unused [`ContentTab::process_group`] id (docs/decisions.md
    /// D54). Only ever incremented; group ids are never reused.
    next_process_group: u64,
    /// How many tabs may share one `WebKitWebProcess`
    /// ([`pick_process_group`], D54/D57) — `Config::max_tabs_per_web_process`
    /// as of window creation.
    max_tabs_per_web_process: usize,
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
    /// Per-site content-blocking exceptions (Issue #22, D59). Kept
    /// alongside `blocklist` for the same reason: every tab's content
    /// webview, however and whenever it comes into existence, must see the
    /// same exception list. Consulted only by the Windows/WebView2
    /// subresource-blocking hook today (`ui::webview2_blocking`); main-frame
    /// navigation blocking does not use it yet (D17's `with_navigation_handler`
    /// predates this field). Kept unconditionally (not `#[cfg(windows)]`,
    /// unlike `host`) so `BrowserWindow::new`'s signature — and therefore
    /// `app.rs`, which is otherwise OS-agnostic — does not need a
    /// platform-specific branch just to build this struct; `#[allow(dead_code)]`
    /// on macOS/Linux is the cheaper cost of the two.
    #[cfg_attr(not(windows), allow(dead_code))]
    site_exceptions: Arc<SiteExceptions>,
    /// Per-origin permission decisions (Issue #24, docs/decisions.md D60).
    /// Kept for the same reason as `blocklist`: every tab's content
    /// webview, however and whenever it comes into existence, is built
    /// through [`content_webview_builder`] with the same store.
    site_permissions: Arc<SitePermissionStore>,
    /// `Config::download_dir_override` as of window creation (Issue #30's
    /// settings screen, see docs/decisions.md D67). Kept alongside
    /// `blocklist`/`site_permissions` for the same reason: every tab opened
    /// later must build its download handlers with the same override.
    download_dir_override: Option<String>,
}

/// Result of [`BrowserWindow::clear_all_site_data`]: how many webviews were
/// asked to clear their site data and how many of those returned an error.
/// `first_error` is kept only for the stderr message (`app.rs`'s
/// `log_failure` pattern) — the pass/fail judgement itself is
/// [`crate::browser::site_data::summarize`] (docs/decisions.md D66), which
/// takes only `attempted`/`failed` and stays engine-agnostic.
#[derive(Debug)]
pub struct SiteDataClearResult {
    pub attempted: usize,
    pub failed: usize,
    pub first_error: Option<wry::Error>,
}

impl BrowserWindow {
    /// Create the window, the toolbar webview, and the first tab's content
    /// webview (bound to `initial_tab`, loading `initial_url`).
    ///
    /// `initial_url` is *not* always `config.homepage`: session restore
    /// (Issue #25, see docs/decisions.md D65) builds `Tabs` with the
    /// previously active tab's own `current_url` before `BrowserWindow` is
    /// ever constructed, and that — not the configured homepage — is what
    /// the first real webview must load. The ordinary (non-restored) case
    /// still passes `config.homepage` here, since that is exactly what
    /// `Tabs::new(config.homepage.clone())` set as the same tab's
    /// `current_url` too.
    ///
    /// `policies` carries the site-scoped stores every tab must share (see
    /// [`SitePolicies`]). They are stored on `self` so every tab opened
    /// later — or rebuilt on resume from suspension — is built through the
    /// same [`content_webview_builder`] with the same policies as the first
    /// tab.
    pub fn new(
        event_loop: &EventLoopWindowTarget<UserEvent>,
        config: &Config,
        proxy: EventLoopProxy<UserEvent>,
        initial_tab: TabId,
        initial_url: &str,
        policies: SitePolicies,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let SitePolicies {
            blocklist,
            site_exceptions,
            site_permissions,
        } = policies;
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
            .with_window_icon(load_window_icon())
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
        // Downloads (docs/decisions.md D53): on WebKitGTK, wry registers a
        // webview's download handlers on the `WebContext` it is built
        // against, not on the webview — so with the shared `context` above
        // they must be registered exactly once, on the first webview built
        // against it (this toolbar), and never again on the content
        // webviews. See `download_handler_host` for the full reasoning and
        // for why the toolbar in particular (wry's default per-webview
        // "accept" handler would otherwise win the `decide-destination`
        // signal and VeloX's handler would never run).
        let download_dir_override = config.download_dir_override.clone();
        let toolbar_builder =
            match download_handler_host(config.private, DOWNLOAD_HANDLERS_PER_CONTEXT) {
                DownloadHandlerHost::SharedContext => {
                    with_download_handlers(toolbar_builder, &proxy, download_dir_override.clone())
                }
                DownloadHandlerHost::EachContentWebview => toolbar_builder,
            };
        let toolbar = attach(toolbar_builder)?;

        let content_blocking_enabled = config.content_blocking_enabled;
        let content_builder = content_webview_builder(
            initial_tab,
            initial_url,
            content_rect,
            &proxy,
            WebviewIsolation {
                private: config.private,
                context: context.as_mut(),
                // The first content webview: nothing to relate to yet. It
                // starts process group 0, the first group later tabs can
                // join (D54).
                related: None,
            },
            ContentPolicy {
                blocklist: Arc::clone(&blocklist),
                content_blocking_enabled,
                site_permissions: Arc::clone(&site_permissions),
                download_dir_override: download_dir_override.clone(),
            },
        );
        let content = attach(content_builder)?;
        // Windows-only subresource blocking (Issue #22, D59): a no-op on
        // every other platform (`attach` compiles away entirely there — see
        // its doc comment). Attached after `attach()` because it needs the
        // *built* `wry::WebView` (specifically `WebViewExtWindows::webview`)
        // to reach the raw `ICoreWebView2`, which does not exist yet on a
        // bare `WebViewBuilder`.
        #[cfg(windows)]
        crate::ui::webview2_blocking::attach(
            &content,
            initial_tab,
            Arc::clone(&blocklist),
            Arc::clone(&site_exceptions),
            content_blocking_enabled,
            proxy.clone(),
        );

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
            find_bar_height: config.find_bar_height,
            find_bar_visible: Cell::new(false),
            proxy,
            contents,
            active: Some(initial_tab),
            next_process_group: 1,
            max_tabs_per_web_process: config.max_tabs_per_web_process,
            context,
            private: config.private,
            blocklist,
            content_blocking_enabled,
            site_exceptions,
            site_permissions,
            download_dir_override,
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
            self.find_bar_height,
            self.find_bar_visible.get(),
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
    /// docs/decisions.md D54.
    pub fn open_tab(
        &mut self,
        id: TabId,
        url: &str,
        is_loading: impl Fn(TabId) -> bool,
    ) -> wry::Result<()> {
        let (_, content_rect) = self.layout();
        // Which `WebKitWebProcess` to put this tab in (D54): join the
        // fullest idle group that still has room, through any live webview
        // of that group (they are all in the same process, so which one
        // does not matter); otherwise start a new group, which makes
        // WebKitGTK start a fresh web process for this tab.
        let live = self
            .contents
            .iter()
            .filter(|(_, tab)| tab.webview.is_some())
            .map(|(tab_id, tab)| (tab.process_group, is_loading(*tab_id)));
        let (process_group, related) = match pick_process_group(live, self.max_tabs_per_web_process)
        {
            Some(group) => {
                // Only a tab that still *has* a webview can be related to
                // (a suspended tab's entry keeps its stale `process_group`
                // with `webview: None`). Matching on the group alone here
                // used to pick such an entry first, yielding `related:
                // None` — a fresh `WebKitWebProcess` wearing an existing
                // group id, so every later tab "joining" that group also
                // got its own process (found by the `VELOX_DEBUG` trace
                // below; see docs/decisions.md D56).
                let related = self
                    .contents
                    .values()
                    .find(|tab| tab.process_group == group && tab.webview.is_some())
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
            ContentPolicy {
                blocklist: Arc::clone(&self.blocklist),
                content_blocking_enabled: self.content_blocking_enabled,
                site_permissions: Arc::clone(&self.site_permissions),
                download_dir_override: self.download_dir_override.clone(),
            },
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
        // Same Windows-only hook as `BrowserWindow::new` — see its call
        // site's doc comment. Covers every tab opened after startup and
        // every tab rebuilt on resume from suspension (`resume_tab` reuses
        // this function), so a resumed tab does not silently lose
        // subresource blocking.
        #[cfg(windows)]
        crate::ui::webview2_blocking::attach(
            &webview,
            id,
            Arc::clone(&self.blocklist),
            Arc::clone(&self.site_exceptions),
            self.content_blocking_enabled,
            self.proxy.clone(),
        );
        if std::env::var_os("VELOX_DEBUG").is_some() {
            // Which `WebKitWebProcess` group this tab landed in (D54) —
            // the one piece of placement state nothing else surfaces, and
            // exactly what a memory investigation (D48/D54/D56) needs to
            // see. Same opt-in as `app.rs`'s event tracing.
            eprintln!(
                "velox[debug]: tab {id:?} -> process group {process_group} (related: {})",
                related.is_some()
            );
        }
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

    /// Whether tab `id`'s page is currently playing audio, for the
    /// automatic suspension policy's "active media" protection
    /// (`browser::suspension`, Issue #63) — a tab the user is listening
    /// to is never suspended automatically. `false` for a suspended or
    /// unknown tab (nothing to protect).
    ///
    /// Read from WebKitGTK's `WebKitWebView:is-playing-audio` property via
    /// the `webkit2gtk::WebView` wry already hands out
    /// (`WebViewExtUnix::webview`, the same accessor
    /// [`with_related_content_view`] uses) — through GLib's generic
    /// property API rather than the `webkit2gtk` crate's typed getter, so
    /// no new dependency is needed (docs/decisions.md D6). On every other
    /// platform this is always `false`: wry exposes no equivalent there
    /// yet, so the protection simply does not apply (documented in D56).
    pub fn is_playing_audio(&self, id: TabId) -> bool {
        self.contents
            .get(&id)
            .and_then(|tab| tab.webview.as_ref())
            .is_some_and(webview_is_playing_audio)
    }

    /// Which `WebKitWebProcess` group (D54, [`pick_process_group`]) tab
    /// `id`'s live webview is in, for the process-unit reclaim order of the
    /// automatic suspension policy (`browser::suspension::reclaim_order`,
    /// docs/decisions.md D56). `None` for a suspended or unknown tab (no
    /// webview, so no process). On platforms other than Linux/BSD the group
    /// id is still assigned but does not correspond to a shared process
    /// (see [`with_related_content_view`]); the policy then merely prefers
    /// emptying "groups" that are not real, which is harmless.
    pub fn process_group_of(&self, id: TabId) -> Option<u64> {
        self.contents
            .get(&id)
            .filter(|tab| tab.webview.is_some())
            .map(|tab| tab.process_group)
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

    /// Clear all site data (cookies, cache, local/session storage,
    /// IndexedDB, service workers — `WebsiteDataTypes::ALL`/
    /// `WKWebsiteDataStore::allWebsiteDataTypes`/`COREWEBVIEW2_BROWSING_
    /// DATA_KINDS` on the three engines VeloX ships, see docs/decisions.md
    /// D66) for every webview this window currently holds a handle to.
    ///
    /// This calls `wry::WebView::clear_all_browsing_data()` — a public,
    /// safe, cross-platform method wry 0.56.1 already implements for
    /// WebKitGTK/WKWebView/WebView2 (D66) — once per webview, never touching
    /// files on disk directly: the engine owns whatever store backs it
    /// (WebKitGTK's `WebsiteDataManager`, WKWebView's
    /// `WKWebsiteDataStore`, WebView2's `ICoreWebView2Profile`) and clears
    /// it through its own API while the webview keeps running, so there is
    /// no risk of deleting a file the running process still has open (the
    /// danger called out in the issue).
    ///
    /// Iterating the toolbar plus every awake tab, rather than clearing
    /// once through a single webview, is what makes this correct in *both*
    /// data-boundary modes without branching on `self.private` at all
    /// (D14/D15/D49):
    /// - **Normal mode**: the toolbar and every tab share one `WebContext`
    ///   (D49), so clearing through any one of them clears the same
    ///   underlying store the others see too — attempting all of them is
    ///   redundant but harmless, and guarantees something is cleared even
    ///   if every tab happens to be suspended (`ContentTab::webview` is
    ///   `None` then; the always-live toolbar still shares the context).
    /// - **Private mode**: the toolbar and every tab instead each get their
    ///   *own* ephemeral, unshared store (D15) — clearing only one would
    ///   leave the others' cookies/storage behind — so every live webview
    ///   must be attempted individually to actually clear all of them. A
    ///   suspended private tab has nothing to attempt: its ephemeral store
    ///   already went away with its webview.
    ///
    /// Never aborts partway through: every webview is attempted regardless
    /// of earlier failures (the acceptance condition "削除失敗時に安全に
    /// エラー処理される" — a stuck/torn-down webview should not stop the
    /// rest from being cleared), and the outcome is reported as counts
    /// rather than propagated as a single `wry::Result` so `app.rs` can
    /// still log something useful (`browser::site_data::summarize`) instead
    /// of only the first error.
    pub fn clear_all_site_data(&self) -> SiteDataClearResult {
        let mut attempted = 0usize;
        let mut failed = 0usize;
        let mut first_error = None;

        let mut attempt = |result: wry::Result<()>| {
            attempted += 1;
            if let Err(err) = result {
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        };

        attempt(self.toolbar.clear_all_browsing_data());
        for tab in self.contents.values() {
            if let Some(webview) = &tab.webview {
                attempt(webview.clear_all_browsing_data());
            }
        }

        SiteDataClearResult {
            attempted,
            failed,
            first_error,
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

    /// Whether the in-page find bar (Issue #43) is currently showing.
    pub fn find_bar_visible(&self) -> bool {
        self.find_bar_visible.get()
    }

    /// Show or hide the find bar: resizes the toolbar webview to make (or
    /// reclaim) room, the same way [`Self::set_bookmark_bar_visible`] does —
    /// see docs/decisions.md D69 for why this is its own independent,
    /// additive row rather than a [`Panel`] variant. Hiding does **not**
    /// clear any highlight left in a content webview by itself; callers
    /// that mean "close and clear" call [`Self::clear_find_highlights`]
    /// first (see `app::close_find_bar`).
    pub fn set_find_bar_visible(&self, visible: bool) -> wry::Result<()> {
        self.find_bar_visible.set(visible);
        self.sync_layout()?;
        self.toolbar
            .evaluate_script(&toolbar::set_find_bar_visible_script(visible))
    }

    /// Push the find bar's "N/M" match counter. `active` is the 0-based
    /// index `browser::find::FindState::active` reports (`None`/`total: 0`
    /// both render as "0/0" — see `ui/toolbar.html`'s `veloxSetFindStatus`).
    pub fn set_find_status(&self, total: usize, active: Option<usize>) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_find_status_script(total, active))
    }

    /// Search tab `tab_id`'s content webview's DOM for `query` (already
    /// normalized by the caller — see `browser::find::normalize_query`) and
    /// highlight every match, reporting the total back as
    /// [`UserEvent::FindMatchesUpdated`]. A no-op — not an error — for an
    /// unknown or currently suspended `tab_id` (nothing to search), the same
    /// contract [`Self::fetch_page_title`] uses.
    ///
    /// See docs/decisions.md D69 for why this is JS run via
    /// `evaluate_script_with_callback` rather than a native find API: no
    /// engine wry 0.56 supports exposes one it can safely reach. `query` is
    /// embedded as a JSON string literal and put through the same
    /// `escape_js_line_terminators` hardening `ui::toolbar`'s `set_*_script`
    /// functions use (D62) before it ever reaches [`find_search_script`], so
    /// a search term containing `"`/`\`/U+2028/U+2029 cannot break out of
    /// the generated script.
    pub fn search_in_page(
        &self,
        tab_id: TabId,
        query: &str,
        case_sensitive: bool,
    ) -> wry::Result<()> {
        let webview = match self
            .contents
            .get(&tab_id)
            .and_then(|tab| tab.webview.as_ref())
        {
            Some(webview) => webview,
            None => return Ok(()),
        };
        let script = find_search_script(&find_query_literal(query), case_sensitive);
        let proxy = self.proxy.clone();
        webview.evaluate_script_with_callback(&script, move |raw| {
            let total = extract_js_string_result(&raw)
                .and_then(|json| serde_json::from_str::<FindSearchResult>(&json).ok())
                .map(|result| result.total)
                .unwrap_or(0);
            let _ = proxy.send_event(UserEvent::FindMatchesUpdated { tab_id, total });
        })
    }

    /// Highlight the match at `index` (0-based, into the array
    /// [`Self::search_in_page`] populated) in tab `tab_id`'s content
    /// webview and scroll it into view, deactivating whichever match was
    /// previously active. Fire-and-forget — there is nothing to report
    /// back. A no-op for an unknown/suspended tab.
    pub fn highlight_find_match(&self, tab_id: TabId, index: usize) -> wry::Result<()> {
        let webview = match self
            .contents
            .get(&tab_id)
            .and_then(|tab| tab.webview.as_ref())
        {
            Some(webview) => webview,
            None => return Ok(()),
        };
        webview.evaluate_script(&find_activate_script(index))
    }

    /// Remove every find highlight left in tab `tab_id`'s content webview
    /// (unwrapping the `<span>` wrappers [`Self::search_in_page`] inserted)
    /// and clear the DOM-side match bookkeeping. Called when the find bar
    /// closes, the query is cleared, or the underlying page is about to
    /// navigate away (`app::close_find_bar`). A no-op for an unknown/
    /// suspended tab — nothing to clear, e.g. the tab already closed.
    pub fn clear_find_highlights(&self, tab_id: TabId) -> wry::Result<()> {
        let webview = match self
            .contents
            .get(&tab_id)
            .and_then(|tab| tab.webview.as_ref())
        {
            Some(webview) => webview,
            None => return Ok(()),
        };
        webview.evaluate_script(&find_clear_script())
    }

    /// Replace the downloads panel's contents (Issue #16, see
    /// docs/decisions.md D28).
    pub fn set_downloads(&self, entries: &[&DownloadEntry]) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_downloads_script(entries))
    }

    /// Replace the settings screen's contents (Issue #30, see
    /// [`toolbar::SettingsView`] and docs/decisions.md D67).
    pub fn set_settings(&self, view: &toolbar::SettingsView<'_>) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_settings_script(view))
    }

    /// Apply the chrome (toolbar/tab-strip) theme override (Issue #30's
    /// Appearance tab). Takes effect immediately — unlike every other
    /// settings-screen field, this never goes through `Config`/a restart;
    /// see docs/decisions.md D67. Never touches web page content (wry 0.56
    /// exposes no per-webview `prefers-color-scheme` override).
    pub fn set_theme(&self, theme: crate::browser::Theme) -> wry::Result<()> {
        self.toolbar
            .evaluate_script(&toolbar::set_theme_script(theme))
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

    /// Asynchronously read tab `tab_id`'s full page markup
    /// (`document.documentElement.outerHTML`) and report it back as
    /// [`UserEvent::ViewSourceReady`] for View Source (Issue #45, see
    /// docs/decisions.md D72). `page_url` is the page this source belongs
    /// to, captured by the caller *before* the async round trip — same
    /// reasoning as [`Self::fetch_favicon`]'s `page_url` parameter: if
    /// `tab_id` has already navigated elsewhere by the time this resolves,
    /// the result is attributed to the page it was actually requested for,
    /// not whatever loaded next (an accepted raciness, same class as every
    /// other `evaluate_script_with_callback` fetch here — see
    /// docs/decisions.md D12). All escaping/truncation of the returned
    /// markup happens afterwards, in pure Rust (`browser::view_source`) —
    /// this method only ever hands back the page's raw, **unescaped**
    /// source; nothing here renders it.
    ///
    /// A no-op — not an error — for an unknown or currently suspended
    /// `tab_id` (no webview to read from), the same contract every other
    /// `fetch_*`/`search_in_page` method above uses.
    pub fn fetch_page_source(&self, tab_id: TabId, page_url: String) -> wry::Result<()> {
        let webview = match self
            .contents
            .get(&tab_id)
            .and_then(|tab| tab.webview.as_ref())
        {
            Some(webview) => webview,
            None => return Ok(()),
        };
        let proxy = self.proxy.clone();
        webview.evaluate_script_with_callback(VIEW_SOURCE_FETCH_SCRIPT, move |raw| {
            let html = extract_js_string_result(&raw).unwrap_or_default();
            let _ = proxy.send_event(UserEvent::ViewSourceReady {
                page_url: page_url.clone(),
                html,
            });
        })
    }
}

/// Reads the current page's full markup for View Source (Issue #45, see
/// docs/decisions.md D72): `document.documentElement.outerHTML`, the same
/// value a page's own devtools "View Page Source" reproduces. Wrapped in
/// try/catch like [`RESOLVE_FAVICON_SCRIPT`]: a document in a state this
/// cannot be read from (should not normally happen) yields an empty string
/// rather than propagating a JS exception into the
/// `evaluate_script_with_callback` result.
///
/// Deliberately a *live-DOM* snapshot, not a second network fetch of the
/// original response bytes — see docs/decisions.md D72 for the alternatives
/// considered (a raw HTTP re-fetch would need a whole separate networking
/// path wry does not expose, and would show different markup for JS-authored
/// pages than what is actually on screen) and its accepted trade-off (a page
/// that mutated its own DOM after load shows the *current* DOM, not the
/// bytes the server originally sent).
const VIEW_SOURCE_FETCH_SCRIPT: &str = r#"(() => {
  try {
    return document.documentElement.outerHTML;
  } catch (err) {
    return "";
  }
})();"#;

/// `WebView::evaluate_script_with_callback` hands back the JS result
/// serialized as a JSON string (see wry's `eval`); unwrap that one layer to
/// get the actual string `document.title` evaluated to.
fn extract_js_string_result(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(raw).ok()
}

// --- In-page find (Issue #43), see docs/decisions.md D69 ---

/// The JSON object [`find_search_script`]'s completion value stringifies —
/// unwrapped by [`extract_js_string_result`], then this, in
/// [`BrowserWindow::search_in_page`]'s callback. A missing/malformed value
/// (should not happen; defensive only) is treated as "zero matches" rather
/// than panicking or leaving the find bar showing a stale count.
#[derive(Deserialize)]
struct FindSearchResult {
    total: usize,
}

/// Embeds `query` as a JSON string literal, hardened against
/// U+2028/U+2029 breaking a JS string literal early exactly the way
/// `ui::toolbar`'s `set_*_script` functions are (D62) — reused here via
/// `toolbar::escape_js_line_terminators` rather than a second copy of that
/// logic, since this splices into a script too (just for the content
/// webview instead of the toolbar's).
fn find_query_literal(query: &str) -> String {
    let json = serde_json::Value::String(query.to_owned()).to_string();
    toolbar::escape_js_line_terminators(&json)
}

/// Builds the script [`BrowserWindow::search_in_page`] evaluates in a
/// content webview: clears any highlight left by a previous search, then
/// (if `query_literal` is non-empty) walks every text node under
/// `document.body` — skipping `<script>`/`<style>`/`<noscript>`/
/// `<textarea>`/`<input>` subtrees — wrapping each literal-substring match
/// in a `<span class="velox-find-hl">` (`ui/toolbar.html` styles this
/// class; the toolbar webview and content webview share no CSS, so this
/// style has to be injected as an inline `<style>` the first time a search
/// runs — see the script body). The completion value is
/// `JSON.stringify({ total: N })` — parsed back by [`FindSearchResult`].
///
/// `query_literal` must already be a JSON string literal (see
/// [`find_query_literal`]) — this function does not escape it itself.
/// Matching is always a literal substring, never a user-supplied regex: the
/// query is regex-escaped client-side before being handed to `RegExp` so a
/// search term containing `.`/`*`/`(` etc. is never interpreted as a
/// pattern (see docs/decisions.md D69's "一致方式" note — regex/whole-word
/// search is out of scope for this issue).
///
/// **Known limitation** (documented, not fixed, in D69): a match cannot
/// span a text-node boundary, so text broken up by an inline element (e.g.
/// `<b>` in the middle of a word) will not be found — the same limitation a
/// naive per-text-node walker always has. Hidden text (`display:none` etc.)
/// is not specially excluded either, unlike a browser's native find; this
/// keeps the script simple and fast at the cost of occasionally matching
/// text a user cannot see.
fn find_search_script(query_literal: &str, case_sensitive: bool) -> String {
    let flags = if case_sensitive { "g" } else { "gi" };
    let mut script = String::new();
    script.push_str("(() => {\n");
    script.push_str("  \"use strict\";\n");
    script.push_str("  const HL_CLASS = \"velox-find-hl\";\n");
    script.push_str("  const STYLE_ID = \"velox-find-style\";\n");
    script.push_str("  if (!document.getElementById(STYLE_ID)) {\n");
    script.push_str("    const style = document.createElement(\"style\");\n");
    script.push_str("    style.id = STYLE_ID;\n");
    script.push_str(
        "    style.textContent = \".velox-find-hl{background:#ffd54f !important;color:#000 !important;}.velox-find-hl-active{background:#ff7043 !important;}\";\n",
    );
    script.push_str("    (document.head || document.documentElement).appendChild(style);\n");
    script.push_str("  }\n");
    script.push_str("  const prevMatches = window.__veloxFindMatches || [];\n");
    script.push_str("  for (const el of prevMatches) {\n");
    script.push_str("    if (!el || !el.parentNode) continue;\n");
    script.push_str("    const parent = el.parentNode;\n");
    script.push_str("    parent.replaceChild(document.createTextNode(el.textContent), el);\n");
    script.push_str("    parent.normalize();\n");
    script.push_str("  }\n");
    script.push_str("  window.__veloxFindMatches = [];\n");
    script.push_str("  window.__veloxFindActiveIndex = -1;\n");
    script.push_str(&format!("  const query = {query_literal};\n"));
    script.push_str("  if (!query || !document.body) {\n");
    script.push_str("    return JSON.stringify({ total: 0 });\n");
    script.push_str("  }\n");
    script.push_str("  const escaped = query.replace(/[.*+?^${}()|[\\]\\\\]/g, \"\\\\$&\");\n");
    script.push_str(&format!("  const flags = {flags:?};\n"));
    script.push_str("  let re;\n");
    script.push_str("  try {\n");
    script.push_str("    re = new RegExp(escaped, flags);\n");
    script.push_str("  } catch (e) {\n");
    script.push_str("    return JSON.stringify({ total: 0 });\n");
    script.push_str("  }\n");
    script.push_str(
        "  const SKIP_TAGS = new Set([\"SCRIPT\", \"STYLE\", \"NOSCRIPT\", \"TEXTAREA\", \"INPUT\"]);\n",
    );
    script.push_str(
        "  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {\n",
    );
    script.push_str("    acceptNode(node) {\n");
    script.push_str("      const parent = node.parentElement;\n");
    script.push_str(
        "      if (!parent || SKIP_TAGS.has(parent.tagName)) return NodeFilter.FILTER_REJECT;\n",
    );
    script.push_str("      if (!node.nodeValue) return NodeFilter.FILTER_SKIP;\n");
    script.push_str("      re.lastIndex = 0;\n");
    script.push_str(
        "      return re.test(node.nodeValue) ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP;\n",
    );
    script.push_str("    }\n");
    script.push_str("  });\n");
    script.push_str("  const nodes = [];\n");
    script.push_str("  let n;\n");
    script.push_str("  while ((n = walker.nextNode())) { nodes.push(n); }\n");
    script.push_str("  const matches = [];\n");
    script.push_str("  for (const node of nodes) {\n");
    script.push_str("    const text = node.nodeValue;\n");
    script.push_str("    re.lastIndex = 0;\n");
    script.push_str("    let match;\n");
    script.push_str("    let lastIndex = 0;\n");
    script.push_str("    let any = false;\n");
    script.push_str("    const frag = document.createDocumentFragment();\n");
    script.push_str("    while ((match = re.exec(text)) !== null) {\n");
    script.push_str("      any = true;\n");
    script.push_str("      if (match.index > lastIndex) {\n");
    script.push_str(
        "        frag.appendChild(document.createTextNode(text.slice(lastIndex, match.index)));\n",
    );
    script.push_str("      }\n");
    script.push_str("      const span = document.createElement(\"span\");\n");
    script.push_str("      span.className = HL_CLASS;\n");
    script.push_str("      span.textContent = match[0];\n");
    script.push_str("      frag.appendChild(span);\n");
    script.push_str("      matches.push(span);\n");
    script.push_str("      lastIndex = match.index + match[0].length;\n");
    script.push_str(
        "      if (match[0].length === 0) { re.lastIndex += 1; lastIndex = re.lastIndex; }\n",
    );
    script.push_str("    }\n");
    script.push_str("    if (!any) continue;\n");
    script.push_str("    if (lastIndex < text.length) {\n");
    script.push_str("      frag.appendChild(document.createTextNode(text.slice(lastIndex)));\n");
    script.push_str("    }\n");
    script.push_str("    node.parentNode.replaceChild(frag, node);\n");
    script.push_str("  }\n");
    script.push_str("  window.__veloxFindMatches = matches;\n");
    script.push_str("  return JSON.stringify({ total: matches.length });\n");
    script.push_str("})();");
    script
}

/// Builds the script [`BrowserWindow::highlight_find_match`] evaluates:
/// deactivates whichever match `window.__veloxFindActiveIndex` last pointed
/// at, activates the match at `index`, and scrolls it into view. Assumes
/// [`find_search_script`] already ran in this page load (harmless no-op via
/// the `matches[index]` guard if it did not, e.g. a stale index after the
/// page navigated).
fn find_activate_script(index: usize) -> String {
    let mut script = String::new();
    script.push_str("(() => {\n");
    script.push_str("  \"use strict\";\n");
    script.push_str("  const matches = window.__veloxFindMatches || [];\n");
    script.push_str("  const prevIndex = window.__veloxFindActiveIndex;\n");
    script.push_str("  if (typeof prevIndex === \"number\" && matches[prevIndex]) {\n");
    script.push_str("    matches[prevIndex].classList.remove(\"velox-find-hl-active\");\n");
    script.push_str("  }\n");
    script.push_str(&format!("  const index = {index};\n"));
    script.push_str("  const el = matches[index];\n");
    script.push_str("  if (el) {\n");
    script.push_str("    el.classList.add(\"velox-find-hl-active\");\n");
    script.push_str("    el.scrollIntoView({ block: \"center\", inline: \"nearest\" });\n");
    script.push_str("  }\n");
    script.push_str("  window.__veloxFindActiveIndex = index;\n");
    script.push_str("})();");
    script
}

/// Builds the script [`BrowserWindow::clear_find_highlights`] evaluates:
/// unwraps every `<span class="velox-find-hl">` [`find_search_script`]
/// inserted back into plain text and resets the DOM-side bookkeeping.
/// Idempotent — safe to call with no search having run (`window.
/// __veloxFindMatches` is then `undefined`, treated as empty).
fn find_clear_script() -> String {
    r#"(() => {
  "use strict";
  const prev = window.__veloxFindMatches || [];
  for (const el of prev) {
    if (!el || !el.parentNode) continue;
    const parent = el.parentNode;
    parent.replaceChild(document.createTextNode(el.textContent), el);
    parent.normalize();
  }
  window.__veloxFindMatches = [];
  window.__veloxFindActiveIndex = -1;
})();"#
        .to_owned()
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
    /// should join (docs/decisions.md D54, see [`with_related_content_view`]).
    /// `None` for the very first content webview (there is nothing to join
    /// yet) and always `None` when `private` is `true`: a private webview
    /// goes through wry's `.with_incognito(true)` path, which builds its
    /// own ephemeral `WebContext` per webview (D15) — relating it to
    /// another view would make WebKitGTK take the *related* view's context
    /// instead, silently changing what "private" isolates, so the private
    /// process layout stays exactly as it was before D54 (see
    /// `docs/memory-analysis.md` §9.4/§10).
    related: Option<&'a WebView>,
}

/// Site-scoped policy every content webview is built with: the ad/tracker
/// blocklist (docs/decisions.md D17) and the site permission store
/// (docs/decisions.md D60), bundled for the same reason
/// [`WebviewIsolation`] exists — keeping [`content_webview_builder`] under
/// clippy's argument-count lint as the set of cross-cutting, per-tab
/// policies grows.
struct ContentPolicy {
    /// Ad/tracker filter rules content blocking matches against.
    blocklist: Arc<FilterList>,
    /// Whether content blocking is currently active.
    content_blocking_enabled: bool,
    /// Per-origin camera/microphone/geolocation/notifications/clipboard
    /// decisions (Issue #24, docs/decisions.md D60). Loaded once at startup
    /// and shared read-only across every tab's webview — nothing in this
    /// iteration mutates it at runtime, so a plain `Arc` (no lock) is
    /// enough; see the doc comment on [`content_webview_builder`]'s
    /// `with_permission_handler` call for why.
    site_permissions: Arc<SitePermissionStore>,
    /// `Config::download_dir_override` (Issue #30, docs/decisions.md D67),
    /// threaded through to this tab's `with_download_handlers` call.
    download_dir_override: Option<String>,
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
/// docs/decisions.md D17). The same uniformity is why the permission
/// handler below (docs/decisions.md D60) lives here too, rather than being
/// bolted on only where a tab happens to be created first.
fn content_webview_builder<'a>(
    id: TabId,
    url: &str,
    content_rect: LogicalRect,
    proxy: &EventLoopProxy<UserEvent>,
    isolation: WebviewIsolation<'a>,
    policy: ContentPolicy,
) -> WebViewBuilder<'a> {
    let WebviewIsolation {
        private,
        context,
        related,
    } = isolation;
    let ContentPolicy {
        blocklist,
        content_blocking_enabled,
        site_permissions,
        download_dir_override,
    } = policy;
    // Never relate a private webview (see `WebviewIsolation::related`).
    let related = if private { None } else { related };
    let nav_proxy = proxy.clone();
    let block_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let devtools_proxy = proxy.clone();
    let new_window_proxy = proxy.clone();
    // Current origin of this tab, for the permission handler below
    // (docs/decisions.md D60): `with_permission_handler`'s callback
    // receives only a `PermissionKind`, no URL/origin (see the vendored
    // `wry` source cited in D60), so this webview's navigation handler —
    // the one place in this builder that *does* see every URL it loads —
    // keeps it up to date. Starts from the tab's initial `url` so a
    // permission requested before the first `NavigationStarted` (unlikely,
    // but not impossible) still resolves against a real origin rather than
    // `None`. A `Mutex` (not a `Cell`, which is not `Sync`) because
    // `with_permission_handler` requires `Send + Sync` — unlike the rest of
    // this file's state, which is never read outside the main thread's
    // `UserEvent` dispatch (see the crate-level architecture doc comment),
    // this closure may run off it, since the trait bound is the same across
    // every `wry` platform backend regardless of whether a given one
    // actually needs it.
    let current_origin = Arc::new(Mutex::new(site_permissions::origin_of(url)));
    let permission_origin = Arc::clone(&current_origin);
    let builder = with_related_content_view(new_webview_builder(context), related)
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
            // Keep the permission handler's notion of "current origin" in
            // sync with what this tab is actually about to show — see
            // `current_origin`'s doc comment above and docs/decisions.md
            // D60. Only updated for a navigation that is actually allowed
            // to proceed (this function already returned above for a
            // blocked one), so a denied navigation never overwrites the
            // origin of the page still on screen. `if let Ok` rather than
            // `unwrap`/`expect`: a poisoned mutex (only possible if some
            // other holder of this `Arc` panicked while holding the lock)
            // must not crash page navigation — see CLAUDE.md's
            // "`unwrap()`/`expect()` の乱用を避ける" rule; worst case the
            // permission handler below keeps resolving against a stale
            // origin instead.
            if let Ok(mut origin) = current_origin.lock() {
                *origin = site_permissions::origin_of(&url);
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
        // Site permissions (Issue #24, docs/decisions.md D60): `wry` 0.56
        // does have a cross-platform permission-request hook — unlike D17's
        // content-blocking investigation, this one is not a dead end — but
        // it hands the handler only a `PermissionKind`, synchronously, with
        // no origin and no way to suspend the decision for a custom prompt
        // (see D60 for the exact source citations). `resolve_permission`
        // below is the actual decision logic (unit-tested directly, no
        // `wry` involved); this closure is just wiring: look up the
        // request's origin from `permission_origin`, ask the store, and
        // translate the answer into what `wry` expects.
        .with_permission_handler(move |kind| {
            let origin = permission_origin.lock().ok().and_then(|o| o.clone());
            resolve_permission(&site_permissions, origin.as_deref(), kind)
        });
    // Downloads (Issue #16, docs/decisions.md D28 / D53): only where wry
    // scopes download handlers to the webview itself. On WebKitGTK in
    // non-private mode they belong to the shared `WebContext` and are
    // registered once, on the toolbar webview in `BrowserWindow::new` —
    // see `download_handler_host`.
    match download_handler_host(private, DOWNLOAD_HANDLERS_PER_CONTEXT) {
        DownloadHandlerHost::EachContentWebview => {
            with_download_handlers(builder, proxy, download_dir_override)
        }
        DownloadHandlerHost::SharedContext => builder,
    }
}

/// Map `wry`'s permission-kind enum onto VeloX's own
/// [`crate::browser::site_permissions::PermissionKind`] (Issue #24,
/// docs/decisions.md D60). `wry::PermissionKind` is `#[non_exhaustive]` and
/// covers far more kinds than VeloX's own list tracks (window management,
/// MIDI, local fonts, ...) — every one of those, and anything a future
/// `wry` version adds, falls into the wildcard arm and maps to
/// [`PermissionKind::Other`], which [`SitePermissionStore::resolve`] always
/// blocks regardless of any stored decision. This is the browser-agnostic
/// enum staying decoupled from `wry` (see `site_permissions`' module doc
/// comment) while still degrading safely for anything it does not
/// explicitly know about.
fn map_permission_kind(kind: WryPermissionKind) -> PermissionKind {
    match kind {
        WryPermissionKind::Camera => PermissionKind::Camera,
        WryPermissionKind::Microphone => PermissionKind::Microphone,
        WryPermissionKind::Geolocation => PermissionKind::Geolocation,
        WryPermissionKind::Notifications => PermissionKind::Notifications,
        WryPermissionKind::ClipboardRead => PermissionKind::ClipboardRead,
        _ => PermissionKind::Other,
    }
}

/// Decide how to answer one `wry` permission request (docs/decisions.md
/// D60): the actual logic behind [`content_webview_builder`]'s
/// `with_permission_handler` closure, pulled out as a free function so it
/// is unit-testable without building a real webview.
///
/// `origin` is `None` for a request with no known/meaningful origin (the
/// tab has not navigated anywhere with an `http`/`https` origin yet, or
/// [`crate::browser::site_permissions::origin_of`] rejected its URL, e.g.
/// `file://`/`about:`) — treated the same as an unmapped
/// [`PermissionKind::Other`]: always [`PermissionResponse::Deny`], never
/// looked up in `store`, since a permission decision with nothing to scope
/// it to cannot be "site-scoped" at all.
///
/// For a known origin and a kind VeloX tracks, an explicit stored decision
/// ([`Resolution::Allow`]/[`Resolution::Block`]) is applied directly — no
/// `wry`-native prompt runs in that case. With no stored decision
/// ([`Resolution::Ask`]) this returns [`PermissionResponse::Default`],
/// which — per the doc comment on `wry`'s `with_permission_handler` (see
/// D60) — hands the request to the platform's own default behavior:
/// WebView2 and WKWebView show their native permission prompt (so the user
/// still sees an explicit Allow/Block UI, just not a VeloX-drawn one) while
/// WebKitGTK denies. VeloX has no way to observe what the user chose in
/// that native prompt (see D60), so it is never written back into `store`
/// — only an explicit VeloX-level decision (not implemented in this
/// iteration; see D60's "future work") ever is.
fn resolve_permission(
    store: &SitePermissionStore,
    origin: Option<&str>,
    kind: WryPermissionKind,
) -> PermissionResponse {
    let Some(origin) = origin else {
        return PermissionResponse::Deny;
    };
    match store.resolve(origin, map_permission_kind(kind)) {
        Resolution::Allow => PermissionResponse::Allow,
        Resolution::Block => PermissionResponse::Deny,
        Resolution::Ask => PermissionResponse::Default,
    }
}

/// Whether wry registers a webview's download handlers on the
/// [`WebContext`] the webview is built against rather than on the webview
/// itself. `true` on WebKitGTK, where `with_download_started_handler` /
/// `with_download_completed_handler` end up in
/// `WebContext::register_download_handler` →
/// `WebKitWebContext::connect_download_started` (wry 0.56.1,
/// `src/webkitgtk/mod.rs` → `webkitgtk/web_context.rs`); `false` on
/// WKWebView (a per-webview download delegate) and WebView2 (a
/// per-controller `add_DownloadStarting`). See docs/decisions.md D53.
const DOWNLOAD_HANDLERS_PER_CONTEXT: bool = cfg!(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
));

/// Which webview(s) VeloX's download handlers are registered on — the
/// output of [`download_handler_host`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadHandlerHost {
    /// Register once, on the first webview built against the shared
    /// `WebContext` (the toolbar, built before any tab in
    /// `BrowserWindow::new`), and on no content webview.
    SharedContext,
    /// Register on every content webview, never on the toolbar.
    EachContentWebview,
}

/// Decide where VeloX's download handlers go (docs/decisions.md D53).
///
/// `per_context` is [`DOWNLOAD_HANDLERS_PER_CONTEXT`] in production (a
/// parameter so the decision table below is unit-testable on every
/// platform); `private` is whole-app private browsing (D14).
///
/// - `per_context && !private` (WebKitGTK, normal mode): the toolbar and
///   every tab share one `WebContext` (D49), and wry appends *each*
///   webview's handlers to that context's `download-started` signal. Two
///   things go wrong if the handlers are attached per content webview as
///   they are elsewhere:
///   1. wry's `WebViewAttributes::default()` already carries a
///      `download_started_handler: Some(|_, _| true)`, so even the toolbar
///      webview (which registers no handler of its own) adds a
///      `download-started` listener. `WebKitDownload::decide-destination`
///      uses `g_signal_accumulator_true_handled`, so the *first* connected
///      listener that returns `true` stops the rest — and since the
///      toolbar is built first, its do-nothing default wins every time:
///      VeloX's real handler never runs, `UserEvent::DownloadStarted` is
///      never sent, the downloads panel stays empty, and the file lands
///      in wry's own default directory (bypassing `VELOX_DOWNLOAD_DIR`
///      and `downloads::prepare_destination`'s sanitizing).
///   2. `WebKitDownload::finished` has no such accumulator, so every
///      content webview ever built on the context — closed tabs included,
///      nothing ever disconnects — fires `UserEvent::DownloadCompleted`
///      once per download: N tabs, N events.
///
///   Registering exactly once, on the toolbar (the first webview on the
///   shared context), makes VeloX's handler the first `decide-destination`
///   listener and the only `finished` listener. Content webviews still
///   add wry's default started handler each (unavoidable with wry 0.56.1's
///   builder API), but it is never reached.
/// - Otherwise (private mode: every webview gets its own ephemeral
///   context per D15; or WKWebView/WebView2: per-webview delegates): the
///   handlers must live on each content webview, exactly as before D53.
fn download_handler_host(private: bool, per_context: bool) -> DownloadHandlerHost {
    if per_context && !private {
        DownloadHandlerHost::SharedContext
    } else {
        DownloadHandlerHost::EachContentWebview
    }
}

/// Attach VeloX's download handlers (Issue #16, docs/decisions.md D28) to
/// `builder`. Called from exactly one place per webview kind, chosen by
/// [`download_handler_host`].
///
/// Both handlers fire on every backend VeloX ships on (confirmed from wry
/// 0.56.1's source — see D28). `with_download_started_handler` must decide
/// synchronously (return `bool`) and may rewrite the destination `PathBuf`
/// in place, so the destination resolution itself
/// (`browser::downloads::prepare_destination`, which sanitizes the
/// suggested file name and avoids same-name collisions) has to run right
/// here, not after a round trip through the event loop — the same
/// synchronous-decision-then-async-notify shape the navigation handler in
/// [`content_webview_builder`] uses for content blocking. VeloX always
/// accepts every download (`true`, matching wry's own default), so this
/// only ever *redirects* a download, never blocks one.
///
/// `download_dir_override` is `Config::download_dir_override` as of window
/// creation (Issue #30's settings screen, Downloads tab — see
/// docs/decisions.md D67); `None` keeps the pre-#30 behavior
/// (`VELOX_DOWNLOAD_DIR`/the platform default, via
/// `browser::downloads::resolve_download_dir_with_override`).
fn with_download_handlers<'a>(
    builder: WebViewBuilder<'a>,
    proxy: &EventLoopProxy<UserEvent>,
    download_dir_override: Option<String>,
) -> WebViewBuilder<'a> {
    let download_started_proxy = proxy.clone();
    let download_completed_proxy = proxy.clone();
    builder
        .with_download_started_handler(move |url, destination| {
            let suggested_name = destination
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Prefer VeloX's own directory resolution (the settings
            // screen's override, then `VELOX_DOWNLOAD_DIR`, see
            // docs/decisions.md D28/D67) over whatever default wry already
            // picked; fall back to wry's own
            // suggested directory only if neither the override nor an
            // environment variable could be resolved at all, rather than
            // failing the download outright.
            let dir =
                downloads::resolve_download_dir_with_override(download_dir_override.as_deref())
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
        assert_eq!(pick_process_group([], MAX_TABS_PER_WEB_PROCESS), None);
    }

    #[test]
    fn the_cap_is_configurable_and_one_disables_sharing() {
        // cap 1: every group is already full, so a new tab always starts
        // its own process (the pre-D54 behavior, D57).
        assert_eq!(pick_process_group([(0, false)], 1), None);
        // A higher cap keeps taking tabs into the same group.
        let three: Vec<(u64, bool)> = vec![(0, false); 3];
        assert_eq!(pick_process_group(three.iter().copied(), 4), Some(0));
        assert_eq!(pick_process_group(three.iter().copied(), 3), None);
        // 0 is meaningless (a tab must live somewhere) and is read as 1,
        // matching `Config`'s own "0 means unset" rule.
        assert_eq!(pick_process_group([(0, false)], 0), None);
    }

    // --- Site permissions (Issue #24, docs/decisions.md D60) ---

    #[test]
    fn known_wry_kinds_map_onto_velox_kinds() {
        assert_eq!(
            map_permission_kind(WryPermissionKind::Camera),
            PermissionKind::Camera
        );
        assert_eq!(
            map_permission_kind(WryPermissionKind::Microphone),
            PermissionKind::Microphone
        );
        assert_eq!(
            map_permission_kind(WryPermissionKind::Geolocation),
            PermissionKind::Geolocation
        );
        assert_eq!(
            map_permission_kind(WryPermissionKind::Notifications),
            PermissionKind::Notifications
        );
        assert_eq!(
            map_permission_kind(WryPermissionKind::ClipboardRead),
            PermissionKind::ClipboardRead
        );
    }

    #[test]
    fn unmapped_wry_kinds_fall_back_to_other() {
        // A representative sample of the kinds VeloX does not track —
        // every one of these must land on `Other`, which always denies.
        assert_eq!(
            map_permission_kind(WryPermissionKind::Midi),
            PermissionKind::Other
        );
        assert_eq!(
            map_permission_kind(WryPermissionKind::WindowManagement),
            PermissionKind::Other
        );
        assert_eq!(
            map_permission_kind(WryPermissionKind::DisplayCapture),
            PermissionKind::Other
        );
        assert_eq!(
            map_permission_kind(WryPermissionKind::Other),
            PermissionKind::Other
        );
    }

    #[test]
    fn resolve_permission_denies_when_there_is_no_origin() {
        let store = SitePermissionStore::new();
        assert_eq!(
            resolve_permission(&store, None, WryPermissionKind::Camera),
            PermissionResponse::Deny
        );
    }

    #[test]
    fn resolve_permission_denies_an_unmapped_kind_even_with_a_known_origin() {
        let store = SitePermissionStore::new();
        assert_eq!(
            resolve_permission(&store, Some("https://example.com"), WryPermissionKind::Midi),
            PermissionResponse::Deny
        );
    }

    #[test]
    fn resolve_permission_defers_to_the_platform_default_when_undecided() {
        let store = SitePermissionStore::new();
        assert_eq!(
            resolve_permission(
                &store,
                Some("https://example.com"),
                WryPermissionKind::Camera
            ),
            PermissionResponse::Default
        );
    }

    #[test]
    fn resolve_permission_applies_a_stored_allow() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Camera,
            crate::browser::site_permissions::PermissionDecision::Allow,
            1,
        );
        assert_eq!(
            resolve_permission(
                &store,
                Some("https://example.com"),
                WryPermissionKind::Camera
            ),
            PermissionResponse::Allow
        );
    }

    #[test]
    fn resolve_permission_applies_a_stored_block() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Microphone,
            crate::browser::site_permissions::PermissionDecision::Block,
            1,
        );
        assert_eq!(
            resolve_permission(
                &store,
                Some("https://example.com"),
                WryPermissionKind::Microphone
            ),
            PermissionResponse::Deny
        );
    }

    #[test]
    fn resolve_permission_is_scoped_to_the_given_origin() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://a.example",
            PermissionKind::Camera,
            crate::browser::site_permissions::PermissionDecision::Allow,
            1,
        );
        assert_eq!(
            resolve_permission(&store, Some("https://b.example"), WryPermissionKind::Camera),
            PermissionResponse::Default
        );
    }

    #[test]
    fn process_group_joins_the_fullest_idle_group_with_room() {
        // Group 1 has 2 live tabs, group 0 has 1: fill group 1 first.
        assert_eq!(
            pick_process_group(
                [(0, false), (1, false), (1, false)],
                MAX_TABS_PER_WEB_PROCESS
            ),
            Some(1)
        );
        // Ties go to the lower id, deterministically.
        assert_eq!(
            pick_process_group([(3, false), (2, false)], MAX_TABS_PER_WEB_PROCESS),
            Some(2)
        );
    }

    #[test]
    fn process_group_starts_fresh_once_every_group_is_full() {
        let full: Vec<(u64, bool)> = vec![(0, false); MAX_TABS_PER_WEB_PROCESS];
        assert_eq!(
            pick_process_group(full.iter().copied(), MAX_TABS_PER_WEB_PROCESS),
            None
        );
        // A full group is skipped in favor of one with room, however small.
        let mut mixed = full;
        mixed.push((7, false));
        assert_eq!(pick_process_group(mixed, MAX_TABS_PER_WEB_PROCESS), Some(7));
    }

    #[test]
    fn process_group_never_joins_a_group_that_is_still_loading() {
        // One loading tab makes its whole group busy, however much room
        // it has; with no other group, start fresh (parallel loads).
        assert_eq!(
            pick_process_group([(0, true), (0, false)], MAX_TABS_PER_WEB_PROCESS),
            None
        );
        // A smaller idle group beats a bigger busy one.
        assert_eq!(
            pick_process_group(
                [(0, true), (0, false), (1, false)],
                MAX_TABS_PER_WEB_PROCESS
            ),
            Some(1)
        );
    }

    #[test]
    fn download_handlers_go_to_shared_context_only_on_webkitgtk_normal_mode() {
        // WebKitGTK, normal mode: one registration on the shared context.
        assert_eq!(
            download_handler_host(false, true),
            DownloadHandlerHost::SharedContext
        );
        // WebKitGTK, private mode: every webview has its own ephemeral
        // context (D15), so per-webview registration is both correct and
        // the only option.
        assert_eq!(
            download_handler_host(true, true),
            DownloadHandlerHost::EachContentWebview
        );
        // WKWebView / WebView2: per-webview delegates, regardless of mode.
        assert_eq!(
            download_handler_host(false, false),
            DownloadHandlerHost::EachContentWebview
        );
        assert_eq!(
            download_handler_host(true, false),
            DownloadHandlerHost::EachContentWebview
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
        assert_eq!(
            effective_toolbar_height(48, 320, false, 30, false, 34, false),
            48
        );
        assert_eq!(
            effective_toolbar_height(48, 320, true, 30, false, 34, false),
            368
        );
    }

    #[test]
    fn effective_height_adds_bookmark_bar_height_only_when_visible() {
        assert_eq!(
            effective_toolbar_height(48, 320, false, 30, true, 34, false),
            78
        );
        assert_eq!(
            effective_toolbar_height(48, 320, false, 30, false, 34, false),
            48
        );
    }

    #[test]
    fn effective_height_sums_bar_and_panel_when_both_are_showing() {
        // The bar and a panel are independent, additive components (see
        // docs/decisions.md D35) — not alternatives like `set_panel`'s own
        // variants are.
        assert_eq!(
            effective_toolbar_height(48, 320, true, 30, true, 34, false),
            398
        );
    }

    #[test]
    fn effective_height_adds_find_bar_height_only_when_visible() {
        assert_eq!(
            effective_toolbar_height(48, 320, false, 30, false, 34, true),
            82
        );
        assert_eq!(
            effective_toolbar_height(48, 320, false, 30, false, 34, false),
            48
        );
    }

    #[test]
    fn effective_height_sums_all_four_components_when_all_showing() {
        // The find bar (Issue #43, docs/decisions.md D69) is independent and
        // additive too, exactly like the bookmark bar — all four rows can
        // stack.
        assert_eq!(
            effective_toolbar_height(48, 320, true, 30, true, 34, true),
            432
        );
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
            OPEN_FIND_BAR_MESSAGE,
            VIEW_SOURCE_MESSAGE,
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
        assert_eq!(
            parse_content_shortcut(OPEN_FIND_BAR_MESSAGE),
            Some(ContentShortcut::OpenFindBar)
        );
        assert_eq!(
            parse_content_shortcut(VIEW_SOURCE_MESSAGE),
            Some(ContentShortcut::ViewSource)
        );
        for n in 1u8..=8 {
            assert_eq!(
                parse_content_shortcut(&format!("velox:activate-tab-{n}")),
                Some(ContentShortcut::ActivateTabAt(n))
            );
        }
    }

    // --- In-page find (Issue #43), see docs/decisions.md D69 ---

    #[test]
    fn find_query_literal_escapes_quotes_and_backslashes() {
        assert_eq!(find_query_literal(r#""a"\b"#), r#""\"a\"\\b""#.to_owned());
    }

    #[test]
    fn find_query_literal_escapes_u2028_and_u2029_line_terminators() {
        // Same D62 hardening `ui::toolbar`'s `set_*_script` functions apply,
        // reused here (not duplicated) via `toolbar::escape_js_line_terminators`.
        let literal = find_query_literal("foo\u{2028}bar\u{2029}");
        assert!(literal.contains("\\u2028"), "{literal}");
        assert!(literal.contains("\\u2029"), "{literal}");
        assert!(!literal.contains('\u{2028}'));
        assert!(!literal.contains('\u{2029}'));
    }

    #[test]
    fn find_search_script_embeds_the_query_literal_and_case_flags() {
        let script = find_search_script(&find_query_literal("hello"), false);
        assert!(script.contains("const query = \"hello\";"));
        assert!(script.contains("\"gi\""));
        assert!(script.ends_with("})();"));

        let script = find_search_script(&find_query_literal("hello"), true);
        assert!(script.contains("\"g\""));
        assert!(!script.contains("\"gi\""));
    }

    #[test]
    fn find_search_script_neutralizes_quotes_and_script_closing_sequences() {
        // A search term is arbitrary text a user typed, potentially copied
        // from the very (untrusted) page being searched — it must come
        // through as inert JSON string content embedded in `const query =
        // ...`, never break out of that statement (D62/D69).
        let literal = find_query_literal(r#""; document.body.innerHTML = "pwned"; //"#);
        let script = find_search_script(&literal, false);
        assert!(script.contains(&format!("const query = {literal};\n")));
        // The generated script still parses as the single intended
        // statement shape — the injected quotes/semicolons are backslash-
        // escaped inside the JSON string, not raw JS syntax.
        assert!(!script.contains("innerHTML = \"pwned\"; //\";\n"));
    }

    #[test]
    fn find_search_script_neutralizes_regex_metacharacters_in_the_query() {
        // The query is matched as a literal substring, never interpreted as
        // a regex pattern — the generated script must regex-escape it
        // client-side rather than splice it into `new RegExp` raw.
        let script = find_search_script(&find_query_literal("a.b*c"), false);
        assert!(script.contains("query.replace(/[.*+?^${}()|[\\]\\\\]/g"));
    }

    #[test]
    fn find_activate_script_embeds_the_index() {
        let script = find_activate_script(3);
        assert!(script.contains("const index = 3;"));
        assert!(script.contains("scrollIntoView"));
        assert!(script.ends_with("})();"));
    }

    #[test]
    fn find_clear_script_unwraps_previous_highlights() {
        let script = find_clear_script();
        assert!(script.contains("__veloxFindMatches"));
        assert!(script.contains("replaceChild"));
        assert!(script.ends_with("})();"));
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
    fn parse_content_shortcut_does_not_panic_on_hostile_content_webview_input() {
        // The content webview loads arbitrary, potentially hostile web
        // pages (unlike the toolbar webview) — this is the least-trusted
        // IPC boundary in the app (Issue #35, docs/decisions.md D18/D23/
        // D62), so it gets the same "malformed/huge input must never
        // panic" treatment as `ui::toolbar::parse_command`.
        let huge = "a".repeat(5_000_000);
        assert_eq!(parse_content_shortcut(&huge), None);

        for hostile in [
            "\0\0\0",
            "velox:new-tab\0",
            "velox:activate-tab-18446744073709551616", // overflows u32/usize
            "🚀日本語velox:new-tab",
            "\u{202e}velox:new-tab",
        ] {
            assert_eq!(parse_content_shortcut(hostile), None, "{hostile:?}");
        }
    }

    #[test]
    fn favicon_script_falls_back_to_a_same_origin_guess() {
        assert!(RESOLVE_FAVICON_SCRIPT.contains("link[rel~=\"icon\"]"));
        assert!(RESOLVE_FAVICON_SCRIPT.contains("/favicon.ico"));
    }

    // --- View Source (Issue #45), see docs/decisions.md D72 ---

    #[test]
    fn view_source_fetch_script_reads_outer_html_and_is_exception_safe() {
        assert!(VIEW_SOURCE_FETCH_SCRIPT.contains("document.documentElement.outerHTML"));
        // Wrapped in try/catch, like RESOLVE_FAVICON_SCRIPT, so a page whose
        // DOM cannot be read from yields "" instead of propagating a JS
        // exception through `evaluate_script_with_callback`.
        assert!(VIEW_SOURCE_FETCH_SCRIPT.contains("try {"));
        assert!(VIEW_SOURCE_FETCH_SCRIPT.contains("catch"));
    }

    /// The embedded logo must stay decodable into the 8-bit RGBA layout the
    /// window icon needs; a regenerated asset in another format would
    /// otherwise only show up as a missing icon at runtime.
    #[test]
    fn embedded_window_icon_decodes() {
        let icon = decode_window_icon();
        assert!(icon.is_ok(), "{:?}", icon.err());
    }
}
