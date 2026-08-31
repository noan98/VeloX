# Design Decisions

Records of the implementation decisions behind the initial MVP, so future
changes can revisit them with context.

## D1: Web engine — wry (system webviews), not Servo

**Decision**: render web content through [wry](https://crates.io/crates/wry)
0.56, which wraps the platform webview: WebKitGTK on Linux, WKWebView on
macOS, WebView2 (Chromium) on Windows.

**Why not Servo?** Servo was evaluated first, per the project brief:

- Servo's embedding API (`libservo`) is still churning; it is not published
  as a stable crate on crates.io, and embedders are expected to track
  Servo's repository. Pinning VeloX to a moving embedding API contradicts
  the "actually runs, stays buildable" goal.
- Building Servo from source takes tens of minutes and a large toolchain
  (cmake, ninja, plus per-platform SDKs) — a heavy toll on every contributor
  and CI run.
- Web compatibility is still far behind WebKit/Chromium; the MVP goal is
  displaying real pages reliably.

**Why wry over alternatives?**

- Actively maintained (it underpins Tauri) with a documented, stable API.
- Small dependency surface for us: the engine ships with the OS, so VeloX
  binaries stay small and build times stay short.
- One codebase covers macOS / Linux / Windows today.

**Cost / revisit condition**: VeloX does not control the engine version and
the engine differs per OS. If VeloX later needs engine-level features
(custom networking, real content blocking, per-tab process control), that is
the moment to re-evaluate Servo embedding (or CEF) — the `ui`/`browser`
layering keeps that swap localized to `src/ui/window.rs`.

## D2: Window/event loop — tao, not winit

wry supports both. tao is maintained by the same project as wry, is gtk-based
on Linux (matching WebKitGTK, so no X11-only hacks — wry's winit example
explicitly panics on Wayland), and its event loop model is what Tauri
exercises in production against wry.

## D3: Browser chrome as an HTML toolbar in a second webview

Alternatives considered: native widgets per platform (three implementations),
a Rust GUI toolkit like egui/iced (a second rendering stack in the binary),
or drawing chrome inside the content webview (no isolation from untrusted
pages).

A dedicated toolbar webview reuses the engine we already ship, keeps chrome
isolated from page content, and needs zero additional dependencies. The
toolbar HTML is `include_str!`-ed into the binary, so there is nothing to
install alongside the executable.

## D4: Back/Forward via the engine's session history

`history.back()` / `history.forward()` are executed in the content webview
(wry exposes no dedicated go-back API, but the JS History API drives the same
WebKit/WebView2 session history). VeloX intentionally does not keep its own
history stack: the engine's history already handles redirects, `pushState`
and fragment navigation correctly, and a mirrored stack would drift. The
`Tab` struct mirrors only the current URL and loading flag for the UI.

Trade-off accepted for the MVP: without a queryable canGoBack/canGoForward,
the back/forward buttons are always enabled and act as no-ops at the history
edges.

## D5: Single-threaded state via event loop messages

Webview callbacks (IPC, navigation, page-load) run at unpredictable points.
Instead of sharing state behind mutexes, every callback posts a `UserEvent`
through tao's `EventLoopProxy`, and all state mutation happens in one match
statement on the main thread. Simpler to reason about, nothing to lock, and
one obvious place to add tracing/metrics later.

## D6: Dependency policy

Every direct dependency has one clear job:

| Crate | Job |
|---|---|
| `wry` | webview embedding (the engine) |
| `tao` | window + event loop |
| `serde` / `serde_json` | typed toolbar IPC messages; safe JS string escaping |
| `url` | parsing/normalizing address bar input |
| `gtk` (Linux only) | placing two webviews in one gtk window |

Notably absent on purpose: an async runtime (nothing here is async), an
error-handling framework (std `Result` + one `Box<dyn Error>` boundary at
startup suffices), and any HTTP client (the engine owns networking).

## D7: Address-bar input handling

Only `http(s)`, `file`, `about` and `data` URLs are accepted; schemeless
input is retried as `https://<input>`; anything else is rejected and the
address bar snaps back to the current page. No search-engine fallback yet —
that is a product decision (default engine, privacy) deferred until settings
exist. Rejecting unknown schemes also keeps surprises (e.g. `javascript:`)
out of the engine.

## D8: Multiple tabs — one content webview per tab, kept alive while open

**Decision**: `BrowserWindow` owns one content `WebView` per open tab
(keyed by `TabId`, a never-reused `u64`), all attached to the same window at
once. Switching the active tab toggles `WebView::set_visible`/`set_bounds`
on the outgoing and incoming webview; it never destroys and rebuilds a
webview on switch.

**Why not one webview, reloaded per tab switch?** That would be simpler (one
`WebView` field, load a different URL on activation) but throws away
scroll position, in-progress form input, and JS-side state (e.g.
`pushState` history, unsaved editor content) on every switch — the issue's
acceptance criteria rule this out directly ("タブ切替時に表示中ページの状態
… が失われない").

**Why not a single Rust-side `TabId -> WebView` map with all webviews
visible/stacked, relying on z-order?** wry only exposes `set_bounds` /
`set_visible`, not z-order control uniform across the gtk/WKWebView/WebView2
backends; explicit visibility toggling is the portable primitive and is
simple to reason about (`BrowserWindow::activate_tab` is the one place tab
switching happens).

**Tab collection lives in `browser::tabs::Tabs`, not in `BrowserWindow`**:
`Tabs` (open/close/activate, id issuing, active-tab bookkeeping) has zero
webview/window dependencies, so it is unit-tested directly — the acceptance
criterion "cargo test covers open/close/activate, including closing the last
tab" is covered here without a display. `BrowserWindow` mirrors only which
`TabId`s currently have a content webview; `app.rs` is the single place that
keeps `Tabs` and `BrowserWindow` in sync (`Tabs` is always updated first,
then pushed to `BrowserWindow`).

**Ids, not indices**: `TabId` is a `u64` issued once by `Tabs` and never
reused, rather than a `Vec` index. Toolbar IPC messages (`close_tab`,
`activate_tab`) carry a tab id set by a `veloxSetTabs` render that may be
stale by the time the user clicks (another tab already closed, shifting
indices) — an id lookup just misses cleanly (logged, no-op) instead of
silently acting on the wrong tab.

**Built for suspension, not implementing it**: `ContentTab::webview` (in
`ui::window`) is `Option<WebView>` specifically so a follow-up "tab
suspension" feature (dropping a background tab's webview to reclaim memory,
issue #5) can `take()` it and rebuild later from the surviving `Tab` state,
without reshaping this struct. VeloX does not suspend tabs today — every
open tab's webview is always `Some` — this is scaffolding, not a feature.

**Trade-off accepted**: every open tab keeps a live webview (and the memory
that comes with it) for the lifetime of this issue; that is exactly the gap
issue #5 (tab suspension) is scoped to close.

## D9: Tab suspension — drop the webview, keep the URL; no engine cache tuning

**Decision**: implement tab suspension exactly as D8 scaffolded it —
`ContentTab::webview.take()` drops a background tab's webview, and
reactivation rebuilds it from the surviving `Tab` state (just `current_url`).
Manual suspension (a tab-strip button) ships now; automatic suspension
(idle-time policy) ships too, but disabled by default. Engine-level cache
tuning was investigated and **not applied** — see below.

### What is lost on suspension

Only `Tab::current_url` survives. Everything the *engine* held is gone with
the webview:

- Scroll position and in-progress form input (already lost on an ordinary
  tab *close*; suspension is the same loss, just reversible).
- JS-side state: `pushState`/`replaceState` history, in-memory app state,
  open WebSocket/EventSource connections.
- The engine's own session history (back/forward) — D4 already declined to
  mirror this in Rust, so resuming a tab is indistinguishable from a fresh
  navigation to `current_url`; the back button will not reach whatever the
  tab's history held before suspension.

This is deliberately the same shape of loss D8 already accepted for closing
a tab, applied to a tab that is not closed — suspension trades that state for
memory, reversibly (the URL comes back; nothing else does). The tab strip
marks a suspended tab distinctly (dimmed, 💤) precisely so this is not a
silent surprise: what looks like "still open" is actually "will reload from
scratch when you switch to it."

### Never the active tab; idle clock lives in `browser::tabs::Tabs`

The active tab is never a suspension candidate — the visible tab always
needs a live webview. This is enforced twice: `Tabs::suspend` refuses on the
state side (so a bug in `ui::window` can never even attempt it), and
`BrowserWindow::suspend_tab` defensively refuses again on the webview side.

The automatic-suspension policy — "how long has a background tab sat idle" —
is pure, clock-injected Rust in `browser::tabs::Tabs`
(`idle_background_tabs(now, idle_after)`, `next_idle_deadline(idle_after)`),
matching D8's precedent of keeping tab-collection logic engine/UI-independent
and unit-tested without a window. "Idle since" is the moment a tab stopped
being active, stamped on the *outgoing* tab by `Tabs::activate_at`/`open_at`
— additive wrappers around the existing `activate`/`open` (which stay
untouched) that stamp the departing tab before switching. `app::run`'s event
loop drives `tao::event_loop::ControlFlow::WaitUntil(next_deadline)` instead
of a fixed `Wait` so it wakes itself up exactly when a tab crosses the
threshold, rather than polling on a fixed tick.

**Default is disabled** (`Config::auto_suspend_after: None`): the issue asks
for manual suspension before automatic, and an unexpected suspension (losing
scroll position/form input the user didn't ask to lose) is a worse default
than doing nothing. Enabling it today means changing `Config::default()` in
code — `Config` has no runtime loading yet (its own doc comment: "currently
compile-time defaults only"); wiring it to an env var (in the shape of #3's
`VELOX_PERF_METRICS`) is straightforward once that pattern exists on `main`,
but is not added speculatively here.

### Effect measurement: deferred to #3, not duplicated

The acceptance criterion "process-tree RSS decreases, measurably, after
suspension" needs #3's `browser::metrics::sample_process_tree_rss`, which is
merged in a separate, already-PR'd branch (`claude/issue-3-perf-metrics`) not
yet in this stacked branch's history. Calling it here would not compile, and
reimplementing an equivalent sampler here would fork the same logic across
two issues. So:

- This branch does not call `browser::metrics` and does not reimplement RSS
  sampling.
- What *is* covered here, in code and tests: `BrowserWindow::suspend_tab`
  unconditionally drops the `Option<WebView>` it holds
  (`tab.webview.take()`), and `browser::tabs::Tabs::suspend` records the
  transition on the state side (`browser::tabs::tests::suspend_*`,
  `browser::tab::tests::suspend_*`). A `WebView`'s `Drop` releasing its
  WebKitGTK/WKWebView/WebView2-side resources is the platform's contract,
  not VeloX's to re-test.
- Manual verification procedure, once #3 is on `main` (or merged into this
  branch): run with `VELOX_PERF_METRICS=1 VELOX_PERF_RSS_INTERVAL_MS=1000
  cargo run`, open several tabs and let each load a real page, note the
  periodic `velox[perf] rss` total, suspend the background tabs (button or
  `auto_suspend_after`), and confirm the next sample's total drops. This
  browser cannot be run in the sandboxed/headless environment this issue was
  implemented in, so this procedure — not an automated measurement — is the
  acceptance check for that criterion until #3 lands here.

### Cache-control investigation (wry 0.56.1, `~/.cargo/registry/.../wry-0.56.1`)

The issue asks to survey what cache tuning the system webviews expose
through wry before touching anything. Findings, read directly from the
vendored `wry` 0.56.1 source (not from docs, since the issue warned a guessed
API would fail to build):

| Platform / backend | What wry exposes | Notes |
|---|---|---|
| WebKitGTK (`src/webkitgtk/mod.rs`) | Nothing tunable. `set_webview_settings` hardcodes `settings.set_enable_page_cache(true)` on every webview; no builder method or `WebContext` setter changes it. | The *disk* cache model/size lives on `libwebkit2gtk`'s `WebKitWebContext`/`CacheModel`, reachable only through the raw `webkit2gtk::WebContext` (`WebContextExt::context()`), i.e. by adding `webkit2gtk` as a **new**, Linux-only direct dependency and bypassing wry's own API. |
| WKWebView (macOS, `src/wkwebview/mod.rs`) | Nothing. No cache- or memory-related method anywhere in the backend. | WKWebView's own cache is entirely OS-managed; no public hook in wry at all. |
| WebView2 (Windows, `src/webview2/mod.rs`) | `WebViewExtWindows::set_memory_usage_level(MemoryUsageLevel::Low \| Normal)` — `#[cfg(target_os = "windows")]` only, added specifically for "app going inactive" scenarios (wraps `ICoreWebView2Controller4::SetMemoryUsageTargetLevel`, Runtime ≥114.0.1823.32; a no-op on older runtimes). | The closest thing to a purpose-built hook for this exact feature, but Windows-only. |
| All platforms (`WebView::clear_all_browsing_data`) | Wipes cookies, cache, and local storage together — no way to target just the cache. | Wrong tool: applying it on suspend would silently log the user out of every suspended tab's site, which is a correctness regression, not a memory optimization. |

**Applied**: none of the above. Reasoning:

- WebKitGTK's only lever requires a new direct dependency
  (`webkit2gtk`, Linux-only) reaching past wry's own API, which D6's
  dependency policy asks to justify — and the payoff (tuning a cache model
  enum) is speculative next to the deterministic, cross-platform win that
  dropping the whole webview already gives.
- WKWebView exposes nothing at all; there is no decision to make there.
- WebView2's `set_memory_usage_level` is the one genuinely relevant, well-
  targeted hook (it is *for* exactly this: backgrounded content) — but it is
  `#[cfg(target_os = "windows")]`, and applying a Windows-only soft memory
  hint to the *other* two platforms' background tabs (the ones not yet
  suspended, e.g. still under the idle threshold) would be a partial,
  asymmetric feature that suspension's actual mechanism (dropping the
  webview outright) already dominates for the tabs it applies to. Deferred:
  worth adding as a `#[cfg(windows)]`-gated call on tabs that just went to
  the background but have not crossed the auto-suspend threshold yet, as a
  cheap "soften while waiting" step — tracked as a possible small follow-up,
  not blocking this issue.
- `clear_all_browsing_data` is excluded outright: correctness regression
  (drops cookies/session), not a cache optimization.

**Revisit condition**: if profiling ever shows the *webview construction*
step on resume (not the steady-state memory of a suspended tab) dominating —
e.g. cold cache making every resume feel like a first load — that is the
moment to reconsider `webkit2gtk`'s `CacheModel`/`WebsiteDataManager` as a
deliberate new Linux-only dependency, or the WebView2 memory-level hint as a
`cfg(windows)` addition, each on its own merits rather than as a bundle.

## D10: History/bookmarks persistence — JSON files, no new dependency

History (`browser::history::HistoryStore`) and bookmarks
(`browser::bookmarks::BookmarkStore`) are plain, serde-derived structs;
`browser::persistence` is a thin wrapper that reads/writes each as its own
pretty-printed JSON file (`history.json`, `bookmarks.json`) using `serde`/
`serde_json`, both already dependencies (D6). No database crate was added —
the issue's own guidance is to start file-based and revisit (SQLite or
similar) only once entry counts make that necessary; `HistoryStore` already
enforces a cap (`Config::history_max_entries`, default 5000), so an
unbounded file is not a near-term risk.

The collection logic (de-duplication of consecutive visits/bookmarked URLs,
the history cap, ordering, removal) lives entirely in `history.rs`/
`bookmarks.rs` as pure functions with no filesystem or UI dependency, per the
existing `browser::navigation`/`browser::tab` pattern — that is what the unit
tests exercise. `persistence.rs` only turns a store into/from JSON on disk
and is intentionally "dumb"; its own tests are the round-trip-through-a-temp-
directory kind, not collection-logic tests.

**Data directory resolution**: a `dirs`-style crate was considered and
rejected for the same reason new dependencies generally are (D6) — the need
is met by reading the one environment variable each platform already
guarantees (`XDG_DATA_HOME`/`HOME` on Linux/BSD, `HOME` on macOS, `APPDATA`
on Windows), with `VELOX_DATA_DIR` as an explicit override used by nothing
in-repo today but available for tests or portable installs. If VeloX later
needs more XDG-adjacent behavior (config dir, cache dir, respecting
`XDG_DATA_DIRS` for lookup, etc.) that breadth is the point at which pulling
in `dirs` starts paying for itself; one directory for one file pair does not
justify it yet. When no relevant environment variable is set, VeloX degrades
to an unpersisted in-memory session (logged once at startup) rather than
failing to start.

## D11: History/bookmarks UI is a panel in the toolbar webview, not a content-webview page

The toolbar and content areas are two separate native webviews with
independently managed bounds (D3); a dropdown rendered inside the toolbar
webview's HTML cannot visually extend over the content webview's screen
region, since each webview is clipped to its own allocated rect. Two designs
were considered:

- **A dedicated internal page** (e.g. `velox://history`) loaded into the
  *content* webview, in the spirit of `chrome://`/`about:` pages in other
  browsers. Rejected for now: making it interactive (delete/clear buttons)
  would need either an IPC channel on the content webview — which breaks the
  D3 security boundary that untrusted page content can never talk to
  Rust — or intercepting `velox://`-scheme navigations in the existing
  content `navigation_handler`, which is reachable by *any* loaded page
  (`<a href="velox://history?clear=1">`), not just our own generated one,
  without extra provenance tracking real browsers use to gate `chrome://`
  navigation to trusted origins only. Solvable, but more machinery than this
  issue needs.
- **A panel inside the toolbar webview, grown into view** (chosen): opening
  the history/bookmarks panel resizes the toolbar webview's own native
  bounds (`toolbar_height` + `Config::panel_height`, see
  `ui::window::effective_toolbar_height`) instead of overlaying anything;
  the panel's HTML/CSS just fills whatever height the webview actually has
  (`body { display:flex; flex-direction:column }`, `#panel { flex:1 }`).
  This keeps the panel inside the already-trusted webview — no new IPC
  surface, no scheme interception — at the cost of the content webview
  visibly shrinking while a panel is open (acceptable: it is a deliberate,
  user-triggered, temporary state, closed automatically on navigating from a
  panel entry).

`BrowserWindow` tracks which panel is open in a `Cell<Option<Panel>>`
(interior mutability, main-thread-only) because the window-resize handler in
`app.rs` only holds `&BrowserWindow`, not the mutable app state, and still
needs to preserve the open panel's extra height across a resize.

## D12: Page titles are fetched asynchronously and applied best-effort

wry's `WebView::evaluate_script` cannot return a value synchronously; only
`evaluate_script_with_callback` can, and its callback fires later (and off
the call stack that triggered it). So `LoadFinished` records a history entry
with `title: None` immediately (using the URL as the interim display value),
then asks `document.title` for that entry's `id` and applies whatever comes
back whenever it arrives (`UserEvent::PageTitleResolved`). This matches the
staged design the issue itself suggested as acceptable. A blank/whitespace
title is dropped rather than overwriting a previously known one (covers
pages that briefly have an empty `<title>` before script sets it); the
result always applies to the `id` it was requested for regardless of what
the tab is showing by the time it arrives, so a late title only ever affects
the (now possibly no-longer-current) history entry it belongs to.

## D13: Single choke point for disabling history recording

`app::record_visit_if_enabled` is the only place `HistoryStore::record_visit`
is called from, gated by `AppState::history_enabled`. This is the seam the
following issue, #7 (private browsing), was expected to use, and now does:
`app::run` sets `history_enabled = !config.private` at startup, so private
browsing needed no change to `HistoryStore` itself, `persistence`, or any
other call site — see D14 below. Bookmarking is deliberately **not** gated
by this flag — explicitly saving a bookmark is a distinct user action from
passive visit recording, and real browsers typically keep letting users
bookmark from a private window/tab — but the same `AppState`-level pattern
would extend to it if a future decision says otherwise.

## D14: Private browsing (#7) — whole-app mode, not a separate private window

**Decision**: private browsing is a single flag (`Config::private`,
sourced from the `VELOX_PRIVATE` env var or a `--private` CLI arg parsed by
hand — see D6, no CLI-parsing crate added) that puts the *entire* running
process into private mode for its whole lifetime. There is no "open a
private window" action and no per-tab private state.

**Why not a separate private window, as the issue's own implementation
notes suggested as the more realistic long-term shape?** VeloX is
single-window today (docs/architecture.md, "Adding tabs later" is still a
roadmap item, not implemented). A second, independently-private window
needs multi-window support to exist first — `BrowserWindow` today owns
*the* `tao::window::Window` for the process, and `app::run`'s event loop
closes the whole process on the first `CloseRequested`. Building
multi-window support as a side effect of this issue would both blow its
scope and pre-empt a design that deserves its own decision (e.g. how
`AppState` — currently one struct for the one window — splits per window).
The issue's own text names exactly this tradeoff and accepts starting with
an app-wide flag; this decision records that choice formally.

**Extension path when multi-window support lands**: the seams are already
where they'd need to be to make each window's privacy independent without
revisiting this issue's code:

- `Config::private` becomes a per-window construction parameter instead of
  a process-wide startup flag (e.g. `BrowserWindow::new` already takes it
  from `&Config` today — that just stops being *the* process config and
  becomes *a* window's config).
- `.with_incognito(...)` is already a per-`WebViewBuilder` call
  (`ui::window.rs`), not a global engine setting, so giving one window's
  content webview an ephemeral store while another's stays persistent needs
  no change there.
- `AppState::history_enabled` (above) would need to move from one
  process-wide bool to a per-window (or per-tab, once tabs exist) value,
  since recording has to be gated per window rather than for the whole app.
- The toolbar badge/color and window-title suffix (below) are already
  computed from a `bool` passed in at window-build time, so per-window
  values just flow through unchanged.

## D15: `wry::WebViewBuilder::with_incognito` — availability and per-platform backing

Verified against wry 0.56.1's source (`~/.cargo/registry/src/.../wry-0.56.1`,
not assumed from memory, since a nonexistent API here would simply fail to
compile): `with_incognito(bool)` exists on `WebViewBuilder` and is wired on
every platform VeloX ships on. Only the *content* webview gets
`.with_incognito(config.private)` — the toolbar webview only ever loads our
own embedded `TOOLBAR_HTML` (`with_html`, no site content, no cookies), so
there is nothing there for incognito to isolate.

Per-platform backing (all give a non-persistent, in-memory-only store; none
of this is emulated by VeloX itself):

- **WebKitGTK (Linux/BSD)** — `webkitgtk/mod.rs`: when `incognito` is set,
  wry builds the webview against `WebContext::new_ephemeral()` instead of
  the default/shared context, and **ignores any custom `WebContext`**
  passed via `attributes.context` (wry's own doc comment on `incognito`
  says so explicitly). VeloX does not pass a custom context today, so this
  does not affect us, but it would matter if `WebContext` ever gets used
  for something else (e.g. proxy config) alongside incognito.
- **WKWebView (macOS/iOS)** — `wkwebview/mod.rs`: when `incognito` is set,
  the webview's `WKWebViewConfiguration` gets
  `WKWebsiteDataStore::nonPersistentDataStore()`. This path does not depend
  on the "custom data store by identifier" OS-version gate (macOS 14+ /
  iOS 17+) that the *non*-incognito branch uses for
  `pl_attrs.data_store_identifier` — `nonPersistentDataStore()` itself has
  been available since far earlier OS versions, so incognito is not
  version-gated on Apple platforms.
- **WebView2 (Windows)** — `webview2/mod.rs`: `incognito` maps to
  `ICoreWebView2ControllerOptions3::SetIsInPrivateModeEnabled(true)`, which
  is only reachable by casting the environment to
  `ICoreWebView2Environment10`. If that cast fails (an old WebView2 Runtime
  that predates that interface), the whole `controller_opts` block is
  skipped and **`incognito` is silently not applied** — no error is
  surfaced by wry. In practice the WebView2 Runtime auto-updates
  (Evergreen distribution), so this is a theoretical edge case rather than
  an expected one, but it means Windows is the one platform where "private
  mode was requested" is not a hard guarantee at the engine level; nothing
  in VeloX today detects or reports this fallback.

No VeloX code branches on platform for this — `.with_incognito(bool)` is
one call in `ui::window.rs`, and wry owns the platform dispatch.
