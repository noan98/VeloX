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

**Recording a visit** happens at the same point session-history state
already updates: `UserEvent::LoadFinished` calls
`app::record_visit_if_enabled`, the single choke point private browsing
gates via `AppState::history_enabled` — see docs/decisions.md D13 and the
"Private browsing" section below. Recording is not limited to the active
tab: every tab's `LoadFinished` runs through this same path, so a page
finishing in a background tab is recorded too. The entry is
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
  Bookmarks stay ungated (an explicit user action, same call as normal
  mode — see docs/decisions.md D11).

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
- **`Restoring` is real, not a synonym for `Active`**, so a future
  asynchronous session restore (#25) has a state for "selected, but nothing
  is showing yet" instead of needing to add one later. Every current caller
  still collapses `Suspended -> Restoring -> Active` into one call
  (`Tab::resume`), because today's webview rebuild
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
  `current_url`, are the state a future #25 session-restore feature is
  expected to read from — persistence format/schema is #25's own decision,
  not defined here.

## Tab suspension

Status: manual suspension shipped, automatic suspension implemented and
opt-in (default off). See docs/decisions.md D9 for the full rationale,
including the WebKitGTK/WKWebView/WebView2 cache-control investigation, and
D20 for how suspension fits into the `TabState` model above.

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
- **Automatic suspension**: `Config::auto_suspend_after: Option<Duration>`
  (default `None`, i.e. disabled) is the idle threshold — how long a
  background tab must have sat unviewed before it is eligible. The pure
  policy logic lives entirely in `browser::tabs::Tabs`:
  `idle_background_tabs(now, idle_after)` (which `Background`-state tabs
  have crossed the threshold) and `next_idle_deadline(idle_after)` (the
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
  This is independent of the `TabState` transition itself (some tests
  intentionally use the plain `activate`/`open`, which skip the clock
  stamp but still run the state transition correctly). The currently active
  tab's timestamp is never read, since the active tab is always excluded
  from suspension candidates regardless of its value.

## Performance extension points

Implemented in `browser::metrics` (see D16 in `docs/decisions.md`), gated by
`Config::perf_metrics` / `Config::perf_rss_interval` (opt-in via
`VELOX_PERF_METRICS=1`, `VELOX_PERF_RSS_INTERVAL_MS`, same pattern as
`VELOX_DEBUG`). All arithmetic/formatting/process-tree-walking is pure Rust
in `src/browser/metrics.rs`, unit-tested without a window.

- **Startup time**: `main.rs` captures `process_start` before building
  `Config`, and passes it into `app::run`. `metrics::StartupTimestamps`
  records window creation, the toolbar's first `Ready`, and the first
  `LoadFinished` (≈ time-to-first-page) against it; `app::run` prints one
  `velox[perf] startup …` line to stderr once all three have fired.
- **Page load time**: `UserEvent::NavigationStarted` → `LoadFinished` is
  bracketed by `metrics::PageLoadTimer` in `app::run`, logging one
  `velox[perf] page_load …` line per load. Timers are kept per `TabId`, so
  a background tab loading concurrently with the active one does not
  overwrite its start time.
- **Memory**: `metrics::sample_process_tree_rss(pid)` walks the whole
  process tree (WebKit's network/render helpers included) and sums RSS. It
  is a standalone public function with no dependency on `Config` or the
  running app — callable on demand from anywhere. Tab suspension is the
  primary lever for reducing memory: dropping a background tab's webview
  releases that process-tree's share of RSS, and measuring the before/after
  delta with this function is its first real use case (see
  docs/decisions.md D9). When `perf_rss_interval` is set, `app::run` also
  spawns a background thread that samples it periodically and logs
  `velox[perf] rss …` lines. Implementation reads `/proc` directly on Linux
  (no extra dependency); other Unix falls back to parsing `ps` output;
  Windows is not implemented yet (`RssError::Unsupported`).
- **Tab switch time**: `BrowserWindow::activate_tab` is the single code path
  for switching the visible tab, so it can be timed trivially — not
  instrumented yet.
- The `Config` struct is the home for these toggles; `Config::from_env_and_args`
  layers the environment-variable overrides onto `Config::default`.

When metrics are off, `app::run` never spawns the RSS thread and every
checkpoint is a single `Option`-is-`None` check with no `Instant::now()`
call — the disabled path stays effectively free.

The layering matters more than any single hook: measurements attach to the
application layer, so swapping or tuning the engine below does not invalidate
them.
