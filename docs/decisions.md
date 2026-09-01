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

## D16: Performance metrics — `/proc` directly, no new dependency

**Decision**: implement startup timestamps, page-load duration and
process-tree RSS sampling (Issue #3) as a pure, UI/engine-independent module
(`browser::metrics`), reading `/proc` directly on Linux instead of adding a
crate like `sysinfo`.

**Why not `sysinfo` (or similar)?** `sysinfo` (and comparable crates) pull in
a much larger surface than this needs — full per-process CPU/disk/network
stats, a whole-system snapshot API, and platform backends for OSes VeloX does
not even build native RSS support for yet. VeloX's own dependency policy
(D6) asks "why is this needed" before adding a crate; here the actual need
is two integers per process (`PPid`, `VmRSS`) out of a text file the kernel
already exposes, which is a few dozen lines of parsing — well within "not
worth a dependency" territory. Per D6's own logic (the engine already ships
what we need, don't duplicate it in a crate), the direct-`/proc` route is
also what let this land with zero new lines in `Cargo.toml`.

**Design**:

- `browser::metrics` holds only pure logic: `StartupTimestamps` (four
  checkpoints → a `Duration` report), `PageLoadTimer` (start/finish →
  `Duration`), and `sample_process_tree_rss(pid)` (walks `/proc`, sums
  `VmRSS` over the whole descendant tree — WebKit's network/render/GPU
  helper processes included, since a single-PID reading would understate
  real memory use). All formatting and tree-walking is unit-tested with
  synthetic data; the `/proc` integration itself is exercised against the
  test process's own tree (including a spawned child) since CI runs on
  Linux.
- `sample_process_tree_rss` is a standalone function — no `Config` or app
  state involved — specifically so Issue #5 (tab suspension) can call it
  directly to measure RSS before/after suspending a tab, independent of
  whether startup/page-load logging is enabled.
- Enabling is a `Config` flag (`perf_metrics`, plus `perf_rss_interval` for
  the periodic RSS sampler), following the existing `VELOX_DEBUG` env-var
  pattern via `Config::from_env_and_args` (`VELOX_PERF_METRICS`,
  `VELOX_PERF_RSS_INTERVAL_MS`). When off, `app::run` never calls
  `Instant::now()` for these checkpoints and never spawns the sampling
  thread — the check is a single `Option::is_none()`/`bool` per event, so
  the disabled path carries no meaningful overhead.
- Output is structured `eprintln!` lines (`velox[perf] …`) for now, matching
  the issue's "stderr is enough for now" scope; visualization/CI benchmarks
  are left to follow-up issues.

**Cost / revisit condition**: the `/proc` path only covers Linux precisely.
Non-Linux Unix falls back to shelling out to `ps` (untested by this
project's Linux-only CI); Windows returns `RssError::Unsupported` for now.
If accurate cross-platform RSS becomes a priority before this project adds a
Windows/macOS CI leg, that is the point to revisit a crate (or
platform-specific APIs) with the actual OSes to test against, rather than
guessing at `ps`/API behavior blind.

## D17: Content blocking — navigation-level only, wry 0.56 exposes no subresource hook

**Decision**: VeloX blocks ad/tracker domains at main-frame navigation time,
via `WebViewBuilder::with_navigation_handler` returning `false` for a
blocked URL (`src/ui/window.rs`). The matching itself is a small,
hand-written EasyList-subset parser/matcher (`src/browser/blocklist.rs`,
`FilterList`) — pure logic, no engine dependency, fully unit-tested.
Subresource-level blocking (images/scripts/XHR/fetch) is **not**
implemented; the reasons follow.

**What was checked**: wry 0.56.1's public API was read directly from the
vendored crate source
(`~/.cargo/registry/src/*/wry-0.56.1/src/lib.rs`, `webview2/mod.rs`,
`wkwebview/*`), specifically every `WebViewBuilder::with_*` method and the
CHANGELOG, looking for a request-interception hook usable for subresources.

**Findings, per platform**:

- **Cross-platform surface (what wry actually exposes today)**:
  `with_navigation_handler(Fn(String) -> bool)` — "decide if incoming url is
  allowed to navigate"; this is a frame-navigation decision (matches
  WebKitGTK's `decide-policy` for navigation actions / WKWebView's
  `decidePolicyForNavigationAction` / WebView2's `NavigationStarting`), not a
  per-subresource one. `with_custom_protocol` /
  `with_asynchronous_custom_protocol` intercept requests, but only for a
  custom URI *scheme* registered up front (e.g. `wry://`) — they are not
  invoked for ordinary `http`/`https` subresource loads, so they cannot be
  used as a generic ad-request filter. No `with_web_resource_request_handler`
  (or any request/response interception hook) exists in this version.
- **WebKitGTK (Linux)**: the issue's implementation notes point at WebKit's
  `WebKitUserContentFilter` (content-blocker JSON, compiled via
  `webkit_user_content_filter_store_save`). wry does not bind
  `WebKitUserContentFilter`, `WebKitUserContentManager`'s filter APIs, or the
  underlying `webkit2gtk` request-decision signals anywhere in its public
  surface or its `webkit2gtk`-backed internals that `wry::WebViewBuilder`
  exposes to callers.
- **WKWebView (macOS)**: same gap for `WKContentRuleList` /
  `WKContentRuleListStore` — not present in `wry::WebViewBuilderExtWebview2`
  or the `webkit2gtk`/`wkwebview` platform modules' public re-exports.
- **WebView2 (Windows)**: same gap for `ICoreWebView2.WebResourceRequested`
  (or `AddWebResourceRequestedFilter`) — wry's `webview2` module wires
  webview2-com's navigation and permission events, but not
  `WebResourceRequested`.

**Conclusion**: reaching any of the three platform-native subresource
hooks from `wry::WebViewBuilder` would require either (a) an upstream PR to
wry adding a cross-platform `with_web_resource_request_handler`-style API
(all three platforms have the underlying native hook; wry simply doesn't
bind it yet), or (b) reaching past `wry::WebView` into the raw platform
webview object it wraps (`webkit2gtk::WebView` / `WKWebView` /
`ICoreWebView2`) and driving the native filtering API directly per
platform — three separate, `unsafe`-adjacent implementations, which the
project's dependency/complexity budget (D6) and the "no unsafe" rule rule
out for this iteration. Per the issue's acceptance criteria, this
investigation result is recorded here instead of a subresource
implementation.

**Cost / revisit condition**: revisit once either upstream wry lands a
request-interception hook (watch the wry CHANGELOG), or VeloX outgrows wry
entirely (see D1's revisit condition, which already lists "real content
blocking" as a trigger for re-evaluating the engine layer).

**Filter list**: a small (~45-rule) hand-picked list of known ad/tracker
domains ships embedded in the binary (`src/browser/default_blocklist.txt`,
via `include_str!`), written in the same `||domain^` / `@@||domain^` syntax
EasyList/EasyPrivacy use so a real list can be dropped in later. It is
**not** a copy of EasyList/EasyPrivacy and nothing is fetched over the
network — this environment cannot verify network-list-update code, and it
would add scope (refresh scheduling, caching, format coverage) beyond a
single content-blocking PR. `Config::extra_blocklist_path` lets a user
point at a larger/updated list file, merged on top of the built-in one at
startup (`FilterList::merge`); fetching that file from a URL is left to a
follow-up issue.

**Why no `adblock`-style crate**: the matching needed here (domain-anchor
block/exception, nothing else) is a few dozen lines with no external
dependency; pulling in a full adblock-rule-engine crate for that would
violate D6's "every dependency has one clear job" bar, and would also add
network-fetching and cosmetic-filtering machinery VeloX does not use.

## D18: DevTools — feature gating, shortcut delivery, and the IPC trust boundary

**Scope**: only the content webview gets devtools (`WebViewBuilder::with_devtools(true)`).
The toolbar webview is trusted, first-party chrome; there is nothing on it to
inspect, and giving it an inspector would be one more surface with no
corresponding benefit.

**Feature gating (`devtools` Cargo feature)**: wry only compiles
`WebView::open_devtools` / `close_devtools` / `is_devtools_open` under
`#[cfg(any(debug_assertions, feature = "devtools"))]`, and on macOS the
underlying implementation pokes a private WKWebView preference
(`developerExtrasEnabled`) to get an in-page inspector at all — wry's own
docs say not to ship that in an App-Store release build. Debug builds get it
for free via `debug_assertions`; the only decision left is whether to turn on
wry's `devtools` Cargo feature (which would also flip release builds).

We split the `wry` dependency by target in `Cargo.toml` instead of adding the
feature project-wide:

```toml
[target.'cfg(not(target_os = "macos"))'.dependencies]
wry = { version = "0.56", features = ["devtools"] }

[target.'cfg(target_os = "macos")'.dependencies]
wry = "0.56"
```

Result: Linux (WebKitGTK) and Windows (WebView2) have devtools in both debug
and release builds — the issue's "ほぼ完結する" case, no private-API concern
there. macOS only has it in debug builds (`debug_assertions`); a macOS
release binary does not compile the private-API path at all, matching wry's
own guidance. `src/ui/window.rs`'s `BrowserWindow::open_devtools` mirrors the
same `#[cfg(any(debug_assertions, not(target_os = "macos")))]` condition, with
a stderr-logging stub (the `app.rs` `log_failure` philosophy — never a
compile error or a crash) for the excluded macOS-release case.

**Shortcut delivery — why an injected script over a tao accelerator**: the
issue asks for F12 (and macOS's Cmd+Opt+I) to work while the content webview
has focus. We considered tao's menu accelerators and
`WindowEvent::KeyboardInput` first, since both already exist in tao/wry and
would need no IPC plumbing. Rejected: the content webview is a native child
widget (a `gtk::Fixed`-positioned WebKitGTK widget on Linux, WKWebView on
macOS, WebView2 on Windows — see D3's diagram); once it has keyboard focus,
the OS delivers key events straight to that child widget, not to tao's
window-level event loop, so neither approach reliably observes the keypress
while the page is focused — the exact failure mode the issue calls out.
Requiring the user to click the toolbar first to regain window-level focus
before F12 works would defeat the point of a shortcut.

Instead, `BrowserWindow::new` injects a small JS snippet into the content
webview via `with_initialization_script` (so it runs before any page script,
on every navigation) that listens for `keydown` in the **capture** phase and
calls `event.preventDefault()` + `window.ipc.postMessage(...)` on a match.
Capture-phase + inject-first gives our listener first refusal even against
pages that install their own keydown handlers to swallow F12 (a known trick
some sites use against browser devtools) — not a hard security guarantee,
just best-effort, which is all this needs.

**The IPC trust boundary**: `docs/architecture.md`'s "Why a webview
toolbar?" treats the toolbar/content split as a real security boundary
(untrusted page content must never reach the chrome). The toolbar's existing
IPC handler (`src/ui/toolbar.rs`) deserializes a structured, trusted
`ToolbarCommand` enum — that parser must never run against content-webview
input, since arbitrary pages could then forge any toolbar command (navigate,
reload, etc.) via `window.ipc.postMessage`.

So the content webview gets its *own*, separate `with_ipc_handler`, wired to
its own `UserEvent::OpenDevtoolsRequested` — not `UserEvent::ToolbarMessage`
and not `ToolbarCommand`. It does not deserialize JSON or interpret any
page-supplied data at all: it does one exact string comparison against a
fixed sentinel (`OPEN_DEVTOOLS_MESSAGE` in `src/ui/window.rs`) and ignores
everything else. A malicious page can at worst trigger "open devtools on
your own page" (harmless — the user's own DevTools, on the page the user is
already looking at) by posting that exact string; it cannot reach the
toolbar's command parser, and no page input is ever trusted as structured
data. If a second content-originated action is ever needed later, it should
get its own sentinel/variant rather than growing this handler into a second
`ToolbarCommand`-style parser.

The toolbar also grew a `ToolbarCommand::OpenDevtools` (button click) for
discoverability, going through the existing trusted channel — a page cannot
reach it.

Both paths converge on one method, `BrowserWindow::open_devtools`, which
opens devtools for "the active content webview." Kept as a single method
(rather than inlining a lookup at each call site) so that only this one
place has to know how "active" is resolved.

**Integration with #2 (multiple tabs, D8) and #5 (tab suspension, D9)**: by
the time this landed, `BrowserWindow` had already grown from a single
`content: WebView` field into `contents: HashMap<TabId, ContentTab>` plus
`active: Option<TabId>`, with each `ContentTab::webview` an `Option<WebView>`
that is `None` while the tab is suspended. `open_devtools` resolves the
active tab through the same private `active_webview()` helper every other
per-active-tab operation (`navigate`, `go_back`, `reload`, ...) already uses,
and is a logged no-op — never a panic — when there is no active tab or the
active tab is currently suspended (nothing to open an inspector on until
it's resumed). The devtools setup itself (`with_devtools(true)`,
`with_initialization_script(devtools_shortcut_script())`, and the dedicated
`with_ipc_handler` described above) lives inside `content_webview_builder`,
the one function every content webview is built through — the initial tab,
a tab opened later, and a suspended tab rebuilt on resume — so the F12
shortcut keeps working no matter when or how a given webview came to exist,
rather than only on the webview that existed at `BrowserWindow::new` time.

## D20: Explicit tab lifecycle state machine, and the `Restoring` state's synchronous collapse

**Scope**: issue #12 ("タブセッション状態とWebViewライフサイクルを整理"). Before
this issue, a tab's lifecycle was two independent, implicit signals:
`Tab::suspended: bool` and "is this id `Tabs::active`". Nothing stopped code
from producing a state that made no sense (e.g. the active tab marked
suspended), and there was no single place documenting which combinations
were even meaningful. This issue replaces that with one explicit
`browser::tab::TabState` enum — `Active` / `Background` / `Suspended` /
`Restoring` — plus transition methods that reject an invalid move via
`Result<TabState, InvalidTabTransition>` instead of leaving it to callers to
avoid by convention.

**The five valid edges** (see `TabState`'s doc comment for the diagram):
`Active -> Background`, `Background -> Active`, `Restoring -> Active`,
`Background -> Suspended`, `Suspended -> Restoring`. Every other pair —
eleven combinations, including every self-transition — is rejected. Two
existing guards fall out of this for free instead of needing a separate
check: `Tabs::suspend` no longer special-cases "refuse the active tab" or
"refuse an already-suspended tab" itself — `TabState::suspend` only accepts
`Background`, so both cases are simply invalid transitions, caught in the
same place every other invalid transition is.

**Invariant `Tabs` upholds**: exactly one tab — the one at `Tabs`' internal
active index — is ever `Active` or `Restoring`; every other tab is
`Background` or `Suspended`. `Tab`'s transition methods are `pub(super)`,
so `Tabs` is the only thing that can move a tab between states, and every
`Tabs` method that changes which tab is active (`open`, `activate`, the
replacement tab picked by `close`) routes through
`Tabs::resolve_activation`, which is what upholds the invariant. This is
also why `idle_background_tabs`/`next_idle_deadline` could drop their old
`tab.id() != active` check in favor of `tab.state() == TabState::Background`
— under the invariant, those are the same set of tabs, but the state check
doesn't need `Tabs` to hand it `active` at all.

**Why `Restoring` exists as a real state instead of being folded into
`Active`**: resuming a suspended tab conceptually has two steps — "this tab
was selected while suspended" and "its webview is now live" — and #25
(session restore / crash recovery) is exactly the kind of future work that
needs to represent a tab as selected-but-not-yet-showing-anything (e.g.
while its persisted session is still being read from disk, well before any
webview exists to rebuild). Modeling that later would mean revisiting this
same enum a second time. So `Restoring` is defined now, with real edges
(`Suspended -> Restoring -> Active`) and real unit test coverage for both
edges independently.

**Why every current caller still collapses it into one call
(`Tab::resume`)**: `ui::window::BrowserWindow`'s webview rebuild
(`build_gtk`/`build_as_child` inside `content_webview_builder`, called from
`resume_tab`) is synchronous — wry hands back a `WebView` or an `Err`
immediately, there is no "rebuild started, will finish later" callback to
hook a state change to. Splitting `Suspended -> Restoring` and
`Restoring -> Active` across two separate `app.rs`-visible calls today would
add a state transition with no observable gap between its two halves — a
distinction with no current caller, and therefore nothing to test beyond
what `Tab::resume`'s own unit tests already assert (that it goes through
`begin_restore` before landing on `Active`, and that it is rejected if the
tab was not `Suspended` to begin with). If a future issue makes the rebuild
genuinely asynchronous, `Tab::begin_restore` and `Tab::activate` already
exist as the two calls to spread across that gap — this issue is scoped to
defining that seam, not building the async machinery behind it.

**`ActivationEffect`, and why `browser::tabs` doesn't call into `ui::window`
itself**: `Tabs::activate`/`activate_at`/`close` return
`Option<browser::ActivationEffect>` (`Switch` or `Resume`) instead of the
old bare `bool`/`Option<TabId>`, so `app.rs` knows whether the tab it just
made active needs `BrowserWindow::activate_tab` (already has a live webview)
or `BrowserWindow::resume_tab` (webview was dropped, rebuild it) — without
re-deriving that from `Tab::is_suspended()` after the fact, which would
already be stale by the time `Tabs` finishes resolving the transition.
`browser::tabs` still has zero knowledge of `wry`/`ui::window` itself (see
the ownership boundary below); it only ever reports *which* webview
operation is needed, never performs one.

**WebView ownership — restated explicitly, not just implied by module
structure**: `browser::` (including `Tab`/`Tabs`) never holds, imports, or
references a web engine handle of any kind — no `wry`/`tao`/`gtk` types
appear anywhere under `src/browser/`, checked simply by `browser::` having
no such dependency in its `use` statements (`Cargo.toml`'s `wry`/`tao`
dependencies are only ever imported under `src/ui/` and `src/app.rs`). The
actual content `WebView` for a tab is owned exclusively by
`ui::window::BrowserWindow`, in its `contents: HashMap<TabId, ContentTab>`.
`Tab::state` and `ui::window`'s `ContentTab::webview: Option<WebView>` are
two independent representations of the same underlying fact (does this
tab's content currently have a live webview), kept in sync by `app.rs`
always updating `Tabs` first and then pushing the result into
`BrowserWindow` (already established by D8; unchanged here) — this issue
does not merge the two representations into one, since doing so would give
`browser::` a `wry` dependency, which is exactly the coupling the issue
asks to avoid ("タブ状態がUI/WebView実装から過度に結合されていない").

**Event delivery safety**: `Tabs::get`/`get_mut` already returned `None` for
an unknown `TabId` before this issue (never panicked); what this issue adds
is explicit test coverage for the case the issue calls out by name — a
`TabId` for a tab that has since been *closed*, not just one that never
existed (`stale_tab_id_lookups_return_none_instead_of_panicking` in
`browser::tabs::tests`) — across every `Tabs` method a stale id can reach
(`get`, `get_mut`, `activate`, `activate_at`, `suspend`, `close`). Separately,
`UserEvent::PageTitleResolved` gained a `tab_id: TabId` field (previously
only the history entry's own `u64` id) so a resolved title can be routed
back to the originating `Tab` (`Tab::title`, new this issue) through the
same `Tabs::get_mut` — a closed tab's id is silently ignored there exactly
like every other tab-addressed event.

**New `Tab` fields, and what's deliberately not built on top of them yet**:
`title: Option<String>` and `favicon: browser::Favicon` (`Unknown` or
`Url(String)`) are added as state, cleared on every `on_navigation_started`
so a stale value is never shown as if it described the page now loading.
`title` is wired up minimally (set from `UserEvent::PageTitleResolved`,
which was already being fetched for the history panel — see D12) as a
demonstration that the field is real and reachable, not dead code; actually
*rendering* a title or favicon in the tab strip is explicitly deferred to
#11, per the issue's own scope note. `last_active`/`last_active_at` already
existed (D9) and are unchanged — they double as the "how stale is this
tab" signal a future #25 restore policy would want, alongside `current_url`
and `TabState` itself (was this tab suspended when the session ended).
Actually persisting any of this to disk is #25's own decision to make, not
this issue's.

## D21: Tab strip — title/favicon rendering, favicon resolution, and shrink-then-scroll

**Scope**: issue #11 ("複数タブ管理を実用レベルまで拡張"), the parts of it that
are tab-strip presentation rather than tab lifecycle (D20 already covers the
state machine `Tab::title`/`Tab::favicon` were added for).

**Title**: `TabSummary` grew a `title: Option<String>` field, mirroring
`Tab::title()`. `sync_tab_strip` (`app.rs`) copies it straight across; the
toolbar's `veloxSetTabs` picks `tab.title || tab.url || "New Tab"` as the
label, same fallback chain the tab's tooltip (`el.title`) uses. Nothing
about *when* a title is fetched changed except one gap this issue also
closes: `fetch_page_title` used to only run when `record_visit_if_enabled`
returned a real `history_id`, i.e. never in private mode (D13/D14) — so a
private-mode tab strip would have shown bare URLs forever. It now always
runs, passing `0` as `history_id` when there is none to attach the title to;
`HistoryStore::update_title` starts issuing ids at `1`, so `0` is a safe
sentinel that simply finds nothing to update rather than colliding with a
real entry — see `app.rs`'s `UserEvent::LoadFinished` handler.

**Favicon — resolving a URL vs. fetching an image, and why that split
matters for "don't block on network"**: the issue's constraint is that
favicon retrieval must never block on external network I/O, and VeloX adds
no HTTP client dependency (D6). The design splits the work in two, matching
where each half is already cheap:

1. **Resolving *which* URL to try** is synchronous, in-page JS — no network
   trip. `ui::window::RESOLVE_FAVICON_SCRIPT` runs via the same
   `evaluate_script_with_callback` fire-and-forget pattern D12 established
   for page titles (`BrowserWindow::fetch_favicon`, called right alongside
   `fetch_page_title` on `LoadFinished`): it looks for
   `document.querySelector('link[rel~="icon"][href]')` (matching `icon`,
   `shortcut icon`, `apple-touch-icon`, etc. — `rel~=` is a whitespace-token
   match) and reads its already-browser-resolved-to-absolute `.href`;
   failing that, it falls back to `new URL("/favicon.ico", location.href)`.
   The whole thing is wrapped in `try { … } catch { return ""; }`, and an
   empty result is dropped rather than reported — never a crash, per the
   issue's "取得失敗は...握りつぶしてログするだけ". The result comes back as
   `UserEvent::FaviconResolved { tab_id, url }`, applied via
   `Tab::set_favicon_url` exactly like a resolved title.
2. **Actually fetching the image** is left entirely to the toolbar webview's
   own `<img src="...">` tag once `veloxSetTabs` renders it — a real browser
   engine's normal async image loading, off the main Rust thread's back
   entirely. `TabSummary.favicon: Option<String>` carries only the URL from
   step 1; Rust never touches the image bytes. A failed load (404, timeout,
   unreachable host) fires the `<img>`'s own `error` event, which just
   removes the element (`.tab-favicon` CSS comment) — no broken-image icon,
   no Rust-side error path needed at all.

**Consequence for the toolbar webview's incognito setting**: before this
issue, only content webviews were `with_incognito(private)` — the toolbar
was documented as never needing it because it "only ever loads our own
embedded HTML, never site content" (D15). Favicon `<img>` tags break that:
the toolbar webview now makes real requests to page-controlled origins. In
private mode, letting those persist cookies/cache the private-browsing
promise (D14) says nothing survives would be a real leak. Fix: the toolbar
webview's `WebViewBuilder` also gets `.with_incognito(config.private)` now
(`BrowserWindow::new`); non-private mode is unaffected (favicons render and
cache normally). No content-blocking is applied to favicon image requests —
out of scope for this issue; a future issue could route them through
`FilterList` too if that turns out to matter in practice.

**Tab width and scrolling**: `#tabstrip` already had `overflow-x: auto`
(D8); what it lacked was Chrome/Firefox's "shrink before you scroll"
behavior — with the old `.tab { flex: none; min-width: 80px; }`, tabs never
shrank below their natural content width, so 10+ tabs jumped straight to a
wide scrollable strip with no narrowing step. Fix, entirely CSS: `#tabs`
(the flex row holding the `.tab` elements, itself a flex item of
`#tabstrip` alongside the fixed-size `#new-tab` button) is now
`flex: 1 1 auto; min-width: 0`, and each `.tab` is `flex: 1 1 180px;
max-width: 180px; min-width: 60px`. Tabs shrink together, proportionally,
down to 60px as more are opened; only once every open tab is already at
60px does the combined width exceed `#tabstrip`'s box and its
`overflow-x: auto` start scrolling. This is a standard CSS pattern (a
`min-width` floor on flex-shrinking children inside a scrollable ancestor)
and needed no new dependency. Verified by reading the rule, not by running
it — see the "headless" caveat below.

**What's still unverified**: this repo's CI/dev environment is headless
(`docs/architecture.md`'s webview-in-a-real-window model has no display to
attach to here), so the actual pixel behavior — tabs visibly narrowing,
scrolling smoothly past 10+ tabs, a favicon actually rendering — has not
been eyeballed in a running VeloX. What's covered instead: the CSS rules
themselves (present and structured as described above), the JS/Rust
plumbing (`TabSummary`/`veloxSetTabs` serialization round-trip, unit-tested
in `src/ui/toolbar.rs`), and the underlying `browser::tabs::Tabs` state
handling many tabs without panicking or losing the single-active-tab
invariant (`many_tabs_stay_internally_consistent` in
`browser::tabs::tests`). A human should confirm the visual result once this
lands somewhere with a display.

## D22: Tab-management keyboard shortcuts — same delivery pattern as D18, two trust boundaries

**Scope**: issue #11's Ctrl/Cmd+T/W/Shift+T/Tab/Shift+Tab/1-9.

**Delivery mechanism reused wholesale from D18**: the content webview is a
native child widget; once it has keyboard focus, tao's window-level
`WindowEvent::KeyboardInput`/accelerators do not see the keypress (D18's
"why an injected script over a tao accelerator" applies unchanged — nothing
new to investigate there, and this issue does not reopen that question).
`ui::window::tab_shortcut_script()` is a second `with_initialization_script`
injected alongside `devtools_shortcut_script()` into every content webview
(same `content_webview_builder`, so it applies to the initial tab, a
newly-opened tab, and a tab rebuilt on resume, exactly like D18's
reasoning): a capture-phase `keydown` listener that matches
Ctrl/Cmd(+Shift)+key combinations and posts one of a fixed set of sentinel
strings (`"velox:new-tab"`, `"velox:close-tab"`,
`"velox:reopen-closed-tab"`, `"velox:next-tab"`, `"velox:prev-tab"`,
`"velox:activate-tab-1"`..`"velox:activate-tab-8"`,
`"velox:activate-tab-last"`) over `window.ipc`.

**Both modifiers, always, rather than branching on OS**: the issue asks for
Cmd on macOS and Ctrl on Linux/Windows to both work. Rather than sniffing
the platform in JS (`navigator.platform` is itself unreliable/deprecated)
or building two separate scripts, the listener simply checks
`event.ctrlKey || event.metaKey`. This trivially satisfies "handle both" —
Cmd never fires outside macOS and Ctrl-as-a-browser-shortcut is harmless
(if slightly extra) to also accept there — without any OS-detection code to
get wrong. `docs/architecture.md`'s "OS 固有 API を抽象化する" principle is
met by there being no OS-specific branch to abstract in the first place.

**Trust boundary — two parsers, not one grown wider**: D18 already
established that the content webview's IPC channel must never become a
second `ToolbarCommand`-style structured-command parser, since page content
is untrusted. This issue's shortcut messages are exact-string sentinels
only — `ui::window::parse_content_shortcut` is a closed `match` over the
fixed set above (including 8 literal `"velox:activate-tab-N"` arms, not a
runtime-formatted comparison, so there is no string-building on the hot
path and no way for a near-miss string to be accepted); anything else,
including any attempt at JSON, is silently ignored, same as
`OPEN_DEVTOOLS_MESSAGE`. The result is `ui::window::ContentShortcut`, a
small enum carrying **no `TabId`** — every variant means "act on the active
tab", resolved in `app.rs` from `Tabs::active_id()`, mirroring how
`OpenDevtoolsRequested` already never trusted the sending webview's
identity either. This sidesteps a real question (which of possibly several
content webviews "is" the source of a shortcut?) by observing it doesn't
need answering: only the visible, focused webview can plausibly receive a
real keypress, so "the active tab" and "the tab that sent this" already
coincide in practice, and treating it as always-the-active-tab keeps the
handler simple and consistent with D18's precedent.

The toolbar webview is trusted first-party chrome (same status as its
existing `ToolbarCommand` channel), so its half of this feature — a second
capture-phase `keydown` listener in `toolbar.html`, for when the address
bar/panel has focus rather than the page — sends real structured
`ToolbarCommand` variants (`CloseActiveTab`, `ReopenClosedTab`, `NextTab`,
`PrevTab`, `ActivateTabByIndex { index }`, `ActivateLastTab`) through the
existing trusted channel, no sentinel strings involved. `app.rs` dispatches
both the toolbar's structured commands and the content webview's sentinel
shortcuts to the *same* small set of shared functions
(`open_new_tab`/`close_tab`/`reopen_closed_tab`/`apply_activation`), so the
two trust boundaries stay separate at the parsing layer while sharing every
line of actual tab-management logic underneath.

**Why `CloseActiveTab` and not "just reuse `CloseTab { id }`"**: the
content webview's untrusted channel cannot supply an `id` at all (nothing
page-supplied is trusted as data, per D18), and the toolbar's own keyboard
listener does not need to resolve one either if the shortcut's meaning is
already "the active tab" — so `ToolbarCommand` grew a dedicated
`CloseActiveTab` variant instead of asking either JS side to compute an id
`app.rs` already knows how to resolve itself.

## D23: Closed-tab stack (Ctrl/Cmd+Shift+T) — bounded LIFO of URLs, in `browser::tabs`

**Scope**: issue #11's "最後に閉じたタブの復元", tracked as its own decision
because the issue explicitly calls for pure, unit-tested logic here rather
than folding it silently into `Tabs::close`.

`browser::tabs::ClosedTabs` (private to the module — `Tabs` is the only
thing that touches it) is a `Vec<String>` of closed tabs' URLs, capped at
`ClosedTabs::CAP = 20` (an arbitrary but generous bound: comfortably more
than anyone reopens in one sitting, without growing unboundedly across a
long session of opening/closing many tabs). `push` evicts the oldest entry
(`Vec::remove(0)`) once at capacity; `pop` (via `Vec::pop`, so the
most-recently-pushed URL comes back first) is the LIFO read. `Tabs::close`
is the single choke point that feeds it — every successful close, whichever
of the three ways it was triggered (the tab strip's "×", `CloseActiveTab`,
or `ContentShortcut::CloseTab`) all end up calling `Tabs::close` — pushes
the closing tab's `current_url()` before removing it; a refused close (the
last remaining tab, or an unknown id) pushes nothing, so it is not
reopenable. `Tabs::reopen_closed(now)` pops one URL and is otherwise
identical to `Self::open_at` — a reopened tab is a brand new tab/webview at
that URL, never a restoration of scroll position, form input, or session
history, since all of that was already gone the moment the original
webview was dropped on close (same "a suspended/closed tab's webview is
just gone" stance D9 takes for suspension). `app.rs`'s `reopen_closed_tab`
then does exactly what `open_new_tab` does for a brand new tab: build the
webview (`BrowserWindow::open_tab`) and activate it.

Not implemented: any attempt to dedupe against a tab that is already open
at the same URL, or to persist the stack across a restart. Both are
reasonable follow-ups but outside this issue's "実装すべき差分" list; #25
(session restore) is the natural place persistence would eventually belong,
since it already owns the question of what tab state survives a restart.

## D24: `target="_blank"`/`window.open()` — wry 0.56's `with_new_window_req_handler`, confirmed from source

**Scope**: issue #11 asked this to be verified against the actual wry 0.56
source in `~/.cargo/registry` before writing any code, not assumed. This
records what was actually found there
(`~/.cargo/registry/src/index.crates.io-*/wry-0.56.1/src/{lib.rs,
webkitgtk/mod.rs, webview2/mod.rs, wkwebview/class/wry_web_view_ui_delegate.rs}`).

**The API exists and is available on every backend VeloX targets**:
`WebViewBuilder::with_new_window_req_handler(impl Fn(String,
NewWindowFeatures) -> NewWindowResponse + 'static)` (`src/lib.rs`) is wired
up identically in shape across all three backends VeloX ships on:

- **WebKitGTK** (`src/webkitgtk/mod.rs`, `connect_create` on the webview's
  `create` signal): fires synchronously for both `window.open()` and
  `target="_blank"` link activation, handing back the requested URL
  (`action.request().uri()`) before any native window is created.
- **WebView2** (`src/webview2/mod.rs`, `add_NewWindowRequested`): fires for
  the same two triggers, extracting the URL via `args.Uri(...)`. The
  callback runs through `Self::dispatch_handler` (Windows' documented
  reentrancy requirement for this event — calling back into WebView2
  synchronously from inside its own callback would deadlock) but still
  resolves on the same event loop, before the deferred `args` completes.
- **WKWebView** (`src/wkwebview/class/wry_web_view_ui_delegate.rs`,
  `webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:`):
  same two triggers, URL from `action.request().URL()`.

All three converge on the same three-way `NewWindowResponse`:

- `Allow` — the *default* platform behavior: a brand new, bare native
  window/webview outside VeloX's own window/tab model entirely (a second
  `gtk::ApplicationWindow` on Linux, a `NSWindow` on macOS, WebView2's own
  default handling on Windows). No toolbar, no tab strip entry, nothing
  VeloX's `Tabs`/`BrowserWindow` know about — exactly the "新規タブとして
  扱う" outcome the issue does *not* want.
- `Create { webview }` — the caller constructs the platform-native webview
  itself (`webkit2gtk::WebView`, `ICoreWebView2`, or
  `Retained<WKWebView>`) and hands it back, with the requirement that it
  share the opener's environment/configuration
  (`WebViewBuilderExtUnix::with_related_view` /
  `WebViewBuilderExtWindows::with_environment` /
  `WebViewBuilderExtMacos::with_webview_configuration`, per each platform
  module's own doc comments). This would let a `window.open()` result
  become a real second content webview VeloX manages — but doing so needs
  three platform-specific code paths, each threading a *different* opener
  handle (`NewWindowOpener`'s per-platform `webview`/`environment`/
  `target_configuration` fields) through to a matching
  `WebViewBuilderExt*` call, and there is no cross-platform way to build a
  wry `WebView` from a "please match this specific opener" webview
  attribute without those extension traits. That is meaningfully more
  surface than this issue needs.
- `Deny` — refuse the new webview/window outright; the backend does nothing
  further with the request.

**Decision: `Deny`, plus opening the URL as a normal new VeloX tab from Rust
— not `Create`**. Every `content_webview_builder` call (so every tab, not
just the initial one) returns `NewWindowResponse::Deny` from its
`with_new_window_req_handler` closure, having first sent the URL out as
`UserEvent::NewTabRequested(url)`. `app.rs` handles that exactly like
`ToolbarCommand::NewTab`, just with the requested URL instead of the
homepage (`open_new_tab`, shared by both) — a completely ordinary new tab,
built the same way any other tab is, with its own `TabId`, its own entry in
`BrowserWindow::contents`, content blocking, devtools, and the shortcut
scripts all applying exactly as they do to every other tab. This satisfies
the issue's "基本ケースが新規タブとして扱える" acceptance criterion without
needing `Create`'s per-platform opener-sharing plumbing — the one piece of
this issue's scope deliberately kept small. A future issue could pursue
`Create` if there turns out to be a concrete need for the new tab to share
the opener's storage partition/session (e.g. an OAuth popup flow that
expects `window.opener` semantics) that a plain new tab does not provide;
nothing here forecloses that later.

**What's unverified**: same headless-environment caveat as D21 — this was
confirmed by reading wry's source (the handler signature, the three
backends' call sites, and their semantics) and by the existing
`ToolbarCommand::NewTab` code path this reuses being already covered by
`app.rs`'s tests, but a real `target="_blank"` click or `window.open()`
call opening a new VeloX tab has not been observed running, since there is
no display to run VeloX against here.
