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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Icon, Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use crate::app::UserEvent;
use crate::browser::context_menu;
#[cfg(not(target_os = "windows"))]
use crate::browser::downloads;
use crate::browser::metrics::{self, IpcDirection};
use crate::browser::perf_log::IpcLog;
use crate::browser::save_page;
use crate::browser::site_permissions::SitePermissionStore;
use crate::browser::suspension::{BackgroundMemoryTarget, SuspendMechanism};
use crate::browser::{
    group_by_date, Candidate, DownloadEntry, FilterList, HistoryEntry, SiteExceptions, TabId,
    WindowId,
};
use crate::config::Config;
use crate::ui::toolbar::{self, Panel};

mod content_scripts;
mod content_webview;
mod download_handlers;
mod engine;

use content_scripts::{
    context_menu_render_script, extract_js_string_result, find_activate_script, find_query_literal,
    find_search_script, find_search_total, CONTEXT_MENU_HIDE_SCRIPT, FIND_CLEAR_SCRIPT,
    RESOLVE_FAVICON_SCRIPT, VIEW_SOURCE_FETCH_SCRIPT,
};
use content_webview::{content_webview_builder, ContentPolicy, WebviewIsolation};
use download_handlers::{
    download_handler_host, file_name_or, report_save_page_failure, send_save_page_started,
    with_download_handlers, DownloadHandlerHost, DOWNLOAD_HANDLERS_PER_CONTEXT,
    SAVE_PAGE_NO_WEBVIEW_MESSAGE,
};
use engine::{
    apply_memory_target, attach_webview, effective_suspend_mechanism, freeze_webview,
    new_webview_builder, thaw_webview, webview_is_playing_audio,
};

pub use download_handlers::DOWNLOAD_SUCCESS_FLAG_IS_SHARED;

/// A rectangle in logical pixels: `(x, y, width, height)`.
type LogicalRect = (u32, u32, u32, u32);

/// A tab-management keyboard shortcut reported by the content webview's
/// shortcut IPC channel (see `content_scripts::parse_content_shortcut`).
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
    /// Ctrl/Cmd+N (Issue #29): open a new window. The content-webview half
    /// of `ui::toolbar::ToolbarCommand::NewWindow` — both are handled by the
    /// same shared function in `app.rs`. See docs/decisions.md D68.
    NewWindow,
    /// Ctrl/Cmd+Shift+N (Issue #27): open a new private window. The
    /// content-webview half of
    /// `ui::toolbar::ToolbarCommand::NewPrivateWindow` — both are handled by
    /// the same shared function in `app.rs`. See docs/decisions.md D74.
    NewPrivateWindow,
    /// Ctrl/Cmd+F (Issue #43): open the in-page find bar. The
    /// content-webview half of `ui::toolbar::ToolbarCommand::OpenFindBar` —
    /// both are handled by the same shared function in `app.rs`. See
    /// docs/decisions.md D69.
    OpenFindBar,
    /// Ctrl/Cmd+S (Issue #46): save the current page ("名前を付けて保存").
    /// The content-webview half of `ui::toolbar::ToolbarCommand::SavePage` —
    /// both are handled by the same shared function in `app.rs`. See
    /// docs/decisions.md D76.
    SavePage,
    /// Ctrl/Cmd+P (Issue #40): print the active tab's page. The
    /// content-webview half of `ui::toolbar::ToolbarCommand::Print` — both
    /// are handled by the same shared function in `app.rs`. See
    /// docs/decisions.md D75.
    Print,
    /// Ctrl/Cmd+U (Issue #45): view the active tab's page source. The
    /// content-webview half of `ui::toolbar::ToolbarCommand::ViewSource` —
    /// both are handled by the same shared function in `app.rs`. See
    /// docs/decisions.md D72.
    ViewSource,
}

/// Result of [`BrowserWindow::export_tab_as_pdf`] — what to tell the user
/// (via the print-status banner, `app::save_active_tab_as_pdf`)
/// immediately, before an async [`crate::app::UserEvent::PdfExportFinished`]
/// (if any) arrives. See docs/decisions.md D75.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfExportRequest {
    /// Windows: the COM call was dispatched; the real outcome arrives later
    /// as `UserEvent::PdfExportFinished`.
    Started,
    /// The tab has no live webview right now — an unknown id, or (in
    /// practice this should not happen for the always-live active tab, see
    /// `browser::TabState`'s invariant) a suspended one.
    NoWebview,
    /// macOS/Linux: no headless export path exists (see
    /// `BrowserWindow::export_tab_as_pdf`'s `#[cfg(not(windows))]` doc
    /// comment) — the caller should point the user at [`BrowserWindow::
    /// print_tab`]'s native dialog instead.
    UnsupportedPlatform,
    /// Windows: a COM call failed *synchronously* (an interface cast, or
    /// creating the settings object) — no async result will follow, unlike
    /// `Started`.
    Failed { message: String },
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

/// Convert [`crate::browser::ResolvedTheme`] (the `browser::`-layer, `tao`-
/// independent mirror — see its doc comment for why it exists at all) to the
/// real `tao::window::Theme` `Window::set_theme` expects. The one place this
/// conversion needs to happen (Issue #31/D71); everything upstream of it
/// (`browser::native_window_theme`) stays UI-toolkit-independent and
/// unit-tested without `tao`.
fn tao_theme_of(theme: crate::browser::ResolvedTheme) -> tao::window::Theme {
    match theme {
        crate::browser::ResolvedTheme::Light => tao::window::Theme::Light,
        crate::browser::ResolvedTheme::Dark => tao::window::Theme::Dark,
    }
}

/// `window` の内側サイズ (論理ピクセル) を `(width, height)` で返す。
fn logical_inner_size(window: &Window) -> (u32, u32) {
    let size = window.inner_size().to_logical::<u32>(window.scale_factor());
    (size.width, size.height)
}

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
///
/// `Clone` (Issue #29/D68): opening a second window (Ctrl/Cmd+N) needs the
/// exact same site-scoped policies the first window was built with — every
/// field here is an `Arc`, so cloning is cheap (a refcount bump, not a deep
/// copy) and every window ends up sharing the *same* underlying
/// `FilterList`/`SiteExceptions`/`SitePermissionStore` instances.
#[derive(Clone)]
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
    /// This window's own id (Issue #29, docs/decisions.md D68) — issued by
    /// `browser::Windows` before this `BrowserWindow` is built and carried
    /// unchanged for its whole lifetime. Baked into every `UserEvent` this
    /// window's webviews send that `app.rs` cannot otherwise attribute to a
    /// window (see the module doc comment on why `TabId` alone is not
    /// enough: it is only unique *within* one window's own `Tabs`).
    id: WindowId,
    window: Window,
    #[cfg(gtk_backend)]
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
    /// How a suspended tab's memory is reclaimed (Issue #243) —
    /// `Config::suspend_mechanism` as of window creation, **downgraded to
    /// what this build and this runtime can actually do**
    /// ([`effective_suspend_mechanism`], D138 決定3). See
    /// [`Self::suspend_tab`] and [`Self::suspend_mechanism`].
    suspend_mechanism: SuspendMechanism,
    /// What background-but-awake tabs are told about memory (Issue #242) —
    /// `Config::background_memory_target` as of window creation. Applied by
    /// [`Self::activate_tab`] as tabs move on and off screen.
    background_memory_target: BackgroundMemoryTarget,
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
    /// This window's own private-browsing flag (Issue #27, see
    /// docs/decisions.md D74 — originally D14's whole-app-only flag; D74
    /// moved it onto each window individually once multiple windows with
    /// different privacy could coexist). Set once, from `new`'s `private`
    /// parameter, and never changes for this window's lifetime. Kept so
    /// tabs opened after startup (`open_tab`, and `resume_tab` through it)
    /// are built with the same ephemeral-or-persistent data store this
    /// window started with.
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
    /// Where to log Rust → JS toolbar IPC traffic (Issue #66) — `None` when
    /// `config.perf_metrics` is off, in which case [`Self::eval_toolbar`]
    /// is a single `Option::is_none` check with no clock read, matching
    /// this project's other metrics-off-path guarantees (see
    /// `metrics::PerfRecord`'s module doc comment).
    ipc_log: Option<IpcLog>,
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
    /// `private` decides *this* window's own privacy (Issue #27, D74) —
    /// deliberately a separate parameter from `config`, not `config.private`
    /// read directly: `config` is shared by every window `app::run`/
    /// `app::open_new_window` builds in one process, but a private window
    /// (Ctrl/Cmd+Shift+N) can coexist with normal ones, so the two must be
    /// able to disagree with `config.private`. `app::open_new_window`'s
    /// regular ("new window", Ctrl/Cmd+N) path still passes `config.private`
    /// through unchanged, which is exactly what made the very first (D68)
    /// multi-window implementation's behavior correct already: a process
    /// launched with `--private` keeps every window it opens private.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_loop: &EventLoopWindowTarget<UserEvent>,
        id: WindowId,
        config: &Config,
        proxy: EventLoopProxy<UserEvent>,
        initial_tab: TabId,
        initial_url: &str,
        policies: SitePolicies,
        private: bool,
        // Issue #66: `None` when `config.perf_metrics` is off (both
        // `app::run`'s primary-window construction and
        // `app::open_new_window` pass `None` in that case) — see
        // `Self::eval_toolbar`.
        ipc_log: Option<IpcLog>,
        // Issue #182 (D92): filled in with two intermediate timestamps when
        // this is the process's *first* window and metrics are on, so that
        // `app::run` can split `process_start` → `window_created` (644ms of
        // Windows' 717ms `startup_first_load_ms`, see
        // `docs/performance-targets.md` §21) into tao's native window,
        // the engine's first-webview initialization, and the rest. `None`
        // for every window opened later (`app::open_new_window`) and
        // whenever metrics are off — see `metrics::WindowBuildTimings`.
        mut build_timings: Option<&mut metrics::WindowBuildTimings>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let SitePolicies {
            blocklist,
            site_exceptions,
            site_permissions,
        } = policies;
        // A window-title suffix is a second, independent tell for private
        // mode (docs/decisions.md D14/D74): unlike the toolbar badge it
        // survives being covered by another window in a taskbar/alt-tab
        // switcher.
        let window_title = if private {
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
        // Issue #182: the native window (HWND / GtkWindow) now exists but
        // has no webview yet, so everything after this point in `new` is
        // engine cost rather than toolkit cost.
        if let Some(timings) = build_timings.as_deref_mut() {
            timings.native_window_built = Some(Instant::now());
        }

        // On Linux/BSD every webview goes in one `gtk::Fixed` container (see
        // `engine::create_webview_host`); everywhere else wry supports true
        // child webviews directly (see `attach` below).
        #[cfg(gtk_backend)]
        let host = engine::create_webview_host(&window)?;

        #[cfg(gtk_backend)]
        let attach = |builder: WebViewBuilder<'_>| attach_webview(&host, builder);
        #[cfg(not(gtk_backend))]
        let attach = |builder: WebViewBuilder<'_>| attach_webview(&window, builder);

        let (width, height) = logical_inner_size(&window);
        let (toolbar_rect, content_rect) = split_layout(width, height, config.toolbar_height);

        // Shared across the toolbar and every content webview in non-private
        // mode only — see docs/decisions.md D49 and `BrowserWindow::context`'s
        // doc comment for why private mode gets `None` instead of a context
        // that `.with_incognito(true)` would just ignore anyway.
        let mut context = if private {
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
            // content webview (docs/decisions.md D14/D15/D74) — otherwise a
            // favicon fetch could persist cookies/cache private browsing is
            // supposed to leave no trace of.
            .with_incognito(private)
            // `id` (`WindowId`) is baked into every event this window's
            // webviews send (docs/decisions.md D68), including the
            // toolbar's own IPC messages — `app.rs` needs it to know which
            // window's `Tabs` a `ToolbarCommand` applies to. `Copy`, so
            // capturing it here and again in `content_webview_builder` below
            // just copies it, exactly like `TabId` is already captured by
            // several sibling closures in that function.
            .with_ipc_handler(move |request| {
                let _ = ipc_proxy.send_event(UserEvent::ToolbarMessage(id, request.into_body()));
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
        let toolbar_builder = match download_handler_host(private, DOWNLOAD_HANDLERS_PER_CONTEXT) {
            DownloadHandlerHost::SharedContext => {
                with_download_handlers(toolbar_builder, id, &proxy, download_dir_override.clone())
            }
            DownloadHandlerHost::EachContentWebview => toolbar_builder,
        };
        let toolbar = attach(toolbar_builder)?;
        // Issue #182: the process's first webview is up. On Windows that
        // means the WebView2 environment has been created and
        // `msedgewebview2.exe` spawned — a one-time cost the *second*
        // webview (the content one, attached just below) does not pay
        // again, which is exactly the split this checkpoint exists to show.
        // D43 measured the same asymmetry on Linux/WebKitGTK from
        // throw-away diagnostics; #182 makes it a permanent metric.
        // Last use of `build_timings`, so this moves it out rather than
        // reborrowing.
        if let Some(timings) = build_timings {
            timings.toolbar_webview_built = Some(Instant::now());
        }

        let content_blocking_enabled = config.content_blocking_enabled;
        let content_builder = content_webview_builder(
            id,
            initial_tab,
            initial_url,
            content_rect,
            &proxy,
            WebviewIsolation {
                private,
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
        // Issue #176 Stage 2 の調査 probe。休止はしない — この実行環境の
        // WebView2 Runtime が休止 API を持っているかを 1 回だけ記録する
        // (`ui::webview2_suspend` の module doc を参照)。
        #[cfg(windows)]
        crate::ui::webview2_suspend::log_support_once(&content);
        // 設定された機構を、この build / この Runtime が実際にできることへ
        // 落とす (D138 決定3)。`content` がタブへ move される前にここで
        // 一度だけ解決する — 答えはプロセス内で変わらない。
        let suspend_mechanism = effective_suspend_mechanism(config.suspend_mechanism, &content);
        #[cfg(windows)]
        crate::ui::webview2_blocking::attach(
            &content,
            id,
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
            id,
            window,
            #[cfg(gtk_backend)]
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
            suspend_mechanism,
            background_memory_target: config.background_memory_target,
            context,
            private,
            blocklist,
            content_blocking_enabled,
            site_exceptions,
            site_permissions,
            download_dir_override,
            ipc_log,
        })
    }

    /// Current toolbar/content rectangles for the window's present size,
    /// accounting for whether a history/bookmarks panel is currently open
    /// (it grows the toolbar webview and shrinks the content area).
    fn layout(&self) -> (LogicalRect, LogicalRect) {
        let (width, height) = logical_inner_size(&self.window);
        let toolbar_height = effective_toolbar_height(
            self.toolbar_height,
            self.panel_height,
            self.open_panel.get().is_some(),
            self.bookmark_bar_height,
            self.bookmark_bar_visible.get(),
            self.find_bar_height,
            self.find_bar_visible.get(),
        );
        split_layout(width, height, toolbar_height)
    }

    /// Recompute webview bounds after the window was resized (or a panel was
    /// opened/closed). Every open tab's content webview is resized, not just
    /// the active one, so a background tab is laid out correctly the moment
    /// it becomes visible instead of only on its own activation.
    pub fn sync_layout(&self) -> wry::Result<()> {
        let (toolbar_rect, content_rect) = self.layout();
        self.toolbar.set_bounds(to_bounds(toolbar_rect))?;
        let bounds = to_bounds(content_rect);
        for webview in self.live_webviews() {
            webview.set_bounds(bounds)?;
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
                    .filter(|tab| tab.process_group == group)
                    .find_map(|tab| tab.webview.as_ref());
                (group, related)
            }
            None => {
                let group = self.next_process_group;
                self.next_process_group += 1;
                (group, None)
            }
        };
        let builder = content_webview_builder(
            self.id,
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
        // Not a `&self` method: `builder` may already hold a `&mut` borrow
        // of `self.context` (docs/decisions.md D49), and a `&self` method
        // call would borrow all of `self`, conflicting with it. Passing the
        // target field directly keeps the two borrows disjoint — see
        // `engine::attach_webview`'s doc comment.
        #[cfg(gtk_backend)]
        let webview = attach_webview(&self.host, builder)?;
        #[cfg(not(gtk_backend))]
        let webview = attach_webview(&self.window, builder)?;
        // Same Windows-only hook as `BrowserWindow::new` — see its call
        // site's doc comment. Covers every tab opened after startup and
        // every tab rebuilt on resume from suspension (`resume_tab` reuses
        // this function), so a resumed tab does not silently lose
        // subresource blocking.
        #[cfg(windows)]
        crate::ui::webview2_blocking::attach(
            &webview,
            self.id,
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
        if let Some(webview) = self.active.and_then(|prev| self.tab_webview(prev)) {
            webview.set_visible(false)?;
            // Issue #242: the tab just left the screen, so it may
            // economize — unless the user is listening to it (#247).
            //
            // The automatic suspension policy already refuses to reclaim
            // a tab that is playing (`browser::suspension`, D56), and
            // CLAUDE.md's design principle 5 says the same. Asking a tab
            // we have decided is too valuable to suspend to economize
            // anyway would be inconsistent, so the hint follows the same
            // rule rather than waiting for a measurement to say whether
            // the engine happens to keep audio intact under `Low`.
            let target = if webview_is_playing_audio(webview) {
                BackgroundMemoryTarget::Normal
            } else {
                self.background_memory_target
            };
            apply_memory_target(webview, target);
        }
        let Some(tab) = self.contents.get(&id) else {
            // Should not happen: `app.rs` only ever activates a tab it
            // just opened or that is already tracked here.
            eprintln!("velox: activate_tab: unknown tab {id:?}");
            return Ok(());
        };
        if let Some(webview) = &tab.webview {
            let (_, content_rect) = self.layout();
            webview.set_bounds(to_bounds(content_rect))?;
            webview.set_visible(true)?;
            // Issue #242: and the tab arriving on screen must be
            // taken back off the hint, or it would keep economizing
            // while the user is looking at it. Unconditional rather
            // than paired with the knob: a tab can have been put at
            // `Low` by an earlier activation even if the knob were
            // somehow read differently later, and `Normal` is the
            // engine's own default, so saying it is always safe.
            apply_memory_target(webview, BackgroundMemoryTarget::Normal);
        }
        self.active = Some(id);
        Ok(())
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
        let Some(tab) = self.contents.get_mut(&id) else {
            eprintln!("velox: suspend_tab: unknown tab {id:?}");
            return Ok(());
        };
        // Issue #243: `Freeze` keeps the webview and asks the engine to
        // suspend the page in place. It is only ever *attempted* — a
        // platform without a freeze path, a runtime too old for
        // `ICoreWebView2_3`, or a page the engine declines all fall through
        // to the discard below, so a tab is never left awake while
        // `browser::Tabs` believes it is suspended.
        if self.suspend_mechanism == SuspendMechanism::Freeze {
            if let Some(webview) = tab.webview.as_ref() {
                if freeze_webview(webview, &self.proxy, self.id, id) {
                    // The webview stays; whether the engine actually
                    // suspended it arrives later as
                    // `UserEvent::TabFreezeFinished`, which discards it if
                    // the answer is no.
                    return Ok(());
                }
            }
        }
        // Dropped here: this is the memory reclaim.
        tab.webview.take();
        Ok(())
    }

    /// What the **engine** thinks tab `id`'s suspension state is (Issue
    /// #243), as opposed to what `browser::Tabs` records. `None` when the
    /// tab is unknown, has no webview left (so there is nothing to ask), or
    /// the platform has no freeze path at all.
    ///
    /// A cross-check, not a source of truth — VeloX's own state stays
    /// authoritative. It exists because the freeze arm of a measurement is
    /// worthless if the engine quietly disagrees, and `success: true` from
    /// `TrySuspend` only says the call returned, not that the page is still
    /// suspended some time later.
    ///
    /// **Only ever called behind `VELOX_DEBUG`** (`app.rs`). Querying a
    /// suspended WebView2 should not disturb it — `IsSuspended` is the
    /// documented way to ask — but "should not" is not a measurement, and a
    /// benchmark run must not be the place that finds out otherwise.
    pub fn engine_reports_tab_suspended(&self, id: TabId) -> Option<bool> {
        let _webview = self.tab_webview(id)?;
        #[cfg(windows)]
        {
            crate::ui::webview2_suspend::is_suspended(_webview).ok()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Throw tab `id`'s webview away, whatever mechanism suspended it
    /// (Issue #243). Used by `app.rs` when a [`SuspendMechanism::Freeze`]
    /// attempt comes back refused: the tab is already suspended on the
    /// `browser::Tabs` side, so the webview must go or the tab would be
    /// "suspended" while holding a full renderer.
    ///
    /// Deliberately *not* `suspend_tab` with the mechanism forced: this is
    /// the tail of a suspension that already happened, so an unknown id is
    /// not a problem to log — a tab closed while its freeze was in flight is
    /// ordinary.
    ///
    /// **This method does not decide whether discarding is appropriate; the
    /// caller must.** It takes the webview whatever state the tab is in,
    /// active included. An early draft reasoned that a tab could not have
    /// become active without being resumed first, and that being resumed
    /// would have made the pending freeze irrelevant. The second half was
    /// wrong: the in-flight `TrySuspend` still answers, and it answers
    /// *failure* precisely because the tab became visible. `app.rs` guards
    /// the call with `browser::suspension::late_freeze_failure_may_discard`
    /// for exactly that reason.
    pub fn discard_tab_webview(&mut self, id: TabId) -> wry::Result<()> {
        if let Some(tab) = self.contents.get_mut(&id) {
            tab.webview.take();
        }
        Ok(())
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
    /// [`with_related_content_view`](engine::with_related_content_view) uses) — through GLib's generic
    /// property API rather than the `webkit2gtk` crate's typed getter, so
    /// no new dependency is needed (docs/decisions.md D6). On every other
    /// platform this is always `false`: wry exposes no equivalent there
    /// yet, so the protection simply does not apply (documented in D56).
    pub fn is_playing_audio(&self, id: TabId) -> bool {
        self.tab_webview(id).is_some_and(webview_is_playing_audio)
    }

    /// Which `WebKitWebProcess` group (D54, [`pick_process_group`]) tab
    /// `id`'s live webview is in, for the process-unit reclaim order of the
    /// automatic suspension policy (`browser::suspension::reclaim_order`,
    /// docs/decisions.md D56). `None` for a suspended or unknown tab (no
    /// webview, so no process). On platforms other than Linux/BSD the group
    /// id is still assigned but does not correspond to a shared process
    /// (see [`with_related_content_view`](engine::with_related_content_view)); the policy then merely prefers
    /// emptying "groups" that are not real, which is harmless.
    pub fn process_group_of(&self, id: TabId) -> Option<u64> {
        self.contents
            .get(&id)
            .filter(|tab| tab.webview.is_some())
            .map(|tab| tab.process_group)
    }

    /// The mechanism this window will actually carry a suspension out with
    /// ([`effective_suspend_mechanism`]) — what `app.rs` hands to
    /// `browser::suspension::plan` so the memory signal's arithmetic matches
    /// what will really happen (docs/decisions.md D138 決定1・決定3).
    pub fn suspend_mechanism(&self) -> SuspendMechanism {
        self.suspend_mechanism
    }

    /// Wake suspended tab `id` and make it the visible tab.
    ///
    /// Which of two things this does is decided by what the tab still has,
    /// not by re-reading [`Self::suspend_mechanism`] — the knob can only be
    /// read at window creation, but a tab suspended before it mattered must
    /// still resume correctly:
    ///
    /// - **The webview is gone** ([`SuspendMechanism::Discard`], and any
    ///   failed freeze): rebuild it, loading `url` — its last known address,
    ///   because everything else (scroll position, in-progress form input,
    ///   JS-side session history) went with the webview. This is exactly
    ///   [`Self::open_tab`] followed by [`Self::activate_tab`]: rebuilding a
    ///   dropped webview for a `TabId` that `contents` already tracks is the
    ///   same operation as building the first one for a new tab.
    /// - **The webview is still there** ([`SuspendMechanism::Freeze`],
    ///   Issue #243): resume it in place and show it. `url` is not reloaded
    ///   — the point of freezing is that the page, and its state, survived.
    ///
    /// `is_loading` is passed through to [`Self::open_tab`] in the first
    /// case and unused in the second (nothing is being loaded).
    pub fn resume_tab(
        &mut self,
        id: TabId,
        url: &str,
        is_loading: impl Fn(TabId) -> bool,
    ) -> wry::Result<()> {
        // Issue #243. A frozen tab kept its webview, so there is nothing to
        // rebuild — `activate_tab` below makes it visible, and touching a
        // suspended WebView2 resumes it implicitly anyway; the explicit
        // `Resume` here keeps VeloX's intent in the code rather than
        // relying on that side effect.
        if let Some(webview) = self.tab_webview(id) {
            thaw_webview(webview, id);
            return self.activate_tab(id);
        }
        self.open_tab(id, url, is_loading)?;
        self.activate_tab(id)
    }

    /// タブ `id` の生きている content webview。未知のタブや休止中
    /// (`webview: None`) のタブでは `None`。
    fn tab_webview(&self, id: TabId) -> Option<&WebView> {
        self.contents.get(&id)?.webview.as_ref()
    }

    /// 生きている (休止していない) すべてのタブの content webview。
    fn live_webviews(&self) -> impl Iterator<Item = &WebView> {
        self.contents
            .values()
            .filter_map(|tab| tab.webview.as_ref())
    }

    /// タブ `tab_id` の content webview で `script` を評価する。未知・休止中の
    /// タブでは何もせず `Ok(())` を返す (評価する webview がない)。
    fn eval_in_tab(&self, tab_id: TabId, script: &str) -> wry::Result<()> {
        self.tab_webview(tab_id)
            .map_or(Ok(()), |webview| webview.evaluate_script(script))
    }

    /// The active tab's content webview, if any is currently active.
    fn active_webview(&self) -> Option<&WebView> {
        self.tab_webview(self.active?)
    }

    /// アクティブタブの webview に `action` を適用する。アクティブタブが
    /// ない (または休止中の) ときは何もせず `Ok(())`。
    fn with_active_webview(
        &self,
        action: impl FnOnce(&WebView) -> wry::Result<()>,
    ) -> wry::Result<()> {
        self.active_webview().map_or(Ok(()), action)
    }

    /// Load `url` in the active tab's content webview.
    pub fn navigate(&self, url: &str) -> wry::Result<()> {
        self.with_active_webview(|webview| webview.load_url(url))
    }

    /// Go back in the active tab's session history (no-op at the oldest
    /// entry).
    pub fn go_back(&self) -> wry::Result<()> {
        self.with_active_webview(|webview| webview.evaluate_script("history.back();"))
    }

    /// Go forward in the active tab's session history (no-op at the newest
    /// entry).
    pub fn go_forward(&self) -> wry::Result<()> {
        self.with_active_webview(|webview| webview.evaluate_script("history.forward();"))
    }

    /// Reload the active tab's current page.
    pub fn reload(&self) -> wry::Result<()> {
        self.with_active_webview(WebView::reload)
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

        for webview in std::iter::once(&self.toolbar).chain(self.live_webviews()) {
            attempt(webview.clear_all_browsing_data());
        }

        SiteDataClearResult {
            attempted,
            failed,
            first_error,
        }
    }

    /// Push one Rust → JS update into the toolbar webview, instrumenting it
    /// as an [`IpcDirection::Out`] event when `self.ipc_log` is set (Issue
    /// #66, `config.perf_metrics`). Every `set_*`/`focus_address_bar`
    /// method below routes its `evaluate_script` call through here instead
    /// of calling `self.toolbar.evaluate_script` directly, so
    /// counting/sizing/timing Rust → JS toolbar traffic needed no change at
    /// any of those call sites beyond this one. `name` is a short, stable
    /// label (e.g. `"set_tabs"`) rather than something derived from
    /// `script` itself — parsing the generated JS back out to recover a
    /// name would cost more than the call this is measuring. Metrics-off
    /// path: a single `Option::is_none` check, no clock read, matching
    /// this project's other `PerfContext`-gated call sites.
    fn eval_toolbar(&self, name: &'static str, script: &str) -> wry::Result<()> {
        let Some(ipc_log) = &self.ipc_log else {
            return self.toolbar.evaluate_script(script);
        };
        let started = Instant::now();
        let result = self.toolbar.evaluate_script(script);
        ipc_log.record(IpcDirection::Out, name, script.len(), started);
        result
    }

    /// Show `url` in the toolbar's address bar.
    pub fn set_url_display(&self, url: &str) -> wry::Result<()> {
        self.eval_toolbar("set_url", &toolbar::set_url_script(url))
    }

    /// Toggle the toolbar's loading indicator.
    pub fn set_loading(&self, loading: bool) -> wry::Result<()> {
        self.eval_toolbar("set_loading", &toolbar::set_loading_script(loading))
    }

    /// Update the toolbar's blocked-request counter badge. The caller
    /// (`app.rs`) passes the *active* tab's `blocked_count` — background
    /// tabs accumulate their own counts but do not touch this badge until
    /// they become active (see `Tab::on_navigation_blocked` and
    /// `UserEvent::NavigationBlocked`'s `TabId`).
    pub fn set_block_count(&self, count: u32) -> wry::Result<()> {
        self.eval_toolbar("set_block_count", &toolbar::set_block_count_script(count))
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
        self.eval_toolbar("set_tabs", &toolbar::set_tabs_script(tabs))
    }

    /// Toggle the bookmark ("star") button's active state.
    pub fn set_bookmark_active(&self, active: bool) -> wry::Result<()> {
        self.eval_toolbar(
            "set_bookmark_active",
            &toolbar::set_bookmark_active_script(active),
        )
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
        self.eval_toolbar(
            "focus_address_bar",
            &toolbar::set_focus_address_bar_script(url),
        )
    }

    /// Replace the omnibox candidate dropdown's contents (Issue #15). Does
    /// not itself open/close `Panel::Omnibox` — the caller (`app.rs`)
    /// decides that from whether `candidates` is empty, via `set_panel`.
    pub fn set_candidates(&self, candidates: &[Candidate]) -> wry::Result<()> {
        self.eval_toolbar(
            "set_candidates",
            &toolbar::set_candidates_script(candidates),
        )
    }

    /// Show or hide the toolbar's always-visible private-browsing indicator.
    pub fn set_private(&self, private: bool) -> wry::Result<()> {
        self.eval_toolbar("set_private", &toolbar::set_private_script(private))
    }

    /// This window's own private-browsing flag, fixed at construction (see
    /// `new`'s `private` parameter and docs/decisions.md D74). `app.rs`'s
    /// `ToolbarCommand::Ready` handler reads this — rather than
    /// `Config::private`, which is process-wide and therefore wrong the
    /// moment a private and a normal window coexist — to push the correct
    /// initial state to *this* window's own toolbar via `set_private` above.
    pub fn is_private(&self) -> bool {
        self.private
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
        self.eval_toolbar("set_panel", &toolbar::set_panel_script(panel))
    }

    /// Replace the history panel's contents, grouped into date sections
    /// (今日/昨日/過去7日/それ以前 — see docs/decisions.md D29) relative to
    /// `now` (unix seconds). `entries` is expected newest-first, e.g. from
    /// `HistoryStore::entries_newest_first` or `HistoryStore::search`.
    pub fn set_history(&self, entries: &[&HistoryEntry], now: u64) -> wry::Result<()> {
        let groups = group_by_date(entries.iter().copied(), now);
        self.eval_toolbar("set_history", &toolbar::set_history_script(&groups))
    }

    /// Replace the bookmarks panel's contents (folders and their entries).
    pub fn set_bookmarks(&self, view: &toolbar::BookmarksView<'_>) -> wry::Result<()> {
        self.eval_toolbar("set_bookmarks", &toolbar::set_bookmarks_script(view))
    }

    /// Replace the always-visible bookmark bar's contents (Issue #19, see
    /// docs/decisions.md D35). Same underlying data as [`Self::set_bookmarks`]
    /// — `app.rs` builds one [`toolbar::BookmarksView`] per refresh and
    /// pushes it to both.
    pub fn set_bookmark_bar(&self, view: &toolbar::BookmarksView<'_>) -> wry::Result<()> {
        self.eval_toolbar("set_bookmark_bar", &toolbar::set_bookmark_bar_script(view))
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
        self.eval_toolbar(
            "set_bookmark_bar_visible",
            &toolbar::set_bookmark_bar_visible_script(visible),
        )
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
        self.eval_toolbar(
            "set_find_bar_visible",
            &toolbar::set_find_bar_visible_script(visible),
        )
    }

    /// Push the find bar's "N/M" match counter. `active` is the 0-based
    /// index `browser::find::FindState::active` reports (`None`/`total: 0`
    /// both render as "0/0" — see `ui/toolbar.html`'s `veloxSetFindStatus`).
    pub fn set_find_status(&self, total: usize, active: Option<usize>) -> wry::Result<()> {
        self.eval_toolbar(
            "set_find_status",
            &toolbar::set_find_status_script(total, active),
        )
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
        let Some(webview) = self.tab_webview(tab_id) else {
            return Ok(());
        };
        let script = find_search_script(&find_query_literal(query), case_sensitive);
        let proxy = self.proxy.clone();
        let window_id = self.id;
        webview.evaluate_script_with_callback(&script, move |raw| {
            let total = find_search_total(&raw);
            let _ = proxy.send_event(UserEvent::FindMatchesUpdated {
                window_id,
                tab_id,
                total,
            });
        })
    }

    /// Highlight the match at `index` (0-based, into the array
    /// [`Self::search_in_page`] populated) in tab `tab_id`'s content
    /// webview and scroll it into view, deactivating whichever match was
    /// previously active. Fire-and-forget — there is nothing to report
    /// back. A no-op for an unknown/suspended tab.
    pub fn highlight_find_match(&self, tab_id: TabId, index: usize) -> wry::Result<()> {
        self.eval_in_tab(tab_id, &find_activate_script(index))
    }

    /// Remove every find highlight left in tab `tab_id`'s content webview
    /// (unwrapping the `<span>` wrappers [`Self::search_in_page`] inserted)
    /// and clear the DOM-side match bookkeeping. Called when the find bar
    /// closes, the query is cleared, or the underlying page is about to
    /// navigate away (`app::close_find_bar`). A no-op for an unknown/
    /// suspended tab — nothing to clear, e.g. the tab already closed.
    pub fn clear_find_highlights(&self, tab_id: TabId) -> wry::Result<()> {
        self.eval_in_tab(tab_id, FIND_CLEAR_SCRIPT)
    }

    // --- Print / PDF export (Issue #40), see docs/decisions.md D75 ---

    /// Print tab `tab_id`'s page (Ctrl/Cmd+P) via the OS's own native print
    /// UI: `wry::WebView::print()`, a stable, safe, public method the base
    /// `WebView` type exposes on every platform VeloX ships — no `unsafe`,
    /// no new dependency, no WebView2/COM interface-generation question at
    /// all (see D75 for why this, and not the raw
    /// `ICoreWebView2_16::Print`/`ShowPrintUI` COM API, is what Ctrl/Cmd+P
    /// calls). Each backend does something different under this one call:
    /// runs `window.print()` in the page's own JS on Windows (WebView2 is
    /// Chromium-based, so this opens Chromium's own print preview, whose
    /// "Microsoft Print to PDF" destination is how "PDFとして保存" is meant
    /// to be reached from here — see D75), a native `NSPrintOperation`
    /// modal on macOS, and a native GTK print dialog on Linux (WebKitGTK).
    ///
    /// A no-op for an unknown/suspended tab (`Ok(())`, same contract as
    /// `search_in_page`/`clear_find_highlights` above) — this should not
    /// happen in practice, since `tab_id` is always the active tab and the
    /// active tab is never suspended (see `browser::TabState`'s invariant,
    /// docs/decisions.md D20), but a stale id must still not panic.
    ///
    /// **Known limitation, documented in D75**: none of the three
    /// platforms' `print()` implementations report a genuine print-job
    /// failure back through this `Result` — a cancelled dialog, no printer
    /// configured, or a driver error are all invisible to Rust (wry 0.56's
    /// public API has no completion callback for this path, unlike the PDF
    /// export below). An `Err` here only ever means the call itself could
    /// not be dispatched (e.g. a script-evaluation failure), which is still
    /// surfaced via the print-status banner (see [`Self::set_print_status`])
    /// for the "印刷失敗時にエラーを表示" acceptance criterion, but that
    /// criterion is only partially satisfiable through this API.
    pub fn print_tab(&self, tab_id: TabId) -> wry::Result<()> {
        self.tab_webview(tab_id).map_or(Ok(()), WebView::print)
    }

    /// Push (or clear, with `None`) the print/PDF-export status banner —
    /// shared by [`Self::print_tab`]'s failure path and the async
    /// [`crate::app::UserEvent::PdfExportFinished`] result.
    pub fn set_print_status(&self, message: Option<&str>) -> wry::Result<()> {
        self.eval_toolbar(
            "set_print_status",
            &toolbar::set_print_status_script(message),
        )
    }

    /// Windows-only headless PDF export (see `ui::webview2_print` and
    /// docs/decisions.md D75): reaches past `print_tab`'s native dialog
    /// entirely and writes tab `tab_id`'s page straight to `destination`
    /// using `settings`, with no user interaction. Returns immediately;
    /// [`PdfExportRequest::Started`] means the real result arrives later as
    /// [`crate::app::UserEvent::PdfExportFinished`] (this is the one
    /// print-related path that *can* report a genuine failure, since
    /// WebView2's own `PrintToPdfCompletedHandler` reports one — see
    /// `print_tab`'s doc comment for the contrast).
    #[cfg(windows)]
    pub fn export_tab_as_pdf(
        &self,
        tab_id: TabId,
        destination: PathBuf,
        settings: &crate::browser::print::PdfExportSettings,
    ) -> PdfExportRequest {
        let Some(webview) = self.tab_webview(tab_id) else {
            return PdfExportRequest::NoWebview;
        };
        match crate::ui::webview2_print::export_as_pdf(
            webview,
            settings,
            &destination,
            self.proxy.clone(),
            self.id,
            tab_id,
        ) {
            Ok(()) => PdfExportRequest::Started,
            Err(err) => PdfExportRequest::Failed {
                message: err.to_string(),
            },
        }
    }

    /// macOS/Linux: no headless "write straight to a PDF path" API is
    /// reachable through wry 0.56's safe public surface on either platform
    /// (see D75 — this is not a "not implemented yet", it is "nothing to
    /// call") — VeloX degrades to reporting [`PdfExportRequest::
    /// UnsupportedPlatform`] so the caller can point the user at
    /// [`Self::print_tab`]'s dialog instead, per CLAUDE.md's "Windows最優先、
    /// 他 OS は最低限の整備" policy.
    #[cfg(not(windows))]
    pub fn export_tab_as_pdf(
        &self,
        tab_id: TabId,
        _destination: PathBuf,
        _settings: &crate::browser::print::PdfExportSettings,
    ) -> PdfExportRequest {
        if self.tab_webview(tab_id).is_none() {
            return PdfExportRequest::NoWebview;
        }
        PdfExportRequest::UnsupportedPlatform
    }

    /// Replace the downloads panel's contents (Issue #16, see
    /// docs/decisions.md D28).
    pub fn set_downloads(&self, entries: &[&DownloadEntry]) -> wry::Result<()> {
        self.eval_toolbar("set_downloads", &toolbar::set_downloads_script(entries))
    }

    /// Replace the settings screen's contents (Issue #30, see
    /// [`toolbar::SettingsView`] and docs/decisions.md D67).
    pub fn set_settings(&self, view: &toolbar::SettingsView<'_>) -> wry::Result<()> {
        self.eval_toolbar("set_settings", &toolbar::set_settings_script(view))
    }

    /// Apply the chrome theme override (Issue #30's Appearance tab; the
    /// native-window half is Issue #31, see docs/decisions.md D67/D71).
    /// Takes effect immediately — unlike every other settings-screen field,
    /// this never goes through `Config`/a restart. Two independent surfaces
    /// are kept in sync from the one `theme` value:
    ///
    /// - The toolbar/tab-strip/bookmark-bar/panels webview's own
    ///   `data-velox-theme` attribute (`toolbar::set_theme_script`) — never
    ///   web page content, which wry 0.56 exposes no per-webview
    ///   `prefers-color-scheme` override for.
    /// - The native window frame `tao` draws around it (title bar etc, via
    ///   `Window::set_theme`) — added by Issue #31, since an explicit
    ///   Light/Dark choice previously only ever reached the toolbar webview,
    ///   leaving the OS-drawn frame tracking the OS regardless of what the
    ///   user picked (docs/decisions.md D71). `browser::native_window_theme`
    ///   is the pure, unit-tested decision of what to pass `tao`; `None` for
    ///   `Theme::System` deliberately hands control back to `tao`'s own
    ///   OS-tracking default rather than VeloX resolving the OS theme
    ///   itself.
    pub fn set_theme(&self, theme: crate::browser::Theme) -> wry::Result<()> {
        self.window
            .set_theme(crate::browser::native_window_theme(theme).map(tao_theme_of));
        self.eval_toolbar("set_theme", &toolbar::set_theme_script(theme))
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
        let Some(webview) = self.tab_webview(tab_id) else {
            return Ok(());
        };
        let proxy = self.proxy.clone();
        let window_id = self.id;
        webview.evaluate_script_with_callback("document.title", move |raw| {
            if let Some(title) = extract_js_string_result(&raw) {
                let title = title.trim();
                if !title.is_empty() {
                    let _ = proxy.send_event(UserEvent::PageTitleResolved {
                        window_id,
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
        let Some(webview) = self.tab_webview(tab_id) else {
            return Ok(());
        };
        let proxy = self.proxy.clone();
        let window_id = self.id;
        webview.evaluate_script_with_callback(RESOLVE_FAVICON_SCRIPT, move |raw| {
            if let Some(url) = extract_js_string_result(&raw) {
                if !url.is_empty() {
                    let _ = proxy.send_event(UserEvent::FaviconResolved {
                        window_id,
                        tab_id,
                        history_id,
                        page_url: page_url.clone(),
                        url,
                    });
                }
            }
        })
    }

    /// Ctrl/Cmd+S / `ToolbarCommand::SavePage` / `ContentShortcut::SavePage`
    /// (Issue #46, "名前を付けて保存"): save tab `tab_id`'s current page to
    /// disk. `url`/`title` are the values `app.rs` already has for that tab
    /// (`Tab::current_url`/`Tab::title`) at the moment the request was made
    /// — passed in rather than re-read here so this method never needs to
    /// know about `browser::Tabs` at all, the same boundary
    /// [`Self::fetch_page_title`]/[`Self::fetch_favicon`] already keep.
    /// `download_dir_override` is the settings screen's Downloads-tab
    /// override (Issue #30/D67) — only consulted on the non-Windows
    /// fallback path below; see this method's `#[cfg]`'d bodies.
    ///
    /// Every outcome — success, a user-facing failure, or the user
    /// cancelling the Windows save dialog — is reported by sending
    /// [`UserEvent::SavePageStarted`]/[`UserEvent::SavePageFinished`] itself
    /// (this method has no return value): unlike a title/favicon refresh, a
    /// user-initiated "save" that silently did nothing on failure would look
    /// like a bug, not a harmless no-op (Issue #46's "エラー時に原因を
    /// 表示できる" acceptance criterion) — see docs/decisions.md D76 for why
    /// the save flow itself (and format: MHTML vs. a plain HTML snapshot)
    /// differs by platform, and `app.rs`'s handling of both events for why
    /// this reuses the Downloads panel/`DownloadStore` rather than adding a
    /// separate UI.
    #[cfg(target_os = "windows")]
    pub fn request_save_page(
        &self,
        tab_id: TabId,
        url: String,
        title: Option<String>,
        _download_dir_override: Option<String>,
    ) {
        let window_id = self.id;
        let proxy = self.proxy.clone();
        let suggested =
            save_page::suggested_file_name(title.as_deref(), &url, save_page::MHTML_EXTENSION);
        let Some(webview) = self.tab_webview(tab_id) else {
            report_save_page_failure(
                &proxy,
                window_id,
                url,
                suggested,
                SAVE_PAGE_NO_WEBVIEW_MESSAGE.to_owned(),
            );
            return;
        };

        let destination = match crate::ui::save_dialog_windows::show_save_dialog(&suggested) {
            Ok(Some(path)) => path,
            // The user cancelled the dialog - not an error, and not a save
            // that ever started, so no `DownloadEntry` should appear for it
            // either.
            Ok(None) => return,
            Err(err) => {
                report_save_page_failure(&proxy, window_id, url, suggested, err);
                return;
            }
        };

        send_save_page_started(
            &proxy,
            window_id,
            url.clone(),
            file_name_or(&destination, suggested),
            destination.clone(),
        );

        crate::ui::save_dialog_windows::capture_and_write_mhtml(
            webview,
            proxy,
            window_id,
            url,
            destination,
        );
    }

    /// [`Self::request_save_page`]'s non-Windows fallback (see
    /// docs/decisions.md D76): no native Save-As dialog on these platforms
    /// (see the module-level "OS 優先度" policy in CLAUDE.md — macOS/Linux
    /// stay at a minimal, always-builds implementation) — the page is saved
    /// straight to the resolved downloads directory
    /// (`browser::downloads::resolve_download_dir_with_override`), with the
    /// same suggested-name-then-collision-avoided-if-taken handling a real
    /// download already gets (`browser::downloads::prepare_destination`),
    /// as a plain `document.documentElement.outerHTML` snapshot — **images,
    /// external CSS, and other subresources are not fetched or embedded**,
    /// unlike the Windows/MHTML path.
    #[cfg(not(target_os = "windows"))]
    pub fn request_save_page(
        &self,
        tab_id: TabId,
        url: String,
        title: Option<String>,
        download_dir_override: Option<String>,
    ) {
        let window_id = self.id;
        let proxy = self.proxy.clone();
        let suggested =
            save_page::suggested_file_name(title.as_deref(), &url, save_page::HTML_EXTENSION);
        let Some(webview) = self.tab_webview(tab_id) else {
            report_save_page_failure(
                &proxy,
                window_id,
                url,
                suggested,
                SAVE_PAGE_NO_WEBVIEW_MESSAGE.to_owned(),
            );
            return;
        };

        let dir = downloads::resolve_download_dir_with_override(download_dir_override.as_deref())
            .unwrap_or_else(|| PathBuf::from("."));
        let destination = match downloads::prepare_destination(&dir, &suggested) {
            Ok(path) => path,
            Err(err) => {
                report_save_page_failure(
                    &proxy,
                    window_id,
                    url,
                    suggested,
                    format!("保存先フォルダを準備できませんでした: {err}"),
                );
                return;
            }
        };
        send_save_page_started(
            &proxy,
            window_id,
            url.clone(),
            file_name_or(&destination, suggested),
            destination.clone(),
        );

        let finish_proxy = proxy.clone();
        let finish_destination = destination.clone();
        let finish_url = url.clone();
        let start_result = webview.evaluate_script_with_callback(
            "document.documentElement.outerHTML",
            move |raw| {
                let error = match extract_js_string_result(&raw) {
                    Some(html) => {
                        let document = save_page::wrap_outer_html_as_document(&html);
                        std::fs::write(&finish_destination, document.as_bytes())
                            .err()
                            .map(|err| format!("ファイルの書き込みに失敗しました: {err}"))
                    }
                    None => Some("ページのソースを取得できませんでした".to_owned()),
                };
                let _ = finish_proxy.send_event(UserEvent::SavePageFinished {
                    window_id,
                    url: finish_url.clone(),
                    destination: finish_destination.clone(),
                    error,
                });
            },
        );
        if let Err(err) = start_result {
            let _ = proxy.send_event(UserEvent::SavePageFinished {
                window_id,
                url,
                destination,
                error: Some(format!("ページのソース取得を開始できませんでした: {err}")),
            });
        }
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
        let Some(webview) = self.tab_webview(tab_id) else {
            return Ok(());
        };
        let proxy = self.proxy.clone();
        // Issue #29/D68: carry this window's id so the resulting View Source
        // tab opens in the window the request came from — a `tab_id` alone
        // cannot say which window, since two windows can share the same
        // `TabId` value. Same reason `search_in_page` above captures it.
        let window_id = self.id;
        webview.evaluate_script_with_callback(VIEW_SOURCE_FETCH_SCRIPT, move |raw| {
            let html = extract_js_string_result(&raw).unwrap_or_default();
            let _ = proxy.send_event(UserEvent::ViewSourceReady {
                window_id,
                page_url: page_url.clone(),
                html,
            });
        })
    }

    /// Render the right-click context menu (Issue #39, see
    /// docs/decisions.md D78) for tab `tab_id` at viewport coordinates
    /// `(x, y)`. `entries` is the already-decided
    /// [`context_menu::MenuEntry`] list from `browser::context_menu::
    /// build_menu` — this method only ever turns already-sanitized/already-
    /// decided data into a script (see [`context_menu_render_script`]); it
    /// makes no decisions of its own.
    ///
    /// A no-op for an unknown or currently suspended `tab_id`, the same
    /// contract every other per-tab method above uses — there is no webview
    /// to render into.
    pub fn show_context_menu(
        &self,
        tab_id: TabId,
        entries: &[context_menu::MenuEntry],
        x: f64,
        y: f64,
    ) -> wry::Result<()> {
        self.eval_in_tab(tab_id, &context_menu_render_script(entries, x, y))
    }

    /// Remove tab `tab_id`'s context menu overlay from the page, if one is
    /// currently shown — called when the menu is closed without a
    /// selection reaching the point where Rust already knows to discard its
    /// own `OpenContextMenu` state (e.g. the tab is about to navigate away,
    /// see `app::handle_user_event`'s `NavigationStarted`/`LoadStarted`
    /// handling). A no-op for an unknown/suspended tab, or one with nothing
    /// to remove — the script itself checks before touching the DOM.
    pub fn hide_context_menu(&self, tab_id: TabId) -> wry::Result<()> {
        self.eval_in_tab(tab_id, CONTEXT_MENU_HIDE_SCRIPT)
    }

    /// Run the "Copy" context-menu action in tab `tab_id`'s content webview
    /// (`document.execCommand("copy")`, acting on whatever selection is
    /// still current there). A no-op for an unknown/suspended tab.
    pub fn copy_selection(&self, tab_id: TabId) -> wry::Result<()> {
        self.eval_in_tab(tab_id, "document.execCommand('copy');")
    }

    /// Run the "Paste" context-menu action in tab `tab_id`'s content webview
    /// (`document.execCommand("paste")` into whatever editable element still
    /// has focus there). A no-op for an unknown/suspended tab.
    ///
    /// **Known limitation** (docs/decisions.md D78): some engines restrict
    /// programmatic `execCommand("paste")` for security reasons regardless
    /// of caller — this has not been verified to actually paste on all
    /// three of VeloX's engines, only that it does not error out. See D78's
    /// "検証できていないこと" section.
    pub fn paste_into(&self, tab_id: TabId) -> wry::Result<()> {
        self.eval_in_tab(tab_id, "document.execCommand('paste');")
    }

    /// This window's own id (Issue #29). Stable for the window's whole
    /// lifetime — see the `id` field's doc comment.
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// The underlying `tao` window's own id, as tao's event loop reports it
    /// in `Event::WindowEvent { window_id, .. }` — distinct from
    /// [`Self::id`] (`browser::WindowId`, this crate's own identifier).
    /// `app.rs` uses this to find which `BrowserWindow` a given
    /// `WindowEvent` (resize, close request) belongs to when more than one
    /// is open.
    pub fn tao_id(&self) -> tao::window::WindowId {
        self.window.id()
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
    fn tao_theme_of_maps_light_and_dark_straight_across() {
        assert_eq!(
            tao_theme_of(crate::browser::ResolvedTheme::Light),
            tao::window::Theme::Light
        );
        assert_eq!(
            tao_theme_of(crate::browser::ResolvedTheme::Dark),
            tao::window::Theme::Dark
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

    /// The embedded logo must stay decodable into the 8-bit RGBA layout the
    /// window icon needs; a regenerated asset in another format would
    /// otherwise only show up as a missing icon at runtime.
    #[test]
    fn embedded_window_icon_decodes() {
        let icon = decode_window_icon();
        assert!(icon.is_ok(), "{:?}", icon.err());
    }
}
