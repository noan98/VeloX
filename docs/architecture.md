# VeloX Architecture

Status: initial MVP (single window, single tab). This document describes what
exists today and where the extension points are.

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

The window hosts **two** webviews:

```
┌──────────────────────────────────────┐
│ ←  →  ↻   [ https://example.com ]    │  toolbar webview (our HTML, trusted)
├──────────────────────────────────────┤
│            Web Content               │  content webview (untrusted pages)
└──────────────────────────────────────┘
```

- The toolbar is rendered from `src/ui/toolbar.html`, compiled into the
  binary with `include_str!`. Using HTML/CSS for chrome avoids pulling in a
  whole native widget toolkit (or a Rust GUI framework) for three buttons and
  a text field, and it is trivially themeable later.
- Keeping chrome and content in *separate* webviews is a security boundary:
  page content can never script the toolbar, and toolbar IPC messages can
  only originate from our own HTML.

On Linux/BSD both webviews are gtk widgets placed in a `gtk::Fixed` inside
the tao window; on macOS/Windows they are true child webviews
(`build_as_child`). `src/ui/window.rs` hides this difference behind
`BrowserWindow`.

## UI ↔ engine responsibilities

| Concern | Owner |
|---|---|
| Window, layout, resize | `ui::window::BrowserWindow` |
| Toolbar rendering + input | `ui/toolbar.html` (in the toolbar webview) |
| IPC protocol (JSON) | `ui::toolbar` (`ToolbarCommand`) |
| URL normalization | `browser::navigation` |
| Address bar / loading state | `browser::tab::Tab` (mirrored into the toolbar) |
| Visit history (persisted) | `browser::history::HistoryStore` + `browser::persistence` |
| Bookmarks (persisted) | `browser::bookmarks::BookmarkStore` + `browser::persistence` |
| Page rendering, network, cookies | web engine (wry) |
| Session history (back/forward) | web engine (wry) |

VeloX deliberately does **not** duplicate the engine's session history for
back/forward. The engine already tracks redirects, `pushState`, anchors
etc.; a parallel Rust history would drift from reality. `Tab` mirrors only
what the UI needs (current URL, loading flag).

The app-level **visit history** (a persisted "where have I been" log,
separate from the above) and **bookmarks** are a different concern entirely
— see the next section.

## Visit history and bookmarks

`browser::history::HistoryStore` and `browser::bookmarks::BookmarkStore` are
plain, serde-derived, UI/engine-independent collections (de-duplication,
caps, ordering — the unit-tested part); `browser::persistence` is the thin
IO layer that loads/saves each as its own JSON file under a per-platform
data directory (env-var resolved, see docs/decisions.md D8). `app::AppState`
owns one instance of each store plus the resolved data directory for the
process's lifetime; every mutation is followed by writing the affected store
back to disk (best-effort — a write failure is logged, never fatal).

**Recording a visit** happens at the same point session-history state
already updates: `UserEvent::LoadFinished` calls
`app::record_visit_if_enabled`, the single choke point future private
browsing (#7) needs to gate — see docs/decisions.md D11. The entry is
created with `title: None` immediately (the store's own de-duplication
collapses a reload into updating that same entry rather than creating a
new one); `BrowserWindow::fetch_page_title` then asynchronously reads
`document.title` from the content webview and reports it back as
`UserEvent::PageTitleResolved { id, title }`, which fills in the title once
it arrives (see docs/decisions.md D10 for why this is async and
best-effort).

**The history/bookmarks panel** is UI inside the *toolbar* webview, not a
separate page — opening it grows the toolbar webview's own bounds rather
than overlaying the content webview, which two independently-bounded
webviews cannot do. See docs/decisions.md D9 for the alternatives
considered and `ui::window::effective_toolbar_height` /
`BrowserWindow::set_panel` for the mechanics. `ToolbarCommand` gained
`ToggleBookmark`, `TogglePanel { panel }`, `DeleteHistoryEntry { id }`,
`ClearHistory`, and `RemoveBookmark { id }`; opening a history/bookmark
entry reuses the existing `Navigate { input }` command (the stored URL is
already normalized, so it round-trips through `navigation::normalize_input`
unchanged) rather than adding a dedicated "open" command.

## Event flow

All state lives on the main thread. Webview callbacks (which may fire at
awkward moments) never touch state directly — they post a `UserEvent` into
the tao event loop:

```
toolbar JS ──ipc.postMessage(JSON)──► UserEvent::ToolbarMessage ─┐
content webview ──navigation/load callbacks──► UserEvent::…      ├─► app::handle_user_event
                                                                 │      │
        ┌────────────────────────────────────────────────────────┘      │
        ▼                                                               ▼
  BrowserWindow methods (load_url, history.back(), reload, …)      Tab state
        │
        ▼
  toolbar.evaluate_script(veloxSetUrl/veloxSetLoading)   ← UI reflects state
```

This gives one single-threaded state machine: no locks, no races, and every
state change is observable in one place (`app.rs`).

## URL input → page display

1. User types into the address bar and presses Enter.
2. Toolbar JS sends `{"cmd":"navigate","input":"<raw text>"}` over IPC.
3. `ui::toolbar::parse_command` deserializes it into `ToolbarCommand::Navigate`.
4. `browser::navigation::normalize_input` turns the text into a URL
   (`example.com` → `https://example.com/`; unsupported/unparseable input is
   rejected and the address bar snaps back).
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

## Adding tabs later

The pieces already in place:

- `app.rs` talks to a `Tab` value, not to globals. A tab strip means holding
  `Vec<Tab>` + an active index.
- Each tab needs its own content webview. `BrowserWindow` would own
  `Vec<WebView>` and switch visibility/bounds on activation; the toolbar
  webview is shared.
- `ToolbarCommand` is a serde enum — adding `NewTab`/`ActivateTab { id }`
  messages is additive.
- Tab suspension (a roadmap item) maps naturally onto dropping a tab's
  webview while keeping its `Tab` state, and rebuilding it on activation.

## Performance extension points

Design choices made for measurability, and where instrumentation goes next:

- **Startup time**: `main.rs` → `app::run` is a single straight-line path;
  timestamp instrumentation fits at process start, window creation, first
  `Ready`, and first `LoadFinished` (≈ time-to-first-page).
- **Page load time**: `UserEvent::NavigationStarted` → `LoadFinished` already
  brackets every load in one place (`app::handle_user_event`).
- **Memory**: the engine is out-of-process-ish (WebKit's network/render
  helpers); process-tree RSS sampling can be added behind a config flag
  without touching browser logic.
- **Tab switch time**: once tabs exist, activation is a single code path in
  `BrowserWindow`, so it can be timed trivially.
- The `Config` struct is the natural home for benchmark/telemetry toggles.

The layering matters more than any single hook: measurements attach to the
application layer, so swapping or tuning the engine below does not invalidate
them.
