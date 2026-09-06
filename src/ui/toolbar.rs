//! Toolbar (browser chrome) definition and its IPC protocol.
//!
//! The toolbar is rendered by a dedicated webview from [`TOOLBAR_HTML`].
//! JS -> Rust: `window.ipc.postMessage` with a JSON [`ToolbarCommand`].
//! Rust -> JS: `evaluate_script` with the snippets built by
//! [`set_url_script`] / [`set_loading_script`] / [`set_block_count_script`] /
//! [`set_tabs_script`] and friends below.
//!
//! Tabs are identified to the toolbar by a plain `u64` (the toolbar's JS has
//! no notion of `browser::TabId`); `app.rs` converts between the two at the
//! boundary.
//!
//! The toolbar document also runs its own capture-phase `keydown` listener
//! for the tab-management keyboard shortcuts (Ctrl/Cmd+T/W/Shift+T/Tab/1-9),
//! sending the six `ToolbarCommand` variants at the end of the enum below.
//! This is the trusted-webview half of that feature; the content webview's
//! untrusted half lives in `ui::window` — see docs/decisions.md D23. The
//! same listener also sends `focus_address_bar` for Ctrl/Cmd+L, and the
//! address bar's own input handler sends `omnibox_input`/`omnibox_close`
//! for the candidate dropdown (Issue #15, see docs/decisions.md D26).

use serde::{Deserialize, Serialize};

use crate::browser::{
    BookmarkEntry, BookmarkStore, Candidate, DownloadEntry, HistoryGroup, PermissionRecord,
    Settings, ShortcutInfo, Theme,
};

/// The static HTML/CSS/JS that renders the toolbar.
pub const TOOLBAR_HTML: &str = include_str!("toolbar.html");

/// Which dropdown panel (if any) the toolbar is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Panel {
    History,
    Bookmarks,
    /// The download list added by Issue #16 — see docs/decisions.md D28.
    Downloads,
    /// The omnibox candidate dropdown (Issue #15). Reuses the exact same
    /// toolbar-webview resize/panel machinery as `History`/`Bookmarks`
    /// (see docs/decisions.md D11) instead of introducing a second one;
    /// unlike those two it is opened/closed automatically as the user
    /// types (`ToolbarCommand::OmniboxInput`/`OmniboxClose`), never via
    /// `TogglePanel`.
    Omnibox,
    /// The settings screen (Issue #30, see docs/decisions.md D67) — General
    /// / Appearance / Search / Privacy / Security / Performance / Downloads
    /// / Shortcuts / Advanced tabs, opened/closed via `TogglePanel` exactly
    /// like `History`/`Bookmarks`/`Downloads`.
    Settings,
}

/// A command sent from the toolbar UI to the browser.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolbarCommand {
    /// The user submitted the address bar; `input` is the raw typed text.
    Navigate {
        input: String,
    },
    Back,
    Forward,
    Reload,
    /// The "+" button was clicked: open a new tab.
    NewTab,
    /// A tab's close button was clicked.
    CloseTab {
        id: u64,
    },
    /// A tab in the strip was clicked: make it the active tab. Resumes the
    /// tab first (rebuilding its content webview) if it was suspended.
    ActivateTab {
        id: u64,
    },
    /// A tab's suspend button was clicked: drop its content webview to
    /// reclaim memory while keeping its URL, to be reloaded on the next
    /// `ActivateTab`. A no-op for the active tab or an already-suspended
    /// tab (see `browser::tabs::Tabs::suspend`).
    SuspendTab {
        id: u64,
    },
    /// The toolbar document finished loading and wants the current state
    /// (the content page may have started loading before the toolbar was
    /// ready to display it).
    Ready,
    /// The toolbar's inline `<script>` block started executing — sent as
    /// its very first statement, before any DOM lookups or rendering (see
    /// `ui/toolbar.html`). Purely a startup-timing probe (Issue #59, see
    /// docs/decisions.md D43): it splits the `window_created` →
    /// `toolbar_ready` gap into "engine got the document parsed" vs. "the
    /// toolbar's own JS ran" — `app.rs` only feeds it to
    /// `metrics::StartupTimestamps::mark_toolbar_script_started` and
    /// otherwise ignores it.
    ScriptStarted,
    /// Open the content webview's DevTools (Web Inspector).
    OpenDevtools,
    /// The star button was clicked: bookmark the current page, or remove
    /// its bookmark if it already has one.
    ToggleBookmark,
    /// Open or close a history/bookmarks panel; clicking the button for the
    /// panel that is already open closes it.
    TogglePanel {
        panel: Panel,
    },
    /// Remove one entry from the history list (the "x" next to a row).
    DeleteHistoryEntry {
        id: u64,
    },
    /// Remove every history entry ("clear history" in the panel).
    ClearHistory,
    /// Clear all site data (cookies, cache, local/session storage,
    /// IndexedDB, service workers — see docs/decisions.md D66) for every
    /// webview this window currently holds a handle to. Unlike
    /// `ClearHistory` this is not VeloX's own state — it is delegated
    /// straight to `wry::WebView::clear_all_browsing_data()` per webview
    /// (`ui::window::BrowserWindow::clear_all_site_data`), so there is no
    /// local store to clear here and no panel to refresh afterwards.
    ClearSiteData,
    /// The history panel's search box changed. `query` is the raw typed
    /// text; an empty `query` means "search cleared", which `app.rs`
    /// answers by going back to the normal recency-ordered panel instead of
    /// an (empty, since `browser::history::search` treats `""` as "match
    /// nothing" — see docs/decisions.md D30) search result list.
    SearchHistory {
        query: String,
    },
    /// Remove one bookmark (the "x" next to a row in the bookmarks panel).
    RemoveBookmark {
        id: u64,
    },
    /// Close the currently active tab (Ctrl/Cmd+W). Unlike [`Self::CloseTab`]
    /// this carries no `id`: it is what both the toolbar's own keyboard
    /// capture and the content webview's shortcut channel
    /// (`ui::window::ContentShortcut::CloseTab`) send, since neither needs
    /// to know the active tab's id itself — `app.rs` resolves it from
    /// `Tabs::active_id()`.
    CloseActiveTab,
    /// Reopen the most recently closed tab (Ctrl/Cmd+Shift+T). A no-op if
    /// nothing has been closed yet (see `browser::tabs::Tabs::reopen_closed`).
    ReopenClosedTab,
    /// Activate the next tab in display order, wrapping around
    /// (Ctrl/Cmd+Tab).
    NextTab,
    /// Activate the previous tab in display order, wrapping around
    /// (Ctrl/Cmd+Shift+Tab).
    PrevTab,
    /// Activate the tab at this 1-based display position (Ctrl/Cmd+1..8).
    /// A no-op if there is no tab at that position.
    ActivateTabByIndex {
        index: u32,
    },
    /// Activate the last tab in display order (Ctrl/Cmd+9).
    ActivateLastTab,

    // --- Downloads (Issue #16, see docs/decisions.md D28) ---
    /// Open a completed download's file with the OS's default handler
    /// (`browser::downloads::open_path_command`). A no-op for an unknown id
    /// or a download that has not reached `DownloadState::Completed`.
    OpenDownload {
        id: u64,
    },
    /// Open the downloads directory (`browser::downloads::resolve_download_dir`)
    /// with the OS's default file manager.
    OpenDownloadsFolder,
    /// Best-effort cancel of an in-progress download: marks it
    /// `DownloadState::Cancelled` and attempts to delete whatever partial
    /// file exists at its destination. Does **not** stop the underlying
    /// engine transfer — wry 0.56 exposes no API to do that (see
    /// docs/decisions.md D28). A no-op for an unknown id or a download that
    /// already reached a terminal state.
    CancelDownload {
        id: u64,
    },
    /// Remove one entry from the download list (the "x" next to a row).
    /// Never touches the file on disk — only bookkeeping, mirroring
    /// [`Self::DeleteHistoryEntry`]/[`Self::RemoveBookmark`].
    RemoveDownloadEntry {
        id: u64,
    },
    /// Ctrl/Cmd+L: focus the address bar and select its full contents
    /// (Issue #15). Sent by the toolbar's own keydown listener; the content
    /// webview's equivalent is `ui::window::ContentShortcut::FocusAddressBar`,
    /// routed to the same handler in `app.rs`.
    FocusAddressBar,
    /// The address bar's text changed (every keystroke); `input` is the raw,
    /// not-yet-committed text. `app.rs` answers with an updated candidate
    /// list (`BrowserWindow::set_candidates`) and opens/closes the
    /// `Panel::Omnibox` dropdown depending on whether it is non-empty.
    OmniboxInput {
        input: String,
    },
    /// Esc while the omnibox dropdown is open: close it and restore the
    /// address bar to the active tab's actual current URL, selected and
    /// focused (mirrors `FocusAddressBar`'s restore step).
    OmniboxClose,

    // --- Bookmark folders, editing, reordering, and the bookmark bar
    //     (Issue #19, see docs/decisions.md D32/D33/D34/D35) ---
    /// Update an existing bookmark's title/URL/folder (the panel's inline
    /// edit form). `title` is the raw, not-yet-trimmed text — `app.rs` maps
    /// an empty/whitespace-only value to `None`. `url` is re-validated
    /// through `browser::navigation::normalize_input` before it ever
    /// reaches `BookmarkStore::edit` (D33); a rejected URL leaves the
    /// bookmark unchanged. `folder_id` is `None` for "move to root".
    EditBookmark {
        id: u64,
        title: String,
        url: String,
        folder_id: Option<u64>,
    },
    /// Create a new bookmark folder ("＋ フォルダ" in the panel). An
    /// empty/whitespace-only `name` is ignored.
    CreateBookmarkFolder {
        name: String,
    },
    /// Rename an existing bookmark folder. An empty/whitespace-only `name`
    /// is ignored (same rule as `CreateBookmarkFolder`).
    RenameBookmarkFolder {
        id: u64,
        name: String,
    },
    /// Delete a bookmark folder. The bookmarks that were in it move to the
    /// root rather than being deleted (see
    /// `browser::bookmarks::BookmarkStore::remove_folder`).
    RemoveBookmarkFolder {
        id: u64,
    },
    /// Move a bookmark one rank earlier within its folder/root scope (the
    /// panel's "↑" button). A no-op for an unknown id or an entry already
    /// first in its scope.
    MoveBookmarkUp {
        id: u64,
    },
    /// Same as `MoveBookmarkUp`, one rank later ("↓").
    MoveBookmarkDown {
        id: u64,
    },
    /// Toggle the always-visible bookmark bar (Ctrl/Cmd+Shift+B, or its
    /// toolbar button) — distinct from `TogglePanel`, since the bar is a
    /// permanent strip, not a dropdown panel (see docs/decisions.md D35).
    ToggleBookmarkBar,
    /// Open a new window (Ctrl/Cmd+N, Issue #29). Carries no id: unlike
    /// `NewTab`, this never touches the sending window's own state — it is
    /// forwarded straight to `app::open_new_window` before `app.rs` even
    /// resolves which `BrowserWindow` sent it, so it works the same way no
    /// matter which window's toolbar (or content webview —
    /// `ui::window::ContentShortcut::NewWindow`) the request came from. See
    /// docs/decisions.md D68.
    NewWindow,

    // --- Settings screen (Issue #30, see docs/decisions.md D67) ---
    /// The settings screen's "保存" button: replace the persisted settings
    /// wholesale with `settings` (already validated client-side by the
    /// form's own input types, but re-sanitized server-side regardless —
    /// see `browser::settings::Settings::sanitize` — since this is external
    /// input the same way any other IPC command is). Most fields take
    /// effect after the next restart; `appearance` is applied immediately —
    /// see D67.
    // Boxed: `Settings` is far larger than every other variant here
    // (clippy's `large_enum_variant`), and this command is sent at most
    // once per settings-screen "保存" click, never on a hot path.
    UpdateSettings {
        settings: Box<Settings>,
    },
    /// The settings screen's "初期設定に戻す" button: reset every persisted
    /// setting to [`crate::browser::settings::Settings::default`] and save
    /// that. Same apply/persist path as `UpdateSettings`, just with a fixed
    /// value instead of one read from the form.
    ResetSettings,
    // --- In-page find (Issue #43, Ctrl/Cmd+F), see docs/decisions.md D69 ---
    /// Open the find bar for the active tab. Sent by the toolbar's own
    /// keydown listener (Ctrl/Cmd+F while toolbar UI has focus); the
    /// content-webview equivalent is `ui::window::ContentShortcut::OpenFindBar`
    /// (same page has focus). Both funnel into the same `app::open_find_bar`.
    /// Deliberately its own dedicated command rather than a `TogglePanel`
    /// variant: unlike the History/Bookmarks/Downloads panels, opening find
    /// again while it is already open must still refocus/reselect the input
    /// (mirrors why Omnibox also bypassed `TogglePanel` — see
    /// docs/decisions.md D11/D26).
    OpenFindBar,
    /// The find bar's input changed (every keystroke), or its case-sensitive
    /// toggle was flipped (re-sent with the current text so a toggle re-runs
    /// the search immediately). `query` is the raw, not-yet-normalized text;
    /// an empty/whitespace-only value means "search cleared" —
    /// `browser::find::normalize_query` is the single place that decides
    /// this, mirroring how `SearchHistory`'s empty-query case defers to
    /// `browser::history::search` (D30).
    FindQuery {
        query: String,
        case_sensitive: bool,
    },
    /// "▼" button, or Enter in the find input: move to the next match,
    /// wrapping around. A no-op while there are no matches
    /// (`browser::find::FindState::next_match`).
    FindNext,
    /// "▲" button, or Shift+Enter in the find input: move to the previous
    /// match, wrapping around.
    FindPrevious,
    /// "✕" button, or Esc while the find input has focus: close the find
    /// bar and clear any highlight left in the page.
    FindClose,

    // --- Print / PDF export (Issue #40), see docs/decisions.md D75 ---
    /// Ctrl/Cmd+P, or the toolbar's print button: open the OS's native
    /// print UI for the active tab (`ui::window::BrowserWindow::print_tab`).
    /// The content-webview equivalent is
    /// `ui::window::ContentShortcut::Print`; both funnel into the same
    /// `app::print_active_tab`.
    Print,
    /// The toolbar's "PDFとして保存" button: headless PDF export with no
    /// dialog. Windows-only (see `ui::window::BrowserWindow::
    /// export_tab_as_pdf` and D75) — macOS/Linux answer with a print-status
    /// message pointing at [`Self::Print`]'s dialog instead.
    SaveAsPdf,
    // --- View Source (Issue #45, Ctrl/Cmd+U), see docs/decisions.md D72 ---
    /// View the active tab's page source in a new tab. Sent by the
    /// toolbar's own keydown listener (Ctrl/Cmd+U while toolbar UI has
    /// focus); the content-webview equivalent is
    /// `ui::window::ContentShortcut::ViewSource` (page has focus). Both
    /// funnel into the same `app::request_view_source`.
    ViewSource,
}

/// One row of the tab strip, as sent to the toolbar JS by [`set_tabs_script`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabSummary {
    pub id: u64,
    pub url: String,
    /// The page title last reported for this tab (`Tab::title`), if any has
    /// arrived yet. The tab strip falls back to `url` when this is `None` —
    /// see docs/decisions.md D22.
    pub title: Option<String>,
    /// A URL the tab strip can point an `<img>` at for this tab's favicon
    /// (`Tab::favicon`'s `Url` case; `Unknown` becomes `None` here). Loading
    /// it is left entirely to the toolbar webview's own `<img>` tag — see
    /// docs/decisions.md D22 for why that, not a Rust-side HTTP fetch, is
    /// what actually retrieves the image.
    pub favicon: Option<String>,
    pub loading: bool,
    pub active: bool,
    /// Whether the tab is suspended (its content webview has been dropped
    /// to reclaim memory — see docs/decisions.md D9). The tab strip shows
    /// this distinctly so the user can tell a dormant tab from a live one.
    pub suspended: bool,
}

/// One folder's worth of bookmarks, as sent to the toolbar JS inside a
/// [`BookmarksView`] — see docs/decisions.md D32.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BookmarkFolderView<'a> {
    pub id: u64,
    pub name: &'a str,
    pub entries: Vec<&'a BookmarkEntry>,
}

/// The full bookmark tree pushed to both the bookmarks panel
/// (`veloxSetBookmarks`) and the bookmark bar (`veloxSetBookmarkBar`) — the
/// same view, rendered two different ways by the toolbar's own JS. `root` is
/// every bookmark with no folder, `folders` is every folder with its own
/// bookmarks nested inside — a single flat layer, folders never nest inside
/// each other (docs/decisions.md D32). Both `root` and each folder's
/// `entries` are already in manual display order (docs/decisions.md D34).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BookmarksView<'a> {
    pub root: Vec<&'a BookmarkEntry>,
    pub folders: Vec<BookmarkFolderView<'a>>,
}

impl<'a> BookmarksView<'a> {
    /// Build the view straight from a [`BookmarkStore`] — the one place
    /// `app.rs` needs to construct this, shared by the panel and bar
    /// refresh paths so they can never drift apart.
    pub fn from_store(store: &'a BookmarkStore) -> Self {
        Self {
            root: store.entries_in(None).collect(),
            folders: store
                .folders()
                .iter()
                .map(|folder| BookmarkFolderView {
                    id: folder.id,
                    name: folder.name.as_str(),
                    entries: store.entries_in(Some(folder.id)).collect(),
                })
                .collect(),
        }
    }
}

/// Everything the settings screen (Issue #30, docs/decisions.md D67) needs
/// to render every tab at once: the persisted, editable `settings` document
/// itself, plus the two read-only reference views (Shortcuts, Security) that
/// are not part of that persisted document — see `browser::settings`'s
/// module doc comment for why those two are read-only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingsView<'a> {
    pub settings: &'a Settings,
    pub shortcuts: &'a [ShortcutInfo],
    pub site_permissions: &'a [PermissionRecord],
}

/// Hard ceiling on one IPC message's raw body, in bytes — rejected outright,
/// before `serde_json` ever sees it (Issue #35, see docs/decisions.md D62
/// for how the number was chosen). The toolbar webview is VeloX's own
/// trusted, bundled HTML (`TOOLBAR_HTML`), not attacker-controlled content,
/// but this is still cheap defense in depth against a pathological payload —
/// the largest realistic legitimate message is a giant clipboard paste into
/// the address bar (`navigate`) or an omnibox keystroke, both many orders of
/// magnitude smaller than this — and it keeps a single malformed/huge
/// message from spending unbounded time/memory in the JSON parser before
/// `ToolbarCommand`'s own field types get a chance to reject it.
pub const MAX_IPC_PAYLOAD_BYTES: usize = 1 << 20; // 1 MiB

/// Why [`parse_command`] failed: the body was larger than
/// [`MAX_IPC_PAYLOAD_BYTES`] (rejected before parsing), or it was not valid
/// JSON / did not match [`ToolbarCommand`]'s shape.
#[derive(Debug)]
pub enum ParseCommandError {
    /// `len` is the rejected body's byte length.
    TooLarge {
        len: usize,
    },
    Json(serde_json::Error),
}

impl std::fmt::Display for ParseCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseCommandError::TooLarge { len } => write!(
                f,
                "IPC メッセージが大きすぎます ({len} bytes > {MAX_IPC_PAYLOAD_BYTES} bytes)"
            ),
            ParseCommandError::Json(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ParseCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseCommandError::TooLarge { .. } => None,
            ParseCommandError::Json(err) => Some(err),
        }
    }
}

/// Parse a raw IPC message body into a [`ToolbarCommand`]. Rejects a body
/// over [`MAX_IPC_PAYLOAD_BYTES`] outright (see its doc comment) before
/// attempting to parse it at all.
pub fn parse_command(body: &str) -> Result<ToolbarCommand, ParseCommandError> {
    if body.len() > MAX_IPC_PAYLOAD_BYTES {
        return Err(ParseCommandError::TooLarge { len: body.len() });
    }
    serde_json::from_str(body).map_err(ParseCommandError::Json)
}

/// Escape U+2028 (LINE SEPARATOR) and U+2029 (PARAGRAPH SEPARATOR) in an
/// already-serialized JSON document before splicing it into JS source via
/// `evaluate_script`.
///
/// RFC 8259 only requires a JSON string to escape `"`, `\`, and control
/// characters — U+2028/U+2029 are allowed to appear literally — but older
/// ECMAScript grammars treated both as line terminators *even inside a
/// string literal*, so an unescaped one could end a JS string early and let
/// whatever followed run as its own statement. ES2019 fixed this for every
/// engine VeloX ships on (see docs/decisions.md D62), so this is defense in
/// depth rather than a fix for an observed break — but it costs nothing and
/// removes the dependency on that guarantee entirely. Safe to run over a
/// whole serialized JSON document (an object/array, not just one string),
/// since `\u{2028}`/`\u{2029}` can only occur inside a JSON string value to
/// begin with, never as JSON structural syntax.
///
/// `pub(crate)` (rather than private) so `ui::window` can apply the same
/// hardening to the query text it splices into the *content* webview's
/// find-in-page scripts (Issue #43, docs/decisions.md D69) instead of
/// duplicating this logic.
pub(crate) fn escape_js_line_terminators(json: &str) -> String {
    if !json.contains('\u{2028}') && !json.contains('\u{2029}') {
        return json.to_owned();
    }
    json.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// JS snippet that updates the address bar text.
///
/// The URL is embedded as a JSON string literal, so arbitrary URLs cannot
/// break out of the script.
pub fn set_url_script(url: &str) -> String {
    let json = serde_json::Value::String(url.to_owned()).to_string();
    format!("veloxSetUrl({});", escape_js_line_terminators(&json))
}

/// JS snippet that toggles the loading indicator.
pub fn set_loading_script(loading: bool) -> String {
    format!("veloxSetLoading({loading});")
}

/// JS snippet that updates the blocked-request counter badge.
pub fn set_block_count_script(count: u32) -> String {
    format!("veloxSetBlockCount({count});")
}

/// JS snippet that re-renders the tab strip from scratch.
///
/// `tabs` is embedded as a JSON array, so no tab title/URL can break out of
/// the script. Serialization only fails for types that cannot occur here
/// (e.g. non-string map keys), so a failure falls back to an empty tab strip
/// rather than panicking.
pub fn set_tabs_script(tabs: &[TabSummary]) -> String {
    let json = serde_json::to_string(tabs).unwrap_or_else(|_| "[]".to_owned());
    format!("veloxSetTabs({});", escape_js_line_terminators(&json))
}

/// JS snippet that replaces the omnibox candidate dropdown's contents.
///
/// `candidates` is embedded as a JSON array (each field of [`Candidate`] is
/// a plain string/enum tag), the same safe pattern [`set_tabs_script`] uses;
/// a serialization failure (cannot happen for this type in practice) falls
/// back to an empty list rather than panicking.
pub fn set_candidates_script(candidates: &[Candidate]) -> String {
    let json = serde_json::to_string(candidates).unwrap_or_else(|_| "[]".to_owned());
    format!("veloxSetCandidates({});", escape_js_line_terminators(&json))
}

/// JS snippet that forces the address bar's text to `url`, focuses it, and
/// selects its full contents — used for both Ctrl/Cmd+L
/// (`ToolbarCommand::FocusAddressBar`) and the Esc-restore step
/// (`ToolbarCommand::OmniboxClose`). Unlike [`set_url_script`] this always
/// overwrites the field even while it already has focus (that is the whole
/// point here), so it is a distinct JS entry point rather than a call to
/// `veloxSetUrl`.
pub fn set_focus_address_bar_script(url: &str) -> String {
    let json = serde_json::Value::String(url.to_owned()).to_string();
    format!(
        "veloxFocusAddressBar({});",
        escape_js_line_terminators(&json)
    )
}

/// JS snippet that toggles the bookmark ("star") button's active state.
pub fn set_bookmark_active_script(active: bool) -> String {
    format!("veloxSetBookmarkActive({active});")
}

/// JS snippet that toggles the always-visible private-browsing indicator
/// (see docs/decisions.md D14). Pushed once, from the toolbar's `ready`
/// handler, since whole-app private mode never changes for the life of the
/// process.
pub fn set_private_script(private: bool) -> String {
    format!("veloxSetPrivate({private});")
}

/// JS snippet that opens the given panel, or closes whichever panel is open
/// when `panel` is `None`.
pub fn set_panel_script(panel: Option<Panel>) -> String {
    let arg = match panel {
        Some(Panel::History) => "\"history\"",
        Some(Panel::Bookmarks) => "\"bookmarks\"",
        Some(Panel::Downloads) => "\"downloads\"",
        Some(Panel::Omnibox) => "\"omnibox\"",
        Some(Panel::Settings) => "\"settings\"",
        None => "null",
    };
    format!("veloxSetPanel({arg});")
}

/// JS snippet that replaces the history panel's contents, as date-grouped
/// sections (see `browser::history::group_by_date` / docs/decisions.md D29).
///
/// Groups (and their nested entries) are serialized as JSON, which embeds
/// as-is inside `evaluate_script` (an array/object JSON literal is valid JS
/// syntax); URLs and titles never need separate escaping the way
/// [`set_url_script`]'s bare string does.
pub fn set_history_script(groups: &[HistoryGroup<'_>]) -> String {
    format!("veloxSetHistory({});", entries_to_json(groups))
}

/// JS snippet that replaces the bookmarks panel's contents (folders and
/// their entries — see [`BookmarksView`]).
pub fn set_bookmarks_script(view: &BookmarksView<'_>) -> String {
    format!("veloxSetBookmarks({});", value_to_json(view))
}

/// JS snippet that replaces the always-visible bookmark bar's contents.
/// Same [`BookmarksView`] payload as [`set_bookmarks_script`] — the toolbar
/// JS renders it two different ways (a scrollable list vs. a horizontal
/// strip with per-folder dropdowns), rather than Rust building two
/// differently-shaped payloads for what is, underneath, the exact same data
/// (see docs/decisions.md D35).
pub fn set_bookmark_bar_script(view: &BookmarksView<'_>) -> String {
    format!("veloxSetBookmarkBar({});", value_to_json(view))
}

/// JS snippet that shows or hides the bookmark bar strip (Ctrl/Cmd+Shift+B /
/// `ToolbarCommand::ToggleBookmarkBar`). Purely a CSS toggle inside the
/// toolbar webview — `BrowserWindow::set_bookmark_bar_visible` is what
/// additionally resizes the webview's native bounds to match (see
/// docs/decisions.md D35).
pub fn set_bookmark_bar_visible_script(visible: bool) -> String {
    format!("veloxSetBookmarkBarVisible({visible});")
}

/// JS snippet that shows or hides the find bar (Issue #43). No dynamic text
/// is embedded here — only a `bool` — so unlike `set_find_status_script`'s
/// neighbors above, no JSON/escaping step is needed.
pub fn set_find_bar_visible_script(visible: bool) -> String {
    format!("veloxSetFindBarVisible({visible});")
}

/// JS snippet that updates the find bar's "N/M" match counter. `active` is
/// the 0-based index [`crate::browser::find::FindState::active`] reports;
/// the toolbar's own JS adds 1 for display. Both arguments are plain
/// numbers (never user-controlled text), so — like `set_find_bar_visible_script`
/// and `set_block_count_script` — this needs no JSON-embedding/escaping step.
pub fn set_find_status_script(total: usize, active: Option<usize>) -> String {
    match active {
        Some(index) => format!("veloxSetFindStatus({total}, {index});"),
        None => format!("veloxSetFindStatus({total}, null);"),
    }
}

/// JS snippet that shows (with `Some(message)`) or hides (`None`) the
/// print/PDF-export status banner (Issue #40, see docs/decisions.md D75) —
/// shared by a `print_tab` failure and the async
/// `UserEvent::PdfExportFinished` result, success or failure alike. `message`
/// is embedded as a JSON string literal, the same [`set_url_script`]-style
/// pattern (JSON-embed, then [`escape_js_line_terminators`]) every other
/// dynamic-text script here uses — a page title or a raw COM error string
/// (`windows::core::Error`'s `Display`) can contain arbitrary characters and
/// must not be able to break out of the generated script.
pub fn set_print_status_script(message: Option<&str>) -> String {
    match message {
        Some(text) => {
            let json = serde_json::Value::String(text.to_owned()).to_string();
            format!(
                "veloxSetPrintStatus({});",
                escape_js_line_terminators(&json)
            )
        }
        None => "veloxSetPrintStatus(null);".to_owned(),
    }
}

/// JS snippet that replaces the downloads panel's contents. `DownloadEntry`
/// derives `Serialize` directly (see docs/decisions.md D28) rather than
/// going through a separate wire-format struct — the same shape
/// `set_history_script`/`set_bookmarks_script` use for `HistoryEntry`/
/// `BookmarkEntry`.
pub fn set_downloads_script(entries: &[&DownloadEntry]) -> String {
    format!("veloxSetDownloads({});", entries_to_json(entries))
}

/// Serialize `entries` to a JSON array text, with the same
/// U+2028/U+2029-escaping [`set_url_script`] applies to its own string, and
/// the same "cannot actually fail for these types, but never panic if it
/// somehow did" fallback every other `set_*_script` function here uses.
/// JS snippet that replaces the settings screen's contents (Issue #30, see
/// [`SettingsView`] and docs/decisions.md D67). Pushed on `ready` and again
/// after every `update_settings`/`reset_settings` command, so the form
/// always echoes back what was actually persisted (post-`sanitize`), not
/// just what the user typed.
pub fn set_settings_script(view: &SettingsView<'_>) -> String {
    format!("veloxSetSettings({});", value_to_json(view))
}

/// JS snippet that applies the chrome (toolbar/tab-strip) theme override
/// (Issue #30's Appearance tab — see docs/decisions.md D67). Unlike every
/// other settings field this is pushed immediately on
/// `update_settings`/`reset_settings`, not only after a restart — it only
/// ever touches this webview's own `data-velox-theme` attribute (see
/// `ui/toolbar.html`'s `:root`/`[data-velox-theme]` CSS), never web page
/// content, which wry 0.56 exposes no per-webview `prefers-color-scheme`
/// override for.
pub fn set_theme_script(theme: Theme) -> String {
    format!("veloxSetTheme(\"{}\");", theme.as_str())
}

fn entries_to_json<T: Serialize>(entries: &[T]) -> String {
    let json = serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_owned());
    escape_js_line_terminators(&json)
}

/// Same shape and reasoning as [`entries_to_json`], for a single (non-slice)
/// value such as [`BookmarksView`].
fn value_to_json<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    escape_js_line_terminators(&json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{CandidateKind, HistoryDateBucket, HistoryEntry};

    #[test]
    fn parses_navigate_command() {
        let cmd = parse_command(r#"{"cmd":"navigate","input":"example.com"}"#).unwrap();
        assert_eq!(
            cmd,
            ToolbarCommand::Navigate {
                input: "example.com".to_owned()
            }
        );
    }

    #[test]
    fn parses_plain_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"back"}"#).unwrap(),
            ToolbarCommand::Back
        );
        assert_eq!(
            parse_command(r#"{"cmd":"forward"}"#).unwrap(),
            ToolbarCommand::Forward
        );
        assert_eq!(
            parse_command(r#"{"cmd":"reload"}"#).unwrap(),
            ToolbarCommand::Reload
        );
        assert_eq!(
            parse_command(r#"{"cmd":"new_tab"}"#).unwrap(),
            ToolbarCommand::NewTab
        );
        assert_eq!(
            parse_command(r#"{"cmd":"ready"}"#).unwrap(),
            ToolbarCommand::Ready
        );
        assert_eq!(
            parse_command(r#"{"cmd":"script_started"}"#).unwrap(),
            ToolbarCommand::ScriptStarted
        );
        assert_eq!(
            parse_command(r#"{"cmd":"open_devtools"}"#).unwrap(),
            ToolbarCommand::OpenDevtools
        );
    }

    #[test]
    fn parses_tab_commands_with_ids() {
        assert_eq!(
            parse_command(r#"{"cmd":"close_tab","id":3}"#).unwrap(),
            ToolbarCommand::CloseTab { id: 3 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"activate_tab","id":42}"#).unwrap(),
            ToolbarCommand::ActivateTab { id: 42 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"suspend_tab","id":7}"#).unwrap(),
            ToolbarCommand::SuspendTab { id: 7 }
        );
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(parse_command(r#"{"cmd":"self_destruct"}"#).is_err());
        assert!(parse_command("not json").is_err());
    }

    // --- IPC robustness (Issue #35): malformed JSON, huge/deeply-nested
    // payloads, and wrong-shaped fields must all be a clean `Err`, never a
    // panic. The toolbar webview is VeloX's own trusted, bundled HTML, not
    // attacker-controlled content, but `parse_command` is still the one
    // trust boundary between "whatever `window.ipc.postMessage` sent" and
    // real `ToolbarCommand` values — see docs/decisions.md D62. ---

    #[test]
    fn does_not_panic_on_a_grab_bag_of_malformed_ipc_bodies() {
        let bodies = [
            "",
            "   ",
            "{",
            "}",
            "[",
            "null",
            "true",
            "42",
            "\"just a string\"",
            "[1,2,3]",
            "{}",
            r#"{"cmd":null}"#,
            r#"{"cmd":123}"#,
            r#"{"cmd":"navigate"}"#,            // missing required `input`
            r#"{"cmd":"navigate","input":42}"#, // wrong field type
            r#"{"cmd":"navigate","input":null}"#,
            r#"{"cmd":"close_tab","id":"not-a-number"}"#,
            r#"{"cmd":"close_tab","id":-1}"#,
            r#"{"cmd":"close_tab","id":1.5}"#,
            r#"{"cmd":"back","extra_field":"unexpected"}"#, // deny_unknown_fields
            r#"{"CMD":"back"}"#,                            // wrong key case
            "\u{0}\u{0}\u{0}",
            "{\"cmd\":\"navigate\",\"input\":\"\u{0}\"}",
            "not json at all, just text",
            "{\"cmd\": \"navigate\", \"input\": \"a\nb\"}",
        ];
        for body in bodies {
            // Must return, not panic, whichever way it resolves.
            let _ = parse_command(body);
        }
    }

    #[test]
    fn rejects_a_payload_over_the_ipc_size_cap() {
        // A `navigate` command whose `input` alone is comfortably past
        // `MAX_IPC_PAYLOAD_BYTES` — must be rejected as `TooLarge` before
        // `serde_json` ever tries to parse it, and must not panic or hang.
        let huge_input = "a".repeat(MAX_IPC_PAYLOAD_BYTES + 1);
        let body = format!(r#"{{"cmd":"navigate","input":"{huge_input}"}}"#);
        assert!(body.len() > MAX_IPC_PAYLOAD_BYTES);
        match parse_command(&body) {
            Err(ParseCommandError::TooLarge { len }) => assert_eq!(len, body.len()),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn accepts_a_large_payload_comfortably_under_the_cap() {
        // A generous but legitimate payload (e.g. a large clipboard paste
        // into the address bar) must still parse normally — the cap exists
        // to reject pathological sizes, not to second-guess ordinary input.
        let big_input = "a".repeat(MAX_IPC_PAYLOAD_BYTES / 4);
        let body = format!(r#"{{"cmd":"navigate","input":"{big_input}"}}"#);
        assert!(body.len() < MAX_IPC_PAYLOAD_BYTES);
        assert_eq!(
            parse_command(&body).unwrap(),
            ToolbarCommand::Navigate { input: big_input }
        );
    }

    #[test]
    fn size_cap_error_message_mentions_the_limit() {
        let huge_input = "a".repeat(MAX_IPC_PAYLOAD_BYTES + 1);
        let body = format!(r#"{{"cmd":"navigate","input":"{huge_input}"}}"#);
        let err = parse_command(&body).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&MAX_IPC_PAYLOAD_BYTES.to_string()),
            "{message}"
        );
    }

    #[test]
    fn does_not_panic_or_hang_on_deeply_nested_json() {
        // A JSON "bomb": thousands of nested arrays. `ToolbarCommand` could
        // never actually be shaped like this, but the parser still has to
        // walk (or reject) the structure before it can say so — this must
        // come back as an `Err` (serde_json's own recursion-depth guard),
        // never a stack overflow.
        let depth = 100_000;
        let body = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        assert!(parse_command(&body).is_err());

        // Same shape, but nested objects instead of arrays.
        let nested_objects = format!(
            "{}{}",
            r#"{"a":"#.repeat(depth),
            "1".to_owned() + &"}".repeat(depth)
        );
        assert!(parse_command(&nested_objects).is_err());
    }

    #[test]
    fn handles_unicode_and_control_characters_in_command_fields_without_panicking() {
        let bodies = [
            r#"{"cmd":"navigate","input":"日本語のURL.example/パス"}"#,
            r#"{"cmd":"navigate","input":"🚀🔥emoji.example/"}"#,
            "{\"cmd\":\"navigate\",\"input\":\"\u{202e}reversed-looking.example/\"}",
            r#"{"cmd":"search_history","query":"line1\nline2\ttabbed"}"#,
            r#"{"cmd":"create_bookmark_folder","name":"  "}"#,
        ];
        for body in bodies {
            // Every one of these is well-formed JSON with the right field
            // types, so it must parse successfully and never panic.
            assert!(parse_command(body).is_ok(), "{body}");
        }
    }

    #[test]
    fn parses_bookmark_and_panel_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_bookmark"}"#).unwrap(),
            ToolbarCommand::ToggleBookmark
        );
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_panel","panel":"history"}"#).unwrap(),
            ToolbarCommand::TogglePanel {
                panel: Panel::History
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_panel","panel":"bookmarks"}"#).unwrap(),
            ToolbarCommand::TogglePanel {
                panel: Panel::Bookmarks
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"delete_history_entry","id":42}"#).unwrap(),
            ToolbarCommand::DeleteHistoryEntry { id: 42 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"clear_history"}"#).unwrap(),
            ToolbarCommand::ClearHistory
        );
        assert_eq!(
            parse_command(r#"{"cmd":"clear_site_data"}"#).unwrap(),
            ToolbarCommand::ClearSiteData
        );
        assert_eq!(
            parse_command(r#"{"cmd":"search_history","query":"rust"}"#).unwrap(),
            ToolbarCommand::SearchHistory {
                query: "rust".to_owned()
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"search_history","query":""}"#).unwrap(),
            ToolbarCommand::SearchHistory {
                query: String::new()
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"remove_bookmark","id":7}"#).unwrap(),
            ToolbarCommand::RemoveBookmark { id: 7 }
        );
    }

    #[test]
    fn parses_keyboard_shortcut_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"close_active_tab"}"#).unwrap(),
            ToolbarCommand::CloseActiveTab
        );
        assert_eq!(
            parse_command(r#"{"cmd":"reopen_closed_tab"}"#).unwrap(),
            ToolbarCommand::ReopenClosedTab
        );
        assert_eq!(
            parse_command(r#"{"cmd":"next_tab"}"#).unwrap(),
            ToolbarCommand::NextTab
        );
        assert_eq!(
            parse_command(r#"{"cmd":"prev_tab"}"#).unwrap(),
            ToolbarCommand::PrevTab
        );
        assert_eq!(
            parse_command(r#"{"cmd":"activate_tab_by_index","index":3}"#).unwrap(),
            ToolbarCommand::ActivateTabByIndex { index: 3 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"activate_last_tab"}"#).unwrap(),
            ToolbarCommand::ActivateLastTab
        );
        assert_eq!(
            parse_command(r#"{"cmd":"new_window"}"#).unwrap(),
            ToolbarCommand::NewWindow
        );
    }

    #[test]
    fn parses_omnibox_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"focus_address_bar"}"#).unwrap(),
            ToolbarCommand::FocusAddressBar
        );
        assert_eq!(
            parse_command(r#"{"cmd":"omnibox_input","input":"rust ownership"}"#).unwrap(),
            ToolbarCommand::OmniboxInput {
                input: "rust ownership".to_owned()
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"omnibox_close"}"#).unwrap(),
            ToolbarCommand::OmniboxClose
        );
    }

    #[test]
    fn url_script_escapes_quotes_and_backslashes() {
        let script = set_url_script(r#"https://example.com/?q="a"\b"#);
        assert_eq!(script, r#"veloxSetUrl("https://example.com/?q=\"a\"\\b");"#);
    }

    // --- JS injection hardening (Issue #35): a URL/title/etc. containing a
    // JS/HTML-meaningful sequence must end up embedded as inert JSON string
    // content, never something that changes what statement runs. See
    // docs/decisions.md D62. ---

    #[test]
    fn url_script_neutralizes_script_closing_and_html_sequences() {
        // `evaluate_script` hands this straight to the JS engine, not the
        // HTML parser, so `</script>` has no special meaning here — but it
        // must still come through as inert string content, not break the
        // surrounding `veloxSetUrl(...)` call.
        let script = set_url_script(r#"https://example.com/</script><script>alert(1)</script>"#);
        assert!(script.starts_with("veloxSetUrl(\""));
        assert!(script.ends_with("\");"));
        assert!(script.contains(r#"</script><script>alert(1)</script>"#));
    }

    #[test]
    fn url_script_escapes_u2028_and_u2029_line_terminators() {
        // U+2028/U+2029 are valid, unescaped JSON string content (RFC 8259)
        // but were historically JS statement terminators even inside a
        // string literal -- must come through as the literal escape
        // sequence, never the raw codepoint, so the string can never end
        // early no matter which engine evaluates it (see D62).
        let script = set_url_script("https://example.com/\u{2028}payload\u{2029}");
        assert!(script.contains("\\u2028"), "{script}");
        assert!(script.contains("\\u2029"), "{script}");
        assert!(!script.contains('\u{2028}'));
        assert!(!script.contains('\u{2029}'));
    }

    #[test]
    fn focus_address_bar_script_escapes_u2028_and_u2029() {
        let script = set_focus_address_bar_script("https://example.com/\u{2028}\u{2029}");
        assert!(script.contains("\\u2028"), "{script}");
        assert!(script.contains("\\u2029"), "{script}");
        assert!(!script.contains('\u{2028}'));
        assert!(!script.contains('\u{2029}'));
    }

    #[test]
    fn tabs_script_escapes_u2028_and_u2029_in_titles() {
        let tabs = vec![TabSummary {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: Some("line one\u{2028}line two\u{2029}line three".to_owned()),
            favicon: None,
            loading: false,
            active: true,
            suspended: false,
        }];
        let script = set_tabs_script(&tabs);
        assert!(script.contains("\\u2028"), "{script}");
        assert!(script.contains("\\u2029"), "{script}");
        assert!(!script.contains('\u{2028}'));
        assert!(!script.contains('\u{2029}'));
    }

    #[test]
    fn history_script_escapes_u2028_and_u2029_in_titles_and_urls() {
        let entry = HistoryEntry {
            id: 1,
            url: "https://example.com/\u{2028}".to_owned(),
            title: Some("title\u{2029}with separator".to_owned()),
            visited_at: 1,
            favicon: None,
            visit_count: 1,
        };
        let groups = [HistoryGroup {
            bucket: HistoryDateBucket::Today,
            entries: vec![&entry],
        }];
        let script = set_history_script(&groups);
        assert!(script.contains("\\u2028"), "{script}");
        assert!(script.contains("\\u2029"), "{script}");
        assert!(!script.contains('\u{2028}'));
        assert!(!script.contains('\u{2029}'));
    }

    #[test]
    fn loading_script_is_a_bool_literal() {
        assert_eq!(set_loading_script(true), "veloxSetLoading(true);");
        assert_eq!(set_loading_script(false), "veloxSetLoading(false);");
    }

    #[test]
    fn block_count_script_is_an_int_literal() {
        assert_eq!(set_block_count_script(0), "veloxSetBlockCount(0);");
        assert_eq!(set_block_count_script(42), "veloxSetBlockCount(42);");
    }

    #[test]
    fn tabs_script_embeds_a_json_array() {
        let tabs = vec![
            TabSummary {
                id: 1,
                url: "https://a.example/".to_owned(),
                title: Some("A\"s page".to_owned()),
                favicon: Some("https://a.example/favicon.ico".to_owned()),
                loading: false,
                active: true,
                suspended: false,
            },
            TabSummary {
                id: 2,
                url: "https://b.example/?q=\"x\"".to_owned(),
                title: None,
                favicon: None,
                loading: true,
                active: false,
                suspended: false,
            },
            TabSummary {
                id: 3,
                url: "https://c.example/".to_owned(),
                title: None,
                favicon: None,
                loading: false,
                active: false,
                suspended: true,
            },
        ];
        let script = set_tabs_script(&tabs);
        assert!(script.starts_with("veloxSetTabs(["));
        assert!(script.ends_with("]);"));
        // Quotes inside a URL must be escaped, not break out of the array.
        assert!(script.contains(r#"\"x\""#));
        assert!(script.contains(r#""suspended":true"#));
        // A title containing a quote is escaped the same way, not broken out.
        assert!(script.contains(r#"A\"s page"#));
        assert!(script.contains(r#""favicon":"https://a.example/favicon.ico""#));
        // No title/favicon yet serializes as JSON null, not an empty string.
        assert!(script.contains(r#""title":null"#));
        assert!(script.contains(r#""favicon":null"#));

        let empty = set_tabs_script(&[]);
        assert_eq!(empty, "veloxSetTabs([]);");
    }

    #[test]
    fn candidates_script_embeds_a_json_array() {
        let candidates = vec![
            Candidate {
                kind: CandidateKind::NavigateUrl,
                target_url: "https://example.com/".to_owned(),
                label: "https://example.com/".to_owned(),
                detail: None,
            },
            Candidate {
                kind: CandidateKind::Search,
                target_url: "https://duckduckgo.com/?q=a%22b".to_owned(),
                label: r#"a"b"#.to_owned(),
                detail: Some("DuckDuckGo で検索".to_owned()),
            },
        ];
        let script = set_candidates_script(&candidates);
        assert!(script.starts_with("veloxSetCandidates(["));
        assert!(script.ends_with("]);"));
        assert!(script.contains(r#""kind":"navigate_url""#));
        assert!(script.contains(r#""kind":"search""#));
        // A quote inside a label must be escaped, not break out of the
        // array, the same JSON-embedding guarantee `set_tabs_script` gives.
        assert!(script.contains(r#"a\"b"#));
        assert!(script.contains(r#""detail":null"#));

        assert_eq!(set_candidates_script(&[]), "veloxSetCandidates([]);");
    }

    #[test]
    fn focus_address_bar_script_escapes_quotes_and_backslashes() {
        let script = set_focus_address_bar_script(r#"https://example.com/?q="a"\b"#);
        assert_eq!(
            script,
            r#"veloxFocusAddressBar("https://example.com/?q=\"a\"\\b");"#
        );
    }

    #[test]
    fn bookmark_active_script_is_a_bool_literal() {
        assert_eq!(
            set_bookmark_active_script(true),
            "veloxSetBookmarkActive(true);"
        );
        assert_eq!(
            set_bookmark_active_script(false),
            "veloxSetBookmarkActive(false);"
        );
    }

    #[test]
    fn private_script_is_a_bool_literal() {
        assert_eq!(set_private_script(true), "veloxSetPrivate(true);");
        assert_eq!(set_private_script(false), "veloxSetPrivate(false);");
    }

    #[test]
    fn panel_script_names_the_open_panel_or_null() {
        assert_eq!(
            set_panel_script(Some(Panel::History)),
            "veloxSetPanel(\"history\");"
        );
        assert_eq!(
            set_panel_script(Some(Panel::Bookmarks)),
            "veloxSetPanel(\"bookmarks\");"
        );
        assert_eq!(
            set_panel_script(Some(Panel::Omnibox)),
            "veloxSetPanel(\"omnibox\");"
        );
        assert_eq!(set_panel_script(None), "veloxSetPanel(null);");
    }

    #[test]
    fn history_script_embeds_groups_and_entries_as_json() {
        let entry = HistoryEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: Some("Example".to_owned()),
            visited_at: 100,
            favicon: Some("https://example.com/favicon.ico".to_owned()),
            visit_count: 3,
        };
        let groups = [HistoryGroup {
            bucket: HistoryDateBucket::Today,
            entries: vec![&entry],
        }];
        let script = set_history_script(&groups);
        assert!(script.starts_with("veloxSetHistory("));
        assert!(script.contains(r#""bucket":"today""#));
        assert!(script.contains(r#""url":"https://example.com/""#));
        assert!(script.contains(r#""title":"Example""#));
        assert!(script.contains(r#""favicon":"https://example.com/favicon.ico""#));
        assert!(script.contains(r#""visit_count":3"#));
    }

    #[test]
    fn history_script_escapes_untrusted_title_and_url_content() {
        let entry = HistoryEntry {
            id: 1,
            url: r#"https://example.com/?q="a"</script>"#.to_owned(),
            title: Some(r#"a"b\c"#.to_owned()),
            visited_at: 1,
            favicon: None,
            visit_count: 1,
        };
        let groups = [HistoryGroup {
            bucket: HistoryDateBucket::Older,
            entries: vec![&entry],
        }];
        let script = set_history_script(&groups);
        // A double quote inside a JSON string value must be escaped, so the
        // string never terminates early.
        assert!(script.contains(r#"\"a\""#));
        assert!(script.contains(r#"a\"b\\c"#));
    }

    #[test]
    fn bookmarks_script_embeds_the_view_as_json() {
        let entry = BookmarkEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: None,
            created_at: 100,
            folder_id: None,
            favicon: None,
        };
        let view = BookmarksView {
            root: vec![&entry],
            folders: vec![],
        };
        let script = set_bookmarks_script(&view);
        assert!(script.starts_with("veloxSetBookmarks("));
        assert!(script.contains(r#""url":"https://example.com/""#));
        assert!(script.contains(r#""title":null"#));
        assert!(script.contains(r#""root":["#));
        assert!(script.contains(r#""folders":[]"#));
    }

    #[test]
    fn bookmark_bar_script_embeds_the_view_and_folders_with_their_own_entries() {
        let folder_entry = BookmarkEntry {
            id: 2,
            url: "https://b.example/".to_owned(),
            title: Some("B".to_owned()),
            created_at: 200,
            folder_id: Some(9),
            favicon: Some("https://b.example/favicon.ico".to_owned()),
        };
        let view = BookmarksView {
            root: vec![],
            folders: vec![BookmarkFolderView {
                id: 9,
                name: "仕事",
                entries: vec![&folder_entry],
            }],
        };
        let script = set_bookmark_bar_script(&view);
        assert!(script.starts_with("veloxSetBookmarkBar("));
        assert!(script.contains(r#""name":"仕事""#));
        assert!(script.contains(r#""url":"https://b.example/""#));
        assert!(script.contains(r#""favicon":"https://b.example/favicon.ico""#));
    }

    #[test]
    fn bookmark_bar_visible_script_is_a_bool_literal() {
        assert_eq!(
            set_bookmark_bar_visible_script(true),
            "veloxSetBookmarkBarVisible(true);"
        );
        assert_eq!(
            set_bookmark_bar_visible_script(false),
            "veloxSetBookmarkBarVisible(false);"
        );
    }

    #[test]
    fn empty_entry_list_serializes_to_an_empty_array() {
        assert_eq!(set_history_script(&[]), "veloxSetHistory([]);".to_owned());
        let empty_view = BookmarksView {
            root: vec![],
            folders: vec![],
        };
        assert_eq!(
            set_bookmarks_script(&empty_view),
            r#"veloxSetBookmarks({"root":[],"folders":[]});"#.to_owned()
        );
    }

    #[test]
    fn parses_bookmark_folder_edit_and_reorder_commands() {
        assert_eq!(
            parse_command(
                r#"{"cmd":"edit_bookmark","id":1,"title":"New","url":"https://example.com/","folder_id":2}"#
            )
            .unwrap(),
            ToolbarCommand::EditBookmark {
                id: 1,
                title: "New".to_owned(),
                url: "https://example.com/".to_owned(),
                folder_id: Some(2),
            }
        );
        assert_eq!(
            parse_command(
                r#"{"cmd":"edit_bookmark","id":1,"title":"","url":"https://example.com/","folder_id":null}"#
            )
            .unwrap(),
            ToolbarCommand::EditBookmark {
                id: 1,
                title: String::new(),
                url: "https://example.com/".to_owned(),
                folder_id: None,
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"create_bookmark_folder","name":"仕事"}"#).unwrap(),
            ToolbarCommand::CreateBookmarkFolder {
                name: "仕事".to_owned()
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"rename_bookmark_folder","id":3,"name":"新名前"}"#).unwrap(),
            ToolbarCommand::RenameBookmarkFolder {
                id: 3,
                name: "新名前".to_owned()
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"remove_bookmark_folder","id":3}"#).unwrap(),
            ToolbarCommand::RemoveBookmarkFolder { id: 3 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"move_bookmark_up","id":5}"#).unwrap(),
            ToolbarCommand::MoveBookmarkUp { id: 5 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"move_bookmark_down","id":5}"#).unwrap(),
            ToolbarCommand::MoveBookmarkDown { id: 5 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_bookmark_bar"}"#).unwrap(),
            ToolbarCommand::ToggleBookmarkBar
        );
    }

    #[test]
    fn parses_download_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"open_download","id":9}"#).unwrap(),
            ToolbarCommand::OpenDownload { id: 9 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"open_downloads_folder"}"#).unwrap(),
            ToolbarCommand::OpenDownloadsFolder
        );
        assert_eq!(
            parse_command(r#"{"cmd":"cancel_download","id":9}"#).unwrap(),
            ToolbarCommand::CancelDownload { id: 9 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"remove_download_entry","id":9}"#).unwrap(),
            ToolbarCommand::RemoveDownloadEntry { id: 9 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_panel","panel":"downloads"}"#).unwrap(),
            ToolbarCommand::TogglePanel {
                panel: Panel::Downloads
            }
        );
    }

    #[test]
    fn parses_find_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"open_find_bar"}"#).unwrap(),
            ToolbarCommand::OpenFindBar
        );
        assert_eq!(
            parse_command(r#"{"cmd":"find_query","query":"foo","case_sensitive":false}"#).unwrap(),
            ToolbarCommand::FindQuery {
                query: "foo".to_owned(),
                case_sensitive: false,
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"find_query","query":"","case_sensitive":true}"#).unwrap(),
            ToolbarCommand::FindQuery {
                query: String::new(),
                case_sensitive: true,
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"find_next"}"#).unwrap(),
            ToolbarCommand::FindNext
        );
        assert_eq!(
            parse_command(r#"{"cmd":"find_previous"}"#).unwrap(),
            ToolbarCommand::FindPrevious
        );
        assert_eq!(
            parse_command(r#"{"cmd":"find_close"}"#).unwrap(),
            ToolbarCommand::FindClose
        );
    }

    #[test]
    fn parses_print_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"print"}"#).unwrap(),
            ToolbarCommand::Print
        );
        assert_eq!(
            parse_command(r#"{"cmd":"save_as_pdf"}"#).unwrap(),
            ToolbarCommand::SaveAsPdf
        );
    }

    #[test]
    fn parses_view_source_command() {
        assert_eq!(
            parse_command(r#"{"cmd":"view_source"}"#).unwrap(),
            ToolbarCommand::ViewSource
        );
    }

    #[test]
    fn print_status_script_embeds_message_as_json_or_null() {
        assert_eq!(
            set_print_status_script(Some("PDFとして保存しました")),
            "veloxSetPrintStatus(\"PDFとして保存しました\");"
        );
        assert_eq!(set_print_status_script(None), "veloxSetPrintStatus(null);");
    }

    #[test]
    fn print_status_script_neutralizes_quotes_and_backslashes() {
        let script = set_print_status_script(Some(r#""a"\b"#));
        assert_eq!(script, r#"veloxSetPrintStatus("\"a\"\\b");"#);
    }

    #[test]
    fn print_status_script_neutralizes_line_terminators() {
        // Same D62 hardening every other dynamic-text `set_*_script`
        // function here applies (see `escape_js_line_terminators`'s doc
        // comment) — a COM error string or page title could contain either
        // character.
        let script = set_print_status_script(Some("foo\u{2028}bar\u{2029}"));
        assert!(script.contains("\\u2028"), "{script}");
        assert!(script.contains("\\u2029"), "{script}");
        assert!(!script.contains('\u{2028}'));
        assert!(!script.contains('\u{2029}'));
    }

    #[test]
    fn find_bar_visible_script_embeds_bool_only() {
        assert_eq!(
            set_find_bar_visible_script(true),
            "veloxSetFindBarVisible(true);"
        );
        assert_eq!(
            set_find_bar_visible_script(false),
            "veloxSetFindBarVisible(false);"
        );
    }

    #[test]
    fn find_status_script_embeds_numbers_and_null() {
        assert_eq!(
            set_find_status_script(5, Some(2)),
            "veloxSetFindStatus(5, 2);"
        );
        assert_eq!(
            set_find_status_script(0, None),
            "veloxSetFindStatus(0, null);"
        );
    }

    #[test]
    fn downloads_panel_script_names_the_panel() {
        assert_eq!(
            set_panel_script(Some(Panel::Downloads)),
            "veloxSetPanel(\"downloads\");"
        );
    }

    #[test]
    fn downloads_script_embeds_entries_as_json() {
        use crate::browser::{DownloadId, DownloadState};

        let entry = DownloadEntry {
            id: DownloadId::from(1),
            url: "https://example.com/report.pdf".to_owned(),
            file_name: "report.pdf".to_owned(),
            destination: std::path::PathBuf::from("/home/user/Downloads/report.pdf"),
            state: DownloadState::InProgress,
            started_at: 100,
            finished_at: None,
            error: None,
        };
        let script = set_downloads_script(&[&entry]);
        assert!(script.starts_with("veloxSetDownloads(["));
        assert!(script.contains(r#""url":"https://example.com/report.pdf""#));
        assert!(script.contains(r#""file_name":"report.pdf""#));
        assert!(script.contains(r#""state":"in_progress""#));
        assert!(script.contains(r#""destination":"/home/user/Downloads/report.pdf""#));
        assert!(script.contains(r#""error":null"#));

        let empty = set_downloads_script(&[]);
        assert_eq!(empty, "veloxSetDownloads([]);".to_owned());
    }

    // --- Settings screen (Issue #30, D67) ---

    #[test]
    fn parses_settings_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_panel","panel":"settings"}"#).unwrap(),
            ToolbarCommand::TogglePanel {
                panel: Panel::Settings
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"reset_settings"}"#).unwrap(),
            ToolbarCommand::ResetSettings
        );
        let json = format!(
            r#"{{"cmd":"update_settings","settings":{}}}"#,
            serde_json::to_string(&crate::browser::Settings::default()).unwrap()
        );
        assert_eq!(
            parse_command(&json).unwrap(),
            ToolbarCommand::UpdateSettings {
                settings: Box::new(crate::browser::Settings::default())
            }
        );
    }

    #[test]
    fn settings_panel_script_names_the_panel() {
        assert_eq!(
            set_panel_script(Some(Panel::Settings)),
            "veloxSetPanel(\"settings\");"
        );
    }

    #[test]
    fn settings_script_embeds_the_view_as_json() {
        let settings = crate::browser::Settings::default();
        let shortcuts = crate::browser::shortcut_reference();
        let view = SettingsView {
            settings: &settings,
            shortcuts,
            site_permissions: &[],
        };
        let script = set_settings_script(&view);
        assert!(script.starts_with("veloxSetSettings({"));
        assert!(script.contains(r#""schema_version""#));
        assert!(script.contains(r#""shortcuts""#));
        assert!(script.contains(r#""site_permissions":[]"#));
    }

    #[test]
    fn theme_script_names_each_variant() {
        assert_eq!(
            set_theme_script(Theme::System),
            "veloxSetTheme(\"system\");"
        );
        assert_eq!(set_theme_script(Theme::Light), "veloxSetTheme(\"light\");");
        assert_eq!(set_theme_script(Theme::Dark), "veloxSetTheme(\"dark\");");
    }

    #[test]
    fn toolbar_html_declares_expected_hooks() {
        assert!(TOOLBAR_HTML.contains("veloxSetUrl"));
        assert!(TOOLBAR_HTML.contains("veloxSetLoading"));
        assert!(TOOLBAR_HTML.contains("veloxSetBlockCount"));
        assert!(TOOLBAR_HTML.contains("veloxSetTabs"));
        assert!(TOOLBAR_HTML.contains("veloxSetBookmarkActive"));
        assert!(TOOLBAR_HTML.contains("veloxSetPrivate"));
        assert!(TOOLBAR_HTML.contains("veloxSetPanel"));
        assert!(TOOLBAR_HTML.contains("veloxSetHistory"));
        assert!(TOOLBAR_HTML.contains("veloxSetBookmarks"));
        assert!(TOOLBAR_HTML.contains("ipc.postMessage"));
        assert!(TOOLBAR_HTML.contains("new_tab"));
        assert!(TOOLBAR_HTML.contains("close_tab"));
        assert!(TOOLBAR_HTML.contains("activate_tab"));
        assert!(TOOLBAR_HTML.contains("suspend_tab"));
        assert!(TOOLBAR_HTML.contains("toggle_bookmark"));
        assert!(TOOLBAR_HTML.contains("toggle_panel"));
        assert!(TOOLBAR_HTML.contains("delete_history_entry"));
        assert!(TOOLBAR_HTML.contains("clear_history"));
        // Site data (cookies/cache/storage) clearing, Issue #26 (D66).
        assert!(TOOLBAR_HTML.contains("clear_site_data"));
        assert!(TOOLBAR_HTML.contains("search_history"));
        assert!(TOOLBAR_HTML.contains("remove_bookmark"));
        // Keyboard shortcuts (see docs/decisions.md D23): the toolbar's own
        // capture-phase keydown listener, for when the address bar/panel
        // (not the content webview) has focus.
        assert!(TOOLBAR_HTML.contains("close_active_tab"));
        assert!(TOOLBAR_HTML.contains("reopen_closed_tab"));
        assert!(TOOLBAR_HTML.contains("next_tab"));
        assert!(TOOLBAR_HTML.contains("prev_tab"));
        assert!(TOOLBAR_HTML.contains("activate_tab_by_index"));
        assert!(TOOLBAR_HTML.contains("activate_last_tab"));
        // New window (Ctrl/Cmd+N, Issue #29, see docs/decisions.md D68).
        assert!(TOOLBAR_HTML.contains("new_window"));
        // Downloads (Issue #16, see docs/decisions.md D28).
        assert!(TOOLBAR_HTML.contains("veloxSetDownloads"));
        assert!(TOOLBAR_HTML.contains("open_download"));
        assert!(TOOLBAR_HTML.contains("open_downloads_folder"));
        assert!(TOOLBAR_HTML.contains("cancel_download"));
        assert!(TOOLBAR_HTML.contains("remove_download_entry"));
        // Omnibox (Issue #15): Ctrl/Cmd+L, the candidate dropdown, and Esc.
        assert!(TOOLBAR_HTML.contains("veloxSetCandidates"));
        assert!(TOOLBAR_HTML.contains("veloxFocusAddressBar"));
        assert!(TOOLBAR_HTML.contains("focus_address_bar"));
        assert!(TOOLBAR_HTML.contains("omnibox_input"));
        assert!(TOOLBAR_HTML.contains("omnibox_close"));
        // Bookmark folders, editing, reordering, and the bookmark bar
        // (Issue #19, see docs/decisions.md D32/D33/D34/D35).
        assert!(TOOLBAR_HTML.contains("veloxSetBookmarkBar"));
        assert!(TOOLBAR_HTML.contains("veloxSetBookmarkBarVisible"));
        assert!(TOOLBAR_HTML.contains("edit_bookmark"));
        assert!(TOOLBAR_HTML.contains("create_bookmark_folder"));
        assert!(TOOLBAR_HTML.contains("rename_bookmark_folder"));
        assert!(TOOLBAR_HTML.contains("remove_bookmark_folder"));
        assert!(TOOLBAR_HTML.contains("move_bookmark_up"));
        assert!(TOOLBAR_HTML.contains("move_bookmark_down"));
        assert!(TOOLBAR_HTML.contains("toggle_bookmark_bar"));
        // Settings screen (Issue #30, see docs/decisions.md D67).
        assert!(TOOLBAR_HTML.contains("veloxSetSettings"));
        assert!(TOOLBAR_HTML.contains("veloxSetTheme"));
        assert!(TOOLBAR_HTML.contains("update_settings"));
        assert!(TOOLBAR_HTML.contains("reset_settings"));
        assert!(TOOLBAR_HTML.contains("settings-toggle"));

        // Print / PDF export (Issue #40, see docs/decisions.md D75).
        assert!(TOOLBAR_HTML.contains("veloxSetPrintStatus"));
        assert!(TOOLBAR_HTML.contains("\"print\""));
        assert!(TOOLBAR_HTML.contains("save_as_pdf"));
    }

    /// Issue #31/D71: an explicit Light/Dark theme override must reach the
    /// Private Window colors too, not just the non-private ones — regression
    /// test for the bug where `--private-bg`/`--private-fg`/
    /// `--private-field-bg`/`--private-border` were only ever defined by the
    /// plain `:root`/`@media (prefers-color-scheme: dark)` rules, leaving a
    /// Private Window's colors tied to the OS theme even when the user had
    /// explicitly picked the other one via `data-velox-theme`.
    #[test]
    fn private_theme_variables_are_overridden_by_an_explicit_light_or_dark_theme() {
        let light_block = TOOLBAR_HTML
            .split(":root[data-velox-theme=\"light\"] {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("a data-velox-theme=\"light\" override block must exist");
        let dark_block = TOOLBAR_HTML
            .split(":root[data-velox-theme=\"dark\"] {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("a data-velox-theme=\"dark\" override block must exist");
        for var in [
            "--private-bg",
            "--private-fg",
            "--private-field-bg",
            "--private-border",
        ] {
            assert!(
                light_block.contains(var),
                "light theme override block is missing {var}"
            );
            assert!(
                dark_block.contains(var),
                "dark theme override block is missing {var}"
            );
        }
    }
}
