# VeloX Architecture

Status: single window, multiple tabs, tab suspension, visit history and
bookmarks. This document describes what exists today and where the
extension points are.

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

## UI ↔ engine responsibilities

| Concern | Owner |
|---|---|
| Window, layout, resize | `ui::window::BrowserWindow` |
| Per-tab webview lifecycle (open/close/activate) | `ui::window::BrowserWindow` |
| Toolbar + tab strip rendering, input | `ui/toolbar.html` (in the toolbar webview) |
| IPC protocol (JSON) | `ui::toolbar` (`ToolbarCommand`, `TabSummary`) |
| Tab collection (open/close/activate, which tab is active) | `browser::tabs::Tabs` |
| Address bar / loading state per tab | `browser::tab::Tab` (mirrored into the toolbar) |
| Visit history (persisted) | `browser::history::HistoryStore` + `browser::persistence` |
| Bookmarks (persisted) | `browser::bookmarks::BookmarkStore` + `browser::persistence` |
| Page rendering, network, cookies | web engine (wry) |
| Session history (back/forward) | web engine (wry), per content webview |

VeloX deliberately does **not** duplicate the engine's session history for
back/forward. The engine already tracks redirects, `pushState`, anchors
etc.; a parallel Rust history would drift from reality. `Tab` mirrors only
what the UI needs (current URL, loading flag) — one `Tab` per open tab, held
in `Tabs`.

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

**Recording a visit** happens at the same point session-history state
already updates: `UserEvent::LoadFinished` calls
`app::record_visit_if_enabled`, the single choke point future private
browsing (#7) needs to gate — see docs/decisions.md D13. Recording is not
limited to the active tab: every tab's `LoadFinished` runs through this same
path, so a page finishing in a background tab is recorded too. The entry is
created with `title: None` immediately (the store's own de-duplication
collapses a reload into updating that same entry rather than creating a
new one); `BrowserWindow::fetch_page_title` then asynchronously reads
`document.title` from *that tab's* content webview (a no-op if the tab is
suspended and has none) and reports it back as
`UserEvent::PageTitleResolved { id, title }`, which fills in the title once
it arrives (see docs/decisions.md D12 for why this is async and
best-effort).

**The history/bookmarks panel** is UI inside the *toolbar* webview, not a
separate page — opening it grows the toolbar webview's own bounds rather
than overlaying the content webview, which two independently-bounded
webviews cannot do. See docs/decisions.md D11 for the alternatives
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

## Multiple tabs

- `browser::tabs::Tabs` owns `Vec<Tab>` plus an active index. It is plain,
  UI/engine-independent Rust (open/close/activate, id issuing, which tab is
  active after a close) and is the primary unit-test target for tab
  behavior — no window or webview needed. `Tabs` always keeps at least one
  tab open: closing the last remaining tab is a no-op.
- Tab ids (`browser::TabId`, a `u64` newtype) are assigned once by `Tabs` and
  never reused, so a stale id from a delayed `close_tab`/`activate_tab`
  message simply matches nothing instead of hitting the wrong tab.
- `BrowserWindow` owns one content `WebView` per tab (`HashMap<TabId,
  ContentTab>`) plus which tab is active. Opening a tab builds a new content
  webview bound to that `TabId` (its navigation/load handlers close over the
  id, so their `UserEvent`s are tagged); activating a tab hides the
  previously active webview and shows the target one via `set_visible` +
  `set_bounds` — the webview itself is never destroyed, which is what keeps
  scroll position and form input intact across a tab switch. The toolbar
  webview is shared by all tabs.
- `ToolbarCommand` gained `NewTab`, `CloseTab { id }`, and
  `ActivateTab { id }` — purely additive to the existing serde enum. The
  toolbar pushes tab state back with `TabSummary`/`veloxSetTabs`, rendered as
  the tab strip above the address bar (`src/ui/toolbar.html`).
## Tab suspension

Status: manual suspension shipped, automatic suspension implemented and
opt-in (default off). See docs/decisions.md D9 for the full rationale,
including the WebKitGTK/WKWebView/WebView2 cache-control investigation.

- **What "suspended" means**: `ContentTab::webview` (`ui::window`) is
  `Option<WebView>`; suspending a tab `take()`s and drops it
  (`BrowserWindow::suspend_tab`), reclaiming the memory the webview held.
  `browser::tab::Tab` mirrors this with a `suspended` flag and keeps
  `current_url` — the only state that survives. Scroll position,
  in-progress form input, and session history (back/forward) are lost, the
  same trade-off already accepted for tab *close* — suspension is a deeper
  version of the same idea, not a new category of data loss.
- **Never the active tab**: both the manual command and the automatic sweep
  refuse to suspend the currently active tab — `browser::tabs::Tabs::suspend`
  enforces this on the state side, `BrowserWindow::suspend_tab` defensively
  checks again on the webview side. The visible tab always needs a live
  webview.
- **Resuming**: reactivating a suspended tab (clicking it in the tab strip,
  or it becoming active because the tab in front of it closed) rebuilds the
  webview and reloads `current_url` — `BrowserWindow::resume_tab` is exactly
  `open_tab` followed by `activate_tab`, since rebuilding a dropped webview
  for a `TabId` the app already knows about is the same operation as
  building the first one for a brand new tab.
- **Manual suspension**: `ToolbarCommand::SuspendTab { id }`, sent by a
  per-tab button in the tab strip (hidden for the active tab and for a tab
  already suspended, since clicking the tab itself resumes it — no separate
  "resume" affordance is needed). `TabSummary` carries a `suspended` flag so
  the strip can render dormant tabs distinctly (dimmed, a 💤 marker).
- **Automatic suspension**: `Config::auto_suspend_after: Option<Duration>`
  (default `None`, i.e. disabled) is the idle threshold — how long a
  background tab must have sat unviewed before it is eligible. The pure
  policy logic lives entirely in `browser::tabs::Tabs`:
  `idle_background_tabs(now, idle_after)` (which background, non-suspended
  tabs have crossed the threshold) and `next_idle_deadline(idle_after)` (the
  soonest a still-awake background tab will cross it), both clock-injected
  (`now: Instant` passed in, never read internally) so they are
  unit-testable without sleeping a real thread. `app::run`'s event loop
  calls these on every pass and drives `tao::event_loop::ControlFlow` with
  `WaitUntil(next_deadline)` instead of a fixed `Wait`, so the loop wakes
  itself up exactly when needed rather than polling.
- **Idle clock**: a tab's "idle since" timestamp is the moment it stopped
  being the active tab, recorded by `Tabs::activate_at`/`open_at` (thin
  wrappers around the existing `activate`/`open` that additionally stamp the
  *outgoing* active tab before switching) — see `Tab::mark_backgrounded`.
  The currently active tab's timestamp is never read, since the active tab
  is always excluded from suspension candidates regardless of its value.

## Performance extension points

Design choices made for measurability, and where instrumentation goes next:

- **Startup time**: `main.rs` → `app::run` is a single straight-line path;
  timestamp instrumentation fits at process start, window creation, first
  `Ready`, and first `LoadFinished` (≈ time-to-first-page).
- **Page load time**: `UserEvent::NavigationStarted` → `LoadFinished` already
  brackets every load in one place (`app::handle_user_event`).
- **Memory**: the engine is out-of-process-ish (WebKit's network/render
  helpers); process-tree RSS sampling can be added behind a config flag
  without touching browser logic. Tab suspension (above) is the primary
  lever for reducing it — dropping a background tab's webview releases that
  process-tree's share of RSS; measuring the before/after delta is process-
  tree RSS sampling's first real use case (see docs/decisions.md D9).
- **Tab switch time**: `BrowserWindow::activate_tab` is the single code path
  for switching the visible tab, so it can be timed trivially.
- The `Config` struct is the natural home for benchmark/telemetry toggles.

The layering matters more than any single hook: measurements attach to the
application layer, so swapping or tuning the engine below does not invalidate
them.
