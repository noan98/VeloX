# VeloX Architecture

Status: single window, multiple tabs, tab suspension, visit history,
bookmarks, and content blocking. This document describes what exists today
and where the extension points are.

## Overall structure

VeloX separates responsibilities into four layers. Higher layers depend on
lower ones, never the other way around:

```
┌───────────────────────────────────────────────┐
│ UI            src/ui/                         │  window, layout, toolbar
├───────────────────────────────────────────────┤
│ Application   src/app.rs                      │  event loop, wiring
├───────────────────────────────────────────────┤
│ Browser logic src/browser/                    │  URL handling, tab state
├───────────────────────────────────────────────┤
│ Web engine    wry (WebKitGTK/WKWebView/WebView2) │  rendering, session history
└───────────────────────────────────────────────┘
```

`src/browser/` and `src/config/` are pure Rust with no UI or engine types, so
they are unit-tested directly. `src/ui/` owns everything that touches wry/tao.

## Why a webview toolbar?

The window hosts a shared toolbar webview plus **one content webview per open
tab**:

```
┌──────────────────────────────────────┐
│ [ Tab A ] [ Tab B ] [ Tab C ]  +     │  tab strip
│ ←  →  ↻   [ https://example.com ]    │  address bar        } toolbar webview
├──────────────────────────────────────┤                       (our HTML, trusted)
│            Web Content               │  content webview of the *active* tab
└──────────────────────────────────────┘  (untrusted pages; inactive tabs'
                                            webviews exist too, just hidden)
```

- The toolbar (tab strip + address bar) is rendered from
  `src/ui/toolbar.html`, compiled into the binary with `include_str!`. Using
  HTML/CSS for chrome avoids pulling in a whole native widget toolkit (or a
  Rust GUI framework), and it is trivially themeable later.
- Keeping chrome and content in *separate* webviews is a security boundary:
  page content can never script the toolbar, and toolbar IPC messages can
  only originate from our own HTML.
- Every open tab keeps its own content webview alive, not just the active
  one. Switching tabs hides the previous webview and shows the target one
  (`WebView::set_visible` + `set_bounds`) instead of destroying/recreating
  it, so scroll position and in-progress form input survive the switch. Only
  the active tab's webview is visible at any time.

On Linux/BSD every webview (toolbar and each tab's content view) is a gtk
widget placed in one shared `gtk::Fixed` inside the tao window; on
macOS/Windows they are true child webviews (`build_as_child`).
`src/ui/window.rs` hides this difference behind `BrowserWindow`.

On Linux/BSD (WebKitGTK) the engine processes behind those webviews are
shared deliberately rather than one-per-webview: in non-private mode every
webview is built against one `WebContext` (one `WebKitNetworkProcess` for
the whole window, docs/decisions.md D49), and content webviews are built as
*related views* of an existing tab so that up to `MAX_TABS_PER_WEB_PROCESS`
tabs share one `WebKitWebProcess` (D54). A new tab never joins a process
that is still loading another tab's page — a burst of tabs opened
back-to-back fans out over fresh processes and loads in parallel — and the
toolbar webview is never related to content, so the chrome/content security
boundary above is also a process boundary. Private mode keeps one process
pair per webview (wry builds an ephemeral context per incognito webview,
D15). `browser::Tabs` never sees any of this; `BrowserWindow::open_tab`
only takes an `is_loading` probe from `app.rs`.

## UI ↔ engine responsibilities

| Concern | Owner |
|---|---|
| Window, layout, resize | `ui::window::BrowserWindow` |
| Per-tab webview lifecycle (open/close/activate) | `ui::window::BrowserWindow` |
| Toolbar + tab strip rendering, input | `ui/toolbar.html` (in the toolbar webview) |
| IPC protocol (JSON) | `ui::toolbar` (`ToolbarCommand`, `TabSummary`) |
| URL normalization | `browser::navigation` |
| Tab collection (open/close/activate, which tab is active) | `browser::tabs::Tabs` |
| Address bar / loading state per tab | `browser::tab::Tab` (mirrored into the toolbar) |
| Content-blocking rule matching | `browser::blocklist::FilterList` |
| Visit history (persisted) | `browser::history::HistoryStore` + `browser::persistence` |
| Bookmarks (persisted) | `browser::bookmarks::BookmarkStore` + `browser::persistence` |
| Page rendering, network, cookies | web engine (wry) |
| Session history (back/forward) | web engine (wry), per content webview |

VeloX deliberately does **not** duplicate the engine's session history for
back/forward. The engine already tracks redirects, `pushState`, anchors
etc.; a parallel Rust history would drift from reality. `Tab` mirrors only
what the UI needs (current URL, title, favicon, loading flag, lifecycle
state) — one `Tab` per open tab, held in `Tabs`. See "Tab lifecycle state"
below for the state model and docs/decisions.md D20 for the reasoning
behind it.

The app-level **visit history** (a persisted "where have I been" log,
separate from the above) and **bookmarks** are a different concern entirely
— see the next section.

## Visit history and bookmarks

`browser::history::HistoryStore` and `browser::bookmarks::BookmarkStore` are
plain, serde-derived, UI/engine-independent collections (de-duplication,
caps, ordering — the unit-tested part); `browser::persistence` is the thin
IO layer that loads/saves each as its own JSON file under a per-platform
data directory (env-var resolved, see docs/decisions.md D10). `app::AppState`
owns one instance of each store plus the resolved data directory for the
process's lifetime; every mutation is followed by writing the affected store
back to disk (best-effort — a write failure is logged, never fatal).
`browser::input_history::InputHistoryStore` (Issue #20 — typed search
queries, not page visits; see docs/decisions.md D38) follows the exact same
three-part shape (pure store, `persistence::{load,save}_input_history`,
`input_history.json`) as a fourth, independent file alongside
`history.json`/`bookmarks.json`.

**Recording a visit** happens at the same point session-history state
already updates: `UserEvent::LoadFinished` calls
`app::record_visit_if_enabled`, the single choke point private browsing
gates via `AppState::history_enabled` — see docs/decisions.md D13 and the
"Private browsing" section below. Recording is not limited to the active
tab: every tab's `LoadFinished` runs through this same path, so a page
finishing in a background tab is recorded too. The entry is
created with `title: None`/`favicon: None` and `visit_count: 1` immediately
(the store's own de-duplication collapses a reload into updating that same
entry — bumping `visit_count` — rather than creating a new one; see
docs/decisions.md D27 for what `visit_count` does and does not count).
`BrowserWindow::fetch_page_title`/`fetch_favicon` then asynchronously read
`document.title` and a favicon URL from *that tab's* content webview (a
no-op if the tab is suspended and has none) and report them back as
`UserEvent::PageTitleResolved`/`FaviconResolved { tab_id, history_id, .. }`,
which fill in the title/favicon once they arrive (see docs/decisions.md D12
for why this is async and best-effort, D22 for the favicon-URL-only
contract, D27 for threading `history_id` through the favicon fetch the same
way the title fetch already does).

**Date-grouped listing and search** (docs/decisions.md D29/D30) are pure
functions in `browser::history` — `date_bucket`/`group_by_date` classify
entries into 今日/昨日/過去7日/それ以前 sections relative to an injected
`now` (UTC calendar days, no timezone crate); `search` is a
case-insensitive URL/title substring filter over the whole store, not just
the panel's visible window. `ui::window::BrowserWindow::set_history` calls
`group_by_date` and hands the toolbar already-grouped, already-labeled data
(`ui::toolbar::HistoryGroup`/`HistoryDateBucket`); the toolbar's
`veloxSetHistory` only walks and renders it, doing no date arithmetic of
its own. `ToolbarCommand::SearchHistory { query }` (sent on every keystroke
in the panel's search box) is the other input to the same rendering path;
an empty query falls back to the normal recency list rather than an
(empty) search result.

**The history/bookmarks panel** is UI inside the *toolbar* webview, not a
separate page — opening it grows the toolbar webview's own bounds rather
than overlaying the content webview, which two independently-bounded
webviews cannot do. See docs/decisions.md D11 for the alternatives
considered and `ui::window::effective_toolbar_height` /
`BrowserWindow::set_panel` for the mechanics. `ToolbarCommand` gained
`ToggleBookmark`, `TogglePanel { panel }`, `DeleteHistoryEntry { id }`,
`ClearHistory`, `SearchHistory { query }`, and `RemoveBookmark { id }`;
opening a history/bookmark
entry reuses the existing `Navigate { input }` command (the stored URL is
already normalized, so it round-trips through `navigation::normalize_input`
unchanged) rather than adding a dedicated "open" command.

**Bookmark organisation** (docs/decisions.md D32-D34) lives entirely in
`browser::bookmarks`. Folders are a *flat* reference — `BookmarkEntry`
carries `folder_id: Option<u64>` into a separate `BookmarkFolder` list, one
level deep with no nesting; D32 records why a tree was not worth its UI and
cycle-checking cost for this issue's requirements. Deleting a folder
re-parents its bookmarks to the root rather than cascading. Editing goes
through `BookmarkStore::edit`, which pushes a changed URL through
`navigation::normalize_input` so the address-bar rules (rejected schemes,
normalization) apply identically here; a rejected or duplicate URL is
reported back as `BookmarkEditError` instead of being stored. Display order
is the `Vec`'s own element order — `move_up`/`move_down` swap with the
adjacent entry *within the same folder or the root*, so no explicit sort-key
field exists to keep consistent. `BookmarkEntry::favicon` is filled by the
same `UserEvent::FaviconResolved` path the history store uses, keyed by page
URL rather than by entry id (D34).

**The bookmark bar is not a `Panel`.** A panel is transient and mutually
exclusive with the other panels; the bar is a persistent strip that
coexists with whichever panel is open. `ui::window::effective_toolbar_height`
therefore *adds* `Config::bookmark_bar_height` and `Config::panel_height`
independently rather than treating them as alternatives, and both shrink the
content webview (D35). Many bookmarks degrade the same way many tabs do —
shrink, then `overflow-x: auto` scroll (D22) — and a folder opens as a
dropdown inside the toolbar webview. Visibility is Ctrl/Cmd+Shift+B, wired
through both shortcut channels (D18/D23) like every other VeloX shortcut,
and is **session-only**: there is no settings-persistence layer yet, so the
bar's shown/hidden state resets on restart (see #30).

## Private browsing

Private browsing (#7) is a **whole-app** mode, not a per-window one — see
docs/decisions.md D12 for why, and the extension path once multi-window
exists. `Config::private` (set from the `VELOX_PRIVATE` env var or a
`--private` CLI flag, `Config::from_env_and_args`) drives two independent
things at startup, both in place before the window is shown:

- `ui::window::BrowserWindow::new` builds the *content* webview with
  `.with_incognito(config.private)`, so cookies/storage/cache use the
  engine's ephemeral data store and never touch disk (docs/decisions.md
  D13 has the per-platform backing). The toolbar webview never needs this —
  it only ever loads our own embedded HTML.
- `app::run` sets `AppState::history_enabled = !config.private`, so
  `app::record_visit_if_enabled` (see above) never calls
  `HistoryStore::record_visit` for the session — no history entry is ever
  created, so there is nothing for `persist_history` to write either.
  `app::record_input_history_if_enabled` (Issue #20 — typed search
  queries, see docs/decisions.md D38/D39) is gated by the exact same flag,
  the same way. Bookmarks stay ungated (an explicit user action, same call
  as normal mode — see docs/decisions.md D11). None of this affects
  *reading* — the omnibox's history/bookmark/typed-query candidates (#20)
  still surface whatever was already recorded before private mode started,
  matching how the History panel has always behaved; see D39 for the full
  reasoning.

The mode is surfaced continuously, not just once, so it cannot go unnoticed
mid-session: the toolbar webview gets a persistent badge plus a color shift
(`ui/toolbar.html`'s `body.private`, pushed once via
`BrowserWindow::set_private` from the toolbar's `ready` handshake, since the
mode never changes for the life of the process), and the window title gets
a `— プライベート` suffix (visible even when another window covers the
toolbar, e.g. in a taskbar/alt-tab switcher).

## Event flow

All state lives on the main thread. Webview callbacks (which may fire at
awkward moments) never touch state directly — they post a `UserEvent` into
the tao event loop. Each content webview's navigation/load callbacks close
over their own `TabId`, so events from a background tab are tagged and never
confused with the active tab's:

```
toolbar JS ──ipc.postMessage(JSON)──────────► UserEvent::ToolbarMessage ─┐
tab N's content webview ──nav/load callbacks─► UserEvent::…(TabId, …)    ├─► app::handle_user_event
                                                                         │      │
        ┌────────────────────────────────────────────────────────────────┘      │
        ▼                                                                       ▼
  BrowserWindow methods (load_url, history.back(), open_tab,             Tabs (Vec<Tab> +
  close_tab, activate_tab, …) — act on the active tab unless              active index)
  the command is tab-scoped (open/close/activate)
        │
        ▼
  toolbar.evaluate_script(veloxSetUrl/veloxSetLoading/veloxSetTabs) ← UI reflects state
```

`app::handle_user_event`/`handle_toolbar_command` always update `Tabs` first,
then push the result to `BrowserWindow` (webview + toolbar). Events for a
background tab still update its `Tab` state and the tab strip (so a loading
spinner or updated title shows even off-screen), but skip the address
bar/loading indicator, which only reflects the active tab.

This gives one single-threaded state machine: no locks, no races, and every
state change is observable in one place (`app.rs`).

## URL input → page display

1. User types into the address bar and presses Enter (or picks a candidate
   from the omnibox dropdown — see "Omnibox and search" below).
2. Toolbar JS sends `{"cmd":"navigate","input":"<raw text or a candidate's
   already-resolved target_url>"}` over IPC.
3. `ui::toolbar::parse_command` deserializes it into `ToolbarCommand::Navigate`.
4. `app::resolve_navigate_target` resolves `input` to a loadable URL:
   `browser::navigation::classify_input` decides URL vs. search, and for a
   search a query goes through `browser::navigation::build_search_url` with
   `Config::search_engine`'s template. Either way this bottoms out in
   `browser::navigation::normalize_input` — the one place that turns text
   into a URL (`example.com` → `https://example.com/`) and rejects
   unsupported/unparseable input. See "Omnibox and search" below for the
   full URL-vs-search story; a rejected/empty result snaps the address bar
   back to the page actually loaded.
5. `BrowserWindow::navigate` calls `WebView::load_url`.
6. The engine fires navigation/load callbacks; they arrive as `UserEvent`s,
   update `Tab`, and are pushed back into the toolbar
   (`veloxSetUrl`, `veloxSetLoading`).

Back/Forward run `history.back()` / `history.forward()` in the content
webview — i.e. the engine's own session history API. Reload uses
`WebView::reload`.

Startup ordering: the toolbar sends `{"cmd":"ready"}` once its document is
loaded, and the app answers with the current state. Without this handshake
the first `veloxSetUrl` could run before the toolbar's JS exists (the content
page starts loading in parallel).

## Omnibox and search

Issue #15 turns the address bar into an omnibox: URL-vs-search
classification, a configurable search engine, and a candidate dropdown
driven by keyboard (↑/↓/Enter/Esc) or the mouse. Issue #20 layers
history/bookmark candidates, typed-search-query candidates, and ranking on
top — see "History/bookmark/typed-query candidates and ranking (#20)"
below.

**URL vs. search — `browser::navigation::classify_input`.** Every piece of
omnibox input goes through this one function (`Intent::Url(String)` or
`Intent::Search(String)`, or `None` for empty/refused input), never a second
copy of the decision:

- Empty/whitespace-only input → `None`.
- A leading `?` forces a search (`?rust` → search for `rust`), regardless of
  what follows.
- More than one whitespace-separated word → always a search
  (`rust ownership`) — hand-typed URLs never contain a literal space.
- A single "word" is attempted as a URL only when it looks like one (an
  explicit scheme, or a dotted/colon-qualified host —
  `example.com`/`localhost:3000`/`about:blank`); `normalize_input` is the
  *only* place that actually parses/normalizes/rejects it, so scheme
  allow-listing still lives in exactly one function. A URL `normalize_input`
  refuses (unsupported scheme, unparseable) yields `None` here too — it is
  never silently retried as a search (see docs/decisions.md D26).
- A single word that does not look like a URL at all (`rust`) is a search —
  a bare word is almost always meant as a query, not a domain with an
  assumed TLD.

**Search engine — `config::SearchEngine`.** A name plus a query template URL
containing the literal placeholder `{}` (default:
`https://duckduckgo.com/?q={}` — see docs/decisions.md D26 for why
DuckDuckGo). `browser::navigation::build_search_url(template, query)`
percent-encodes `query` via `url::form_urlencoded` (never hand-rolled) and
substitutes it in, only handing back the result if it parses as an
`http`/`https` URL. Selectable via `VELOX_SEARCH_ENGINE` (a preset:
`duckduckgo`/`google`/`bing`/`startpage`/`ecosia`) or a fully custom
`VELOX_SEARCH_ENGINE_NAME`/`VELOX_SEARCH_ENGINE_URL` pair — see
`Config::from_env_and_args`.

**Candidates — `browser::omnibox`.** `build_candidates(input, engine_name,
engine_template, sources, limit)` always starts from `classify_input`: a URL
intent contributes a `NavigateUrl` candidate plus a `Search` candidate for
the same text (mirrors how mainstream browsers offer both when input is
URL-shaped but ambiguous); a search intent contributes just the `Search`
candidate. Every `Candidate::target_url` is already fully resolved, so
executing one is exactly `ToolbarCommand::Navigate { input: candidate.target_url
}` — the same path a plain Enter with no dropdown interaction takes,
`classify_input` treating an already-absolute URL as `Intent::Url` unchanged.

**History/bookmark/typed-query candidates and ranking (#20).**
`browser::omnibox_candidates` implements `CandidateSource` twice against
the trait's contract from `browser/omnibox.rs` (candidates already ranked
best-first, `target_url` already normalized), and `app.rs`'s
`ToolbarCommand::OmniboxInput` handler builds one of each fresh per
keystroke and passes `&[&history_bookmark_source, &input_history_source]`
into `build_candidates` (previously `&[]`):

- **`HistoryBookmarkSource`** merges `HistoryStore`/`BookmarkStore` into one
  ranked list. `browser::ranking::merge_entries` de-duplicates a URL that
  is both visited and bookmarked into a single candidate — `Bookmark` kind
  wins the display (an explicit save is a stronger signal than an
  incidental visit), carrying history's `visit_count`/`visited_at` for
  scoring regardless (see docs/decisions.md D37). `browser::ranking::
  score_page_entry` scores each surviving match on one axis: a
  match-location tier (host-prefix > title-prefix > host-substring >
  title-substring > URL-substring; an entry that matches nowhere is
  dropped, never merely scored low), a proportional-match-length bonus, a
  frecency component (capped visit-count + a recency bucket reusing
  `history::date_bucket`'s own today/yesterday/last-7-days/older
  boundaries — D29), and a flat bonus for being bookmarked. See D36 for the
  exact weights and a worked example, D37 for the de-duplication rule.
- **`InputHistorySource`** resurfaces previously-submitted search queries
  (`browser::input_history::InputHistoryStore`, populated from
  `ToolbarCommand::Navigate`'s raw text whenever `classify_input` reads it
  as `Intent::Search` — never for URL-shaped input, which `HistoryStore`
  already covers once the page loads) as `Search` candidates, ranked by
  `browser::ranking::rank_input_history` (the same match-tier + frecency
  shape as page entries, without the bookmark bonus). See D38 for scope,
  the LRU eviction rule, and persistence (`input_history.json`, alongside
  `history.json`/`bookmarks.json`).

Both sources are constructed fresh from `AppState`'s stores on every
`OmniboxInput`, private-mode or not: they read whatever is already
recorded regardless of `AppState::history_enabled`, matching how the
History *panel* has always behaved — only the *recording* paths
(`record_visit_if_enabled`, `record_input_history_if_enabled`) are gated by
it. See D39 for the full reasoning.

`build_candidates` itself needed no changes for any of this: the two
built-in candidates (`NavigateUrl`/`Search`) are still computed first,
unconditionally, and `sources` are only ever asked for however many slots
remain — history/bookmark/typed-query candidates can never crowd out what
the user literally typed.

**UI wiring.** The candidate dropdown reuses the exact same toolbar-webview
resize mechanism the history/bookmarks panel already has (`ui::toolbar::Panel`,
`BrowserWindow::set_panel`/`sync_layout` — see D11) as a third `Panel::Omnibox`
variant, rather than inventing separate layout code: `app.rs` opens/closes it
from whether `build_candidates` returned anything, driven by every address-bar
keystroke (`ToolbarCommand::OmniboxInput`). Row selection (↑/↓) is purely
client-side in the toolbar's own JS — only Enter/click, which need the chosen
candidate's `target_url`, and each keystroke, which needs a fresh candidate
list, round-trip to Rust. Ctrl/Cmd+L (focus + select-all) and Esc (close +
restore the real current URL) are wired through both keyboard-shortcut
channels the same way every other shortcut is (`ToolbarCommand::FocusAddressBar`/
`OmniboxClose` from the trusted toolbar webview,
`ui::window::ContentShortcut::FocusAddressBar` from the untrusted content
webview — see D18/D23), both handled by one shared `app::focus_address_bar`.

## Content blocking

`browser::blocklist::FilterList` is a small EasyList-subset parser/matcher
(`||domain^` block rules, `@@||domain^` exceptions) built at startup from an
embedded default list (`browser/default_blocklist.txt`) plus an optional
user file (`Config::extra_blocklist_path`). It is pure logic with no engine
dependency, so it is unit-tested directly without a webview.

`app::run` builds one `FilterList`, wraps it in an `Arc`, and hands it to
`BrowserWindow`, which closes over it — together with the content-blocking
enabled flag — inside `content_webview_builder`, the single helper every
content webview (initial tab, a newly opened tab, and a tab rebuilt on
resume from suspension) is constructed through. Putting the check in this
one shared helper, rather than in `BrowserWindow::new` alone, is what makes
blocking apply uniformly regardless of when or how a tab's webview comes
into existence:

```
content webview navigates to `url`  (any tab, any time it gets a webview)
        │
        ▼
FilterList::is_blocked(url)?  (host lookup + domain-suffix match)
   │ yes                              │ no
   ▼                                  ▼
return false (navigation refused)     UserEvent::NavigationStarted(id, url)
UserEvent::NavigationBlocked(id, url) (existing flow)
        │
        ▼
Tab::on_navigation_blocked  →  toolbar block-count badge (active tab only)
```

`UserEvent::NavigationBlocked` carries the `TabId` the block happened in,
the same way every other navigation event does, so a block in a background
tab updates that tab's `Tab::blocked_count` without touching the badge
shown for the active tab; switching tabs re-renders the badge from the
newly active tab's count (`veloxSetBlockCount`), the same pattern already
used for the address bar and the bookmark star.

This only covers **main-frame navigation** — wry 0.56 exposes no hook for
subresource requests (images/scripts/XHR), so ad/tracker resources loaded
*within* an allowed page are not filtered today. See docs/decisions.md D17
for the platform-by-platform investigation and why that gap is not closed
in this iteration.

## Multiple tabs

- `browser::tabs::Tabs` owns `Vec<Tab>` plus an active index. It is plain,
  UI/engine-independent Rust (open/close/activate, id issuing, which tab is
  active after a close) and is the primary unit-test target for tab
  behavior — no window or webview needed. `Tabs` always keeps at least one
  tab open: closing the last remaining tab is a no-op.
- Tab ids (`browser::TabId`, a `u64` newtype) are assigned once by `Tabs` and
  never reused, so a stale id from a delayed `close_tab`/`activate_tab`
  message, or one for a tab that has since closed, simply matches nothing
  (`Tabs::get`/`get_mut` return `None`) instead of hitting the wrong tab or
  panicking.
- `BrowserWindow` owns one content `WebView` per tab (`HashMap<TabId,
  ContentTab>`) plus which tab is active. Opening a tab builds a new content
  webview bound to that `TabId` (its navigation/load handlers close over the
  id, so their `UserEvent`s are tagged, including `PageTitleResolved`'s
  `tab_id`); activating a tab hides the previously active webview and shows
  the target one via `set_visible` + `set_bounds` — the webview itself is
  never destroyed, which is what keeps scroll position and form input intact
  across a tab switch. The toolbar webview is shared by all tabs. **This is
  the ownership boundary the whole state model below is built around**:
  `browser::` (`Tab`/`Tabs`) never references a `wry`/`tao`/`gtk` type —
  `ui::window::BrowserWindow` is the sole owner of any actual `WebView`,
  keyed by `TabId`. See docs/decisions.md D20.
- `ToolbarCommand` gained `NewTab`, `CloseTab { id }`, and
  `ActivateTab { id }` — purely additive to the existing serde enum. The
  toolbar pushes tab state back with `TabSummary`/`veloxSetTabs`, rendered as
  the tab strip above the address bar (`src/ui/toolbar.html`).
- **Tab strip presentation (#11)**: `TabSummary` also carries `title` and
  `favicon` (a URL, not image bytes — resolving it is synchronous in-page JS,
  fetching the image is the toolbar webview's own `<img>` tag, never a
  Rust-side network call; see docs/decisions.md D22). The tab strip's CSS
  shrinks tabs down to a minimum width as more are opened before its
  existing `overflow-x: auto` starts scrolling.
- **Keyboard shortcuts (#11)**: Ctrl/Cmd+T/W/Shift+T/Tab/Shift+Tab/1-9 are
  delivered the same way DevTools' F12 is (D18) — an injected capture-phase
  script in the content webview, since a focused child webview never lets a
  tao-level accelerator see the keypress — plus a second, independent
  listener in the trusted toolbar webview for when the address bar has
  focus instead. See docs/decisions.md D23 for the two channels' different
  trust boundaries (sentinel strings vs. structured `ToolbarCommand`s) and
  why both funnel into the same handful of shared `app.rs` functions.
- **Reopen closed tab (#11)**: `browser::tabs::Tabs` keeps a small, capped,
  pure LIFO stack of recently closed tabs' URLs (`ClosedTabs`,
  docs/decisions.md D24), fed by `Tabs::close` and drained by
  `Tabs::reopen_closed` — reopening always starts a fresh tab/webview at the
  remembered URL, never a restoration of the closed tab's actual session
  state.
- **`target="_blank"`/`window.open()` (#11)**: every content webview's
  `with_new_window_req_handler` denies the platform's own new-window/tab
  handling and instead reports the requested URL as
  `UserEvent::NewTabRequested`, which `app.rs` opens as an ordinary new
  VeloX tab — see docs/decisions.md D25 for the wry-0.56-source-confirmed
  API survey behind that choice.

## Tab lifecycle state

`browser::tab::TabState` is an explicit enum — `Active` / `Background` /
`Suspended` / `Restoring` — replacing what used to be an implicit
`suspended: bool` plus "is this id `Tabs`' active index". See
docs/decisions.md D20 for the full design rationale; this section is the
quick-reference summary.

```text
       ┌────────────┐  another tab activated   ┌────────────┐
       │   Active    │ ────────────────────────►│ Background │
       │ (visible,   │◄──────────────────────── │ (awake,    │
       │  webview    │   this tab activated      │  webview   │
       │  live)      │                           │  live)     │
       └──────┬──────┘                           └──────┬─────┘
              │ (only reachable via Restoring)          │ idle timeout /
              │                                          │ manual suspend
       ┌──────┴──────┐   webview rebuilt          ┌──────▼─────┐
       │  Restoring  │◄────────────────────────── │  Suspended │
       │ (selected,  │   this tab selected         │ (webview   │
       │  webview    │                             │  dropped)  │
       │  rebuilding)│                             └────────────┘
       └─────────────┘
```

- **Invariant**: exactly one tab — the one `Tabs::active_id()` points at —
  is ever `Active` or `Restoring`; every other tab is `Background` or
  `Suspended`. `Tab`'s transition methods are `pub(super)`, so `Tabs` is the
  only thing that can move a tab between states, and every `Tabs` method
  that changes which tab is active (`open`, `activate`/`activate_at`, the
  replacement tab a `close` picks) upholds this invariant via a shared
  private helper (`Tabs::resolve_activation`).
- **Invalid transitions are rejected, not just avoided by convention**:
  each edge above is a separate `TabState` method returning
  `Result<TabState, InvalidTabTransition>`; anything not drawn (including
  every self-transition) is an `Err`. This is what lets
  `Tabs::suspend` drop its old "refuse the active tab" / "refuse an
  already-suspended tab" special cases — `TabState::suspend` only accepts
  `Background`, so both are simply invalid transitions now, caught in the
  one place every other invalid transition is.
- **`Restoring` is real, not a synonym for `Active`**, which is exactly the
  seam session restore (#25, see "Session restore" below) needed: no new
  state had to be added for "selected, but nothing is showing yet". Every
  current caller still collapses `Suspended -> Restoring -> Active` into one
  call (`Tab::resume`), because today's webview rebuild
  (`ui::window::BrowserWindow::resume_tab`) is synchronous — there is no
  observable gap between the two edges yet, just the seam for one.
- **`ActivationEffect`**: `Tabs::activate`/`activate_at`/`close` return
  `Option<browser::ActivationEffect>` (`Switch` or `Resume`) instead of a
  bare `bool`, so `app.rs` knows whether to call
  `BrowserWindow::activate_tab` (already-live webview, just show it) or
  `BrowserWindow::resume_tab` (webview was dropped, rebuild it) without
  re-deriving that from `Tab::is_suspended()` after `Tabs` has already
  resolved the transition.
- **New `Tab` fields**: `title: Option<String>` and `favicon: browser::Favicon`
  (`Unknown` or `Url(String)`), both cleared on every
  `Tab::on_navigation_started` so a stale value from the previous page is
  never shown as current. `title` is set from
  `UserEvent::PageTitleResolved` (which already carries a `tab_id` for
  exactly this); *rendering* either in the tab strip is left to #11. These,
  plus the pre-existing `last_active`/`last_active_at` (D9) and
  `current_url`, are the state session restore (#25, see "Session restore"
  below) reads from — `url`/`title`/`favicon` only; `last_active` stays
  per-process and is not persisted (see D65).

## Tab suspension

Status: manual suspension shipped, automatic suspension implemented and
opt-in (default off) with an adaptive policy (idle time, live-tab cap,
memory budget — Issue #63). See docs/decisions.md D9 for the full
rationale, including the WebKitGTK/WKWebView/WebView2 cache-control
investigation, D20 for how suspension fits into the `TabState` model
above, and D56 for the adaptive policy and its measured effect.

- **What "suspended" means**: `ContentTab::webview` (`ui::window`) is
  `Option<WebView>`; suspending a tab `take()`s and drops it
  (`BrowserWindow::suspend_tab`), reclaiming the memory the webview held.
  `browser::tab::Tab` mirrors this with `TabState::Suspended` and keeps
  `current_url` (plus `title`/`favicon`, which were never engine-owned to
  begin with) — the state that survives. Scroll position, in-progress form
  input, and session history (back/forward) are lost, the same trade-off
  already accepted for tab *close* — suspension is a deeper version of the
  same idea, not a new category of data loss.
- **Never the active tab**: both the manual command and the automatic sweep
  refuse to suspend the currently active tab. This now falls directly out of
  the `TabState` transition rules (`Background -> Suspended` is the only
  valid edge into `Suspended`) rather than a separate check in
  `browser::tabs::Tabs::suspend`; `BrowserWindow::suspend_tab` still
  defensively checks again on the webview side.
- **Resuming**: reactivating a suspended tab (clicking it in the tab strip,
  or it becoming active because the tab in front of it closed) rebuilds the
  webview and reloads `current_url` — `BrowserWindow::resume_tab` is exactly
  `open_tab` followed by `activate_tab`, since rebuilding a dropped webview
  for a `TabId` the app already knows about is the same operation as
  building the first one for a brand new tab. `Tabs` reports this case as
  `ActivationEffect::Resume` so `app.rs` knows to call `resume_tab` instead
  of `activate_tab`.
- **Manual suspension**: `ToolbarCommand::SuspendTab { id }`, sent by a
  per-tab button in the tab strip (hidden for the active tab and for a tab
  already suspended, since clicking the tab itself resumes it — no separate
  "resume" affordance is needed). `TabSummary` carries a `suspended` flag
  (`Tab::is_suspended()`, shorthand for `state() == TabState::Suspended`) so
  the strip can render dormant tabs distinctly (dimmed, a 💤 marker).
- **Automatic suspension** (Issue #63): `Config::suspension` is a
  `browser::suspension::SuspensionPolicy` — three independent, individually
  optional signals, all off by default (`SuspensionPolicy::default()`,
  so a fresh checkout never suspends a tab on its own — D9's rule):
  - *idle time* (`idle_after`, `VELOX_AUTO_SUSPEND_AFTER_MS`): every
    background tab idle at least this long is suspended — the pre-#63
    behavior, unchanged;
  - *live-tab cap* (`max_live_tabs`, `VELOX_MAX_LIVE_TABS`): when more
    tabs than this have a live webview (the active tab counts), the least
    recently used background tabs are suspended until the count fits;
  - *memory budget* (`memory_budget_bytes`, `VELOX_MEMORY_BUDGET_MB`):
    when the process tree's memory exceeds the budget, enough least
    recently used background tabs are suspended to be expected to bring it
    back under (`ESTIMATED_BYTES_PER_TAB`, 64 MiB, from D54's per-tab
    measurement) — the further over budget, the more tabs go in one sweep.
    Only this signal needs a sampler: `app::spawn_memory_pressure_sampler`
    (spawned only when a budget is set) walks the process tree every
    `memory_check_interval` (`VELOX_MEMORY_CHECK_INTERVAL_MS`, default
    2s) with the same `metrics::sample_process_tree_rss` the perf RSS
    sampler uses, and posts `UserEvent::MemorySampled` (PSS where readable,
    RSS otherwise). Each sample drives the memory signal exactly once
    (`AppState::pending_memory_sample` is `take()`n by the sweep), so a
    stale sample never keeps suspending tabs before the previous sweep's
    effect is visible.

  All three combine in one pure function, `suspension::plan` (clock- and
  sample-injected, no `Tabs`/webview dependency, unit-tested exhaustively):
  it takes every live tab as a `Candidate` (built by
  `Tabs::suspension_candidates` — the active tab included and flagged, so
  the policy knows which process group it pins) and an optional fresh
  sample, and returns the
  tabs to suspend with a `SuspendReason` (`idle`/`tab_count`/`memory`,
  logged as the `tab_suspend` perf event). The tab-count and memory
  demands take the larger of the two (not the sum), and idle suspensions
  count toward both. **The unit of reclaim is a web process, not a tab**
  (`suspension::reclaim_order`, D56): dropping a webview inside a process
  that keeps running returns little of its memory to the OS, so the demand
  is met by emptying whole least recently used process groups first
  (`Candidate::process_group`, from `BrowserWindow::process_group_of`) —
  every tab of a group that holds no active/loading/protected tab, even
  when that overshoots the demand — and only then by individual least
  recently used tabs from groups that cannot be emptied.
  **Protected, never suspended automatically**: the active tab (by the
  `TabState` invariant), a tab still loading (dropping a mid-load webview
  wastes the work and repeats it on resume), and a tab playing audio
  (`BrowserWindow::is_playing_audio`, WebKitGTK's `is-playing-audio`
  property; always `false` on other platforms, so the protection simply
  does not apply there). `app::sweep_tabs` runs the plan on every pass of
  the event loop (after every event) and drives
  `tao::event_loop::ControlFlow` with `WaitUntil(next_idle_deadline)` so the
  idle signal wakes the loop exactly when a tab crosses the threshold rather
  than polling; the memory signal wakes it via `MemorySampled`, and the
  tab-count signal is re-evaluated on whatever event changed the tab set.
  The `suspend <index>` automation command drives the same `suspend_tab`
  path as the tab strip's button, which is how the `tab_resume` benchmark
  scenario measures the restore cost (`tab_resume` perf event —
  `TabLatencyKind::Resume`, kept apart from `tab_switch` because rebuilding
  a webview is an order of magnitude slower than a `set_visible`).
- **Idle clock**: a tab's "idle since" timestamp is the moment it stopped
  being the active tab, recorded by `Tabs::activate_at`/`open_at` (thin
  wrappers around the existing `activate`/`open` that additionally stamp the
  *outgoing* active tab before switching) — see `Tab::mark_backgrounded`.
  This is independent of the `TabState` transition itself (some tests
  intentionally use the plain `activate`/`open`, which skip the clock
  stamp but still run the state transition correctly). The currently active
  tab's timestamp is never read, since the active tab is always excluded
  from suspension candidates regardless of its value.

## Session restore

Status: session persistence and startup restore are implemented and
opt-in (default off, Issue #25); WebView/renderer crash *detection* is not
— wry 0.56 exposes no hook for it on the two OSes VeloX ships broadly on.
See docs/decisions.md D65 for the full investigation and rationale; this
section is the quick-reference summary.

- **What is saved, and when**: `browser::session::SessionSnapshot` (one
  `SavedTab { url, title, favicon }` per open tab, in display order, plus
  which one was active) is the persisted shape — deliberately the same
  three fields the tab strip itself renders from
  (`app::sync_tab_strip`'s `TabSummary`), never scroll position, form
  input, or session history (none of that survives a webview being
  dropped at all — suspension already accepts the same loss, D9).
  `app::persist_session` writes it via
  `browser::persistence::{load,save}_session` (`session.json`, the same
  per-platform data directory as `history.json`/`bookmarks.json`, D10) from
  inside `app::sync_tab_strip` — the one function nearly every
  tab-affecting change already calls to keep the tab strip current — rather
  than only at process exit, so a crash or `kill -9` still leaves a recent
  session to restore from. Gated by the same `history_enabled` choke point
  private browsing already uses (D13/D14): a private session never writes
  what tabs it had open to disk.
- **What is restored, and how**: at startup, `app::run` loads
  `session.json` and runs it through
  `browser::session::SessionSnapshot::sanitize` (every URL re-validated
  through `navigation::normalize_input`, the same address-bar gate;
  `active_index` re-resolved rather than trusted) before trusting it at
  all. Only when `Config::restore_previous_session` is on (opt-in,
  `VELOX_RESTORE_SESSION`, off by default like every other auto-behavior
  toggle — D9's rule) and a usable snapshot survives sanitization does
  `browser::tabs::Tabs::restore` build the initial `Tabs` from it instead
  of `Tabs::new(homepage)`.
- **Reuses tab suspension instead of a second "webview regeneration"
  path**: `Tabs::restore` puts every tab *except* the active one directly
  into `TabState::Suspended` (`Tab::new_suspended`) rather than building a
  webview for each up front. `ui::window::BrowserWindow::new` already only
  ever builds a webview for the one tab id it is given, so a restored
  session's other tabs simply have no `ContentTab` entry until the user
  actually activates one — at which point `Tabs::activate` reports
  `ActivationEffect::Resume` exactly as it would for a tab suspended during
  the session, and the ordinary `BrowserWindow::resume_tab` rebuilds its
  webview — no second, restore-specific webview-build path was added:
  restoring 10 tabs costs one real webview at startup, not ten. The one
  actual `ui::window` change was a pre-existing latent bug this surfaced:
  `BrowserWindow::new` always loaded `config.homepage` into the first
  webview regardless of what `Tabs` said that tab's `current_url` was; it
  now takes an explicit `initial_url` (`app::run` passes
  `tabs.active().current_url()`), which is `config.homepage` unchanged in
  the non-restore case.
- **Corruption tolerance**: a missing, truncated, wrong-shape, or
  wrong-typed `session.json` fails to deserialize
  (`browser::persistence::load_session` returns `None`, matching
  `load_history`/`load_bookmarks`'s existing contract) and VeloX falls back
  to a fresh single tab at the homepage — never a failed startup. A
  well-formed file whose *content* is bad (a rejected URL scheme, an
  out-of-range `active_index`) is repaired or dropped entry-by-entry by
  `SessionSnapshot::sanitize` rather than discarding the whole session.
- **Crash detection was investigated, not implemented**: see D65. wry 0.56
  exposes `WebViewBuilderExtDarwin::with_on_web_content_process_terminate_handler`
  for a renderer crash, but only on macOS/iOS; there is no equivalent for
  WebKitGTK or WebView2 in this wry version. Given CLAUDE.md's OS priority
  (Windows first), a macOS-only hook was not wired up. "1 タブの WebView
  障害でブラウザ全体が終了しない" is not actively *tested* as a result, but
  holds structurally: each tab's `WebView` is an independent object behind
  `ui::window`'s per-`TabId` map, and no code path treats one tab's engine
  callbacks as able to reach another tab or the event loop itself.

## Startup URL

`Config::homepage` is resolved at launch from three layers, highest priority
first: a `--homepage <URL>`/`--homepage=<URL>` flag, the `VELOX_HOMEPAGE`
environment variable, then the compiled-in default
(`https://www.google.com/`). `config::resolve_homepage` is a pure function
over those three ingredients, so the precedence and the rejection rules are
unit-tested without touching the real process environment — the same shape as
`resolve_private`/`resolve_perf_env`/`resolve_search_engine`.

Every candidate is validated with `navigation::normalize_input`, the exact
function address-bar input goes through, rather than a second copy of the
scheme rules. A candidate it rejects is skipped in favour of the next one, so
neither a typo nor a hostile `VELOX_HOMEPAGE` can stop VeloX from starting or
turn into a navigable `javascript:` URL. See docs/decisions.md D40.

This exists mainly so `velox-bench run --url <URL>` can point a benchmark
trial at a fixed local fixture instead of a network-dependent page; see
docs/benchmarking.md.

## Performance extension points

Implemented in `browser::metrics` (see D16/D19 in `docs/decisions.md`),
gated by `Config::perf_metrics` (opt-in via `VELOX_PERF_METRICS=1`, same
pattern as `VELOX_DEBUG`). All arithmetic/formatting/process-tree-walking is
pure Rust in `src/browser/metrics.rs`, unit-tested without a window; the one
IO exception is `src/browser/perf_log.rs` (`PerfLog`), which actually writes
the lines — mirrors `persistence.rs`'s role for history/bookmarks.

- **Startup time**: `main.rs` captures `process_start` before building
  `Config`, and passes it into `app::run`. `metrics::StartupTimestamps`
  records window creation, the toolbar's first `Ready`, and the first
  `LoadFinished` (≈ time-to-first-page) against it, plus two sub-checkpoints
  added for Issue #59/D43 that split the `window_created` → `toolbar_ready`
  gap in half: `rust_setup_done` (right before `app::run` enters the event
  loop — everything before this is synchronous Rust: persistence load,
  `AppState` construction) and `toolbar_script_started` (the toolbar
  webview's inline `<script>` block starting to execute, sent as its very
  first statement — see `ui/toolbar.html` and
  `ToolbarCommand::ScriptStarted`). `app::run` writes one `startup` record
  once all five have fired. See docs/decisions.md D43 for what this
  subdivision found: the gap is dominated by `tao`'s GTK/event-loop
  initialization and WebKitGTK's own (content-size-independent) webview
  spin-up cost, not by anything in `toolbar.html` or VeloX's Rust-side
  setup.
- **Page load time**: `UserEvent::NavigationStarted` → `LoadFinished` is
  bracketed by `metrics::PageLoadTimer` in `app::run`, writing one
  `page_load` record per load. Timers are kept per `TabId`, so a background
  tab loading concurrently with the active one does not overwrite its start
  time.
- **Tab create/switch time** (Issue #13): `ToolbarCommand::NewTab` and
  `ActivateTab` are handled synchronously in `app::handle_toolbar_command`
  (the new webview is usable, or the switch visible, by the time the
  handler returns), so no timer type is needed — the handler just brackets
  `Instant::now()` around the existing `Tabs::open_at`/`activate_at` +
  `BrowserWindow` calls and hands the `Duration` to
  `metrics::PerfRecord::tab_latency`, writing one `tab_create` or
  `tab_switch` record. `AppState::perf` is `None` when metrics are off, so
  the only cost on that path is the `Option::is_none` check in
  `app::record_tab_latency` — no extra `Instant::now()` beyond the one
  `open_at`/`activate_at` already takes for their own idle-tracking, which
  runs regardless of metrics.
- **Memory**: `metrics::sample_process_tree_rss(pid)` walks the whole
  process tree (WebKit's network/render helpers included) and sums both RSS
  and PSS (D42). It is a standalone public function with no dependency on
  `Config` or the running app — callable on demand from anywhere. Tab
  suspension is the primary lever for reducing memory: dropping a
  background tab's webview releases that process-tree's share of memory,
  and measuring the before/after delta with this function is its first real
  use case (see docs/decisions.md D9 — note PSS is the metric to use there
  too, per D42). When `perf_rss_interval` is set, `app::run` also spawns a
  background thread that samples it periodically and writes one `rss`
  record per sample. Implementation reads `/proc` directly on Linux (no
  extra dependency): RSS from `/proc/<pid>/status` (`VmRSS:`, always
  available), PSS from `/proc/<pid>/smaps_rollup` (`Pss:`, best-effort —
  `None` per process whose rollup could not be read, never treated as
  zero). Other Unix falls back to parsing `ps` output for RSS only (no PSS
  equivalent there); Windows is not implemented yet (`RssError::Unsupported`,
  neither RSS nor PSS).
- The `Config` struct is the home for all of these toggles;
  `Config::from_env_and_args` layers the environment-variable overrides onto
  `Config::default`.

### Output format and destination (Issue #13)

Every event kind above goes through one type, `metrics::PerfRecord`,
written by a shared `perf_log::PerfLog` (stderr by default, or a file — see
below). `PerfRecord::event_name()` returns one of `startup`, `page_load`,
`tab_create`, `tab_switch`, `tab_resume` (Issue #63), `tab_suspend`
(Issue #63), `measure_start` (Issue #60), `cpu` (Issue #64), `rss`.

- **`VELOX_PERF_FORMAT=text|json`** (default `text`; unset/unrecognized also
  falls back to `text`): selects `Config::perf_format`.
  - `text` reproduces the exact `velox[perf] <event> key=value ...` lines
    this project logged before Issue #13 — enabling metrics, or switching
    formats, never changes this format for anyone already scraping it.
    Example lines:
    ```text
    velox[perf] startup window_created=12.3ms rust_setup_done=12.4ms toolbar_script_started=44.9ms toolbar_ready=45.6ms first_page=120.0ms
    velox[perf] page_load url=https://example.com/ duration=250.0ms
    velox[perf] tab_create id=3 duration=15.2ms
    velox[perf] tab_switch id=3 duration=3.1ms
    velox[perf] tab_resume id=3 duration=2.7ms
    velox[perf] tab_suspend id=1 reason=tab_count
    velox[perf] measure_start
    velox[perf] cpu percent=0.6
    velox[perf] rss pid=4821 processes=5 total_mib=312.4 pss_processes=5/5 pss_mib=180.2 cpu_s=12.34
    ```
    (Issue #108 / D42: `pss_processes=<readable>/<processes>` and `pss_mib`
    were appended to the `rss` line, not inserted — an existing scraper
    matching the original prefix still works. `pss_mib=n/a` when PSS could
    not be read for any process in the tree. Issue #64 appended `cpu_s`
    the same way, `n/a` on platforms where CPU time cannot be read.)
  - `json` emits one JSON object per line (JSON Lines) — **this is the
    format Issue #14's benchmark runner and Issue #36's CI regression check
    should parse.** No `velox[perf] ` prefix, so every line parses as JSON
    on its own; a consumer reading a shared stderr stream (rather than a
    dedicated `VELOX_PERF_OUTPUT` file) should still skip any line that
    fails to parse, since other `velox: ...` diagnostics interleave on the
    same stream. Every record has `event` (string) and `ts_ms` (float,
    milliseconds elapsed since process start — monotonic within one run,
    since it derives from `Instant`), plus:

    | `event`      | fields |
    |--------------|--------|
    | `startup`    | `window_created_ms`, `rust_setup_done_ms`, `toolbar_script_started_ms`, `toolbar_ready_ms`, `first_load_ms` (float ms) |
    | `page_load`  | `url` (string), `duration_ms` (float ms) |
    | `tab_create` | `tab_id` (uint), `duration_ms` (float ms) |
    | `tab_switch` | `tab_id` (uint), `duration_ms` (float ms) |
    | `tab_resume` | `tab_id` (uint), `duration_ms` (float ms) — a switch that had to rebuild a suspended tab's webview (Issue #63); kept apart from `tab_switch` because the two are an order of magnitude apart |
    | `tab_suspend`| `tab_id` (uint), `reason` (string: `idle` / `tab_count` / `memory`) — an automatic suspension (Issue #63); no duration, dropping a webview is synchronous |
    | `measure_start` | *(none)* — the `mark` automation command (Issue #60). Everything logged before the last one is warm-up: `benchmark::aggregate_trials` pools only what follows it, which is what lets a scenario open N tabs before measuring an operation *at* N tabs |
    | `cpu`        | `percent` (float) — CPU used by the whole process tree between the two most recent `rss` samples, as a percentage of one core (Issue #64). A rate, so the first sample of a run emits none |
    | `rss`        | `pid` (uint), `process_count` (uint), `total_rss_bytes` (uint), `total_pss_bytes` (uint or `null`), `pss_process_count` (uint), `total_cpu_seconds` (float or `null`) |

    `total_pss_bytes`/`pss_process_count` were added by Issue #108 (D42).
    `total_pss_bytes` is the PSS counterpart to `total_rss_bytes` — see
    `docs/performance-targets.md` §3.1 for why PSS, not RSS, is the number to
    compare across builds/browsers with different process counts. It is
    `null` (present, not omitted — a consumer must not have to distinguish
    "old VeloX that never had this field" from "this VeloX build could not
    read it") when PSS could not be read for *any* process in the tree
    (unsupported platform, a kernel without `/proc/<pid>/smaps_rollup`, or a
    permissions failure); `pss_process_count` says how many of
    `process_count` processes it *was* read for, so a partial sum (some
    processes' PSS missing, not zeroed) is distinguishable from a complete
    one even when `total_pss_bytes` is non-null.

    Example lines:
    ```json
    {"event":"startup","first_load_ms":120.0,"toolbar_ready_ms":45.6,"ts_ms":120.0,"window_created_ms":12.3}
    {"event":"page_load","duration_ms":250.0,"ts_ms":5310.2,"url":"https://example.com/"}
    {"event":"tab_create","duration_ms":15.2,"tab_id":3,"ts_ms":8420.9}
    {"event":"rss","pid":4821,"process_count":5,"total_rss_bytes":327513600,"total_pss_bytes":188978790,"pss_process_count":5,"ts_ms":10000.0}
    {"event":"rss","pid":4821,"process_count":5,"total_rss_bytes":327513600,"total_pss_bytes":null,"pss_process_count":0,"ts_ms":12000.0}
    ```
    (Field order is whatever `serde_json` produces — alphabetical, since
    this project does not enable the `preserve_order` feature — a
    conforming JSON parser must not depend on it.)
- **`VELOX_PERF_OUTPUT=<path>`**: append perf lines to `<path>` instead of
  stderr — gives Issue #14 a stable file to read without needing to capture
  the whole process's stderr. `app::build_perf_log` opens it once, in
  append mode, at startup; if that fails (bad path, no permission), it logs
  the failure and falls back to stderr rather than losing metrics or
  crashing. Unset (the default) keeps stderr.
- Both are only consulted when `VELOX_PERF_METRICS` is set — matching
  `VELOX_PERF_RSS_INTERVAL_MS`'s existing "no overrides while off" rule — so
  a stray `VELOX_PERF_FORMAT=json` left in a shell does not silently change
  behavior the moment metrics are turned on elsewhere.

When metrics are off, `app::run` never spawns the RSS thread, never builds a
`PerfLog`, and every checkpoint (startup, page load, tab create/switch) is a
single `Option`-is-`None` check with no extra `Instant::now()` call — the
disabled path stays effectively free.

The layering matters more than any single hook: measurements attach to the
application layer, so swapping or tuning the engine below does not invalidate
them.

### Benchmark suite (Issue #14)

`src/browser/benchmark.rs` is the consumer side of the JSON Lines schema
above: it parses `VELOX_PERF_FORMAT=json` output, computes summary
statistics (median/p95/mean/stddev) over repeated trials, and diffs two
saved result files for regression detection. Like the rest of
`src/browser/`, it is pure Rust with no process/WebView dependency, so it is
fully covered by `cargo test`. `src/bin/velox-bench.rs` is the separate,
deliberately unverifiable-headless runner binary that actually launches
`velox` N times and feeds its output through `benchmark.rs`; see
`docs/benchmarking.md` for the full methodology (scenarios, trial/warmup
policy, fixed test pages, exact commands, and what could and could not be
verified in this project's headless dev/CI environment). Issue #36's
CI regression check is expected to call `velox-bench compare`, whose exit
code (`0` = no regression, `1` = a metric regressed beyond
`--threshold-pct`) is the hook it consumes.

## Test strategy: unit tests vs. integration tests

VeloX draws a hard line between two kinds of automated test, matching the
layer split at the top of this document:

- **Unit tests** (`#[cfg(test)]` modules throughout `src/browser/` and
  `src/config/`, plus `src/ui/`'s own script-generation/parsing logic) —
  the vast majority of `cargo test`'s test count. They exercise pure,
  UI/engine-independent logic directly, with no window, no process, no
  filesystem beyond an occasional isolated temp file. They run in a few
  hundred milliseconds, everywhere, unconditionally, and are what
  `cargo clippy`/CI expect to always be green.
- **Integration tests** (`tests/integration.rs`, Issue #34, see
  docs/decisions.md D47 for the full design rationale) — a small, separate
  suite that launches the actual `velox` binary (`CARGO_BIN_EXE_velox`) and
  observes it from the outside: does it finish starting up, do tab
  operations reach real tab-management code, does history actually get
  persisted to disk, does a download from a page reach VeloX's own
  handler exactly once (D53), does the process end on its own. This is the only
  place VeloX exercises `ui::window::BrowserWindow`, the real WebKitGTK/
  WKWebView/WebView2 engine, and `app::run`'s event loop together, end to
  end.

**Why the split matters, concretely**: Issue #72 (D46) found that VeloX
could ship a CI run with 474/474 unit tests green while the actual `velox`
binary never got past `BrowserWindow::new` in that same CI environment —
no crash, no unit test anywhere near that code path, just silence. Unit
tests alone cannot catch this class of regression *by construction*: they
never construct a `BrowserWindow`, spawn a webview, or run the event loop.
The integration suite exists specifically to close that gap for the one
thing unit tests structurally cannot see — real process startup, real tab
lifecycle through a real window, real file persistence.

**Division of labor, precisely**: a behavior belongs in a unit test
whenever it *can* be expressed as pure logic reachable without a window
(URL normalization, tab-state transitions, ranking, automation script
parsing, perf-record formatting, …) — that stays the default, and the vast
majority of VeloX's logic already lives there per the four-layer split
above. The integration suite is deliberately narrow: it does not re-verify
anything a unit test already covers (e.g. it does not re-test every
`VELOX_AUTOMATION_SCRIPT` command or every malformed-script error path —
`browser::automation`'s own unit tests own that), it only proves that the
already-unit-tested pieces are actually wired together through a real
launch. See D47 (and D53 for the downloads test) for exactly what the five
integration tests each guarantee and, as importantly, what they do not.

**Environment gating**: launching `velox` needs a real display (and, on
Linux, a D-Bus session bus — see docs/benchmarking.md's "実行環境要件" and
D46). A developer machine with no X session, or a CI job with no
Xvfb/`dbus-run-session`, must never see `cargo test` turn red over this —
so every integration test checks the actual process environment at run
time (`browser::gui_probe::gui_probe_reason`, a pure decision function
taking already-read booleans, unit-tested on its own) and skips — printing
why, to stdout, as a pass — rather than attempting a launch guaranteed to
hang or fail. `.github/workflows/ci.yml` installs Xvfb and
`dbus-x11` and wraps its `cargo test` step in
`xvfb-run … dbus-run-session -- …` specifically so these tests run for
real there instead of skipping.
