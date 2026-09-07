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

**Update (D41/D42)**: summed RSS overstates VeloX's memory position relative
to a browser (or a build) with more processes, since it counts shared memory
once per process rather than once total — measured in D41 to actually flip
which of two browsers looks lighter. D42 adds PSS to `sample_process_tree_rss`
(same function, `RssSample` gains PSS fields) as the metric to actually
compare on; RSS stays exactly as described above and remains what is always
available, since PSS is best-effort on top of it (older kernels, permissions,
non-Linux). Phase 3's memory work (#61/#62/#63) should read PSS, not RSS, as
the improvement signal.

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

## D19: Performance metrics, part 2 — tab latency, structured output, one record type

**Scope**: Issue #13's remaining acceptance criteria on top of D16/Issue #3
(startup timestamps, page-load duration, and RSS sampling already existed):
tab create/switch latency, a machine-readable output format for a future CI
benchmark (Issue #14/#36), and a common record shape across all five event
kinds.

**Tab latency — no new timer type, bracket `Instant::now()` in `app.rs`**:
`PageLoadTimer` exists because a page load's start (`NavigationStarted`) and
end (`LoadFinished`) are two separate, asynchronous webview callbacks that
can interleave across tabs — the timer has to survive between them. A tab
create (`ToolbarCommand::NewTab`) or switch (`ActivateTab`) has no such gap:
`window.open_tab`/`activate_tab`/`resume_tab` are synchronous wry calls, so
by the time `app::handle_toolbar_command`'s match arm returns, the new
webview is usable (or the switch is visible). So the "timer" is just two
`Instant::now()` reads in the same stack frame — no state to carry between
events, hence no `TabLatencyTimer` type in `browser::metrics`, only
`TabLatencyKind` (an enum tagging which of the two operations a
`Duration` measures) plus `PerfRecord::tab_latency` to format it. This also
satisfies the "keep new logic out of `tab.rs`/`tabs.rs`/`app.rs`'s dispatch
structure" constraint from the concurrent tab-model rework (Issue #12): the
edit in `app.rs` is two brackets around existing calls plus one new
`AppState::perf: Option<PerfContext>` field, not a new abstraction layered
into the tab model.

`AppState::perf` (not a function parameter threaded through
`handle_user_event`/`handle_toolbar_command`) is where the "off path costs
nothing" guarantee lives: `record_tab_latency` returns immediately on
`None` with no `Instant::now()` call, and the one `Instant::now()` it does
need for `started` is not a new cost either — `Tabs::open_at`/`activate_at`
already require an `Instant` argument for their own idle-tracking
bookkeeping (`docs/architecture.md`, "Automatic tab suspension"), regardless
of whether perf metrics are on. Putting the capability on `AppState` instead
of a parameter also meant zero signature changes to the existing dispatch
functions — matching the "minimal, hook-only edits to `app.rs`" constraint
from the Issue #13 tasking (the concurrent Issue #12 branch touches the same
file's tab-handling code).

**One record type — `PerfRecord`, not five ad hoc log lines**: all five
event kinds (`startup`, `page_load`, `tab_create`, `tab_switch`, `rss`) are
built through one enum, `metrics::PerfRecord`, with two renderers:
`to_text()` (byte-for-byte identical to what this project logged before
Issue #13, for the three pre-existing kinds — see the parity tests in
`browser/metrics.rs`) and `to_json(elapsed)` (a JSON object always
containing `"event"` and `"ts_ms"`, plus per-event fields — schema in
`docs/architecture.md`, "Performance extension points"). This is a typed
Rust enum rather than the fully generic "event name + labels: Vec<(&str,
String)> + numbers: Vec<(&str, f64)>" shape the issue sketches; the actual
design goal — a future parser only has to understand one uniform
`{event, ts_ms, ...}` JSON shape instead of five different ad hoc lines —
is met by the *output*, and a closed enum keeps every event's fields
type-checked and the exhaustive `match` in `to_text`/`to_json` a compile
error if a new event is ever added without updating both renderers. Flagged
here as a judgment call worth a second look in review, since "共通のレコード
型" could reasonably be read as asking for the fully generic shape instead.

**Two formats via one `PerfFormat` enum, not a second parallel logging
path**: `VELOX_PERF_FORMAT=text|json` (default `text`) selects
`PerfRecord::to_text`/`to_json_line` inside one write call
(`perf_log::PerfLog::write`), so enabling JSON output can never desync from
what text mode logs — both are two branches over the same `PerfRecord`
value, not two independently-maintained log statements. `text` intentionally
keeps the pre-Issue-#13 `velox[perf] ...` wording exactly (including the
`velox[perf] ` prefix, added once at the `PerfLog::write` call site rather
than per-record) so nothing already parsing/grepping it breaks. `json` drops
that prefix — each line must parse as JSON on its own for Issue #14/#36 to
consume it directly — and documents that a shared stderr stream still
interleaves plain `velox: ...` diagnostics, so a JSON consumer reading
stderr (rather than a dedicated `VELOX_PERF_OUTPUT` file) must skip
lines that fail to parse rather than assume every line is a record.

**`VELOX_PERF_OUTPUT=<path>` — a new IO module, not new logic in
`browser::metrics`**: `browser::metrics`'s module doc comment promises pure,
window-independent logic with no IO; opening/writing a file is IO. Rather
than break that promise or bolt file-handling onto `app.rs` directly, a new
module, `browser::perf_log` (`PerfLog`), takes on exactly the role
`persistence.rs` already plays for history/bookmarks: a small, deliberately
"dumb" IO layer with no policy of its own. It is opened once in
`app::build_perf_log` (append mode; falls back to stderr and logs why on
open failure, never a fatal error) and shared as an `Arc<PerfLog>` between
the main event loop and the RSS sampler thread, with a `Mutex<Sink>` inside
so two threads writing at the same instant cannot interleave into one
unparsable line.

**Why not a new dependency (e.g. a structured-logging or tracing crate)**:
the actual need — one flat JSON object per line, five known event shapes —
is a few dozen lines against the `serde`/`serde_json` this project already
depends on for toolbar IPC (see D6, "every dependency has one clear job").
A tracing/logging framework would add a much larger surface (subscribers,
spans, filtering) for a feature this scoped, and would not obviously make
the "off path costs nothing" guarantee any easier to keep than the
`Option`-gated design already in place.

**Cost / revisit condition**: `ts_ms` is elapsed time since process start
(an `Instant`-derived, monotonic-within-one-run value), not a wall-clock
timestamp — sufficient for Issue #14's within-run benchmarking use case
(ordering and relative timing of events in one process's lifetime), but not
for correlating records across separate process runs by wall-clock time. If
a future consumer needs that, it is a small addition (an absolute
`SystemTime`-based field alongside `ts_ms`, not a redesign) — revisit if
Issue #14 or #36 turn out to need cross-run wall-clock correlation.
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

## D21: Benchmark suite (#14) — pure aggregation module + separate, unverifiable-headless runner binary

**Scope**: Issue #14's acceptance criteria on top of D16/D19's perf-metrics
foundation: run the same benchmark multiple times, compute a representative
value (median/p95), save results and compare against a past run, and record
the benchmark conditions in docs. This project's dev/CI environment is
headless (no display), so "actually launch VeloX and measure it" cannot be
exercised or verified here — the design has to make that limitation not
block the acceptance criteria that *can* be verified.

**Split into a pure module (`src/browser/benchmark.rs`) and a separate
runner binary (`src/bin/velox-bench.rs`), the same shape as
`browser::metrics` / `browser::perf_log`**: the acceptance criteria that
matter most — "run repeatedly", "compute a representative value", "compare
saved results" — are all pure arithmetic over already-produced JSON Lines,
with no dependency on a display, a running VeloX process, or the OS process
tree. Putting that arithmetic in `src/browser/` (rather than inline in a
`src/bin/` binary) means `cargo test` exercises every acceptance criterion
that is testable at all, in this headless environment, with no `#[ignore]`s
and no skipped tests. The runner binary is intentionally thin: spawn the
`velox` binary, wait, read back its `VELOX_PERF_OUTPUT` file, hand it to
`benchmark::aggregate_trials`. It cannot be exercised end-to-end here (the
child process fails to create a window — observed as a GTK init panic in
this container), but that failure path was smoke-tested to confirm
`velox-bench` itself does not crash and does not write a fabricated
"successful" result: it reports zero collected records and exits `1`. The
`aggregate`/`compare`/`list-scenarios` subcommands need no display at all
and were smoke-tested directly with synthetic JSON Lines fixtures.

**Scenario catalog is data, not behavior**: `benchmark::scenario::Scenario`
enumerates the fixed set Issue #14 names (cold/warm startup, first page
load, navigation, tab create/switch, and one `TabCountMemory(n)` variant per
`n` in `[1, 5, 10, 20, 50]`), plus `is_unattended()` marking which three of
those `velox-bench run` can drive without any interaction beyond "launch and
wait". This is the one source of truth `velox-bench list-scenarios` and
`docs/benchmarking.md` both read from, so the scenario list cannot drift
between the CLI's `--scenario` validation and the documentation.

**No automation hook added for tab creation/navigation from outside the
process**: driving those from `velox-bench` would need either a CLI flag
(e.g. "open N tabs at startup") or an external IPC surface into
`app::handle_toolbar_command`, and this Issue's tasking explicitly forbids
touching `src/app.rs`/`src/browser/tab.rs`/`src/browser/tabs.rs` (concurrent
Issue #12 rework of the same files). Rather than route around that with a
parallel, un-reviewed mechanism, those scenarios are implemented as data
model + aggregation only; real measurement is deferred to manual collection
(documented in `docs/benchmarking.md`) or a follow-up Issue that can safely
touch `app.rs` once #12 lands. `Scenario::is_unattended()` makes this
boundary explicit and enforced (`velox-bench run` refuses the other
scenarios with a message pointing at `aggregate`) rather than silently
producing an empty/misleading result.

**Comparison uses the median as the single regression signal, not p95**:
`compare()` reduces each side to one number (`Stats::median`) per metric and
flags a regression when the percentage increase exceeds a caller-supplied
threshold. `p95` is kept in the saved `Stats` for manual inspection but not
used as the automated pass/fail signal — on a shared/noisy runner (the kind
Issue #36's CI is likely to use) the 95th percentile of a small trial count
is exactly the tail that jitters the most, which would make the regression
check noisy rather than useful. This is a judgment call worth revisiting
once #36 has real CI-runner noise data to look at.

**No new dependency; hand-rolled RFC 3339 timestamp formatting**: CLI
argument parsing in `velox-bench` follows the same `--flag value` policy as
`Config::from_env_and_args` (D6: every dependency needs a clear, singular
job; a CLI-parsing crate is not worth it for four subcommands). Filling
`RunEnvironment::generated_at` needs a wall-clock date, and no time/calendar
crate is in the dependency tree; `benchmark::format_unix_time_utc` uses
Howard Hinnant's public-domain `civil_from_days` algorithm (a few lines of
integer arithmetic converting a day count into a proleptic-Gregorian date),
verified in this module's tests against `date -u -d @<epoch> ...` output for
several known timestamps. Kept as a pure `i64 -> String` function (not
reading `SystemTime` itself) so it stays deterministically testable; only
`velox-bench`'s `main` reads the real clock.

**Fixed test pages are static local HTML, not network URLs**: Issue #14
asks for "固定したテストページ/シナリオ" (fixed test pages/scenarios).
`scripts/bench/pages/{minimal,text,dom_heavy}.html` are static, offline,
`file://`-openable fixtures (a near-empty page, a 200-paragraph text page,
and a 5000-node DOM page) rather than real internet URLs, so a benchmark run
never depends on network conditions or a third-party site's own performance
changing out from under a comparison.

**Cost / revisit condition**: the two biggest open gaps are (1) no verified
real-world startup/navigation/tab numbers from this project, since the dev
environment cannot launch VeloX at all, and (2) no automated driver for
tab-count/navigation scenarios. Both should be revisited together: whoever
next touches `app.rs` after Issue #12 lands is well-positioned to add a
minimal "open N tabs at startup" CLI hook that `velox-bench` can drive, and
the first real GUI-capable run of `velox-bench run` should be treated as
this project's first authoritative baseline numbers, not something to
backfill by estimation.
## D22: Tab strip — title/favicon rendering, favicon resolution, and shrink-then-scroll

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

## D23: Tab-management keyboard shortcuts — same delivery pattern as D18, two trust boundaries

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

## D24: Closed-tab stack (Ctrl/Cmd+Shift+T) — bounded LIFO of URLs, in `browser::tabs`

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

## D25: `target="_blank"`/`window.open()` — wry 0.56's `with_new_window_req_handler`, confirmed from source

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

**What's unverified**: same headless-environment caveat as D22 — this was
confirmed by reading wry's source (the handler signature, the three
backends' call sites, and their semantics) and by the existing
`ToolbarCommand::NewTab` code path this reuses being already covered by
`app.rs`'s tests, but a real `target="_blank"` click or `window.open()`
call opening a new VeloX tab has not been observed running, since there is
no display to run VeloX against here.

## D26: Omnibox foundation (#15) — URL-vs-search split, default search engine, dangerous input, dropdown as a third `Panel`

**Scope**: issue #15 asked for the omnibox *foundation* only — input
classification, a configurable search engine, a candidate-list UI with the
two built-in candidates ("load as URL" / "search"), and
Ctrl/Cmd+L/↑/↓/Enter/Esc. History/bookmark candidates and ranking are #20's
job; see `docs/architecture.md`'s "Omnibox and search" section for the full
design and the exact interface #20 builds against
(`browser::omnibox::CandidateSource`).

**Classification lives beside `normalize_input`, never duplicates it.**
`browser::navigation::classify_input` is the one function that decides URL
vs. search (`Intent::Url`/`Intent::Search`/`None`); for the URL case it does
nothing but call the pre-existing `normalize_input` and wrap the result.
This was a hard requirement, not just tidiness: `normalize_input` is also
where scheme allow-listing/rejection lives (`ALLOWED_SCHEMES`, D-nothing —
predates decision numbering), and having two functions independently decide
"is this scheme safe to load" is exactly the kind of drift that eventually
lets something slip through one path and not the other.

**A URL `normalize_input` refuses is refused outright, not retried as a
search.** `classify_input("javascript://alert(1)")` is `None`, the same as
`normalize_input`'s own answer — it is *not* turned into "search DuckDuckGo
for `javascript://alert(1)`". Two reasons: (1) the issue explicitly asks for
"危険スキームは拒否 (既存の挙動を壊さないこと)", which reads as "refused
stays refused", not "refused becomes something else"; (2) blurring "this
input was refused" into "this input was silently reinterpreted" makes the
one rejection path in `normalize_input` harder to reason about as the
single source of truth — every other caller of `classify_input` can keep
assuming `None` means "nothing to do", not "something unexpected happened
instead." A search query built from percent-encoded text is not itself
unsafe (it never reaches a scheme check), but the *decision* to silently
reinterpret a refusal is what this avoids.

**Heuristic for "does a single word look like a URL" — `looks_url_like`**:
contains `:` or `.`. This is deliberately coarse (a bare word with neither,
e.g. `rust`, is a search; anything with either is at least attempted as a
URL via `normalize_input`, which is the real arbiter). The alternative —
trying to recognize real TLDs, or requiring a minimum label count — adds a
maintenance burden (a TLD list going stale) for a heuristic that only ever
gates whether `normalize_input` is *attempted*, never whether something is
accepted. Not covered by this heuristic and left to `normalize_input`'s
existing behavior: a single word with a dot but no real TLD (`3.14`) parses
as a URL syntactically and is treated as one — this predates #15 and is
unchanged by it.

**Behavior change worth calling out**: before #15,
`ToolbarCommand::Navigate` called `normalize_input` directly, so typing a
bare word like `rust` and hitting Enter attempted to load `https://rust/`
(almost certainly a dead navigation). After #15 it calls
`app::resolve_navigate_target`, which goes through `classify_input` first,
so the same input now searches instead. This is the intended fix for the
issue's "検索語を入力して設定した検索エンジンで検索できる" acceptance
criterion — not a regression — but is called out explicitly since it does
change what a real user's existing habits produce.

**Default search engine: DuckDuckGo.** `config::SearchEngine::duckduckgo()`
(`https://duckduckgo.com/?q={}`), selectable away via `VELOX_SEARCH_ENGINE`
or a fully custom `VELOX_SEARCH_ENGINE_NAME`/`VELOX_SEARCH_ENGINE_URL` pair.
Chosen for the same reason VeloX already ships whole-app private browsing
(D14) and content blocking (D17) rather than leaving both to extensions:
DuckDuckGo does not build an ad-profile from search history by default,
which fits a browser whose other defaults already lean toward "does not
track the user by default" — a coherent product stance, not a bolt-on.
Google/Bing/Startpage/Ecosia are one-line presets alongside it (`SearchEngine::preset`)
for anyone who wants a different default without hand-writing a query
template; nothing about the omnibox itself favors DuckDuckGo beyond being
the out-of-the-box choice.

**Query encoding: `url::form_urlencoded`, not hand-rolled percent-encoding.**
`build_search_url` calls `url::form_urlencoded::byte_serialize` (spaces
become `+`, reserved characters are escaped) rather than writing a
percent-encoder — `url` is already a dependency (used throughout
`navigation.rs`) and this is exactly the encoding real search engines expect
for a `?q=` parameter (`application/x-www-form-urlencoded`), so there is no
reason to introduce a second, subtly-different encoding scheme by hand.

**Candidate dropdown reuses `Panel`, not a new layout mechanism.** The
omnibox needs a resizable area under the address bar to show candidate
rows — exactly what D11 already built for the history/bookmarks panel
(`BrowserWindow::set_panel`/`sync_layout`, growing the toolbar webview's
native bounds). Rather than inventing a second grow-the-webview mechanism,
`ui::toolbar::Panel` gained a third variant, `Omnibox`, and
`ToolbarCommand::OmniboxInput`/`OmniboxClose` just call the existing
`set_panel`. This means `window.rs`'s layout code (`effective_toolbar_height`,
`sync_layout`) needed **zero changes** for this issue — deliberately, since
`src/ui/window.rs` is a hotspot three issues are editing concurrently this
cycle. The tradeoff: `Panel` (used by `ToolbarCommand::TogglePanel`, a
user-facing button toggle) now has a variant no button ever sends; the
`TogglePanel` handler's `match` treats `Some(Panel::Omnibox)` as a no-op
alongside `None` rather than pattern-matching it away, since IPC input is
untrusted and a stray `{"cmd":"toggle_panel","panel":"omnibox"}` must not
panic.

**Ctrl/Cmd+L wired through both keyboard channels, following D18/D23's
existing pattern exactly**: `ToolbarCommand::FocusAddressBar` (trusted
toolbar webview, sent directly from its own capture-phase `keydown`
listener) and `ui::window::ContentShortcut::FocusAddressBar` (untrusted
content webview, a new fixed sentinel string
`velox:focus-address-bar` — never JSON, matching every other content
shortcut). Both land on one shared `app::focus_address_bar`, which calls a
new `BrowserWindow::focus_address_bar`: `WebView::focus()` (confirmed
present on every backend VeloX ships on — WebKitGTK/WKWebView/WebView2 —
in wry 0.56.1's source under `~/.cargo/registry`) to move native keyboard
focus to the toolbar webview, then a forced JS update
(`veloxFocusAddressBar`) that overwrites the address bar's value and
selects it even while already focused — unlike `veloxSetUrl`, which
deliberately skips updating a focused field to preserve an in-progress
edit. The same forced-update function backs `OmniboxClose` (Esc): close the
dropdown, then restore-and-select the tab's actual current URL, since Esc
must discard whatever the user was mid-typing.

**What's unverified**: same headless-environment caveat as D22/D25 —
`WebView::focus()`'s existence and per-backend behavior was confirmed by
reading wry 0.56.1's source, and the whole feature is covered by unit tests
for its pure logic (`classify_input`, `build_search_url`, `build_candidates`,
`resolve_search_engine`) plus the JS-embedding/IPC-parsing tests the rest of
the toolbar protocol already has, but no one has actually typed into the
address bar, watched the dropdown open, or pressed Ctrl+L against a running
window — there is no display to run VeloX against here.
## D27: `HistoryEntry` grows `favicon`/`visit_count` — additive to D4's de-duplication, not a redesign of it

**Scope**: issue #18's "URL / title / favicon / visited_at / visit_count"
and "訪問回数を数える". Phase 1 (#4, PR #54) already shipped `id`, `url`,
`title`, `visited_at` plus consecutive-visit de-duplication and a hard
entry cap; this only had to add the two missing fields and decide what
`visit_count` means given the de-duplication `record_visit` already does.

**The two live options, and why the log-preserving one won**: the issue
text spells out the choice directly — collapse every visit to a URL into
one row (a true "1 URL = 1 history entry" model, closer to how a bookmark
store already works), or keep the existing append-only visit log and layer
a count on top. Collapsing globally would have meant `record_visit`
scanning (or index-mapping) the *entire* store for a matching URL on every
visit instead of only ever looking at `entries.last()`, discarding the
existing chronological "list of visits, oldest first" shape three of the
existing tests directly assert on
(`does_not_collapse_non_consecutive_repeat_visits`,
`newest_first_reverses_visit_order`, `ids_stay_unique_across_a_clear`), and
raising a new question this issue does not ask to answer: does re-visiting
an old URL move it back to the top of a "1 row per URL" list, or keep it at
its original position? Neither answer is obviously right, and guessing one
risks conflicting with #15's forthcoming omnibox suggestions (which will
likely want "most recently visited" ordering from this same store).

**Decision: keep the append-only log, add `visit_count` to what
`record_visit`'s existing consecutive-collapse already does.** A visit
that collapses into the store's last entry (the existing "reload, or the
engine re-reporting the same page" case — see the D4-era doc comment this
module already carried) now also increments that entry's `visit_count`
(`saturating_add(1)`, so a pathological reload loop cannot overflow into a
panic); a visit that starts a fresh entry (including a *repeat* visit to a
URL already elsewhere in the log — browsing away and back) still starts at
`visit_count: 1`, exactly as it always has for `title`/`visited_at`. In
other words: **this is additive to D4's de-duplication, not a behavior
change to it** — every pre-existing `record_visit` test still holds
unmodified (`does_not_collapse_non_consecutive_repeat_visits` now also
asserts both surviving entries sit at `visit_count: 1`, not that the count
itself changed). The honest cost of this choice: `visit_count` answers "how
many times in a row was this exact entry re-hit" (useful for "this page
got reloaded/redirected-back-to a bunch just now"), not "how many times
total has this URL ever been visited across the whole log" (which would
need summing across entries — `browser::history::search`/`group_by_date`
below do not attempt this, and nothing in the issue's acceptance criteria
asks for a global per-URL total). A future issue wanting that number has an
easy path: fold `HistoryStore::entries()` by URL at read time, no store
schema change required, since the per-visit log is preserved.

**`favicon: Option<String>`, filled in the same lifecycle as `title`**:
`record_visit` never receives a favicon (mirrors how it never receives a
title beyond the reload-preserves-existing case) — a fresh entry starts
`favicon: None`, and `HistoryStore::update_favicon(id, url)` fills it in
later, a line-for-line mirror of `update_title`. The wiring reuses #11's
existing favicon-resolution pipeline (D22) rather than adding a second one:
`UserEvent::FaviconResolved` already carries a `tab_id`; it now also
carries a `history_id`, threaded through `BrowserWindow::fetch_favicon`
exactly the way `PageTitleResolved`/`fetch_page_title` already thread
`history_id` through for the title — same `0`-is-"no entry" sentinel (valid
because `HistoryStore` ids start at `1`), same fire-and-forget semantics,
same "stale tab_id is a safe no-op" reasoning. `app.rs`'s single
`LoadFinished` call site now calls `fetch_favicon(id, history_id)` instead
of `fetch_favicon(id)` — one call site, one extra argument, no new event
plumbing.

**Migration**: `history.json` written by the pre-#18 store has no
`favicon`/`visit_count` keys at all. Both new fields carry
`#[serde(default)]` (`favicon` to `None`; `visit_count` via
`#[serde(default = "default_visit_count")]` to `1`, since an entry that
exists was visited at least once) rather than requiring a version tag or a
hand-written upgrade pass — `serde_json`'s ordinary "missing key uses the
field's default" behavior is the entire migration, and it is exercised by
a dedicated test
(`pre_issue_18_history_json_without_favicon_or_visit_count_still_loads`)
that deserializes a literal old-shaped JSON string. `persistence.rs`'s
existing "corrupt/unreadable file degrades to an empty store, never a
crash" behavior (D-less, just the module's stated contract) is unaffected
and untouched — this migration only had to matter for a file that *does*
still parse as valid JSON, just an older shape of it.

## D28: Downloads (#16) — wry 0.56's started/completed handlers, no progress or mid-transfer cancel

> **追記 (D53)**: WebKitGTK ではこれらのハンドラは webview ではなく
> `WebContext` 単位で登録される。D49 の共有 context では toolbar webview に
> 1 回だけ登録する必要がある — 経緯と実測は D53 を参照。

**Scope**: Issue #16 ("ダウンロード管理"): start/complete/fail events, a save
location, progress display, cancel, a download list, opening a completed
file, opening the downloads folder, and same-name handling.

### wry 0.56.1 API investigation (read from source, not assumed)

Per the issue's own instruction (and this repo's precedent in D17/D18/D25),
every finding below was confirmed by reading the vendored crate source at
`~/.cargo/registry/src/index.crates.io-*/wry-0.56.1/src/{lib.rs,
webkitgtk/mod.rs, webkitgtk/web_context.rs, webview2/mod.rs, wkwebview/mod.rs,
wkwebview/download.rs}`, not from documentation or memory.

**The API exists and is wired on every backend VeloX ships on.**
`WebViewBuilder::with_download_started_handler(impl FnMut(String, &mut
PathBuf) -> bool)` and `::with_download_completed_handler(impl Fn(String,
Option<PathBuf>, bool))` (`src/lib.rs`) are both real, stable methods:

- **WebKitGTK** (`webkitgtk/web_context.rs`,
  `WebContext::register_download_handler`): hooked to
  `WebContext::connect_download_started`, which itself listens for
  `WebKitDownload::connect_decide_destination` (the started handler; can
  mutate the destination or call `download.cancel()` if the handler returns
  `false`) and `connect_finished`/`connect_failed` (the completed handler).
- **WebView2** (`webview2/mod.rs`): `ICoreWebView2_4::add_DownloadStarting`
  fires the started handler (`args.SetResultFilePath`/`args.SetCancel(true)`
  reflect our return value/path edit back to the engine); a per-download
  `DownloadOperation::add_StateChanged` subscription (registered *inside*
  the same started-handler callback) is what actually calls the completed
  handler once `state != IN_PROGRESS`.
- **WKWebView** (`wkwebview/download.rs`,
  `WryDownloadDelegate`/`download_policy`/`download_did_finish`/
  `download_did_fail`): `download_policy` is the started handler
  (`completion_handler` is called with either the chosen `NSURL` or a null
  pointer to reject); `download_did_finish`/`download_did_fail` are the
  completed handler.

**No byte-level progress callback exists anywhere in this crate.** Neither
`with_download_started_handler` nor `with_download_completed_handler`, nor
any other `WebViewBuilder::with_*` method in `lib.rs`, exposes a
bytes-received/total-bytes signal. WebView2's own `StateChanged` event (used
internally above) is polled only for `state != IN_PROGRESS`, i.e. purely to
detect "finished, one way or another" — wry never reads or forwards
`DownloadOperation::BytesReceived`/`TotalBytesToReceive`, even though the
underlying WebView2 COM API has them. WebKitGTK's `WebKitDownload` has an
`estimated-progress` GObject property and WKWebView's `WKDownload` has a
`progress: Progress` (`NSProgress`) — neither is bound anywhere in wry's
public surface. **Conclusion: percentage/byte-level progress is not
obtainable through wry 0.56's public API on any platform**, full stop — not
a per-platform gap, an every-platform one.

**No mid-transfer cancel handle exists either.** The only way to stop a
download from happening at all is to return `false` from
`download_started_handler` — which every backend translates into "never
start" (`download.cancel()` on WebKitGTK, `args.SetCancel(true)` on
WebView2, a null `NSURL` to the completion block on WKWebView), decided
*synchronously*, before any bytes are written. Once accepted (`true`
returned, which is the only sane default — see below), wry hands the caller
no handle to the in-flight download at all: the started handler's signature
is `(String, &mut PathBuf) -> bool`, no id, no `Arc<Download>`, nothing to
call `.cancel()` on later. The `WebKitDownload`/`DownloadOperation`/
`WKDownload` objects wry's own internals hold *do* have real cancel methods
(`webkit_download_cancel`, `ICoreWebView2DownloadOperation::Cancel`,
`WKDownload::cancel(completionHandler:)`) — wry simply never exposes them to
a caller past the moment of the initial accept/reject decision.

**Alternative considered and not taken: a self-written HTTP downloader.**
The issue itself asks this to be flagged for the parent to decide, so it is
recorded here rather than silently dropped. Real byte-level progress and a
true mid-transfer cancel are only achievable by *not* letting the engine
handle the download at all: reject it in `with_download_started_handler`
and instead fetch the URL ourselves with an HTTP client
(`reqwest`/`ureq`/hand-rolled `std::net::TcpStream` + manual HTTP), reading
the response in chunks to report progress and drop the connection to
cancel. **Not implemented, for three reasons**: (1) it needs a new
dependency — an HTTP client at minimum, likely a TLS stack and possibly an
async runtime behind it — which conflicts with CLAUDE.md's "additional
dependencies need a clear, singular justification" policy (D6) for a
feature this codebase's own precedent (D17) already accepts losing when the
underlying engine API doesn't reach far enough; (2) it silently drops
cookies/session/auth state — the WebView's own cookie jar/session that made
an authenticated download link work in the browser is not reachable from a
bare HTTP client without also reimplementing cookie extraction, which wry
does not expose either, so an authenticated download (e.g. a file behind a
login) would simply break; (3) it duplicates redirect handling, `Referer`/
`Content-Disposition` parsing, and TLS validation the engine already gets
right, doubling the surface that could have a bug. If a future need makes
real progress/cancel a hard requirement, this is the natural next step to
revisit — but it is a second implementation of "download a URL", not a
one-line addition, and deserves its own decision when someone actually
needs it.

**Decision: implement what wry's API actually supports, document the two
gaps as known limitations, do not add a dependency.** Every content webview
(via `content_webview_builder`, so every tab, exactly like every other
per-tab handler in this file — see D17/D18/D25's precedent) gets both
handlers. VeloX always accepts every download (`true`, unconditionally),
matching `WebViewAttributes::default()`'s own documented "allowing all
downloads to match browser behavior" — there is no "block this download"
feature here, so the only thing VeloX's started handler ever does is
*redirect* the destination, never refuse it.

### What "進捗表示" (progress display) actually shows

Since no byte count is available, the downloads panel shows only
`DownloadState` (`InProgress`/`Completed`/`Failed`/`Cancelled`) as a text
status line, plus a small CSS-only pulsing dot on an in-progress row (an
"something is happening" cue, not a percentage — see `ui/toolbar.html`'s
`.download-row.in-progress` rule). This satisfies the acceptance criterion
at the coarsest level wry's API allows; a determinate progress bar is not
implemented and, per the investigation above, is not implementable without
the alternative-downloader path above.

### What "キャンセルできる" actually does

`ToolbarCommand::CancelDownload` (`app::cancel_download`) transitions the
entry to `DownloadState::Cancelled` and best-effort `std::fs::remove_file`s
whatever partial file currently exists at its destination. **This does not
stop the underlying engine transfer** — per the investigation above, there
is no handle to do that through wry's public API. If the engine is still
mid-write when this runs, it may recreate/continue writing to that same
path afterward (WebKitGTK/WebView2/WKWebView all own the file handle
independently of anything VeloX does here); VeloX's own bookkeeping is not
fooled by this, since `DownloadStore::cancel`/`complete`/`fail` all reject
any further transition once an entry has reached a terminal state (see
`terminal_states_reject_every_further_transition` in
`browser::downloads::tests`) — a completion notification racing a cancel
request cannot un-cancel it, so the *displayed* state stays correct even if
a stray file reappears on disk. This is the best available behavior given
the API gap, not a full implementation of the acceptance criterion; flagged
here explicitly as the issue's own reporting checklist asks for ("API 制約
により実現できず落とした機能").

### Filename sanitization and same-name collision avoidance

Implemented as pure functions in the new `src/browser/downloads.rs`
(`browser::` stays free of any `wry`/`tao`/`gtk` dependency, per D20's
boundary — `ui::window` is the only caller that talks to wry's actual
download callbacks):

- **`sanitize_filename`**: takes only the last `/`- or `\`-separated segment
  of the server-supplied name (defeating path traversal and absolute paths
  by construction — there is no directory component left to traverse with,
  not a blocklist of `..` patterns), strips control characters (including
  NUL), rejects a bare `.`/`..`, trims trailing dots/spaces (Windows
  disallows both), escapes the eight/eighteen Windows reserved device names
  (`CON`/`PRN`/`AUX`/`NUL`/`COM1`-`COM9`/`LPT1`-`LPT9`, matched
  case-insensitively against the name up to its first `.`, not merely as a
  prefix — `CONSTITUTION.txt` is untouched) with a leading `_`, and
  truncates to 200 bytes on a UTF-8 char boundary, preserving the extension
  when there is room for it. Falls back to a fixed `"download"` name when
  nothing usable survives (empty, whitespace-only, or all-`.` input).
- **`unique_filename`**: given an injected "does this name already exist"
  predicate (kept generic/pure for unit testing, real usage in
  `build_destination` backs it with `Path::exists`), returns the name
  unchanged if free, otherwise the first free `"{stem} ({n}){ext}"` for
  increasing `n` — `report.pdf` → `report (1).pdf`. The stem/extension split
  is at the *first* `.`, not the last, so a compound extension survives
  intact (`archive.tar.gz` → `archive (1).tar.gz`, not
  `archive.tar (1).gz`) — the same split wry's own bundled WKWebView
  download-destination logic already uses internally
  (`wkwebview/download.rs`), reused here for consistency rather than
  inventing a different convention. A filename with no extension
  (`README` → `README (1)`) and a dotfile with nothing before its first `.`
  (`.gitignore` → `.gitignore (1)`, since a would-be-empty stem falls back
  to treating the whole name as the stem) are both covered explicitly.
- Both are exercised heavily in `browser::downloads::tests` — path
  traversal (relative and absolute, both separators), NUL/control
  characters, bare `.`/`..`, every reserved device name (and the
  false-positive check that a name merely *starting* with one, like
  `COMPANY.pdf`, is left alone), extremely long names (both with and
  without an extension, plus a multi-byte-character truncation-boundary
  case), and the full `unique_filename` collision ladder including compound
  extensions and dotfiles.
- **`prepare_destination`** (`fs::create_dir_all` + `build_destination`) is
  the one IO-performing wrapper, called from `ui::window`'s
  `download_started_handler` before the download is accepted — mirroring
  `persistence::write_json`'s "create the directory, then act" shape.

### Save location

Configurable via `VELOX_DOWNLOAD_DIR`, following exactly the pattern
`persistence::default_data_dir` already established for `VELOX_DATA_DIR`
(D10) — without touching `persistence.rs` itself, which Issue #18 owns in
this stacked-branch round. Platform defaults when unset: `XDG_DOWNLOAD_DIR`
or `$HOME/Downloads` on Linux/BSD, `$HOME/Downloads` on macOS,
`%USERPROFILE%\Downloads` on Windows — no `dirs` crate added, same D6/D10
reasoning. No per-download interactive "choose a folder" dialog is offered:
that would need a native file-picker dependency (e.g. `rfd`) this project
does not have, and the issue's own guidance for VeloX's env-var-driven
config pattern (`persistence.rs`'s precedent) points at a configurable
default directory instead of a picker — read as satisfying "保存先選択" at
the level this codebase's existing conventions support, not a Chrome-style
per-file save dialog.

### `ui::window` wiring: why the destination decision cannot go through
`EventLoopProxy`

`with_download_started_handler`'s closure must return `bool` (and may
mutate the destination `PathBuf`) synchronously, in the same call — unlike
every other wry callback in this codebase (D5), there is no way to defer
this specific decision through a `UserEvent` round trip, since nothing
reads a response back from the event loop into a blocking callback. So
`sanitize_filename`/`resolve_download_dir`/`prepare_destination` (all pure
or thin-IO, needing no `AppState`) run directly inside the closure in
`ui::window::content_webview_builder`, exactly the same
"decide-synchronously-then-notify-asynchronously" shape
`with_navigation_handler` already uses for content blocking (D17) — the
closure decides, then sends `UserEvent::DownloadStarted` so `app.rs`'s
single-threaded `AppState` (D5) can register the entry and refresh the
panel. `DownloadId` issuance stays solely inside `DownloadStore::start`
(called from `app.rs`, on the main event-loop thread) rather than being
generated inside the wry callback — there was no need for a second,
thread-shared id-issuing mechanism (e.g. an `AtomicU64`) when the store
itself can issue ids the moment `app.rs` processes the event, matching how
every other id (`TabId`, `HistoryEntry::id`, `BookmarkEntry::id`) is issued
by its owning store on the main thread, not by whatever callback reported
the underlying action.

### Correlating `download_completed_handler` back to a `DownloadId`

wry's completed handler is one long-lived closure per webview (registered
once at webview-build time, not per download), and it hands back only
`(url, Option<PathBuf>, bool)` — no id of its own, and `path` is `None`
unconditionally on macOS (`wkwebview/download.rs`'s
`download_did_finish`/`download_did_fail` always call
`completed_fn(url, None, success)`), so the correlation cannot always rely
on the destination path. `DownloadStore::resolve_completion(url,
destination)` is the pure matching logic this needs: prefer an exact
destination match among still-`InProgress` entries when a path is given
(exact on Linux/Windows), otherwise fall back to the oldest still-in-progress
entry for that URL (a FIFO assumption, exact for the overwhelmingly common
"one active download per URL at a time" case). **Known limitation**: two
concurrent downloads of the literal same URL on macOS (where no path is
ever available to disambiguate) can have their completion events matched to
the wrong entry — a narrow, documented edge case rather than a silent
correctness bug, covered by
`resolve_completion_falls_back_to_oldest_in_progress_for_the_url_without_a_path`
in `browser::downloads::tests`.

### Opening a completed file / the downloads folder

Both are the same OS primitive — "open this path with the default
handler" — so one function, `browser::downloads::open_path_command`,
returns the right `(program, args)` per platform (`xdg-open` on
Linux/BSD, `open` on macOS, `explorer` on Windows), and `spawn_open` runs
it via `Command::new(program).args(args)` — **never** `sh -c` or any other
shell invocation, since `args` carries a path built from a server-supplied
file name; passing it as a real argument (not interpolated into a shell
command string) leaves no shell metacharacter for it to be misinterpreted
as. `ToolbarCommand::OpenDownload` only acts on a `DownloadState::Completed`
entry (opening an in-progress/failed/cancelled download's file would be
misleading or point at nothing); `ToolbarCommand::OpenDownloadsFolder`
`create_dir_all`s the resolved download directory first so opening it
before anything has ever been downloaded does not surface a confusing
"no such directory" error. Command construction is unit-tested
(`open_path_command_never_goes_through_a_shell`, plus a platform-specific
check for the Linux/BSD branch this project's CI actually compiles);
actually spawning `xdg-open` is **not** exercised by any test — this
project's CI/dev environment is headless, so there is no desktop session
for it to hand a path to; see docs/architecture.md's existing "headless"
caveats (D18/D22/D25) for the same shape of gap.

### Not persisted to disk

Unlike `HistoryStore`/`BookmarkStore`, `DownloadStore` is in-memory only for
the life of the process — the issue's acceptance criteria describe a
session download list ("ダウンロード一覧"), not a cross-restart history, and
`browser::persistence` is Issue #18's file to rewrite in this stacked round
(explicitly off limits here). A future issue could add a
`downloads.json`-style file following exactly `persistence.rs`'s existing
pattern if cross-restart download history becomes a real requirement; not
built speculatively here.

### UI: a fourth panel, same mechanism as history/bookmarks (D11)

`ui::toolbar::Panel` gained a `Downloads` variant, opened/closed the same
way as `History`/`Bookmarks` (`ToolbarCommand::TogglePanel`, growing the
toolbar webview's own bounds — D11's reasoning applies unchanged, nothing
new to decide there). `DownloadEntry` derives `Serialize` directly and is
sent to the toolbar the same way `HistoryEntry`/`BookmarkEntry` are (one
`PathBuf` field uses a `serialize_with` shim to a lossy string, since
`serde`'s built-in `PathBuf` impl errors on non-UTF-8 paths whereas the
toolbar only ever needs something displayable). The panel's row rendering
is a dedicated `veloxSetDownloads` function rather than reusing the
existing generic history/bookmark `renderRows` helper: a download row's
action button depends on state (cancel while `in_progress`, remove once
terminal) and shows a status line instead of a plain timestamp, which is
enough of a different shape that forcing it through the same helper would
have made that helper harder to read for both cases.

### What's unverified

Same headless-environment caveat every prior UI-facing decision in this
project carries (D18/D22/D25): the panel's visual layout, the pulsing
in-progress indicator, and an actual file landing in `~/Downloads` from a
real page have not been eyeballed running VeloX, since this environment has
no display. What is covered instead: every pure function in
`browser::downloads` (sanitization, collision avoidance, directory
resolution's testable branch, command construction) by unit tests, the
toolbar IPC protocol round-trip (`ui::toolbar::tests`), and the JS/HTML
hooks' presence (`toolbar_html_declares_expected_hooks`). A human should
confirm a real download (including the in-progress pulsing dot, opening the
completed file, and opening the downloads folder) once this lands somewhere
with a display.
## D29: History date grouping (今日/昨日/過去7日/それ以前) — UTC calendar days from `std::time`, no `chrono`

**Scope**: issue #18's date-grouped history list, explicitly required to be
"純粋関数として実装し、現在時刻を引数で注入して単体テストできる形" and to
stay on `std::time` rather than adding `chrono`/a timezone-aware crate.

**Day boundaries are computed in UTC, not the machine's local timezone**:
`browser::history::date_bucket(visited_at: u64, now: u64)` divides both
unix-second timestamps by `86_400` to get a day number, then buckets by
`today - day`. This is a real, named simplification, not an oversight: unix
timestamps are timezone-agnostic by construction, and turning one into "the
calendar day a human in timezone X would call this" needs a timezone
database (IANA tzdata, DST rules, etc.) that neither `std::time` nor this
repo's dependency set provides (see D10's "no `dirs`-style crate" policy,
extended here to "no `chrono`/`tz`-aware crate" for the same "each
dependency needs a defensible reason" bar from CLAUDE.md). A user west of
UTC will occasionally see a visit from "yesterday evening, local time"
still under 昨日 rather than 今日 relative to their own midnight (and the
reverse east of UTC) — an off-by-up-to-a-day boundary fuzziness Chrome/
Firefox's chrome-privileged, OS-timezone-aware history UIs do not have. If
this fuzziness turns out to matter in practice, the fix is scoped entirely
to `date_bucket` (its signature already isolates "what day is `now`" from
everything else) — nothing about `group_by_date`'s grouping logic, the wire
format, or the toolbar JS would need to change.

**Pure functions, `now` always injected — never `SystemTime::now()` inside
the logic itself**: `date_bucket` takes `now: u64` as a plain argument, and
`group_by_date` threads it through unchanged; neither ever calls
`SystemTime::now()`/`Instant::now()` itself. This is what makes
`date_bucket_classifies_today_yesterday_last_7_days_and_older` (and its
future-timestamp/clamping sibling) able to pin `now` to an exact,
arbitrary-multiple-of-a-day value and assert every boundary exactly, the
same "pure logic vs. IO/clock at the edges" split this codebase already
uses for `history_max_entries`/de-duplication (D4) and the perf-metrics
timestamp plumbing. The one call to a real clock
(`app::now_unix()`, already existing) happens at the `app.rs` call site
(`refresh_history_panel`/`search_history_panel`), same as every other place
this file needs "now".

**Bucket boundaries — today (diff 0) / yesterday (diff 1) / last 7 days
(diff 2..=6) / older (diff ≥7)**: chosen so "過去7日" reads naturally as "the
last 7 distinct calendar days including today and yesterday", matching how
a Japanese user is likely to read the label, rather than counting
literally-7-more-days on top of today+yesterday (which would read as a
9-day window and strain the label). A `visited_at` at or after `now` (clock
skew, or a system clock that moved backward between recording and display)
clamps to `Today` instead of underflowing `today - day` — defensive against
a corrupted/tampered `history.json` carrying a future timestamp, consistent
with this repo's "malformed persisted data must never panic" stance.

**Grouping, and where the boundary between Rust and JS sits**:
`group_by_date` merges *consecutive* entries sharing a bucket into one
`HistoryGroup`, assuming (but not requiring — a differently-ordered input
just yields more, still-correct groups rather than panicking)
newest-first input. `ui::window::BrowserWindow::set_history` calls it and
`ui::toolbar::set_history_script` serializes `&[HistoryGroup]` directly —
so the toolbar's `veloxSetHistory(groups)` receives already-grouped,
already-labeled (`HistoryDateBucket`'s `#[serde(rename_all =
"snake_case")]`, e.g. `"last_7_days"`) data and only has to walk it and
render one heading per group plus its rows; no date arithmetic happens in
JS at all. This keeps the one piece of logic the issue asked to be
pure-function-tested actually in Rust, rather than duplicated (and
untested) in `toolbar.html`.

## D30: History search — case-insensitive URL/title substring match, pure function over the full store

**Scope**: issue #18's "履歴検索: URLとタイトルに対する部分一致検索(大文字
小文字を無視)", required as "純粋関数 + 単体テスト".

**A free function, `HistoryStore::search` is a one-line wrapper**:
`browser::history::search(entries: impl IntoIterator<Item = &HistoryEntry>,
query: &str) -> Vec<&HistoryEntry>` lowercases `query` once and keeps any
entry whose `url` or `title` (`Option::is_some_and`, so a `None` title is
never a match, not a panic) contains it as a substring after also
lowercasing that field. `HistoryStore::search(&self, query)` just calls it
over `self.entries_newest_first()`. Same "pure logic function first, store
method as a thin wrapper" split D27/D29 already establish, so the matching
rule itself is testable without constructing a `HistoryStore` at all if a
future caller (e.g. #15's omnibox suggestions) wants it over some other
slice of entries.

**An empty query matches nothing, not everything**: deliberately chosen so
the return value alone tells a caller which of two states the panel should
be in — "search box is empty, show the recency panel" vs. "search
box has text, show these (possibly zero) results" — without a caller
needing a second `is_empty()` check on the query alongside the result.
`app.rs`'s `ToolbarCommand::SearchHistory` handler is exactly that caller:
an empty/whitespace-only `query` re-runs `refresh_history_panel` (the
normal recency list) instead of calling `search` and rendering its
(guaranteed-empty) result.

**Search runs over the whole store, not just the panel's visible window**:
the toolbar only ever pushes `config.history_panel_limit` (200 by default)
most-recent entries to the history panel's normal view, but a search sent
via the new `ToolbarCommand::SearchHistory { query }` calls
`HistoryStore::search` — which walks every entry the store holds, capped to
`config.history_panel_limit` only when *rendering* the result — so a match
buried well past the 200 most recent visits is still findable. This reuses
the existing IPC round-trip pattern every other panel action already uses
(`toolbar.html`'s search `<input>` sends `search_history` on every
keystroke; `app.rs` re-pushes `veloxSetHistory` with the filtered,
still-date-grouped result) rather than shipping the entire history log to
the toolbar webview once and filtering client-side, which would not scale
past `history_max_entries` (5000) and would duplicate the case-folding
logic in JS.

## D31: Persistence stays JSON files — SQLite considered and declined for this issue's scope

**Scope**: issue #18's "SQLite等の採用判断をdocs/decisions.mdに記録",
explicitly listed as an implementation item rather than an acceptance
criterion — this issue does not require switching, only requires the
decision and its reasoning to be recorded.

**What SQLite would buy here**: indexed lookup instead of the current
linear scan (`Vec<HistoryEntry>`, `O(n)` for `search`/de-duplication-by-
last-entry/id lookup), and true atomic, partial-failure-safe writes
(`INSERT`/`UPDATE` inside a transaction) in place of `write_json`'s
"serialize the whole store, `fs::write` the whole file" approach, which — as
`persistence.rs`'s own doc comment already concedes — is not crash-atomic:
a process killed mid-`fs::write` can leave a truncated `history.json` (the
existing `corrupt_file_falls_back_to_an_empty_store` test covers surviving
that, not preventing it).

**What SQLite would cost**: `rusqlite` (or an equivalent) is a real new
dependency — a C library either vendored (its `bundled` feature, adding
sqlite3's C sources to every build) or linked against the system's
`libsqlite3` (adding a to-document system-dependency requirement alongside
the existing WebKitGTK one, on top of the two platforms — macOS/Windows —
this repo currently builds on with *zero* extra system packages). It also
means every `browser::history`/`browser::bookmarks` method currently
returning a plain value now returns a `rusqlite::Result`, `persistence.rs`
grows connection-pooling/migration-versioning concerns a flat JSON file
never had, and the entire unit-test suite for both stores (all of
`browser::history::tests`/`browser::bookmarks::tests`, currently pure
in-memory `Vec` manipulation with no IO) would need an on-disk or
`:memory:` SQLite handle per test instead.

**Decision: keep JSON files. Re-evaluate only if `history_max_entries`
(currently 5000, `Config::default`) grows by an order of magnitude or the
panel needs a query SQLite is uniquely suited for (e.g. a real full-text
index) that a linear scan cannot serve interactively.** At 5000 entries, a
full `entries_newest_first()` walk for `search` (D30) or the consecutive-
url check in `record_visit` is a scan over, worst case, a few hundred KB of
in-memory `HistoryEntry` structs — sub-millisecond on any hardware this
runs on, nowhere near a threshold where an indexed query would be
user-visibly faster. `write_json`'s whole-file rewrite on every mutation is
the same story: a full JSON serialization of 5000 entries is a small,
fast operation, not the write-amplification concern it would be at, say,
500,000 entries. Weighed against CLAUDE.md's "依存クレートは必要最小限"
policy and D10's established preference (env-var-resolved paths over a
`dirs` crate, for the exact same "a few dozen lines beats a new dependency
at this scale" reasoning) — a new dependency's `Cargo.lock` diff, added
build-time system requirement, and the amount of `persistence.rs`/
`browser::history`/`browser::bookmarks` this issue would otherwise have had
to migrate wholesale is not worth it for a workload JSON already serves
adequately. This is a workload-scale argument, explicitly not a "SQLite is
categorically wrong for VeloX" one: session restore (#25) or a future
full-text search feature could reasonably tip this the other way, at which
point this decision should be revisited rather than assumed permanent.
**No new crate was added by this issue.**

## D32: Bookmark folders (#19) — flat `folder_id` reference, one layer, no nesting

**Scope**: issue #19's "フォルダ" requirement. Two shapes were on the table:

- **A tree** (folders can contain folders, `Folder { id, parent_id: Option<u64>, ... }`),
  matching how Chrome/Firefox actually let you organize bookmarks.
- **A single flat layer** (chosen): `BookmarkFolder { id, name, created_at }`
  in a separate `Vec` on `BookmarkStore`, and `BookmarkEntry` gains
  `folder_id: Option<u64>` — `None` is the root, `Some(id)` is exactly one
  folder, and a folder can never contain another folder.

**Why flat**: the issue only asks that bookmarks can be "整理できる"
(organized), not that folders nest arbitrarily deep — the acceptance
criteria (`フォルダへ整理できる`) reads as satisfied by one level. A tree
adds real cost on every side this issue also has to build: `BookmarkStore`
would need cycle-safety checks on `parent_id` (an entry can't become its own
ancestor), `BookmarkStore::remove_folder` would need to decide whether
removing a folder cascades to its subfolders or reparents them too, the
bookmarks-panel UI would need a collapsible tree widget instead of the flat
grouped-list-with-headers this issue already uses for the history panel
(D29's date buckets are exactly that shape), and the bookmark bar (D35)
would need nested flyout menus instead of one level of dropdown. None of
that machinery is free, and nothing in the issue asks for it. A flat layer
gets "group bookmarks under a named folder" with none of it: `folder_id`
is either `None` or a real folder's id, full stop — [`BookmarkStore::edit`]
enforces that invariant by falling back to `None` for an unknown id rather
than trusting the caller, and [`BookmarkStore::remove_folder`] reparents
every entry in a removed folder back to the root rather than needing a
cascade-vs-reparent decision for subfolders that cannot exist.

**Consequence for the UI**: the bookmarks panel renders root-level
bookmarks first, then one section per folder (mirroring the history panel's
date-bucket sections), and the bookmark bar renders root bookmarks as
direct buttons plus one button-with-dropdown per folder — see D35. Neither
needs a tree widget.

**Revisit if**: a future issue's users actually ask for sub-folders (nesting
folders inside folders); at that point `folder_id` on `BookmarkEntry` would
need to become `parent_id` on `BookmarkFolder` instead (folders referencing
folders, not entries referencing folders directly), which is a genuine
data-model change, not an additive one — better to make that call once
there is real demand for it than to speculatively build the tree now.

## D33: Bookmark editing — reuses `navigation::normalize_input`, rejects duplicate URLs

**Scope**: issue #19's "編集" requirement (title/URL/folder).

`ToolbarCommand::EditBookmark`'s `url` field is raw, untrusted text from the
panel's inline edit form — exactly like `ToolbarCommand::Navigate`'s `input`
field is raw text from the address bar. `app.rs`'s handler for it calls
`browser::navigation::normalize_input(&url)` — the *same* function, not a
second copy of scheme allow-listing — before ever touching
`BookmarkStore::edit`; a `None` result (empty input, an unparseable URL, or
a rejected scheme like `javascript:`/`ftp:`) rejects the whole edit and
leaves the bookmark unchanged, logged to stderr the same way a rejected
`Navigate` is. This keeps "which schemes VeloX will ever load" defined in
exactly one place (the module doc comment on `navigation::normalize_input`
already says as much), rather than letting a bookmark edit become a second,
easy-to-forget path for a `javascript:` URL to end up persisted to
`bookmarks.json` and then handed to `webview.load_url` the next time that
bookmark is opened.

`BookmarkStore::edit` itself stays a pure function over an already-validated
`String` — it never parses a URL — and enforces the *other* invariant the
module doc comment already establishes: bookmarks are de-duplicated by URL.
Editing an entry's URL to one that another bookmark already owns is
refused (`BookmarkEditError::DuplicateUrl`), the same as two `add()` calls
for the same URL never producing two entries. `app.rs` always re-pushes the
bookmarks panel/bar after an edit attempt, success or failure, so the
panel's inline edit form closes and reverts to showing the entry's actual
(possibly unchanged) state either way — there is no separate "tell the UI
the edit failed" round trip; the next full-state push *is* the correction,
matching how every other rejected mutation in this codebase (a blocked
navigation, a rejected search-engine template) is surfaced.

## D34: Manual bookmark reordering — swap the `Vec`, no separate position field; favicon keyed by URL, captured at fetch time

**Scope**: issue #19's "並び替え" requirement, plus giving `BookmarkEntry` a
`favicon` the same way #18 gave `HistoryEntry` one.

**Reordering**: `BookmarkStore::entries` was already an ordered `Vec` (the
existing `entries_newest_first()` just reverses it for display). Rather
than adding a separate `position: u32`/`order` field that would need to be
kept in sync with the vector on every insert/remove/edit, `move_up`/
`move_down` physically swap the entry with its nearest neighbor *in the
same folder/root scope* — `entries.iter().rposition`/`position` scanning
outward from the entry's own index, skipping over entries that belong to a
different folder. The vector's order **is** the display/manual order, full
stop, so there is nothing to desynchronize and no migration concern for a
pre-#19 `bookmarks.json` (its entries were already in *some* vector order —
creation order — which now doubles as their initial manual order with zero
special-casing). The cost is `O(n)` neighbor lookups instead of `O(1)`
position-field swaps, which is irrelevant at bookmark-collection sizes
(nowhere near history's 5000-entry cap, and no cap is even needed here).
One consequence, called out directly in the panel/bar UI: the bookmarks
panel and bar switch from `entries_newest_first()` (D-nothing, the original
#4 behavior) to `entries_in(folder_id)` (manual/vector order) for display —
introducing manual reordering and then still showing newest-first would
mean "move up" visibly moves a row *down* on screen. `entries_newest_first`
itself is left in place (still correct, still tested) for any future caller
that wants pure recency rather than manual order.

**Favicon**: `BookmarkEntry` grows `favicon: Option<String>` exactly like
`HistoryEntry` did in #18/D27 — `#[serde(default)]` so a pre-#19
`bookmarks.json` (no `favicon`/`folder_id` keys at all) still loads — and is
filled the same way, via `UserEvent::FaviconResolved`. The one wrinkle: a
history entry has a stable `history_id` known *before* the async favicon
fetch starts (D12's fire-and-forget pattern), so the result can be applied
to the right entry no matter what the tab does in the meantime. A bookmark
has no such id to hand the fetch — whether the current page is even
bookmarked, and which bookmark id it would be, isn't fixed at fetch time
the way "this specific history entry" is. Two options: re-read the tab's
`current_url()` when the favicon result comes back, or capture the page URL
up front. Re-reading after the fact is a race — the tab may have navigated
again while the fetch was in flight, misattributing a stale favicon to
whatever page happens to be current when the callback fires. So
`FaviconResolved` grows a `page_url: String` field, set once from the `url`
already in hand at the `LoadFinished` call site (`BrowserWindow::fetch_favicon`'s
new third parameter) — the exact page the fetch was actually started for.
`BookmarkStore::update_favicon_by_url(url, favicon)` then looks up by that
URL, mirroring `remove_by_url`'s existing keyed-by-URL shape rather than
`HistoryStore::update_favicon`'s keyed-by-id one, since a bookmark truly has
no other id available at this call site the way a history entry's
`history_id` is.

## D35: Bookmark bar — a third, additive layout component, not a `Panel`; session-only visibility

**Scope**: issue #19's "ブックマークバー" requirement, and where it sits
relative to D11's existing panel machinery.

**Why not `Panel::BookmarkBar`**: D11 already established one pattern for
"more toolbar-webview real estate on demand" — grow `toolbar_height` by
`panel_height` while a `Panel` is open, treating `History`/`Bookmarks`/
`Downloads`/`Omnibox` as mutually exclusive (opening one closes whichever
was open). The bookmark bar does not fit that shape: the issue asks for an
*always-can-be-on* strip, and it needs to coexist with an open panel — a
user should be able to have the bookmark bar showing *and* the history
panel open at the same time, the bar sitting above the panel, not one
replacing the other. Making it a `Panel` variant would either break that
(closing the bookmark bar every time a real panel opens) or require
special-casing one `Panel` variant to not participate in the
close-the-other-one toggle — more surprising than just giving it its own,
independent, additive slot.

**Layout**: `ui::window::effective_toolbar_height` gained two new
parameters, `bookmark_bar_height`/`bookmark_bar_visible`, and now *sums*
whichever of the bar and an open panel are currently on, instead of the
single `if panel_open { … }` branch D11 introduced:
`toolbar_height + (bar_visible ? bar_height : 0) + (panel_open ? panel_height : 0)`.
`BrowserWindow` tracks `bookmark_bar_visible` in a `Cell<bool>`, the same
interior-mutability reasoning D11 already documented for `open_panel`
(`sync_layout` runs from the window-resize handler with only `&BrowserWindow`).
A new `Config::bookmark_bar_height` (default 30px, roughly one tab-strip
row) joins `Config::panel_height` as a compile-time-defaulted layout
constant — no new environment variable, matching how `panel_height` itself
has none.

**Rendering many bookmarks without breaking the layout**: unlike the tab
strip (D22's "shrink, then scroll" — tabs are fixed-width chrome, so
shrinking them first delays scrolling), a bookmark bar's buttons are
free-form title text of arbitrary length; shrinking them toward zero width
would make them unreadable long before it would ever avoid a scrollbar. So
the bar takes D22's *other* half only: `#bookmark-bar { overflow-x: auto;
white-space: nowrap; }` with each `.bookmark-bar-item { flex: none;
max-width: 160px; text-overflow: ellipsis; }` — items keep a readable,
capped width and the strip scrolls horizontally once they overflow it,
exactly like the tab strip does once every tab has already shrunk to its
own floor. A folder (D32: one flat layer) renders as one such item whose
click toggles a small absolutely-positioned dropdown listing its entries,
rather than expanding inline and pushing every later bookmark sideways —
keeps the bar's own height fixed at `bookmark_bar_height` regardless of how
many entries a given folder holds.

**Visibility is session-only, not persisted**: Ctrl/Cmd+Shift+B and the
toolbar's bar-toggle button both flip `BrowserWindow::bookmark_bar_visible`
for the life of the running process; there is no fourth on-disk file (or a
new key stitched into `bookmarks.json`, which would conflate "bookmark
data" with "a UI preference" in the one file that is supposed to hold only
the former) to remember the choice across restarts. This mirrors how
`Panel` open/closed state itself has never been persisted, and keeps this
issue from having to invent a general settings-persistence layer (there is
none yet) just to remember one boolean. **Revisit if** a future issue adds
a real settings/preferences store for other reasons — persisting the bar's
visibility would then be a trivial addition to it, not a reason to build
that store now.

**Keyboard shortcuts**: Ctrl/Cmd+D (`ToggleBookmark`) and Ctrl/Cmd+Shift+B
(`ToggleBookmarkBar`) both follow the exact two-channel delivery pattern
D18/D23 already established — a fixed-sentinel-string IPC message
(`velox:toggle-bookmark` / `velox:toggle-bookmark-bar`) from an injected
content-webview script for when a page has focus, and a structured
`ToolbarCommand` from the toolbar's own trusted keydown listener for when
the address bar/panel has focus — both funneled into the same
`toggle_current_bookmark`/`toggle_bookmark_bar` functions in `app.rs` so
there is exactly one implementation of each action regardless of which
webview observed the keypress.

## D36: Omnibox ranking (#20) — scoring formula: match tier + frecency + bookmark bonus

**Scope**: issue #20's core ask — "候補ランキング / 重み付けとスコアリング",
implemented as pure functions in the new `browser::ranking` module
(`score_page_entry`, `rank_page_entries`) and unit-tested there, never
inline in `CandidateSource::candidates` — the same "pure logic module,
thin `CandidateSource` wrapper" split D26 already used for
`classify_input`/`build_candidates`.

**The formula.** For a history/bookmark entry against the current
address-bar text `query` (case-insensitive throughout):

```
score = match_tier + match_ratio_bonus + frequency_score + recency_score + bookmark_bonus
```

- **`match_tier`** — where `query` was found, highest first (a match at a
  *higher* tier is checked first and wins outright; lower tiers are never
  even consulted once a higher one hits):

  | tier | condition | points |
  |---|---|---|
  | host prefix | URL's host starts with `query` | 100 |
  | title prefix | title starts with `query` | 90 |
  | host contains | `query` appears anywhere in the host | 70 |
  | title contains | `query` appears anywhere in the title | 60 |
  | url contains | `query` appears anywhere in the full URL (typically the path/query string) | 40 |
  | *(none of the above)* | — | *entry is dropped entirely, not merely scored low* |

  This directly implements the issue's own guidance ("URL のホスト先頭に
  一致 > パス途中に一致"): a host-prefix hit is the strongest signal a
  mainstream browser's omnibox also treats as king, and a bare path/query
  substring match is deliberately the weakest tier that still counts as a
  match at all.
- **`match_ratio_bonus`** (0-20) — `min(len(query) / len(matched_field), 1.0)
  × 20`, measured in `char`s (not bytes, so Japanese text is not penalized
  relative to ASCII). Rewards a match that accounts for more of the field:
  typing `example` against the host `example.io` (ratio ≈ 0.64) scores
  higher than the same `example` against
  `example-with-a-much-longer-domain-name.io` (ratio ≈ 0.15) — both are
  host-prefix matches (tier 100), the ratio is what tells them apart.
- **`frequency_score`** (0-30) — `min(visit_count, 20) × 1.5`. Capped and
  linear rather than logarithmic on purpose: the cap alone already bounds
  the top end (one page visited 5000 times cannot mathematically outscore
  every text-match distinction below it), and a linear curve keeps every
  score in this module an exact, hand-verifiable number for the unit tests
  — no `ln`/`log2` arithmetic to reason about when eyeballing a test's
  expected ordering.
- **`recency_score`** (0-30) — bucketed onto the *exact same*
  今日/昨日/過去7日/それ以前 boundaries `history::date_bucket` already
  established for the history panel (D29): today = 30, yesterday = 20,
  last 7 days = 10, older (or never visited) = 0. One recency vocabulary
  for the whole codebase, not a second independent "how recent counts as
  recent" scale invented for ranking alone.
- **`bookmark_bonus`** (0 or 25, flat) — see D37 below.

Maximum possible score is 100 + 20 + 30 + 30 + 25 = 205; a bare non-match
never appears (it is filtered out, not scored 0), so there is no meaningful
"minimum" beyond whatever the weakest tier (40, url-contains, zero
everything else) plus nothing produces.

**Frecency, not separate frequency/recency signals.** The issue explicitly
suggested "frecency 的な組み合わせ" of `visit_count`/`visited_at`; this
implements it as a straight sum of the two capped/bucketed scores above
(max 60 combined) rather than a product or a single blended metric — a sum
keeps each half independently reasoned-about and testable (see
`ranking::tests::higher_visit_count_scores_higher_all_else_equal` and
`...more_recent_visits_score_higher`, which each hold the other input
fixed), at the cost of not modeling any interaction between "how often" and
"how recently" (Firefox's actual frecency algorithm does model some
interaction via visit-type buckets; VeloX's simpler sum was judged good
enough for an omnibox dropdown of ~6 rows, not a research-grade ranking
system).

**Worked example** (all at `now` = some Tuesday, DuckDuckGo as the search
engine, query `"example"`):

| entry | tier | ratio | freq | recency | bookmark | **total** |
|---|---|---|---|---|---|---|
| `example.com`, visited 20+ times today, bookmarked | host prefix (100) | ratio for `"example"`/`"example.com"` ≈ 0.64 → 12.7 | 30 (capped) | 30 (today) | 25 | **≈197.7** |
| `my-example.net`, never visited, bookmarked only | host contains (70) | ratio for `"example"`/`"my-example.net"` ≈ 0.5 → 10.0 | 0 | 0 | 25 | **105.0** |
| `example.io`, visited once 3 days ago, not bookmarked | host prefix (100) | ratio ≈ 0.64 → 12.7 | 1.5 | 10 (last 7 days) | 0 | **124.2** |
| page titled "A guide to example usage", visited 20+ times today | title contains (60) | ratio for `"example"`/that ~24-char title ≈ 0.29 → 5.8 | 30 | 30 | 0 | **125.8** |
| `other.test/path/example`, bookmarked, never visited | url contains (40) | ratio ≈ 0.3 → 6.1 | 0 | 0 | 25 | **71.1** |

Ranked order for this set: the actively-used bookmark (197.7) first, then
the popular title match (125.8), then the recent host-prefix match
(124.2), then the untouched bookmark that only matches by host-substring
(105.0), and last the weak-tier bookmark (71.1) — i.e. text-match quality
and bookmark status both matter, but neither one alone always wins; see
`ranking::tests` for the individual pairwise assertions this table is built
from (`bookmark_bonus_can_move_a_never_visited_bookmark_above_a_weak_history_match`,
`a_much_more_popular_history_entry_can_still_outrank_a_weaker_tier_bookmark`).

**Deliberately *not* done**: reordering built-in `NavigateUrl`/`Search`
candidates relative to history/bookmark ones by score. `build_candidates`
(D26) always puts the two built-ins first, unconditionally, before any
`CandidateSource` is even asked — #20 does not change that, both because
the issue's integration point explicitly says no signature change is
needed and because "what you literally typed is always the first option"
is itself a load-bearing, mainstream-browser convention worth keeping
regardless of how a history match happens to score. Balance between kinds
*within* the sourced portion (history vs. bookmark vs. re-suggested search
queries) is handled by letting them all compete on one score axis instead
of reserving fixed per-kind slots — a static quota (e.g. "at most 3
history rows") would sometimes evict a highly relevant match just to make
room for a barely-relevant one of a different kind purely to hit a slot
count, which is a worse outcome than the highest-scoring candidates simply
winning regardless of kind.

## D37: History/bookmark de-duplication — one entry per URL, bookmark wins the displayed kind

**Scope**: issue #20's explicit ask — "同一URLが履歴とブックマークの両方に
ある場合、どちらか一方に寄せる".

**Merge, don't pick one store and ignore the other.** `ranking::merge_entries`
builds one `RankableEntry` per unique URL from *both* `HistoryStore` and
`BookmarkStore`, rather than having a `HistoryBookmarkSource` simply prefer
whichever store it happens to check first (which would silently drop the
other store's data for that URL — e.g. a bookmarked page's `visit_count`
would vanish from scoring if bookmarks were checked first and "won"
outright). The merge keeps:

- `is_bookmark: true` — bookmark status, once true, is never lost even
  though `HistoryEntry` itself carries no such flag.
- **The bookmark's title when it has one, otherwise falls back to
  history's title.** Rationale: a bookmark's title is something the user
  explicitly chose to keep (or explicitly edited via
  `BookmarkStore::edit` — Issue #19/D33), which is a stronger signal of
  "this is what I want to call this page" than whatever `<title>` the page
  happened to report when it was last visited. When a bookmark was added
  with no title at all (`BookmarkEntry::title: None` — always possible,
  see `BookmarkStore::add`), history's title is a strictly better fallback
  than showing a bare URL as the label.
- `visit_count`/`last_visited_at` from history unconditionally — a
  bookmark is not a visit log (`BookmarkEntry` has no such fields at all),
  so there is nothing to prefer here; a bookmark-only entry (never visited)
  simply gets `visit_count: 0`/`last_visited_at: None`, which
  `score_page_entry` treats as "no frecency contribution", not as an error
  or a special case.

**The merged entry's `CandidateKind` is `Bookmark`, not `History`, whenever
`is_bookmark` is true** — this is the actual "どちらに寄せるか" call the
issue asked for. Reasoning: a bookmark is an explicit, durable choice the
user made about a URL; a history entry is a side effect of merely having
visited it once. When both exist for the same URL, the explicit signal is
the more meaningful one to show (star icon, not clock icon), even though
the URL is *also* in the visit log. This only affects which icon/kind the
row renders as — the underlying score already accounts for both signals
(frecency from history, `BOOKMARK_BONUS` from the bookmark) regardless of
which kind wins the display.

**`detail` shows the destination URL, not a visit timestamp.**
`Candidate::detail`'s doc comment (D26/#15) suggested "a visit timestamp"
as the likely #20 use; this implementation shows the target URL instead
(only when the label is already showing a title — an untitled entry's
label *is* the URL, so a duplicate `detail` would be redundant) for a
concrete reason: the user is one click/Enter away from loading whatever
`target_url` says, and a page's own `<title>` is attacker-influenced text
that is not a trustworthy stand-in for "where this is about to take me" —
showing the real URL underneath the (possibly misleading) title is a small
but real piece of the "know what you're about to load" security surface
this browser already cares about elsewhere (content blocking, scheme
allow-listing). A timestamp is nice-to-have; the destination URL is the
thing that actually matters before clicking.

## D38: Typed search-query history ("入力履歴") — search queries only, LRU-capped, persisted, purged with "clear history"

**Scope**: issue #20's "入力履歴" line item — remembering raw address-bar
input the user actually submitted, separately from `HistoryStore` (which
only ever records *page visits*, i.e. the URL a search results page ends up
at, never the literal query text that produced it).

**Only `Intent::Search` text is recorded, never `Intent::Url` text.** A new
`browser::input_history::InputHistoryStore` records exactly one thing: the
raw text of a submitted search query (`navigation::classify_input`
returning `Intent::Search`). URL-shaped input the user types is
deliberately *not* also stored here — loading it already lands the
resulting page in `HistoryStore` once the load finishes
(`app::record_visit_if_enabled`), with richer bookkeeping (title, favicon,
precise `record_visit` de-duplication semantics) than a second, parallel
"I also typed this" log could offer. Storing it twice would only add
duplicate, lower-quality data for the omnibox to rank.

**Why this needs to exist at all, given `HistoryStore` already exists**:
a search query's *text* (`"rust ownership"`) is not recoverable from
`HistoryStore` — what gets recorded there is the search engine's results
URL (`https://duckduckgo.com/?q=rust+ownership`), not the human-readable
query. Without a separate store, retyping the start of a query you
searched for last week has nothing to resurface as a suggestion beyond
whatever page you eventually clicked through to.

**Recording point**: `app.rs`'s `ToolbarCommand::Navigate` handler
classifies `input` once (`navigation::classify_input`), and — only when
the result is `Intent::Search` — calls
`record_input_history_if_enabled` before resolving the same `Intent` to a
URL via the new `resolve_intent` helper (split out of the old
`resolve_navigate_target` specifically so classification happens once and
is reused for both purposes). Because `Navigate`'s `input` is the *raw*
typed text only when the user actually typed it and pressed Enter with no
dropdown row highlighted — a candidate-row click or an Enter with a row
highlighted sends that candidate's already-resolved `target_url` instead
(an absolute `https://…` URL, which `classify_input` always reads as
`Intent::Url`, never `Intent::Search`) — this **only captures "type text,
press Enter directly"**, not "type text, then explicitly select the
built-in Search suggestion row from the dropdown and press
Enter/click it". This is a known, accepted gap: the plain-Enter path is by
far the more common way to submit a search (mainstream browsers'
omniboxes do not require touching the dropdown to search), and closing the
gap completely would require `ToolbarCommand::Navigate` to carry the
original raw text alongside a resolved `target_url` for every candidate —
a signature change the issue's integration notes say should not be needed.

**LRU cap, not insertion-order cap — the one place this store's eviction
rule intentionally differs from `HistoryStore`'s (D27).**
`HistoryStore::record_visit` drops the *oldest-inserted* entry once
`max_entries` is hit, which fits a visit log ("what happened, in order").
`InputHistoryStore::record` instead drops the single
*least-recently-used* entry (by `last_used_at`, not insertion order) —
because this store's entire purpose is "what might I want to search for
again", a query re-typed and re-used last week should survive eviction
over one nobody has touched since the day it was first recorded, even if
that one happens to be newer by insertion order alone. Default cap:
`input_history::DEFAULT_MAX_ENTRIES` = 200 — deliberately far smaller than
`Config::history_max_entries` (5000): a few hundred distinct recent
queries is already generous for "does this look familiar", and this is a
plain module constant rather than a new `Config`/env-var knob, matching
how `omnibox::DEFAULT_CANDIDATE_LIMIT` is also a bare constant, not
something #15 exposed for configuration.

**Persisted, like history/bookmarks — `input_history.json`, same "dumb IO"
pattern as `persistence::{save,load}_{history,bookmarks}`.** A search
query you typed a week ago being suggestible today is exactly the feature;
session-only storage would throw that away on every restart for no
privacy benefit beyond what D39 already covers via the `history_enabled`
recording gate. `AppState` gained one more field
(`input_history: InputHistoryStore`), loaded/saved next to `history`/
`bookmarks` in `run()`, following the identical "load at startup, persist
after each mutation, treat every IO failure as non-fatal" shape those two
already use — no new persistence *mechanism*, just a third JSON file
through the existing one.

**`ToolbarCommand::ClearHistory` also clears `input_history`.** Not
explicitly named in the issue's acceptance criteria, but a search query is
part of the same "what have I been doing in this browser" privacy surface
as a page visit — a user who clicks "clear history" almost certainly means
"forget what I searched for" too, and leaving a separate, undiscoverable
store of query text behind after that action would be a surprising privacy
gap, not a feature.

## D39: Omnibox candidates in private mode — read existing data, record nothing new

**Scope**: the issue's explicit call-out — "履歴候補はプライベートモードで
どう振る舞うべきか検討してください… 既存履歴の参照まで止めるかは判断が
必要です".

**Decision: reference (read) yes, record (write) no — for both `HistoryStore`
and the new `InputHistoryStore`.** Concretely:

- `HistoryBookmarkSource`/`InputHistorySource` (the two `CandidateSource`
  impls #20 adds) read `state.history`/`state.bookmarks`/
  `state.input_history` exactly as they stand, with **no** `history_enabled`
  check of their own — they are constructed fresh from the current stores
  on every `OmniboxInput`, private mode or not.
- Recording remains gated exactly as it already was/now is:
  `record_visit_if_enabled` (pre-existing, D13) and the new
  `record_input_history_if_enabled` (D38) both bail out immediately when
  `state.history_enabled` is `false`, so nothing typed or visited during a
  private session is ever added to either store.

**Why reading is fine even though writing is not — this is not a new
policy, it is the policy D13/D14 already established for the History
*panel*.** `run()` loads `history.json`/`bookmarks.json` from disk
unconditionally, regardless of `config.private`; `ToolbarCommand::TogglePanel { panel: History }`
and its `refresh_history_panel` show whatever `state.history` already
holds with no `history_enabled` check either. In other words: launching
VeloX with `--private` already means "the History panel still shows
everything from before you went private, and nothing new gets added to
it" — #20's omnibox candidates are a second read path over the exact same
data with the exact same rule, not a new privacy surface. This also
matches the mainstream-browser convention (Chrome Incognito, Firefox
Private Browsing): a private window's address bar still suggests from your
regular history, because that data already existed on disk and offering it
back to you is not a new leak; what private mode actually promises is that
*this* session's own activity will not be added to it.

**What this means end-to-end for a private session**: a page visited
*before* private mode started can still appear as a `History` candidate; a
page visited *during* private mode never gets recorded, so it can never
appear as one (`record_visit_if_enabled` never runs). A search query typed
before private mode started can resurface via `InputHistorySource`; one
typed during it is never recorded. Bookmarks behave the same in both modes
either way — bookmarking is always an explicit, deliberate action, never
implicit like a page visit, so D14 never gated it and #20 does not either.

**Revisit if**: VeloX ever gains true per-window (not whole-app, see D14)
private browsing — at that point "does an omnibox opened in a private
window suggest from the *other*, non-private window's very-recent history"
becomes a real question this decision does not answer, since today there
is only ever one `AppState`/one set of stores for the whole process.

## D40: Startup URL is overridable (`--homepage` / `VELOX_HOMEPAGE`), default is Google

**Scope**: Issue #106. Until now `Config::homepage` was a compile-time
constant and `Config::from_env_and_args`'s `args` were consulted only for
`--private`, so nothing outside the binary could choose the page VeloX opens.

**Why this blocked Phase 3**: `velox-bench run` (D21) spawns `velox` with
only the `VELOX_PERF_*` variables, so every trial measured whatever the
default homepage was. That made the fixed fixtures in `scripts/bench/pages/`
unreachable, and pinned first-page-load numbers to a network-dependent
external site. Epic #57's first absolute rule is "no optimization without a
benchmark", so this one gap held up the whole performance program.

**Decision**: three layers, highest priority first — a `--homepage <URL>` /
`--homepage=<URL>` flag, the `VELOX_HOMEPAGE` environment variable, then the
compiled-in default. `velox-bench run` grew a `--url` option that it forwards
as `VELOX_HOMEPAGE`, and warns when it is omitted precisely because the
default is network-dependent.

**Validation, not a second parser**: every candidate goes through
`navigation::normalize_input` — the same function the address bar uses (D26).
`javascript:` and other rejected schemes, and anything unparseable, are
*skipped in favour of the next candidate* rather than failing the launch: a
typo in a benchmark script should not leave VeloX with no page to show, and
a hostile value in the environment must not become a navigable URL. The
resolution is a pure function (`resolve_homepage`) so the precedence and the
rejection rules are unit-tested without touching the real environment,
matching `resolve_private`/`resolve_perf_env`/`resolve_search_engine`.

**Default changed** from `https://example.com` to `https://www.google.com/`.
`example.com` is a specification placeholder, not a page anyone wants on
startup; it was only ever a stand-in. Note that this makes the *default*
launch network-dependent, which is exactly why benchmarks must pass `--url`.

**Cost / revisit condition**: the flag only sets the *initial* page. Driving
navigation after startup, or opening N tabs at launch, still has no hook, so
`Scenario::is_unattended()` keeps `navigation`/`tab_create`/`tab_switch`/
`tabs_1..50` manual. Those need a separate automation hook in `app.rs`;
revisit when #58 fixes the benchmark conditions and says which of them must
run unattended in CI.

## D41: Competitive benchmarking measures an external load beacon, and compares memory by PSS

**Scope**: Issue #58 — fixing Phase 3's measurement conditions and targets.

**Why a second harness**: `velox-bench` (D21) aggregates the JSON Lines VeloX
writes about *its own* internal events (window created, toolbar ready,
`LoadFinished`). Those have no counterpart in another browser, so they cannot
express "VeloX vs Chromium". `scripts/bench/compare_browsers.py` therefore
measures only things that mean the same thing in any browser.

**The comparable clock**: process spawn → the page's own `load` event. The
harness serves the fixed fixture from loopback with a small beacon appended,
and timestamps the resulting `GET /loaded`. No browser-internal API, no
DevTools protocol, no network. The same beacon is injected for both browsers,
so it cannot favour either. The fixture files themselves are left untouched.

**Memory is compared by PSS, not summed RSS.** Summing RSS across a process
tree counts every shared page once per process, so a browser with more
processes looks heavier than it is. This is not a theoretical concern: on the
same page at the same instant, VeloX (5 processes) totalled 775 MiB RSS
against Chromium's (9 processes) 855 MiB — but by PSS it was VeloX 424 MiB
against Chromium 329 MiB. **The two metrics give opposite answers to "which
browser uses less memory".** PSS divides each shared page by its sharer count,
which is the right question for browsers that differ in process count.

**Consequence for D16**: `browser::metrics::sample_process_tree_rss` sums RSS
and therefore *overstates* VeloX's memory position. Phase 3's memory work
(#61/#62/#63) must not adopt it as the improvement metric unchanged; adding
PSS is tracked as #108. Tab suspension (D9) has the same exposure — dropping a
webview process removes its whole RSS from the sum, while the shared pages it
was counting survive in the remaining processes, so the RSS delta overstates
the real saving.

**What this does not measure**: rendering quality, JS execution, multi-tab
behaviour, battery. And because VeloX embeds the system WebView, the
comparison is as much "WebKitGTK vs Blink" as "VeloX vs Chromium" — it does
not separate VeloX's own overhead from the engine's, which is exactly the
distinction Epic #57 insists on. Reading a VeloX-vs-Chromium number as a
verdict on VeloX's code would be a mistake.

**Cost / revisit condition**: measured with no GPU, so both browsers fall back
to software rendering and the absolute memory numbers do not transfer to real
hardware — treat them as same-environment relative figures only. Re-measure on
real hardware under #70, and add Firefox (Gecko) as the next comparison target,
since Safari does not exist on Linux and Edge shares Blink.

## D42: `browser::metrics` gains PSS, alongside RSS rather than instead of it

**Scope**: Issue #108, the follow-up D41 named directly — VeloX's own
`browser::metrics::sample_process_tree_rss` (D16) summed only RSS, which D41
showed *overstates* VeloX's memory position relative to a browser with more
processes. Phase 3's memory work (#61/#62/#63) needs the corrected metric
before it starts, or it optimizes against the wrong number.

**One function, two totals — not a second `sample_process_tree_pss`**:
`sample_process_tree_rss` already walks `/proc` once per call to place every
process in the tree and read its RSS; reading PSS for the same process at
the same time is one more file read per process, not a second tree walk. A
separate function would either walk `/proc` twice per sample (wasteful, and
liable to walk a slightly different process set the second time as
processes come and go) or force every caller to thread two results back
together themselves. So `RssSample` gained `total_pss_bytes: Option<u64>`
and `pss_process_count: usize` alongside the existing `total_rss_bytes`, and
`ProcInfo` (the internal per-process record `build_sample` sums over)
gained `pss_bytes: Option<u64>`. The function's name stays
`sample_process_tree_rss` rather than becoming `sample_process_tree_memory`
or similar — RSS is still the one field guaranteed to be present (see
below), so the name still describes what always comes back; PSS rides
along as best-effort.

**PSS source**: `/proc/<pid>/smaps_rollup`'s `Pss:` line (kB), matching
`scripts/bench/compare_browsers.py`'s `_pss_bytes` (added in #58/D41) —
the task explicitly asked to follow that reference implementation, and
doing so means VeloX's own number and the competitive-comparison script's
number are computed the same way, not two independently-written parsers
that could silently drift apart. `smaps_rollup` is a kernel-computed sum
across every mapping (Linux ≥ 4.14), cheaper than parsing
`/proc/<pid>/smaps`'s per-mapping detail for a total this project has no
use for at that granularity.

**RSS must keep working when PSS cannot be read — this is the actual
design constraint, not a footnote.** `smaps_rollup` did not exist before
Linux 4.14, can be permission-gated in some sandboxes, and does not exist
at all on the non-Linux `imp` backends (macOS/*BSD's `ps` fallback has no
PSS equivalent; Windows has neither). RSS has no such gap: it comes from
`/proc/<pid>/status`, which every `/proc` entry this code can already see
at all has. So the two are independent per process — `ProcInfo::pss_bytes`
is `Option<u64>`, defaulting to `None`, and a process missing it is still
summed into `total_rss_bytes` normally. `build_sample` sums PSS only over
processes that have it and separately counts how many did
(`pss_process_count`); `total_pss_bytes` is `(pss_process_count >
0).then_some(sum)` — `None` only when *no* process in the tree could be
read, never a silent `0`, which would be indistinguishable from "measured
zero PSS" and would make a dashboard built on this data quietly show a
9x-too-good number instead of admitting it has nothing.

**Partial reads are visible, not averaged away**: a tree can end up with
some processes' PSS readable and others' not (e.g. one helper process
exited in the gap between listing `/proc` and reading its
`smaps_rollup`). `total_pss_bytes` in that case is `Some` of whatever
*was* read — dropping the whole sample over one unreadable process would
throw away real data — but `pss_process_count < process_count` marks it as
an undercount rather than a complete total, so a consumer comparing two
samples does not mistake a partial 3-of-5-processes sum for the real
number. This is the same rule `compare_browsers.py`'s `process_tree_memory`
follows (its docstring says as much) and the same rule as `total_pss_bytes:
None` above, just one level down.

**JSON schema change**: `rss` events gained `total_pss_bytes` (uint or
`null` — `null`, not an absent key, precisely so a consumer parsing this
field can never confuse "measured, came back zero" with "this build cannot
measure it" with "this build predates the field") and `pss_process_count`
(uint, always present — even `0` is informative: "PSS was attempted for 0
of N processes"). This is additive only; the pre-existing `rss` fields
(`pid`, `process_count`, `total_rss_bytes`) are untouched, so a consumer
written against the pre-#108 schema keeps working unmodified — it just
never sees the two new keys. The text format
(`velox[perf] rss pid=… processes=… total_mib=… pss_processes=<n>/<total>
pss_mib=<value|n/a>`) appends the same two facts at the end of the existing
line rather than inserting them, for the same reason (D19's "never break
existing text output" rule) — a scraper matching the original
`rss pid=… processes=… total_mib=…` prefix is unaffected.

**`velox-bench` side**: `MetricKey` gained `PssTotalBytes`
(`pss_total_bytes`, reading `rss`'s `total_pss_bytes`) and
`PssProcessCount` (`pss_process_count`, reading `rss`'s
`pss_process_count`), taking `MetricKey::ALL` from 8 to 10 entries.
`MetricKey::extract`'s existing `.and_then(Value::as_f64)` step already
turns a JSON `null` into `None` and drops it, with no special-casing
needed — a JSON `null` `total_pss_bytes` behaves exactly like a metric
that never fired for that event, and `aggregate_trials` already omits any
metric with zero samples from the result (`BenchmarkResult::metrics`) — so
"PSS unreadable in this environment" surfaces as `pss_total_bytes` simply
being absent from the saved result, not as a `0.0` a reviewer could
mistake for a real measurement. `docs/benchmarking.md` and
`docs/performance-targets.md` §3.1 are updated to say to compare
`pss_total_bytes`, not `rss_total_bytes`, going forward.

**No new dependency** (D6): the entire addition is one more per-process
file read plus a few lines of the same line-based parsing D16 already used
for `status`, matching D16's own reasoning for staying on direct `/proc`
reads instead of a crate like `sysinfo`.

**Verification**: unit-tested at the `build_sample` level (every-process-
has-PSS, no-process-has-PSS, and the partial-coverage case), at the
`RssSample`/`PerfRecord` text-and-JSON rendering level (`Some`/`None`
displayed and serialized correctly), and at the `velox-bench` extraction/
aggregation level (`MetricKey::PssTotalBytes` correctly disappears on a
JSON `null` while `PssProcessCount` does not). Confirmed against the real
VeloX binary under `Xvfb` per the Issue #108 tasking; see the task's PR
description / commit for the actual PSS figure observed and how it compares
to `compare_browsers.py`'s ~424 MiB (§4 of `docs/performance-targets.md`).
## D43: `window_created` → `toolbar_ready` を細分化して実測した結果、有効な最適化は見つからなかった

**対象**: Issue #59 (T3: `startup_toolbar_ready_ms` を 300ms 以下にする)。

**背景の仮説（Issue #59 の記述、実測前）**: `src/ui/toolbar.html` が 49KB の
単一ファイルであり、その HTML/CSS/JS のパースと初期化スクリプト実行が
`window_created`(224ms) → `toolbar_ready`(528ms) の約 300ms のギャップの
主因ではないか、というのが Issue 起票時点の推測だった。

**計測の細分化**: `browser::metrics::StartupTimestamps` に 2 つの中間チェック
ポイントを追加した（`docs/architecture.md` の「Performance extension
points」も参照）。

1. `mark_rust_setup_done` — `app::run` が history/bookmarks/input_history を
   ディスクから読み込み、`AppState` を組み立て終え、`event_loop.run(...)` を
   呼ぶ直前。ここまでは GTK/webview のイベントループが一切回っていない、純粋
   な Rust 側の同期処理。
2. `mark_toolbar_script_started` — ツールバー webview の `<script>` ブロック
   が実行を開始した瞬間。`toolbar.html` の `<script>` はドキュメント末尾
   （`</body>` 直前）にあり、`send({cmd:"script_started"})` をその最初の文
   として送る。この時点でエンジンはすでに HTML/CSS 全体をパース済みであり、
   ここから既存の `toolbar_ready`（スクリプトの最後の行で送る `ready`）まで
   が「ツールバー自身の JS 実行 + IPC 往復」に相当する。

この 2 点により、`window_created → toolbar_ready` の約 200〜300ms を
「①Rust 側セットアップ」「②エンジンがドキュメントをパースし終えるまで」
「③ツールバー自身の JS 実行」の 3 区間に分解できる。計測オフ時は D19 と同じ
パターン（`Option` の有無だけで分岐、追加の `Instant::now()` なし）で
オーバーヘッドを増やさない。

**実測結果（このリポジトリの計測用サンドボックス、GPU なし Xvfb、
`docs/performance-targets.md` §1 の環境、`minimal.html`、複数試行の中央値）**:

| 区間 | 所要時間 | 内容 |
| --- | ---: | --- |
| `window_created` → `rust_setup_done` | **約 0.1ms** | history/bookmarks/input_history の読み込み + `AppState` 構築 |
| `rust_setup_done` → `toolbar_script_started` | **約 170〜220ms** | ここが実質的にギャップの全て |
| `toolbar_script_started` → `toolbar_ready` | **ほぼ 0ms**（同一 `ts_ms` に丸められる） | ツールバー自身の JS 実行 |

**① は無視できる**: history.json 等が小さい（数百バイト程度）現状では、
永続化の読み込みは測定誤差の範囲。件数が数千件規模まで増えた場合は別だが、
現状のギャップの説明にはならない。

**② が支配的で、しかも `toolbar.html` の内容量に依存しないことを実証した**:
`toolbar.html` を 49KB のフル版から `<script>` 2 行だけの最小版（CSS なし、
DOM 操作なし）に一時的に差し替えて同条件で再計測したところ、
`rust_setup_done → toolbar_script_started` は **約 174〜199ms** とフル版
（約 170〜220ms）とほぼ同じだった。つまりこの区間は toolbar.html の
サイズや複雑さで説明できない。

`src/ui/window.rs::BrowserWindow::new` に一時的な診断タイムスタンプを入れて
さらに分解すると、この区間の内訳は概ね次のとおりだった:

- `EventLoopBuilder::with_user_event().build()`（tao 経由の `gtk_init` 相当）
  だけで **約 95〜130ms**。
- `attach(toolbar_builder)`（`WebViewBuilderExtUnix::build_gtk`、ツールバー
  webview の生成）に **約 60〜100ms**（`window_created` にはこの同期呼び出し
  の分だけが含まれ、その後さらに非同期のエンジン初期化が続く）。
- content webview の `attach` は追加で **約 10〜30ms** のみ（ツールバーが
  先に「初回 webview 作成」のコストを払ったあとなので安い）。

**③ はほぼ 0ms**: スクリプトの先頭（`script_started` 送信）から末尾
（`ready` 送信）までの JS 自体の実行コストは、フル版・最小版どちらでも
測定精度（0.1ms）内に収まった。DOM 要素の取得やイベントリスナー登録は軽い。

**結論 — 有効な最適化は見つからなかった**: `window_created → toolbar_ready`
の実体は、`tao`/GTK のイベントループ初期化と、WebKitGTK が最初の webview
（ツールバー）を生成する際のエンジン側コスト（プロセス起動・IPC 確立を含む
と推測される非同期処理）であり、**VeloX 自身の toolbar.html の内容や
Rust 側の起動処理を変更しても実測上ほとんど動かない**。これは Issue #59
起票時点の仮説（「ここは VeloX 自身のコードでエンジン差ではない」）を実測で
覆す結果である。Epic #57 の原則（「WebView をブラックボックスとして扱う —
VeloX が改善できるのは WebView の周囲だけ」）が、この区間についてはそのまま
当てはまってしまうということでもある。

参考として、`libEGL warning: DRI3 error` と `LIBGL_ALWAYS_SOFTWARE=1
WEBKIT_DISABLE_COMPOSITING_MODE=1` を組み合わせた実験では、この区間がおよそ
半分に短縮された（失敗する DRI3/EGL ネゴシエーションを最初からスキップする
ため）。ただしこれは `docs/performance-targets.md` が明記する「GPU なしの
Xvfb 環境」固有のアーティファクトであり、実 GPU を持つ利用者のマシンでは
再現しない（むしろハードウェアアクセラレーションを恒久的に無効化することに
なり、実機では悪化させる可能性がある）。したがって **本 Issue の変更には
含めていない**。実機再測定は #70 の課題であり、そちらで意味のある差になる
かどうかを判断すべきものと考える。

**このため、本 Issue でのコード変更は計測の細分化のみである**（`toolbar.html`
の圧縮や遅延読み込みといった対症療法は行わない — 効果が実測でゼロと分かって
いる変更を「やった感」のために入れることは、Epic #57 の「ベンチマークなしの
最適化をしない」に反する）。`velox-bench` の before/after 比較でも
`startup_toolbar_ready_ms` は誤差の範囲内で変化なし（計測を追加しただけで
実行パスは変わっていないため、当然の結果）。

**フォローアップ**: エンジン側コスト（tao の `gtk_init`、WebKitGTK の初回
webview 生成）を削減する手段があるとすれば、wry/tao 自体へのパッチや
WebKitGTK のプロセスモデル設定変更が必要になり、「新規依存を避ける」
「`unsafe` 原則禁止」「correctness を壊さない」という本リポジトリの制約の
中では今回のスコープを超える。実機（GPU あり）での再測定 (#70) を先に行い、
このコストがサンドボックス固有かどうかを切り分けたうえで、必要なら新しい
Issue として起票するのが妥当。

## D44: Benchmark automation hook — a read-once opt-in script file, not a socket/RPC server

**Scope**: Issue #112. `velox-bench run` could only drive the three startup
scenarios (`cold_startup`/`warm_startup`/`first_page_load`) unattended —
nothing could open/switch/close tabs or navigate away from the initial page
from outside the process, so `navigation`/`tab_create`/`tab_switch`/
`tabs_1..50` all needed a human at the keyboard (see D21's original scoping
of `Scenario::is_unattended`). Issues #60-#65 and performance targets T2/T4
were blocked on this: Epic #57's rule is "no optimization without a
benchmark", and there was no way to get one for these scenarios at all,
attended or not.

**Why not a listening socket or RPC server**: this was the obvious design
and the one rejected first. `docs/architecture.md`'s "Why a webview
toolbar?" and D18/D23 establish VeloX's one real trust boundary: the
content webview's IPC channel accepts only a fixed set of exact-string
sentinels (never structured, page-supplied data), and only the *toolbar*
webview — trusted first-party chrome — can send a structured command
(`ToolbarCommand`) that actually drives tab management. A TCP/Unix-socket
listener or an HTTP/RPC endpoint that accepted "open a tab"/"navigate to
this URL" commands from an arbitrary external process would be a second,
much wider hole in exactly that boundary: unlike a content webview's
sentinel-only channel, such a server would have to deserialize structured,
externally-supplied commands into real browser actions, on every single
launch, for as long as the process runs — precisely the shape of interface
D18/D23 went out of their way to avoid giving to anything less trusted than
the toolbar. It would also need to actually listen: a bindable port or
socket path that exists on every launch, benchmark or not, is attack
surface a normal user's VeloX process would carry for a feature only ever
used by a benchmark harness talking to itself on the same machine.

**Chosen instead: `VELOX_AUTOMATION_SCRIPT=<path>`, read exactly once at
startup, never listened for again.** If the environment variable is unset —
every normal launch — `browser::automation::parse_script` never runs at
all: zero added attack surface, zero added cost, matching `VELOX_DEBUG`'s
existing opt-in-by-environment-variable precedent in `app.rs` and
`VELOX_HOMEPAGE`'s in D40. When set, `app::run` reads the named file exactly
once, right after `AppState` is built and before the event loop starts
pumping, and parses it with `browser::automation::parse_script` — a pure
function that never panics, rejecting malformed input with a 1-based line
number (`AutomationError`) rather than running a partially-valid script. A
parse failure (bad line, missing file, unreadable file) is logged to stderr
and otherwise ignored, exactly like every other `log_failure`-style guard
in this file — never fatal, matching the project's "UI 系の失敗はクラッシュ
させず継続" rule. There is nothing to "connect to": no port, no socket path,
no listener thread — the script is consumed once, like a config file, not
served.

**Why a file (and a thread walking it) instead of, say, extra CLI flags**:
a flat command list needed conditional pacing (`wait <ms>` between steps,
so a webview has time to actually start a load before the next command
fires) and an explicit end (`quit`), neither of which map cleanly onto a
handful of `--` flags the way `--homepage`/`--private` do. A small
line-oriented format — closer to `docs/benchmarking.md`'s existing
`aggregate --input <path> --input <path> ...` precedent than to a new
flag-parsing surface — keeps `Config::from_env_and_args`'s no-new-CLI-crate
policy (D6) intact and stays trivially diffable/inspectable as a benchmark
artifact, which a wall of CLI flags would not.

**Command set is deliberately closed and tiny**: `open <url>` / `switch
<index>` / `close <index>` / `navigate <url>` / `wait <ms>` / `quit`, plus
`#` comments and blank lines. No expressions, no branching, no loops — this
is not a scripting language, it is a fixed, enumerable command set,
structurally incapable of expressing anything beyond "drive these specific
tab operations". `wait` is capped at `automation::MAX_WAIT_MS` (120s) so a
typo cannot stall a launched browser indefinitely. URLs go through the
exact same `browser::navigation::normalize_input` address-bar input does,
so a script cannot smuggle a `javascript:` URL or anything else the address
bar itself would reject — the automation file gets no more trust over what
URL it can load than a user typing into the omnibox would.

**Delivery mechanism — proxied `UserEvent`s, not a new state-mutation
path**: per D20's layering rule, `browser::automation` (the parser, plus
`velox-bench`'s `generate_bench_script`/`needs_automation_script`/
`recommended_timeout_secs`) has zero `wry`/`tao`/`gtk` dependency and is
fully covered by `cargo test` with no display. Everything that actually
touches a webview lives in `src/app.rs`: a background thread
(`spawn_automation`) walks the parsed `Vec<AutomationCommand>`, sleeping
locally for `Wait` (never blocking the main thread — the UI keeps servicing
normally the whole time) and proxying every other command into the event
loop as `UserEvent::Automation`, using the same `EventLoopProxy::send_event`
fire-and-forget channel every existing webview callback
(`PageTitleResolved`, `NewTabRequested`, ...) already uses to reach the main
thread. `handle_automation_command` then resolves each command by calling
the *exact same* tab-management functions `handle_toolbar_command`/
`handle_content_shortcut` already call — `open_new_tab`, `close_tab`,
`Tabs::activate_at`, the shared `navigate_active_tab` (newly extracted from
`ToolbarCommand::Navigate`'s body so `AutomationCommand::Navigate` can reuse
it) — never a parallel implementation of "open a tab" or "switch tabs".
`Switch`/`Close` address a tab by its live tab-strip *position* (0-based),
resolved against `state.tabs` at the moment each command actually runs
(`tab_id_at`) rather than up front, since tabs opened/closed earlier in the
same script shift what position `N` means; an out-of-range position is a
logged no-op, matching every other `Tabs`/`BrowserWindow` guard in the
file. `AutomationCommand::Quit` is intercepted in `run`'s event loop before
dispatch (it needs `ControlFlow`, which `handle_user_event` does not have
access to) and sets `ControlFlow::Exit`, mirroring how
`WindowEvent::CloseRequested` is already handled at that same match site.

**`Scenario::is_unattended` now always returns `true`**: with the hook in
place, every scenario — not just the three startup ones — can be driven
without a human, so the method's body collapsed from a three-variant
`matches!` to a `match` covering every variant with the same `true`. It is
kept as a real `match` (not a bare `true`) specifically so a future
scenario that genuinely cannot be scripted has one obvious place to say so,
rather than requiring someone to remember to special-case it somewhere
downstream.

**`velox-bench run` generates a script per scenario, not per user**:
`browser::automation::generate_bench_script(scenario, url)` is a pure
function (tested without a display) that builds the right command sequence
for each newly-unattended scenario — e.g. `tabs_N` opens `N - 1` extra tabs
(one already exists at the homepage) then `wait`s long enough for the RSS
sampler to take a couple of samples with all tabs present; `tab_switch`
opens a handful of extra tabs then round-robins `switch` across them;
`navigation` repeatedly `navigate`s to the same fixed page with a
cache-busting query parameter (`?velox-bench-step=N`) so each hop is a
genuinely distinct navigation event rather than a same-URL no-op — and
always ends with `quit`. Because every generated script ends with `quit`,
`velox-bench run`'s trial loop (`wait_for_exit_or_timeout`) polls the child
with `try_wait` instead of always sleeping a fixed `--warmup-secs`: a
scripted trial normally exits on its own well before the timeout, and only
a scenario with no script (or one that never reaches `quit`) still waits
out the full timeout — the pre-#112 fixed-`sleep`-then-kill behavior,
preserved unchanged for `cold_startup`/`warm_startup`/`first_page_load`.
The per-scenario default timeout itself
(`automation::recommended_timeout_secs`) is a small pure estimate (rough
per-step overhead plus each scenario's own explicit `wait`s) — a caller can
still override it with `--warmup-secs` on a slower machine.
`--url` becomes **required** (not just recommended-with-a-warning, as it
already was for the startup scenarios) for every scenario that needs a
script, since there is no reproducible "measure whatever the default
homepage is" fallback for "open N tabs at some page" the way there
arguably almost is for "load one page and wait".

**Verified on real hardware-less Xvfb, not just unit tests**: unlike D21's
original `run`, which shipped with `navigation`/`tab_create`/`tab_switch`/
`tabs_N` scaffolded but never actually exercised end-to-end (see D21 and
the old `docs/benchmarking.md` "この環境での検証状況"), this issue's
`Xvfb`-based run actually drove `tabs_5`/`navigation`/`tab_create`/
`tab_switch` through `velox-bench run` and got back real, scenario-shaped
metrics (`tab_create_ms`/`tab_switch_ms`/`page_load_ms` with plausible `n`
counts) — see `docs/benchmarking.md`'s "この環境での検証状況" for the actual
numbers and their caveats (software rendering, low trial count, not a
performance baseline).

## D45: プロファイリングは既存の外部ツール + `browser::metrics` の組み合わせとし、常設の計測コードは足さない

**対象**: Issue #70 (CPU / Memory Profiling Workflow)。「性能問題を見つけた
あと、どこのコードが原因かまで掘る手順」がリポジトリに無かった問題への対応。
成果物は `docs/profiling.md` と `scripts/profile/` (`pss_sampler.py` /
`run_perf.py` / `run_heaptrack.py` / `flamegraph.py`)。詳細な検証記録は
`docs/profiling.md` §9 を参照 — ここでは「何を選び、なぜか」の決定理由だけを
記録する。

**なぜ VeloX 自身にプロファイリング用のコードを足さなかったか**: `perf`
(CPU) と `heaptrack`/`valgrind --tool=massif` (メモリ) はどちらもプロセスの
外側から動的にアタッチできるツールで、対象バイナリに埋め込みの計装を要求
しない (debug symbols さえあれば十分)。VeloX 自身にサンプリングプロファイラ
やアロケータフックを組み込む選択肢もあったが、(1) 常設の計装は
`docs/decisions.md` D19 が `browser::metrics` について既に立てている方針
(「計測オフ時は追加コストゼロ、オンでも最小限」) をさらに複雑にする、
(2) `perf`/`heaptrack` が持つコールスタックのシンボル解決・折り畳み・
差分表示といった機能を車輪の再発明することになる、(3) 「新規 Rust 依存を
避ける」(`CLAUDE.md`) 制約の中でアロケータフックを自前実装するのは高コスト
で、既に確立されたツールが Linux に存在する以上その労力を正当化できない。
`browser::metrics` (D16/D19/D42) は既に「VeloX が常時知っておくべき数値
(起動時間の内訳、定期 RSS/PSS)」を`velox-bench`のベンチマーク結果に残す
役割を担っており、これは今回も変更していない — 外部ツールは
`browser::metrics` の**代わり**ではなく、`browser::metrics` が答えない
「どの関数/どのアロケーションが原因か」を埋める**補完**として選んだ。
`docs/profiling.md` §3 の使い分け表はこの役割分担をそのまま反映している。

**CPU: `perf`、`cpu-clock` イベントを既定に**。当初は `perf stat -e
cycles,instructions` のようなハードウェアイベントを想定していたが、実測で
このコンテナ (Firecracker 相当の仮想化環境) はハードウェア PMU
がゲストに渡されておらず `<not supported>` になることが分かった
(`docs/profiling.md` §1.2)。ソフトウェアイベント `cpu-clock` (PMU 不要、
一定間隔で「その時点で CPU 上にいるか」をサンプリングする) に切り替えたところ
記録・シンボル解決とも動作した。**この選択はサンドボックス環境固有の
制約への対応であり、実機ではハードウェアイベントの方が精度が高いので
そちらを優先すべき** — `run_perf.py --event cycles` で切り替えられるように
しており、決め打ちにしていない。また `/usr/bin/perf` がこのコンテナの
カスタムカーネルバージョンに対応する `linux-tools` パッケージが存在せず
即座に失敗する問題も実測で確認し (`docs/profiling.md` §1.1)、
`linux-tools-generic` が入れる別バージョンの `perf` バイナリへの
フォールバックを `run_perf.py` に組み込んだ。

**メモリ: `heaptrack` を主、`valgrind --tool=massif` を副に**。両方このコンテナ
に実際にインストール・実行して比較した。`heaptrack` は `LD_PRELOAD` ベースで
オーバーヘッドが小さく、対象プロセスが正常に近い速度で動く。`massif` は
エミュレーション (Valgrind) ベースで大幅に遅くなる代わりに、`heaptrack` が
入らない環境 (この環境のように `apt` はあるがパッケージが引けない場合が
あり得る) でも動く保険として残す。**どちらも意図的に選んだわけではなく
「プロセス境界でアタッチする」という同じ性質を持つ**ため VeloX の Rust
heap と WebKitGTK 側の分離に同じ理由で使える (次項)。

**Rust heap と WebKitGTK 側の分離は新しい仕組みを作らず、プロセス境界に
乗った**。Issue #61 の受け入れ条件「Rust 側と WebView/renderer 側を可能な
範囲で分離して分析」に対し、`heaptrack <velox-binary>` を素朴に実行した
ところ、生成される記録ファイルが 1 個だけ (VeloX 本体プロセスの分のみ) で
あり、`WebKitWebProcess`/`WebKitNetworkProcess` 用のファイルは生成されない
ことを実測で確認した (`docs/profiling.md` §2.1)。WebKitGTK が子プロセスを
起動する際に環境をサニタイズし `LD_PRELOAD` を引き継がせていないためと
考えられる。これは意図して設計した分離ではなく、**既存のプロセスモデル
(D1: WebKitGTK はマルチプロセス) と `heaptrack` の実装 (`LD_PRELOAD` は
プロセス単位) が組み合わさって自然に得られた副産物**であり、そのため
VeloX 側のコード変更は一切不要だった。CPU 側も同じ理屈で、`perf report
--comms=<binary名>` によるコマンド名フィルタで同じ分離ができることを確認
済み。macOS (Instruments が対象プロセスを選ばせる)・Windows (WPA/VS
プロファイラも対象プロセスを選ばせる) でも同じプロセス境界が成り立つはずだが
未検証 — `docs/profiling.md` §7 の実機チェックリストに含めた。

**フレームグラフは自前の SVG 生成スクリプトを新規に書いた**。定番の
`stackcollapse-perf.pl`/`flamegraph.pl` (Brendan Gregg 版) はこの環境の
外部ネットワーク遮断のもとでは `apt`/`pip`/`git clone` いずれでも入手でき
ない (crates.io・github.com とも到達不可であることを実測で確認 — 403/400)。
実行できない前提のツールを手順書に書いても再現できないため、
`scripts/profile/flamegraph.py` として Python 標準ライブラリのみで
「`perf script` の折り畳み → SVG 描画」を実装した。副次的な利点として、
VeloX 自身のシンボル (`velox::` 接頭辞) を青系、WebKit/JSC 系を緑系、
その他 (glib/gtk/libc) を橙系に色分けする、この文書の分離方針をそのまま
可視化に反映させる配色を追加できた (本家 flamegraph.pl にはこの区別は
無い)。折り畳み済みテキスト出力 (`--collapsed-only`) は
`stackcollapse-perf.pl` と同じ行形式 (`"stack;stack;... count"`) にして
あるので、将来インターネットが使える環境で本家 `flamegraph.pl` や他の
可視化ツールに繋ぎたくなった場合の互換性は残している。

**ビルド設定: `strip = true` の `[profile.release]` は変更せず、新しい
`[profile.profiling]` を追加した**。`perf`/`heaptrack`/`massif` はいずれも
debug symbols が無いと関数名どころかソース行まで一切読めない
(生アドレスしか出ない) が、`[profile.release]` の `strip = true` は
D6 以来意図的な選択であり、通常の配布物のサイズと起動性能を保つために
変えるべきではない。`inherits = "release"` で最適化レベル・`lto` を release
と揃えつつ `debug = true` / `strip = false` だけを乗せた別プロファイルに
することで、`target/profiling/` という別ディレクトリに出力させ、
`cargo build --release` の成果物 (`target/release/velox`) には一切影響しない
ことを実測で確認した (strip 済み 1,984,704 バイトのまま、BuildID も
変化なし)。新しい Rust 依存クレートは追加していない。

**検証**: `docs/profiling.md` §9 に、このコンテナで実際に実行して確認した
コマンドと出力を記録した (perf record/report、heaptrack、massif、
`pss_sampler.py`、`run_perf.py`/`run_heaptrack.py` の一括実行、生成された
SVG が妥当な XML であることの確認を含む)。macOS/Windows の手順と、実機
(GPU あり) での数値は未検証であり、`docs/profiling.md` にもその旨を明記
している — 実機再測定は本 Issue のスコープではなく、同文書 §7 の
チェックリストとして次の作業に引き継ぐ。

## D46: Performance Regression Gate (#72) — 2 段階閾値 + 絶対差フロア + 多数決、baseline は同一ジョブ内でその場作成

**Scope**: Issue #72（#36 の性能回帰検知を Phase 3 の実運用レベルへ発展させた
もの）。#59（D43）が実測で明らかにした「単一中央値を固定閾値と突き合わせる
判定ルールはこの環境では成立しない」という課題に対する具体的な設計と実装。

### 実測したノイズ (この Decision の設計根拠)

`docs/performance-targets.md` §9 の -19.0%/-22.4% という 1 件の観測だけでは
判定方式を設計するのに足りないと考え、本 Issue の作業として追加のノイズ計測を
行った。**同一リリースビルド・同一コミット (`b81b1fc08cd73256a15a4ffb4bdafb93745e799e`)・
コード変更なし**で、`cold_startup` シナリオを 10 試行 × 6 セット、連続実行した。

```sh
cargo build --release
(cd scripts/bench/pages && python3 -m http.server 8751 &)
for i in 1 2 3 4 5 6; do
  xvfb-run -a --server-args="-screen 0 1280x900x24" \
    ./target/release/velox-bench run --scenario cold_startup --trials 10 \
    --url http://127.0.0.1:8751/minimal.html --output /tmp/noise/set$i.json
done
```

各セットの中央値 (ms / bytes):

| set | `startup_window_created_ms` | `startup_toolbar_ready_ms` | `startup_first_load_ms` | `pss_total_bytes` | `rss_total_bytes` |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 258.60 | 596.55 | 638.15 | 161,976,320 | 315,203,584 |
| 2 | 249.90 | 591.65 | 624.65 | 180,518,912 | 359,311,360 |
| 3 | 222.50 | 496.85 | 505.90 | 130,053,632 | 377,419,776 |
| 4 | 230.30 | 457.30 | 487.80 | 116,281,856 | 370,688,000 |
| 5 | 190.20 | 402.00 | 407.20 | 149,772,288 | 297,897,984 |
| 6 | 187.40 | 388.80 | 418.80 | 149,641,216 | 309,852,160 |

**このマシンは他セッションと共有されており、実行のたびに ± の負荷が乗る**
（6 セットは合計で約 9 分かけて連続実行しており、外部要因を意図的に変えては
いない）。これを 2 通りに集計した:

1. **隣接セット同士の比較**（`set1↔set2`、`set2↔set3`、… の 5 ペア。CI が
   同一ジョブ内で baseline/candidate を数分の間隔を空けて計測する状況に近い）:

   | metric | 隣接ペアの最大変化率 |
   | --- | ---: |
   | `startup_window_created_ms` | 17.4% |
   | `startup_toolbar_ready_ms` | 16.0% |
   | `startup_first_load_ms` | 19.0% |
   | `pss_total_bytes` | 28.8% |
   | `rss_total_bytes` | 19.6% |

2. **set1 と set6 の比較**（約 9 分離れた 2 点。コミット済みの古い baseline
   ファイルと「今」の測定を比較する状況に近い）:

   | metric | set6(基準)→set1 | set6(基準)→set2 |
   | --- | ---: | ---: |
   | `page_load_ms` (21.05→) | +77.0% | +78.9% |
   | `startup_window_created_ms` (187.40→) | +38.0% | +33.4% |
   | `startup_toolbar_ready_ms` (388.80→) | +53.4% | +52.2% |
   | `startup_first_load_ms` (418.80→) | +52.4% | +49.2% |
   | `pss_total_bytes` (149,641,216→) | +8.2% | +20.6% |
   | `rss_total_bytes` (309,852,160→) | +1.7% | +16.0% |

**結論**: コードを一切変更していないのに、隣接セット間だけでも最大 28.8%、
測定の間隔が数分開くだけで `page_load_ms` は最大 78.9%、
`startup_toolbar_ready_ms` は最大 53.4% 動く。`page_load_ms` の 78.9% は
絶対値では 21.05ms→37.65ms、わずか 16.2ms の変化にすぎない — 相対閾値だけ
では小さい絶対値の指標を正しく扱えないことも同時に分かった。

（コマンド・生の JSON は `/tmp/noise/set{1..6}.json` として作業時に確認した
ものであり、この PR には含めていない — 再現手順は上記コマンドのとおり。
`results/baseline/cold_startup-linux-xvfb.json` として commit したのは、
このうち別途 15 試行で採り直した 1 本。）

### 採用した判定方式

`src/browser/benchmark.rs::evaluate_gate`（純粋 Rust、`browser::benchmark`
の既存方針（D21）を踏襲し `cargo test` で完全に検証）に実装した。

1. **2 段階の重大度 (`Severity::{Ok,Warn,Fail}`)**: `warn_pct`(既定 20.0) と
   `fail_pct`(既定 60.0) の 2 つの相対閾値。`fail_pct` は隣接セットの最大
   ノイズ (28.8%, PSS) に約 31pt、set1/set6 間の最悪ノイズのうち絶対差
   フロアで吸収できないもの (53.4%, `startup_toolbar_ready_ms`) にも
   約 7pt のマージンを残す水準に設定した。`warn_pct`(20.0) は隣接セットの
   起動系メトリクスのノイズ (16.0〜19.0%) とほぼ同水準に置いており、
   「これくらいはこの環境では日常的に動く、人間が一瞥する値」という位置
   づけにしている。
2. **メトリクスごとの最小絶対差 (`MetricKey::min_significant_delta`)**:
   相対閾値とは独立な安全弁。`page_load_ms` のように絶対値が小さい指標は
   20ms 未満の変化を無視する（`page_load_ms` の実測 +78.9% は 16.2ms の
   変化だったので、このフロアで吸収される）。メモリ系は 5MiB、プロセス数
   系は 1 プロセスをフロアとした。
3. **複数候補測定の多数決**: `evaluate_gate` は baseline 1 つに対し
   candidate を複数 (`&[&BenchmarkResult]`) 受け取れる。**過半数の
   candidate が独立に `fail_pct` を超えたときのみ** その指標を Fail とする
   （1 候補なら 1/1 必要、2 候補なら 2/2 必要、3 候補なら 2/3 で成立）。
   これは「連続 N 回悪化して初めて fail」という Issue 側の候補案を、PR
   履歴を跨がず 1 回の CI ジョブ内で近似する実装である。
4. **試行数不足の検出**: baseline/candidate のいずれかの `Stats::count` が
   `MIN_TRIALS_FOR_CONFIDENT_GATE`(5) 未満なら `low_confidence` を立て、
   その指標は多数決の結果に関わらず Fail に昇格させない（Warn 止まり）。

固定閾値 1 本 + 単発比較という素朴な方式（当初 #59 が指摘した「成立しない」
方式そのもの）を採らなかった理由は、上の実測データが直接示すとおり — この
環境ではその方式は必ずどちらか一方で壊れる (閾値を低くすれば通常運転でも
false fail、高くすれば実際の劣化を見逃す)。2 段階 + 絶対差フロア + 多数決の
組み合わせは、単一の数字ではこの環境のノイズ分布 (小さい絶対値の指標は
%が暴れる／隣接ペアと広い間隔とでノイズの桁が違う／短時間に相関したノイズが
乗ることがある) を吸収しきれないという実測結果から導いた。

**正直に書く残存リスク**: `fail_pct`(60%) と set1/set6 間で実際に観測した
`startup_toolbar_ready_ms` の最悪値 (53.4%) との差はわずか 7pt しかなく、
統計的な閾値だけでこの環境の最悪ノイズを完全に吸収できているとは言えない。
この残差を吸収しているのは閾値の値そのものではなく、**baseline と
candidate を同一 CI ジョブ内で数分以内に連続測定するという設計**（下記）
である — 詳しくは `docs/performance-targets.md` §10 も参照。実測で
Fail が出て、直前の変更に妥当な原因が見当たらない場合は、まず
`perf-gate.yml` を再実行する運用を推奨する (flaky test の再実行と同じ扱い)。

### CI で GUI ベンチを回す/回さない判断

**回す。** `.github/workflows/perf-gate.yml` を新設し、`libwebkit2gtk-4.1-dev`
と `xvfb` を導入したうえで、実際に `velox-bench run`（Xvfb 経由）を実行する。
Issue #106 と本 Issue の実測で、この方式の Xvfb 環境で `cold_startup` が
確実に計測できることは確認済みであり、GUI を回さない代替 (純粋ロジックの
マイクロベンチ、コンパイル時定数、バイナリサイズ等) では T1 が対象とする
`startup_*`/`page_load_ms` そのものを測ることができない — それらの指標を
支配しているのは D43 が明らかにしたとおり `tao`/GTK の初期化と WebKitGTK の
webview 生成であり、Rust 側の純粋ロジックには現れない。コストは同一ジョブ内
で 2 回ビルド + 3 回計測 (baseline 1 回 + candidate 2 回、Xvfb 込みで見積もり
数分) だが、`ci.yml` 本体とは別ワークフローに分離し、通常の fmt/clippy/test
のフィードバック速度には影響させない。

### baseline: 固定ファイルではなく同一ジョブ内でその場作成

**`results/baseline/cold_startup-linux-xvfb.json`（この dev/agent コンテナで
採取。commit `b81b1fc` 紐付け、§7 の形式）は、CI のブロッキング判定には
使わない。** 上記の実測（set1/set6 間で無変更バイナリが最大 78.9% 動く）が
示すとおり、機械やセッションが変わる比較はこの環境では意味をなさない ——
`docs/performance-targets.md` §1 が最初から明記している「異なる日・異なる
マシンで取った数値を並べて比較しない」という制約が、GitHub Actions の
ランナー (ローカルよりさらにノイズが大きいと予想される共有環境) と
コミット済みファイルの間には確実に当てはまる。

そこで `perf-gate.yml` は baseline も **同一ジョブ・同一ランナー内で** その場
作成する: PR の merge-base コミットをチェックアウトしてビルドし、その
バイナリを baseline として計測してから PR head に戻る。baseline と
candidate の間の時間差は「ビルド 1 回分」程度に収まり、上記の「隣接セット」
ノイズ (最大 28.8%) に近い条件になる。コミット済みの `results/baseline/`
ファイルは、経時トレンドを人が目視で追うための参考情報としてのみ残す
(`docs/benchmarking.md` §5 参照)。

### 終了コードの設計

`velox-bench gate` の終了コードは `0`=OK, `3`=WARN(非ブロッキング),
`1`=FAIL(ブロッキング), `2`=引数エラー等（既存サブコマンドと同じ規約）。
`compare` の 0/1 の 2 値ではブロッキングと非ブロッキングを CI 側で区別
できないため、`gate` では意図的に別の値を割り当てた。`perf-gate.yml` は
`FAIL` のときのみジョブを失敗させ、`WARN` は Job Summary に出すだけで
ビルドを止めない。

### #36 との関係

#72 は #36 の受け入れ条件（PR でベンチマーク実行・baseline との差分確認・
閾値設定・環境差による誤検知の考慮）を全て満たし、かつ #36 が要求していな
かった「複数回測定」「多数決」「試行数不足検出」まで実装したため、**この
PR で #36 と #72 の両方を close する。**

**Cost / revisit condition**: `warn_pct`/`fail_pct` は実測に基づく初期値
であり、GitHub Actions 実ランナーでの運用実績（ローカルより大きいノイズが
出る可能性が高い、`docs/benchmarking.md` の既存の記述どおり）が蓄積したら
見直すこと。現状は `cold_startup` シナリオのみを CI でゲートしており、
`Scenario::is_unattended()` が `true` の残り 2 シナリオ (`warm_startup`,
`first_page_load`) や、外部駆動フックが未実装の `navigation`/`tab_*`/
`tabs_N` 系は対象外 — 将来 Issue でそれらの自動駆動フックが実装されたら
`perf-gate.yml` に追加を検討する。

### CI 環境での実行条件 (実測により確定)

このゲートを GitHub Actions で動かすには **`dbus-run-session` が必須**である。
無いと VeloX は `BrowserWindow::new` の中でブロックし、クラッシュもせず perf
ログを 1 行も書かないままタイムアウトする。WebKitGTK は web process を別
プロセスとして起動し UI プロセスとの IPC に D-Bus を使うため、セッションバスが
無いと子プロセスが起動できず、UI プロセスが待ち続ける。

**この結論に至るまでに 3 つの仮説を実測で棄却した。** 同じ症状で再度同じ道を
辿らないよう記録しておく。

1. 使えない `/dev/dri` と DMABuf レンダラ — ソフトウェアレンダリングを強制
   しても、しなくても同じように失敗した
2. bubblewrap サンドボックスと非特権ユーザ名前空間 —
   `kernel.apparmor_restrict_unprivileged_userns=0` で `unshare -U` が成功する
   状態でも失敗した
3. WebKitGTK のサンドボックスそのもの — `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1`
   でも失敗した

**推論上の教訓**: 当初 D-Bus 仮説を「開発用コンテナでも
`DBUS_SESSION_BUS_ADDRESS` は未設定だが動作する」という理由で早々に棄却したが、
これは誤りだった。ローカルには AT-SPI の dbus エラーがそもそも出ておらず、
両環境の dbus の状態は同一ではない。**「ローカルで X 無しに動く」ことは
「CI の失敗が X と無関係」を意味しない。**

決め手になったのは環境の推測をやめてプロセスを直接観測したことである。有効な
観測点は `docs/benchmarking.md`「実行環境要件」の表を参照。とくに
**perf ログが「空」ではなく「一度も作られない」**という区別が、`app.rs` の
`build_perf_log` が `event_loop.run` より後ろにある事実と合わさって、ブロック
位置を `BrowserWindow::new` に特定する決め手になった。

## D47: Integration Test 基盤 (#34) — `VELOX_AUTOMATION_SCRIPT` を駆動機構に再利用し、GUI 不可環境は実行時判定でスキップ

**対象**: Issue #34。`cargo test` の 474 件 (本 Issue の作業開始時点) はすべて
`src/browser/` の純粋ロジックに対する単体テストで、実際に `velox` バイナリを
起動する経路が 1 つも検証されていなかった。これは仮説上の欠落ではなく実害と
して観測済みだった — 直前の Issue #72 (D46) では、**474 テストが全緑のまま、
CI 上の VeloX は `BrowserWindow::new` から先へ進まず起動すらしていなかった**
(D-Bus セッションバスの欠如)。クラッシュせず、perf ログを 1 行も書かず、ただ
無応答になるという壊れ方であり、既存の単体テストの守備範囲 (`src/browser/`
の純粋ロジック) には原理的に入らない。成果物は `tests/integration.rs`
(Cargo の統合テスト、`cargo test` で自動実行される) と、それが `browser::
gui_probe::gui_probe_reason` として使う 1 つの純粋関数
(`src/browser/gui_probe.rs`)。

### 駆動機構: 新しい制御チャネルを作らず `VELOX_AUTOMATION_SCRIPT` を再利用する

Issue #112 (D44) で導入済みの `VELOX_AUTOMATION_SCRIPT` — 起動時に一度だけ
読み込まれる読み取り専用のスクリプトファイルで、`open`/`switch`/`close`/
`navigate`/`wait`/`quit` を既存の `UserEvent` ディスパッチへ流し込む —
をそのまま統合テストの駆動機構として使う。D44 が listening socket/RPC
サーバを明確に却下した理由 (D18/D23 の IPC 信頼境界 — 構造化コマンドを外部
プロセスから受け付けられるのはトップレベルの信頼されたツールバー webview
だけ、という原則) は本 Issue でも変わらない。**統合テスト専用の別チャネルを
新設することは、D44 がわざわざ避けた「常設の待ち受け口」を、テストという
名目でもう一つ増やすことに等しく、採らなかった。** `tests/integration.rs`
は `velox-bench run` が `browser::automation::generate_bench_script` で
自動生成するのと同じ書式のスクリプトを、テストごとに手書きして
`VELOX_AUTOMATION_SCRIPT` に渡す — `velox-bench` と統合テストは同じ入り口を
共有する 2 つの独立した利用者であり、どちらも本番コードに一切変更を要求
しない (実際、本 Issue で `src/app.rs`/`src/browser/automation.rs` に変更は
無い)。

### GUI が無い環境は実行時判定でスキップする (`#[ignore]` ではなく)

**要求は「開発者のマシンや GUI の無い CI で `cargo test` が赤くなっては
いけない」。** `#[ignore]` はコンパイル時に固定される属性であり、「この
実行で Xvfb/D-Bus が実際に使えるかどうか」というランタイムの状態を反映
できない (CI 側ではこの統合テストを常に有効化して実行したいが、それ以外の
実行環境では動くとも動かないとも決め打てない)。そこで各テストは冒頭で
実際の環境変数を見て判定し、起動を試みる前に穏当にスキップする
(`skip_without_gui!` マクロ、成功終了・stdout に理由を出力するだけで
`#[ignore]`/failure のどちらでもない)。

判定ロジックは 2 つの層に分けた (D20 の層分離を踏襲):

- **純粋な決定関数** `browser::gui_probe::gui_probe_reason(has_display:
  bool, has_dbus_session: bool) -> Option<&'static str>` — 実際の環境変数を
  一切読まず、真偽値を受け取って「起動を試みるべきでない理由」を返すだけ。
  `cargo test` から (ディスプレイの有無に関わらず) 完全にカバーされる、
  `src/browser/gui_probe.rs` の単体テスト対象。
- **実際の環境読み取り** は `tests/integration.rs` 側 (`gui_skip_reason`) —
  Linux では `DISPLAY`/`WAYLAND_DISPLAY` と `DBUS_SESSION_BUS_ADDRESS` を
  読み、`gui_probe_reason` に渡す。macOS/Windows は通常のデスクトップ
  セッションが GUI アプリを動かせることを前提に無条件で `None` (スキップ
  しない) — この 2 プラットフォームは本 Issue の実機確認対象外 (VeloX
  自体の CI が Linux のみ、`docs/benchmarking.md`) だが、判定を "Linux 以外
  は常に試す" にしておくことで、将来 macOS/Windows CI が追加されたときに
  この統合テストも自動的に有効になる。

**この 2 つの環境変数を選んだ理由**は `docs/benchmarking.md`「実行環境要件」
と D46 の実測そのもの: `DISPLAY`/`WAYLAND_DISPLAY` が無ければ WebKitGTK の
ウィンドウ自体が作れず、`DBUS_SESSION_BUS_ADDRESS` (`dbus-run-session` が
設定する) が無ければ D-Bus 無しで WebKitGTK の web process が起動できず
#72 と同じ無応答になる。どちらも「試す前から結果が分かっている」状況を
検出するための最小限のシグナルであり、`gui_probe_reason` 自身のドキュメント
コメントが明記する通り、両方揃っていることは「動作を保証」しない —
それ以外の失敗モードまで先回りして検出することは意図的にスコープ外とした。

### 何を保証し、何を保証しないテストなのか

`tests/integration.rs` の 4 テストはいずれも実際に `velox` バイナリを
起動し、外形から観測する (D19 の "browser logic を UI/engine から分離する"
方針の裏返しとして、この統合テストは意図的に UI/engine を含む全体を
外側から見る):

1. **起動完了** (`startup_completes_and_records_a_startup_event`) — #72 の
   壊れ方そのものへの回帰テスト。`VELOX_PERF_METRICS=1`
   `VELOX_PERF_OUTPUT=<path>` で起動し、`startup` perf レコードが実際に
   1 件書かれることを検証する。プロセスが自発終了しない場合
   (`Child::try_wait` がタイムアウトまで `None` を返し続ける場合) は
   明示的に `panic!` させ、テストヘルパがタイムアウト後に kill して
   "成功" 扱いにすることは一切しない — この区別 (`exit_status: Option<
   ExitStatus>` が `None` かどうか) がこのテストスイート全体の要である。
2. **タブ操作** (`tab_operations_produce_expected_tab_create_and_tab_switch_
   records`) — 自動操作スクリプトで複数タブを開き・切り替え・閉じ、
   `tab_create`/`tab_switch` perf レコードの件数と `tab_id` の distinctness
   を検証する。`browser::automation::parse_script` 自体のパース網羅性は
   既に `automation.rs` の単体テストが持っているので、ここで検証したいのは
   「パースされたコマンドが実際に `app::handle_automation_command` →
   `Tabs`/`BrowserWindow` まで届くか」だけである。
3. **永続化** (`visiting_pages_persists_history_json`) — `VELOX_DATA_DIR`
   を一時ディレクトリに向けてページを 2 つ訪問し、`history.json` が
   書かれ、`browser::persistence::load_history` で読み戻した内容
   (URL の並び、`visited_at`、`visit_count`) が妥当であることを検証する。
   `src/browser/history.rs` の単体テストは `HistoryStore` を直接叩くだけで
   ファイル I/O も webview も経由しないため、ここが唯一の end-to-end 経路。
4. **`quit` による自発終了** (`quit_command_exits_the_process_with_code_
   zero`) — タイムアウト kill と自発終了を区別する
   (`exit_status.is_some()` であることそのものを検証、`.success()` だけでは
   「タイムアウト後に kill されて `Some` になった」場合と区別できない —
   実際にはこのテストファイルの `launch_and_wait` はタイムアウト時に
   `exit_status: None` を返す設計なのでこの取り違えは起きないが、それでも
   `signal()` が `None` であることまで確認して「シグナルで終わっていない」
   ことを明示している) 上で、終了コードが 0 であることを検証する。

**保証しないもの**: トークン化された UI レンダリング (toolbar/omnibox の
見た目)、パフォーマンス数値そのものの妥当性 (`docs/benchmarking.md` が
別途扱う領域)、`VELOX_AUTOMATION_SCRIPT` の全コマンド・全異常系の網羅
(既存の `automation.rs` 単体テストの役割)、macOS/Windows での実機動作。
`docs/architecture.md`「テスト戦略」に単体テストとの守備範囲の違いを
まとめた。

### 固定ページは `file://`、ネットワークも loopback サーバも使わない

`scripts/bench/pages/*.html` を `file://` URL として読み込む。loopback HTTP
サーバ (`velox-bench`/`perf-gate.yml` が使う方式) ではなく `file://` を
選んだのは、統合テストが複数プロセスを並行して起動しうる中で **ポートを
一切使わなければ衝突の可能性そのものが無くなる**ため — サーバのポート
割り当て・多重起動時の再利用待ちといった調整が不要になる。`browser::
navigation::normalize_input` は `file` スキームを既に許可済み (アドレス
バー・`VELOX_AUTOMATION_SCRIPT`・`VELOX_HOMEPAGE` のいずれとも同じ経路) な
ので、この選択に本番コードの変更は要らない。

### 実機確認 (Xvfb + `dbus-run-session`)

```sh
cargo build --release
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- cargo test
```

上記で単体テスト 478 件 (474 + `gui_probe` の新規 4 件) と統合テスト 4 件が
全て緑になることを確認済み (統合テスト全体で約 6 秒)。GUI が無い素の
`cargo test` (この環境の既定シェル、`DISPLAY`/`DBUS_SESSION_BUS_ADDRESS`
いずれも未設定) では 4 件とも即座にスキップ (0.00 秒) され、478 件の単体
テストのみが実行されることも確認済み。

**わざと壊して検出できることも確認した** (`src/app.rs` を一時的に改変・
ビルド・実行し、確認後に元に戻す形で実施。差分はコミットしていない):

- `quit` の `ControlFlow::Exit` 設定を削除 (= quit が効かなくなる) →
  4 テスト全てがタイムアウト経由で `panic!` (「#72 と同じ無応答の可能性」
  という明示メッセージ付き) して失敗した。kill 後に静かに成功扱いになる
  ことはなかった。
- `mark_startup(..., mark_first_load_finished)` の呼び出しを削除 (= quit
  自体は正常に効くが `startup` レコードが二度と書かれなくなる) →
  起動完了テストだけが「`startup` レコードが 0 件だった」という具体的な
  assertion 失敗で落ち、他の 3 テストはタイムアウトではなく正常に完走
  した (quit 自体は壊していないため) — タイムアウト起因の失敗と、レコード
  内容起因の失敗が別々の理由で正しく区別されることも確認できた。

### スキップを CI では失敗に変える (`VELOX_INTEGRATION_REQUIRE_GUI`)

GUI を起動できない環境でのスキップは `cargo test` からは**ただの pass に
見える**。開発者のマシンに X セッションが無い場合はそれが正しい挙動だが、
**CI では正反対に危険**である。`xvfb-run` / `dbus-run-session` の設定が壊れたり
外されたりすると、統合テストが 4 件とも黙ってスキップし、ジョブは緑のまま
**このファイルが何も検証しなくなる**。

これは #72 の失敗そのものと同じ形をしている — 単体テストが全部通っている
一方で VeloX は起動すらしていなかった。緑であることと検証されていることは
別である。

そこで `VELOX_INTEGRATION_REQUIRE_GUI` を設けた。設定されているとスキップは
`assert!` による明確な失敗になる。`.github/workflows/ci.yml` の
テストステップはこれを設定するので、CI が統合テストのカバレッジを静かに
失うことはない。ローカルの素の `cargo test` は従来どおり穏当にスキップする。

実測で以下を確認済み。

| 条件 | 結果 |
| --- | --- |
| strict + D-Bus あり (CI と同条件) | 4 件実行して pass |
| strict + D-Bus 無し | 4 件とも FAILED |
| 素のローカル (DISPLAY 無し) | 4 件スキップして緑 |

あわせて、**テストが実際に壊れを検出することも実測で確認した。**
`src/app.rs` の `AutomationCommand::Quit` による `ControlFlow::Exit` を無効化
したところ、4 件すべてがタイムアウト経由で失敗した (110 秒)。確認後にソースは
元に戻してある。

## D48: メモリ超過の主因は WebKitGTK/Blink のエンジン差ではなく、VeloX 自身の webview/`WebContext` の使い方だった

**対象**: Issue #61 (「なぜ WebKitGTK ベースの VeloX が Blink より PSS で
重いのか」の切り分け。実装は含まない — 詳細な測定データ・再現手順は
`docs/memory-analysis.md`、`docs/performance-targets.md` §5/§11 参照)。

**背景の仮説 (#58 時点、実測前)**: `docs/performance-targets.md` §5 (#58)
は「VeloX 側のオーバーヘッドなのか、WebKitGTK と Blink の差なのか」を未解決
のまま残し、「後者ならエンジン側であり Epic #57 の原則上手が出せない」と
書いていた。#59/D43 が T3 で「VeloX 側で手が出せるはずだった区間が実は
エンジン側だった」という結果になっていたため、#61 でも同様に「エンジン側で
手が出せない」という結論になる可能性を排除せずに調査を始めた。

**実測結果はその逆だった**: 3 つの独立した測定が同じ結論を指した。

1. **プロセス別 PSS 内訳** (`scripts/profile/process_breakdown.py`、本
   Issue で新規追加): VeloX (1 タブ) は `velox` 本体 + `WebKitWebProcess`
   ×2 + `WebKitNetworkProcess`×2 の 5 プロセス構成で、合計約 416 MiB。
   webview は 1 タブぶんしか無いのに `WebKitWebProcess`/
   `WebKitNetworkProcess` が 2 個ずつあるのは、`docs/architecture.md` の
   D3 (「Browser chrome as an HTML toolbar in a second webview」) の設計
   どおり、VeloX が toolbar 用と content 用の 2 つの webview を常に同時に
   持つためである。
2. **VeloX 自身の Rust heap** (`heaptrack`、既存の D42/D45 の手法を使用):
   ピーク malloc heap は 32.08 MiB で、`minimal.html`/`dom_heavy.html`
   (DOM 要素 5000 個) の違いにも、1〜5 タブの違いにも一切依存せず完全に
   一致した (6 回の計測すべてで同じ値)。ツリー全体 PSS の 1〜8% に過ぎず、
   タブが増えるほど比率はさらに薄まる — Rust heap を削っても全体には
   ほぼ効かないことを確認した。
3. **エンジンだけの比較**: VeloX の Rust コードを一切含まない、GTK
   ウィンドウ 1 つ + `WebKitWebView` 1 つだけの最小 C プログラム
   (`webkit2gtk-4.1`/`gtk+-3.0` に直接リンク、使い捨て、リポジトリには
   含めていない。全文は `docs/memory-analysis.md` §5.3) を書いて計測した
   ところ、合計 PSS は約 296〜299 MiB (2 回計測) — **これは Chromium (1
   タブ、318〜328 MiB) より軽かった。** WebKitGTK というエンジン自体が
   Blink より PSS で重いという証拠は、この環境では見つからなかった。

**VeloX (415〜420 MiB) と単一 webview の WebKitGTK ベースライン (296〜299
MiB) との差 (約 116〜125 MiB) は、toolbar 用の 2 個目の webview 1 個分の
コストでほぼ説明がつく** — これは「webview を 1 個追加するごとに PSS が
どれだけ増えるか」を別途タブ数のスケーリング実験で測った値 (次項) とほぼ
一致する。

**タブ数を増やすとさらに悪化する** (`scripts/bench/tab_scaling.py`、本
Issue で新規追加。既存の `velox-bench run --scenario tabs_N` を使わなかった
理由は下記): `minimal.html` で 1/5/10/20 タブを計測すると、VeloX の 1 タブ
あたりの PSS 増分は約 92〜122 MiB (平均約 106 MiB/タブ) で、Chromium の
約 9.7〜10.1 MiB/タブ (平均約 9.9 MiB/タブ) の約 10.7 倍だった。20 タブでは
VeloX (2421.9 MiB) は Chromium (464.8 MiB) の約 5.2 倍になる。プロセス数は
VeloX が追加タブ 1 個ごとに正確に +2 (`WebKitWebProcess`+
`WebKitNetworkProcess` のペア)、Chromium は概ね +1 (共有ネットワーク/GPU/
zygote プロセスの上にレンダラ 1 個だけ追加) で、PSS の増分の差とちょうど
対応している。

**根本原因をソースコードで特定した**: VeloX は webview を作るたび
(toolbar 用に 1 回、content webview はタブごとに 1 回) に
`wry::WebViewBuilder::new()` を呼んでおり、`.web_context(...)` で既存の
`WebContext` を明示的に共有させている箇所はソース中どこにも無い
(`src/ui/window.rs`、`grep -rn "\.web_context(" src/` はゼロ件)。`wry`
0.56.1 の WebKitGTK バックエンド (`~/.cargo/registry/.../wry-0.56.1/src/
webkitgtk/mod.rs` 257〜268 行目) は `attributes.context` が渡されなければ
毎回新しい `WebContext` を作る。WebKitGTK は `WebContext` ごとに独立した
`WebProcess`/`NetworkProcess` のプールを持つため、**webview を作るたびに
新しい `WebContext` が生まれ、それがそのまま新しい `WebProcess`+
`NetworkProcess` のペアになる。** 実測でも、自動操作で webview を 6 個
(toolbar + content 5 個) にした状態で `WebKitWebProcess` がちょうど 6 個
生成されることを確認した。

**なぜ既存の `velox-bench run --scenario tabs_N` を使わなかったか**: 素直に
実行すると `pss_total_bytes` の中央値がタブ数に依らずほぼ一定 (134〜147
MiB) という明らかに誤った値になった。原因は `spawn_rss_sampler`
(`src/app.rs`) が `VELOX_PERF_RSS_INTERVAL_MS` の既定値
(`config::DEFAULT_PERF_RSS_INTERVAL` = 5000ms) で起動直後から即座にサンプ
リングを始めるループであるのに対し、`tabs_N` の自動操作スクリプト
(`browser::automation::generate_bench_script` の `TabCountMemory` 分岐) が
生成するシナリオ全体の所要時間が (`tabs_1`/`tabs_5` では) 5000ms 未満で
終わることが多く、「起動直後の 1 回目」のサンプルしか記録に残らないため
だった。これは #61 のコード変更ではなく既存の測定手法の限界の発見であり、
本 Issue では修正せず (メモリ削減以外のコード変更も本 Issue のスコープ外)、
代わりに `scripts/bench/tab_scaling.py` を新規に書いて計測した (VeloX には
`open` の間に明示的な `wait` を挟む自動操作スクリプトを渡し、Chromium には
コマンドライン引数に URL を複数渡してタブを開かせ、どちらも全タブ安定後に
1 回だけプロセスツリー PSS を採る)。`tabs_N` シナリオの PSS/RSS メトリクス
自体の改善は将来の Issue に委ねる。

**#59/D43 との対比が今回の核心**: #59 は「VeloX 側で手が出せるはず」と
思われていた区間が実測するとエンジン側 (tao/GTK 初期化、WebKitGTK の
webview 生成) だった。#61 は逆に、「エンジン差だろう」と予想されていた
PSS 超過分の大半が、実測すると VeloX 自身の実装 (wry への webview の作り
方) に起因していた。**どちらも「実測するまで分からない」という Epic #57
の原則そのものの実例であり、憶測で先回りして結論を出していたら両方とも
逆の判断をしていたことになる。**

**T2 (Chromium 比 +10% 以内) の扱い**: 「達成不能」と判定する根拠は今回の
実測には無い。toolbar/content 間 (非プライベートモード限定、後述) の
`WebContext` 共有は、Epic #57 の「エンジンをブラックボックスとして扱う」
原則に反しない (WebKit の内部を触るのではなく、wry への webview の作らせ方
を変えるだけ) 有望な方向として見つかった。ただし**これは実装・計測して
いない仮説**であり、#59/T3 のように「効果ゼロと判明したので見送る」のとは
違う — 逆に「効果があると確定した」わけでもない。次にやるべきことは:

1. **最優先**: `src/ui/window.rs` で toolbar と (非プライベートモードの)
   content webview に同じ `WebContext` を渡すよう変更し、
   `compare_browsers.py`/`tab_scaling.py` で before/after を計測する。
2. タブ間の `WebContext` 共有 (プールするかどうか、何個まで共有するか) は
   別の変更として検討する。WebKitGTK の related-view process pool が 1 つ
   の `WebContext` に対して実際に何個の `WebProcess` を使うかは未検証。
3. **プライベートモードでは同じ手が使えない可能性が高い**: `wry` は
   `.with_incognito(true)` のとき `attributes.context` を無視して毎回
   `WebContext::new_ephemeral()` を作る (wry 自身のドキュメントコメントが
   明言。既存の D15 が同じ事実を記録している)。VeloX は toolbar/content
   両方に `.with_incognito(config.private)` を渡しているため、
   `config.private == true` の間は webview ごとの `WebContext` 独立が
   wry 自身の設計であり、VeloX 側の呼び出し方を変えても (今のバージョンの
   wry では) 解消できない可能性が高い。これは Epic #57 が言う「エンジンを
   ラップするライブラリがブラックボックスで手が出せない」領域に該当し
   うる。実際に手が出せるかどうかは #62 で確認する。

**検証**: `docs/memory-analysis.md` に、このコンテナで実際に実行して確認
したコマンドと生データ (プロセス別内訳 3〜6 試行、heaptrack 6 条件、タブ
スケーリング 3 試行×4 タブ数、最小 WebKitGTK アプリ 2 試行) を記録した。
すべて中央値または範囲で報告し、単発の測定だけで結論を出した箇所は無い
(§7 にばらつきの一覧がある)。macOS (WKWebView) / Windows (WebView2) での
検証、50 タブでの計測、`heaptrack -p <PID>` による `WebKitWebProcess` への
直接アタッチ、`WebContext` 共有の実装・計測はいずれも未実施 —
`docs/memory-analysis.md` §7/§8 に明記した。

## D49: toolbar/タブ間で `WebContext` を共有 — 効果は部分的 (`NetworkProcess` は統合できたが `WebProcess` は残った)、実装は残す

> **追記 (D53)**: この共有により、Linux の非プライベートモードでは
> ダウンロードハンドラ (D28) が一度も呼ばれなくなっていた (toolbar webview
> が持つ wry の既定ハンドラが `decide-destination` を先に処理する)。
> 原因と修正は D53 を参照。

**対象**: Issue #118 (D48/#61 が「有望だが未検証」とした仮説の実装フェーズ。
Epic #57)。詳細な測定データ・再現手順は `docs/memory-analysis.md` §9。

**背景**: D48 は「`WebContext` を共有すれば `WebKitWebProcess`/
`WebKitNetworkProcess` の重複が減るはず」という仮説と同時に、2 つの未検証
の留保を明記していた — (1) 共有しても `NetworkProcess` (1 タブあたり約
17 MiB) しか消えず、支配的な `WebProcess` (同 155 MiB) は残る可能性がある、
(2) プライベートモードでは wry の制約で同じ手が使えない可能性が高い。
**「効果ゼロ」という結論もあり得る、その場合は変更を入れずに記録して終える**
というルールで着手した。

### 実装

`src/ui/window.rs` の `BrowserWindow` に `context: Option<wry::WebContext>`
を追加。`config.private == false` のときだけ `WebContext::new(None)` を
起動時に 1 つ生成し、toolbar と全タブの content webview がこの 1 つを
`WebViewBuilder::new_with_web_context(&mut context)` (wry 0.56.1 が唯一
提供する共有経路 — チェーン可能な `.web_context(...)` は存在しない、ソース
確認済み) 経由で共有して構築されるようにした。`config.private == true` の
ときは `context` を最初から `None` にする (`WebViewBuilder::new()` を使う、
従来どおり) — D15 のとおり wry は `.with_incognito(true)` で
`attributes.context` を無視して毎回 `WebContext::new_ephemeral()` を作る
ため、共有 context を渡しても無視されるだけであり、そもそも渡さない設計に
した。

### 実測結果: 2 つの留保はどちらも的中した

同一セッション内で before (`git stash` でこの変更を外してビルド) /
after (この変更を適用してビルド) を `scripts/bench/tab_scaling.py`
(1/5/10/20 タブ、各 3 試行) と `scripts/profile/process_breakdown.py`
(実プロセスの直接観測) で比較した。

**留保 (1) は的中した — `NetworkProcess` は統合できたが `WebProcess` は
残った。** toolbar+5 タブ (webview 6 個) を自動操作で開かせて実プロセスを
数えたところ、`WebKitNetworkProcess` は webview がいくつあっても常に
**1 個**に統合された一方、`WebKitWebProcess` は webview 1 個につき 1 個の
まま、まったく統合されなかった (6 個)。WebKitGTK の `WebContext` は
`NetworkProcess` の生成単位ではあるが `WebProcess` の生成単位ではない
(related-view process pool を明示的に使わない限り webview ごとに独立)、
ということが実測で確定した。

**留保 (2) も的中した — プライベートモードは変更の影響を一切受けなかった。**
`VELOX_PRIVATE=1` で toolbar+3 タブ (webview 4 個) を起動すると
`WebKitWebProcess`/`WebKitNetworkProcess` とも 4 個ずつ、完全に 1:1 の
まま。実装のとおり private では `context: None` を渡しているため wry の
`.with_incognito` パスがそのまま従来どおり動き、プロセス構成・PSS 特性
ともに変更前と同一だった。

**それでも「効果ゼロ」ではなかった — `NetworkProcess` の統合だけで
measurable な PSS 減少が出た。** 1/5/10/20 タブで PSS が -2.5%〜-11.0%
減少し、タブ 1 個あたりの増分プロセス数は +2→+1 (Chromium と同じ増分
パターン) に変わった。プロセス数の変化は trial 間で完全に決定的 (分散
ゼロ) であり、対照群として同時に測定した Chromium (このセッションでは
無変更) は ±1.5% 以内の変動しかなかった — VeloX 側の PSS 減少がこの環境の
測定ノイズ (`docs/memory-analysis.md` §7 が報告する最大 8.6%) の範囲内の
偶然ではなく、実装変更由来であると判断できる根拠である。

### T2 (Chromium 比 +10% 以内) は未達のまま

PSS の大半 (1 タブ時で全体の 74.4%、#61 §2.1) を占める `WebKitWebProcess`
が統合されなかったため、Chromium との差はほとんど縮まっていない: 1 タブで
+48.6%→+45.5%、20 タブで +428.1%→+402.2%。1 タブあたりの増分も約 107→約
101 MiB/タブとわずかに縮んだだけで、タブが増えるほど Chromium との差が
拡大し続ける傾向そのものは変わっていない。**この変更は「メモリ超過の主要因
を解決した」わけではなく「効果が実測で確認できる部分的な改善」である。**

### データ分離とプライベート/通常の混在

自作の cookie テストページで確認した (`docs/memory-analysis.md` §9.5):
通常モードの 2 タブは Cookie を共有する (`WebContext` 共有の意図通り、
実ブラウザと同じ正しい挙動)。プライベートモードは通常モードの永続データを
一切見ず、かつプライベート内の各タブも互いに共有しない (これは wry の
`.with_incognito` パスの既存の挙動であり、本変更が新たに導入したものでは
ない)。`BrowserWindow::context` は `config.private` に基づき起動時に一度
だけ決まる (D14: プロセス全体で固定) ため、共有 `WebContext` がプライベート
webview に渡る経路はコード上そもそも存在しない。

### startup/page load への影響なし

`cold_startup` シナリオを `velox-bench gate` (baseline 10 試行 + candidate
2×10 試行、同一セッション内、既定閾値 warn 20%/fail 60%、D46) で評価し、
総合判定 **OK** (全指標 OK)。`pss_process_count`/`pss_total_bytes` はむしろ
改善方向、`startup_*`/`page_load_ms` 系は悪化なし。

### 判断: 実装は残す

**#59/D43 (「効果ゼロと判明したので見送る」) とは異なる、第三の結果に
なった。** 効果は部分的だが measurable かつ決定的 (プロセス数の変化に
分散ゼロ) であり、対照群の Chromium が同時測定でノイズ範囲内に収まって
いたことから、偶然ではなく実装変更由来と判断できる。データ分離を壊さず、
プライベートモードに影響を与えず、startup/page load を悪化させず、
統合テスト・単体テストが全て通ることも確認した。Epic #57 の「ベンチマーク
なしの最適化をしない」は「効果が無ければ入れない」であって「効果が部分的
なら入れない」ではないため、実装を revert する理由はないと判断した。

**Revisit condition**: T2 達成には `WebKitWebProcess` 自体の共有が必要。
wry 0.56.1 は `WebViewBuilderExtUnix::with_related_view(webview:
webkit2gtk::WebView)` という別の API (`WebContext` 共有とは独立) を公開
しているが、`webkit2gtk::WebView` という wry の外側の型を要求するため、
VeloX が現状 `wry::WebView` しか保持しない設計 (D20 の層分離) を崩さずに
使えるかは未検証。WebKitGTK の related-view process pool の一般的な挙動
(#61 §5.4 が未検証としていた点) も含め、次の Issue で検証することを
推奨する (`docs/memory-analysis.md` §9.9)。



## D50: `tabs_N` の PSS/RSS サンプル不足 (#119) — サンプリング間隔の自動短縮 + サンプル数不足の明示的な警告

**対象**: Issue #119。D48 (#61) が「別スクリプト (`scripts/bench/tab_scaling.py`)
で回避した」とだけ記録し、修正を先送りしていた欠陥そのものを本 Issue で
修正する: `velox-bench run --scenario tabs_1|tabs_5|tabs_10|tabs_20|tabs_50`
が記録する `pss_total_bytes`/`rss_total_bytes` が、タブ数を変えてもほとんど
変化しない。

**原因 (D48 が発見済みだった内容の再確認)**: `spawn_rss_sampler`
(`src/app.rs`) は起動直後に 1 回サンプルを取ってから
`VELOX_PERF_RSS_INTERVAL_MS` (既定 5000ms, `config::DEFAULT_PERF_RSS_
INTERVAL`) 間隔でループするだけの単純なスレッドである。一方
`browser::automation::generate_bench_script` の `TabCountMemory` 分岐は
`open` を待ち時間なしで連続実行し、全タブを開き終えてから
`MEMORY_STABILIZE_MS` (3000ms) だけ待って `quit` する。`tabs_1`/`tabs_5` の
ようにシナリオ全体が 5000ms 未満で終わることが多く、この場合サンプラの
「起動直後の 1 回目」のサンプル (まだタブが 1 つも開いていない状態) しか
記録に残らない。**一見それらしい数字が出るため、気づかずに誤った結論を
出す危険がある**のが最も悪い点で、#34/#72 と同じ「緑なのに何も検証して
いない」構造の問題である。

**実測で再現した (このコンテナ、`minimal.html`、3 試行、修正前バイナリ)**:

| シナリオ | `pss_process_count` (median) | `pss_total_bytes` (median, MiB) |
| --- | ---: | ---: |
| `tabs_1` | 5 | 147.1 |
| `tabs_5` | 5 | 98.4 |
| `tabs_10` | 5 | 142.8 |
| `tabs_20` | 5 | 138.2 |

`pss_process_count` が常に 5 (= toolbar + content 1 タブぶんの webview 構成、
D48 §2.1 参照) のまま動かないことから、全試行で「起動直後、まだタブを
1 つも開いていない」状態しかサンプリングできていないことが直接確認できる。

**選んだ対策: `tabs_N` に限定してサンプリング間隔を自動的に短縮する
(候補 1)。`MEMORY_STABILIZE_MS` を単純に延ばす (候補 3) や、自動操作
コマンドに新しい「今すぐ 1 サンプル採る」命令を追加する (候補 2) は
採らなかった。**

- **候補 3 (単純に `MEMORY_STABILIZE_MS` を延ばす) を採らなかった理由**:
  タスクの制約「計測時間を不必要に延ばさないこと」に反する。既定間隔
  5000ms に対して安全マージンを持たせるには `MEMORY_STABILIZE_MS` を
  10 秒以上にする必要があり、`tabs_50` まで含めると `--trials` を重ねる
  ほど CI のゲートジョブが顕著に遅くなる。間隔そのものを短くする方が、
  同じ確実性をずっと小さい時間コストで得られる。
- **候補 2 (自動操作コマンドに `sample` のような新命令を追加する) を
  採らなかった理由**: D44 が明記しているとおり、`VELOX_AUTOMATION_SCRIPT`
  の命令セットは「`open`/`switch`/`close`/`navigate`/`wait`/`quit` に
  限定した、意図的に閉じた集合」であり、それ自体が信頼境界の設計判断
  (D18/D23) の一部になっている。命令を追加するたびにこの閉じた集合の
  前提が崩れ、`app.rs` 側の `handle_automation_command`/`parse_script`
  にも手を入れる必要が生じる — 得られる効果 (サンプリング間隔の問題は
  純粋にタイミングの問題であり、命令セットの表現力不足が原因ではない)
  に見合わない変更コストだと判断した。
- **候補 1 を選んだ理由**: 問題の本質は「サンプラのループ間隔が、
  `tabs_N` シナリオの所要時間に対して長すぎる」ことだけであり、
  `VELOX_PERF_RSS_INTERVAL_MS` という既存の調整点 (Issue #108/D42 以前
  から存在する) を `velox-bench run` 側から自動的に渡すだけで直る。
  新しい環境変数もコマンドも増えず、`Config`/`app.rs`/`automation` の
  信頼境界には一切触れない。

**実装**: `browser::automation::recommended_rss_interval_ms(scenario)`
(`src/browser/automation.rs`) が `Scenario::TabCountMemory(_)` にだけ
`Some(MEMORY_STABILIZE_MS / TARGET_STABILIZED_RSS_SAMPLES)` (定数は
3000ms / 4 = 750ms) を返し、起動系 3 シナリオと `navigation`/
`tab_create`/`tab_switch` には `None` を返す (`config::DEFAULT_PERF_RSS_
INTERVAL` のまま、挙動不変)。`velox-bench run` (`src/bin/velox-bench.rs`)
は `--rss-interval-ms` が明示されていないときだけ、この値を
`VELOX_PERF_RSS_INTERVAL_MS` として子プロセスに渡す — 明示指定は常に
優先される。

**なぜ `MEMORY_STABILIZE_MS` (固定 3000ms) から逆算し、シナリオ全体の
推定所要時間 (`recommended_timeout_secs` が使う `PER_STEP_OVERHEAD_MS`
のような経験則) からは逆算しなかったか**: `MEMORY_STABILIZE_MS` は
タブ数に関係なく常に同じ長さの `wait` であり、実際の `open` 1 回あたりの
実時間 (環境や tab_count に依存し、見積もりが外れうる) に一切依存しない。
サンプラスレッドは main スレッドの `open` 処理とは独立して動き続けるので、
「750ms 間隔なら 3000ms の固定窓に約 4 サンプル入る」という保証は
tab_count や実際の open 所要時間が見積もりとズレても崩れない。実測でも
`tabs_20` まで含めて全試行で `pss_process_count` の中央値と p95 が完全に
一致しており (後述の表)、この窓の中で確実にサンプルが取れていることを
確認した。

**安全網: サンプル数が不足しているときに黙って値を返さない
(`browser::benchmark::memory_sample_confidence`)**。タスクの要件そのもの
であり、間隔の自動調整だけでは「それでも環境が遅くて足りない」ケース
(遅いマシン、`--rss-interval-ms` を利用者が意図的に大きく上書きした場合
など) を救えない。`scenario::Scenario::TabCountMemory` のときだけ、
集計済み `Stats` の `pss_total_bytes` (無ければ `rss_total_bytes` に
フォールバック — D42 が既に確立した「PSS は best-effort、RSS は必ず
ある」という前提と同じ理由) の `count` を `trials × MIN_RSS_SAMPLES_
PER_TRIAL` (既定 2、`recommended_rss_interval_ms` が窓あたり約 4 サンプル
を狙うのに対して十分な余裕を持たせた保守的な下限) と比較する。純粋な
判定ロジックとして `src/browser/benchmark.rs` に置き (D20 の層分離)、
`MemorySampleConfidence::{NotApplicable, Sufficient, Insufficient}` を
返す — `wry`/`tao`/`gtk` に一切依存せず、ちょうど・1 件不足・0 件・
複数 trials・PSS 不在時の RSS フォールバックなど境界値を含めて
`cargo test` で検証した (`src/browser/benchmark.rs` の
`memory_confidence_*` テスト群)。

**D42 との整合**: D42 は PSS の部分読み取りについて「`None` ではなく
部分和を返す」設計を選んだ。`memory_sample_confidence` はこの前提を
崩さない — サンプル数が不足していても `BenchmarkResult.metrics` から
`pss_total_bytes`/`rss_total_bytes` を削除したり `null` にしたりはせず、
採取できたサンプルをそのまま結果ファイルに書き出す。その代わり
`velox-bench run`/`aggregate` が `Insufficient` を検出したら stderr に
警告を出し、終了コード `1` を返す (`total_events == 0` の既存の警告と
同じパターン) — 「データを隠す」のではなく「信頼できないと明示した上で
そのまま渡す」設計であり、D42 の哲学をそのまま一段上 (1 サンプル内の
プロセス網羅性ではなく、1 run 内のサンプル数の網羅性) に適用したもの。

**検証 (実測、`xvfb-run` + `dbus-run-session`、`minimal.html`、3 試行)**:

修正前 (既定 5000ms 間隔のまま) と修正後 (自動短縮 750ms) の
`pss_total_bytes` 中央値:

| シナリオ | 修正前 (MiB) | 修正後 (MiB) | `pss_process_count` (修正後) |
| --- | ---: | ---: | ---: |
| `tabs_1` | 147.1 | 427.0 | 5 |
| `tabs_5` | 98.4 | 682.2 | 13 |
| `tabs_10` | 142.8 | 939.4 | 23 |
| `tabs_20` | 138.2 | 1399.8 | 43 |

`pss_process_count` が `5 → 13 → 23 → 43` と、D48 §4.3 が実測した
「追加タブ 1 個ごとに正確に +2 プロセス」の関係にきれいに一致している。
`#61` の `tab_scaling.py` の参照値 (中央値: 1 タブ 409.6 MiB、5 タブ
777.7 MiB、10 タブ 1387.0 MiB、20 タブ 2421.9 MiB) と比べると、桁は同じで
単調に増える傾向も一致するが、絶対値は `tabs_10`/`tabs_20` で
2〜4 割ほど低め — `tab_scaling.py` は各 `open` の間に明示的な待機
(`--settle-per-open-ms`、既定 300ms) を挟んだ上でさらに安定待ちするため、
本 Issue の `generate_bench_script` (`open` を待機なしで連続実行) より
WebKit 側がメモリを「温める」時間が長い。タブ数に応じて明確に増えており
桁が合っていることは確認できた (完全一致は測定条件が異なるため求めていない
— Issue 本文どおり)。`--rss-interval-ms` を意図的に大きく (8000ms) 指定
して同じ `tabs_5` を再実行すると、`pss_process_count` が 5 のまま
(サンプル不足の再現) になり、`memory_sample_confidence` の警告と
終了コード `1` が実際に発火することも確認した。

**変えていないもの**: `recommended_rss_interval_ms` は `TabCountMemory`
以外に `None` を返すため、`cold_startup`/`warm_startup`/`first_page_load`
(#72 の性能回帰ゲートが依存する 3 シナリオ) と `navigation`/`tab_create`/
`tab_switch` は `VELOX_PERF_RSS_INTERVAL_MS` を一切渡されず、既定挙動の
まま変わらない — 実測でも `cold_startup` の出力に間隔の自動調整メッセージ
が出ないこと、終了コードが `0` のままであることを確認した。

**検証コマンド・詳細な数値・`memory_sample_confidence` の境界値テストの
一覧は `docs/benchmarking.md`「実行環境要件」の該当箇所を参照。**

## D51: Windows リリースビルドは GitHub Actions (`release-windows.yml`) で行う

**対象**: 「Windows だけで良いので release ビルドが欲しい」という要望。
開発環境 (Linux/macOS) からは Windows バイナリをクロスコンパイルできない
(wry の WebView2 バックエンドが Windows SDK/リンカを必要とする) ため、
GitHub Actions の `windows-latest` ランナーで `cargo build --release
--locked` を実行する専用 workflow を追加した。

**判断**:

- **`ci.yml` とは分離する。** Linux CI は PR ごとに走る品質ゲートで、
  Windows のリリースビルドは「配布物を作る」という別の目的である。同居
  させると PR のたびに Windows ビルド (数分) が走り、CI の待ち時間だけが
  増える。
- **起動方法は `workflow_dispatch` と `v*` タグ push の 2 つ。** 前者は
  どのブランチからでも手動で試せる (成果物は Actions の Artifacts、30 日
  保持)。後者はそれに加えて GitHub Release を作成し zip を添付する。
  `main` への push では走らせない — 毎回 Release を作る必要は無い。
  例外として、この workflow ファイル自身を変更する PR では走らせる
  (`pull_request` + `paths` フィルタ)。`workflow_dispatch` は `main` に
  マージされるまで Actions タブに現れず手動実行できないため、これが無いと
  workflow の変更をマージ前に検証する手段が無い。
- **`--locked` を付ける。** `Cargo.lock` と一致しない依存解決になった場合
  はビルドを失敗させ、「リポジトリにあるロックファイルで再現できる
  バイナリ」だけを配布物にする。
- **スモークテストは「実行ファイルの存在とサイズ」のみ。** `velox` は
  `--help` のような GUI を開かずに終了する引数を持たないので、ヘッドレス
  なランナーで起動しても検証にならない。将来 `--version`/`--help` を
  追加すればそれを呼ぶ形に置き換える。
- **コード署名はしない。** 署名証明書が無いため、配布した exe は初回
  実行時に SmartScreen の警告が出る。個人利用の範囲では許容し、必要に
  なった時点で別途検討する。
- **`#![windows_subsystem = "windows"]` は付けない (現状維持)。** 付けると
  exe 起動時のコンソールウィンドウは消えるが、`eprintln!` によるエラー
  ログ (`app.rs` の `log_failure` パターン) が見えなくなる。ログの扱いを
  決めてから別 Issue で対応する。

**成果物**: `velox-<version>-windows-x86_64.zip` (velox.exe, velox-bench.exe,
README.md, LICENSE) と、その SHA-256 (`.zip.sha256`)。

## D52: アプリアイコン — `.exe` リソースは `build.rs` で埋め込み、ウィンドウアイコンは実行時に設定

**対象**: `assets/logo/VeloX.svg` として追加されたロゴをアプリのアイコンに
する。「アイコン」は表示される場所ごとに設定経路が異なる:

| 場所 | 設定経路 | 対象 OS |
|---|---|---|
| Explorer / タスクバー / スタートメニューの exe アイコン | exe のリソースセクション | Windows |
| ウィンドウのタイトルバー / タスクバー / Alt+Tab | `tao::window::Icon` (`WindowBuilder::with_window_icon`) | Windows, Linux |
| Dock / Finder | `.app` バンドルの `.icns` | macOS (バンドル化していないため未対応) |

**判断**:

- **元画像から派生アセットを事前生成してコミットする** (`assets/icon/`)。
  `VeloX.svg` は実体が base64 埋め込みの 526×514 PNG なので、SVG として
  実行時にラスタライズする価値がない。純 Python スクリプトで PNG を取り
  出し (`scripts/icon/generate.py`)、正方形にパディングして面積平均で縮小し、`velox.ico` (256/128/64/
  48/32/16、256 のみ PNG 圧縮エントリ、他は BMP エントリ) と
  `velox-128.png` / `velox-256.png` を生成した。ビルド時に画像処理クレート
  (`image` 等) を持ち込むより依存が軽く (D6)、生成物は差分レビューできる。
  ロゴを差し替えるときは `python3 scripts/icon/generate.py` で再生成する (縮小時はアルファを
  プリマルチプライして平均する — 透明部分の黒縁を避けるため)。
- **exe のリソースアイコンは `build.rs` + `winresource` で埋め込む。**
  `[target.'cfg(windows)'.build-dependencies]` に限定し、Linux/macOS では
  `build.rs` が no-op になるため、Linux CI (`ci.yml`) のビルド時間や依存に
  影響しない。`winresource` は `winres` の保守されているフォークで、
  `.rc` をコンパイルするのに MSVC ツールチェーンの `rc.exe` を使う (GitHub
  Actions の `windows-latest` に同梱)。埋め込みに失敗しても
  `cargo:warning` を出すだけでビルドは失敗させない — アイコンは見た目の
  問題であり、リリース workflow (D51) を止める理由にはならない。
- **ウィンドウアイコンは実行時に `velox-128.png` をデコードして設定する。**
  `include_bytes!` で埋め込むので実行ファイルの隣に assets ディレクトリは
  不要。デコードには純 Rust の `png` クレートを追加した (D6 の「必要最小限」
  の範囲内: 依存は `png` とその圧縮ライブラリのみで、システムライブラリを
  要求しない)。生の RGBA バイト列をコミットする案も検討したが、画像として
  プレビューも差分確認もできず保守性が悪いので退けた。128px にしたのは
  各 OS がタイトルバー / タスクバー用に縮小しかしないため (256px にしても
  バイナリが 15KB 増えるだけで見た目は変わらない)。
- **デコード失敗はログして続行する。** `load_window_icon` は `Option<Icon>`
  を返し、失敗時は stderr に出して `None` を渡す (`app.rs` の `log_failure`
  パターンと同じ方針)。アイコンが無くてもブラウザは起動しなければならない。
  埋め込み PNG が期待する 8-bit RGBA でデコードできることは単体テスト
  (`embedded_window_icon_decodes`) で担保する。
- **macOS は対象外 (現状維持)。** `.app` バンドルを作っていないので Dock
  アイコンの設定経路が無い。バンドル化 (cargo-bundle 等) を導入するときに
  `velox-256.png` から `.icns` を生成して合わせて対応する。

## D53: ダウンロードハンドラは WebKitGTK では `WebContext` 単位 — 共有 context (D49) では toolbar webview に 1 回だけ登録する

**対象**: Issue #127。「D49 で toolbar と全タブが 1 つの `WebContext` を
共有するようになった結果、`content_webview_builder` が webview ごとに登録している
`with_download_started_handler` / `with_download_completed_handler` が同じ
`WebContext` に N 個積まれるのではないか」という調査依頼。関連: D28
(ダウンロード)、D49 (`WebContext` 共有)、#124 / PR #125 (related view による
`WebProcess` 共有。同じ `content_webview_builder` を触る)。

### 結論 (先に要約)

- **重複どころか、Linux の非プライベートモードでは VeloX のダウンロード
  ハンドラが D49 (#118) 以降まったく呼ばれていなかった。** ダウンロードは
  完了するが、`UserEvent::DownloadStarted` は届かず、ダウンロードパネルは
  常に空、保存先は `VELOX_DOWNLOAD_DIR` でも `prepare_destination` の
  サニタイズ済みパスでもなく wry の既定 (`dirs::download_dir()`、無ければ
  **カレントディレクトリ**) だった。
- `UserEvent::DownloadCompleted` は「これまでに作られた content webview の
  数」だけ届く (閉じたタブの分も含む)。エントリが無いので実害は
  `could not correlate download completion` のログが N 行出るだけだが、
  構造としては予想どおり N 重登録になっていた。
- プライベートモード (webview ごとに ephemeral context) では 1 回ずつ
  正しく動いており、差は D49 の共有 context の有無そのものだった。
- 修正: ダウンロードハンドラの登録先を `download_handler_host` で決める。
  WebKitGTK かつ非プライベートなら **共有 context 上に最初に作られる
  toolbar webview に 1 回だけ**、それ以外 (プライベートモード、macOS/Windows)
  は従来どおり各 content webview。修正後は 1/3 タブ・タブクローズ後・
  同名 2 回・プライベート 1/3 タブの全シナリオで Started/Completed が
  各 1 回、パネル 1 件、保存先は `VELOX_DOWNLOAD_DIR`、同名 2 回目は
  `(1)` 付きになった。

### 原因 (wry 0.56.1 のソースから確認)

1. **登録単位が `WebContext`。** `src/webkitgtk/mod.rs` (671 行付近) は
   `attributes.download_started_handler` / `download_completed_handler` の
   どちらかが `Some` なら `web_context.register_download_handler(...)` を
   呼び、`webkitgtk/web_context.rs` の同関数は
   `WebKitWebContext::connect_download_started` にクロージャを 1 つ
   **追加**する (置き換えではない)。その中で各 `WebKitDownload` に
   `decide-destination` / `failed` / `finished` を接続する。
2. **wry の既定属性が「何もしない started ハンドラ」を持つ。**
   `src/lib.rs` の `WebViewAttributes::default()` は
   `download_started_handler: Some(Box::new(|_, _| true))` を設定している
   (868 行付近)。つまり **ダウンロードハンドラを付けていない toolbar
   webview も** `download-started` リスナを共有 context に登録する。
   `BrowserWindow::new` は toolbar を最初に作るので、このリスナが先頭に
   来る。
3. **`decide-destination` は `g_signal_accumulator_true_handled`。**
   WebKitGTK 2.52.6 `WebKitDownload.cpp` (`webkit_download_class_init`) で
   確認。`TRUE` を返したハンドラで伝播が止まるため、先頭にいる toolbar の
   既定ハンドラ (`|_, _| true`、保存先は wry の既定のまま) が毎回勝ち、
   後ろに並ぶ content webview の VeloX ハンドラには順番が回らない。
4. **`finished` にはアキュムレータが無い** (void シグナル) ので、
   `with_download_completed_handler` を付けた content webview の数だけ
   `DownloadCompleted` が飛ぶ。toolbar は completed ハンドラを持たない
   (`None`) ので、その分は数に入らない。閉じたタブの分も残る — wry は
   `WebView` の drop でシグナルを切断しない。

### 実測 (このコンテナ、`xvfb-run`、WebKitGTK 2.52.6、debug ビルド)

再現手順: `scripts/bench/pages/download.html` (読み込み完了時に
`download` 属性付きリンクを 1 回クリックし `velox-test.txt` を data: URL
からダウンロードする) を loopback で配信し、`VELOX_AUTOMATION_SCRIPT` で
タブを開いてから `navigate` する。計測用に `app.rs` の
`DownloadStarted`/`DownloadCompleted` 分岐と、ローカルにコピーした wry の
`register_download_handler` に一時的な `eprintln!` を入れた (コミットして
いない)。

| シナリオ | context 上の `download-started` リスナ数 | `DownloadStarted` | `DownloadCompleted` | パネル件数 | 保存先 |
|---|---|---|---|---|---|
| 修正前・1 タブ | 2 (toolbar 既定 + タブ) | 0 | 1 | 0 | **cwd** (`scripts/bench/pages/velox-test.txt`) |
| 修正前・3 タブ | 4 | 0 | 3 | 0 | cwd |
| 修正前・3 タブ → 2 タブ閉じて 1 タブ | 4 (閉じた分も残る) | 0 | 3 | 0 | cwd |
| 修正前・プライベート 1 / 3 タブ | 1 (ephemeral context 単位) | 1 | 1 | 1 | `VELOX_DOWNLOAD_DIR` |
| 修正後・1 / 3 タブ / クローズ後 | 2 / 4 / 4 (wry の既定分は残る) | 1 | 1 | 1 | `VELOX_DOWNLOAD_DIR` |
| 修正後・同名 2 回 (2 タブ) | 3 | 2 | 2 | 2 | `velox-test.txt`, `velox-test (1).txt` |
| 修正後・プライベート 1 / 3 タブ | 1 | 1 | 1 | 1 | `VELOX_DOWNLOAD_DIR` |

修正前の「cwd に保存」は wry の `dirs::download_dir()` がこのコンテナでは
`None` (XDG user dirs 未設定) で `current_dir()` にフォールバックした結果。
XDG が設定された通常のデスクトップなら `~/Downloads` になるが、いずれにせよ
`VELOX_DOWNLOAD_DIR` と D28 のファイル名サニタイズ (`sanitize_filename`) を
素通りしている点は同じで、D28 が「security-critical」とした経路が Linux
では機能していなかった。

### 修正の設計

- `src/ui/window.rs` に `DOWNLOAD_HANDLERS_PER_CONTEXT` (WebKitGTK 系
  target で `true`) と純粋関数 `download_handler_host(private, per_context)
  -> DownloadHandlerHost { SharedContext | EachContentWebview }` を追加し、
  決定表を単体テストで固定した。ハンドラ本体は `with_download_handlers`
  に切り出し、呼び出し箇所は `BrowserWindow::new` の toolbar builder
  (`SharedContext` のとき) と `content_webview_builder` (`EachContentWebview`
  のとき) の 2 箇所だけ。
- **なぜ toolbar なのか。** 共有 context に最初に作られる webview だから。
  wry 0.56.1 の builder API では既定の started ハンドラを `None` に戻す手段
  が無く、`WebContext` に直接ハンドラを登録する公開 API も無い
  (`WebContextImpl::context` は非公開、`webkit2gtk` を直接依存に足せば
  可能だが D6 の依存最小方針に反する)。「最初の webview に付ける」以外に
  先頭を取る方法が無い。toolbar HTML 自身がダウンロードを起こすことは
  無いが、ハンドラは context 単位なので toolbar に付いていることに意味上の
  問題は無い。
- **content webview には付けない** (共有 context のとき)。付けると
  `finished` リスナが N 個になる (上記 4)。wry の既定 started ハンドラは
  content webview ごとに残るが、先頭にいる VeloX のハンドラが `true` を
  返して止めるので到達しない。1 クロージャ + シグナル接続 1 つが閉じた
  タブの分も残るが、`EventLoopProxy` すら掴んでいない (`|_, _| true`) ので
  リークとしては無視できる。
- **他の案を退けた理由。** `app.rs` 側で `(url, started_at)` により重複
  排除する案は、そもそも `DownloadStarted` が届いていない (原因 3) ので
  解決にならない。「最初の content タブだけに付ける」案は toolbar の既定
  ハンドラより後ろになるので同じく効かない。webview 構築順を変えて content
  を先に作る案は D49/PR #125 の構造と z-order に手を入れることになり、
  toolbar に付けるより広い変更になる。
- プライベートモードと macOS/Windows は経路を一切変えていない
  (`EachContentWebview` = 修正前と同じ呼び出し)。

### テスト

- 単体: `ui::window::tests::download_handlers_go_to_shared_context_only_on_webkitgtk_normal_mode`
  (決定表 4 通り)。
- 統合 (`tests/integration.rs`
  `downloads_with_several_tabs_open_are_handled_exactly_once`): 3 タブで
  `download.html` を 2 回 `navigate` し、`VELOX_DOWNLOAD_DIR` に
  `velox-test.txt` と `velox-test (1).txt` だけができること、stderr に
  `could not correlate download completion` が 0 行であることを確認する。
  修正前のコードではこのテストが失敗する (ファイルが cwd = リポジトリ
  ルートに落ち、ダウンロード先ディレクトリが空になる) ことを確認済み。
  `launch_and_wait_with` (追加環境変数 + stderr のファイル出力) を
  そのために足した。

### 残る留保 (未計測、ソース読みのみ)

- wry 0.56.1 の `register_download_handler` は `failed` フラグ
  (`Rc<RefCell<bool>>`) を **登録 1 回につき 1 つ** 作り、`connect_failed`
  で `true` にした後リセットしない。ソース上は「一度失敗したら、その登録
  経由の以後の完了通知がすべて `success = false` になる」ように読める。
  D53 で登録が 1 つに集約されたことで、その影響範囲は「タブ 1 つ」から
  「セッション全体」に広がる。このコンテナでは途中切断するサーバを
  用意してもダウンロードが `finished` まで到達せず (WebKit 側で失敗が
  通知されないまま終わった) 再現できなかったため、事実確認と
  `app.rs` 側の緩和策 (`success = false` でも保存先ファイルの存在で
  再判定する等) は Issue #128 とする。
- 上記の「失敗したダウンロードで `DownloadCompleted` 自体が届かない」
  挙動 (パネルのエントリが `InProgress` のまま残る) も #128 で扱う。

**Revisit condition**: wry をアップグレードしたとき。`WebViewAttributes::
default()` の `download_started_handler` が `None` になる、あるいは
`WebContext` にハンドラを直接登録できる API が入れば、toolbar に付ける
迂回は不要になる。`download_handler_host` の決定表を変えるだけで済む
構造にしてある。

## D54: タブ間で `WebKitWebProcess` を共有 (`with_related_view`) — 最大 4 タブ/プロセス、読み込み中のプロセスには相乗りしない

**対象**: Issue #124 (D49 の Revisit condition「T2 達成には `WebKitWebProcess`
自体の共有が必要」の実装フェーズ。Epic #57)。詳細な測定データ・再現手順は
`docs/memory-analysis.md` §10。

**背景**: D49 (#118) で `WebContext` を共有しても `WebKitNetworkProcess` しか
統合されず、PSS の大半 (1 タブ時で 74.4%) を占める `WebKitWebProcess` は
webview ごとに残った。WebKitGTK が web process を共有するのは明示的に
*related* な view 同士だけで、wry 0.56.1 はこれを
`WebViewBuilderExtUnix::with_related_view(webkit2gtk::WebView)` として公開
している。D49 が未検証としていた 2 点 — (1) `webkit2gtk::WebView` という
wry の外側の型を D20 の層分離を崩さずに扱えるか、(2) related-view process
pool が実際に `WebProcess` を統合するか — を実装して確かめた。

### 実装

- **(1) は問題なかった**: wry 自身の `WebViewExtUnix::webview()` が既存の
  `wry::WebView` から `webkit2gtk::WebView` を返すので、新しい依存クレート
  は不要。WebKitGTK 型に触るのは `src/ui/window.rs` の
  `with_related_content_view` 1 関数 (Linux/BSD 以外では恒等関数) だけで、
  `browser::` は引き続き `wry`/`gtk` 型を知らない。
- **(2) も成立した**: toolbar+5 タブで `WebKitWebProcess` 6 個 → 2 個
  (toolbar 1 + content 共有 1)。
- **toolbar は共有しない**: 特権 UI (D18/D23 の IPC 信頼境界の内側) と
  ページコンテンツを同一レンダラプロセスに置かない。D3 の「chrome と
  content を別 webview に分ける」セキュリティ境界がプロセス境界にもなる。
- **プライベートモードは対象外**: wry は `.with_incognito(true)` で webview
  ごとに ephemeral `WebContext` を作る (D15) が、related view を指定すると
  WebKitGTK は related view 側の context を使うため、「private が何を分離
  するか」が変わってしまう。従来どおり webview ごとに独立したプロセス
  ペアのまま (実測で無変更を確認)。
- **`WebContext` との整合**: wry は related view 指定時に builder へ
  `.web_context()` を呼ばず、WebKitGTK が related view の context を継承
  するため、D49 の共有 `WebContext` と矛盾しない。

### 3 段階で確定した設計 — 「全タブ 1 プロセス」は採用しなかった

| 案 | 20 タブ PSS (before 2165.8 MiB) | `tab_switch` の `page_load_ms` (before 18.4ms) | 判定 |
| --- | ---: | ---: | --- |
| A: 全タブを 1 つの `WebProcess` に | 1441.8 (-33.4%) | 135.9 / 133.1 (+638% / +623%) | **不採用** (gate FAIL) |
| B: 1 プロセス最大 4 タブ | 1609.6 (-25.7%) | 88.5 / 98.3 (+381% / +435%) | **不採用** (gate FAIL) |
| **C: B + 読み込み中のプロセスには相乗りしない** | **1612.3 (-25.6%)** | **21.8 / 21.1 (+18.5% / +14.7%)** | **採用** (gate OK) |

`WebProcess` のメインスレッドは 1 本なので、タブを待ち時間なしで連続オープン
すると (`velox-bench` の `tab_switch` シナリオ) 同一プロセスに乗った
ページの読み込みが直列化する — 4 コア環境で別プロセスなら並列に進んでいた
ものが、案 A では min 値ですら 10.3ms → 39.9ms。Epic #57 のルール 4
(「メモリを過剰に解放して復帰時のページロードが遅くなる場合は改善と
みなさない」) はこれを許さない。案 B の上限だけでは 4 ページの同時ロードが
残るため足りず、**案 C: 「読み込み中のタブがいるプロセスには新しいタブを
入れない」** で解決した。burst オープン (前のタブがまだ読み込み中) は従来
どおり新しいプロセスに散り、定常状態 (前のタブの読み込みが済んでから次を
開く) だけ共有される。逐次オープンの `tab_create` シナリオでは逆に
`page_load_ms` が -32〜-34%、`tab_create_ms` が -12% 速くなった (既存
プロセスにページを足すほうが新プロセスを起こすより速い)。

機構: `ContentTab::process_group` (プロセスごとの不透明な id) と純粋関数
`pick_process_group(live_tabs: (group, loading))` — 空きがあり、かつ読み込み
中のタブを含まないグループのうち最も埋まったものを選ぶ (プロセスを埋めて
から新しいものを作る)。無ければ新グループ = 新プロセス。読み込み状態は
`browser::Tabs` が持つので、`app.rs` が `is_loading` probe クロージャを
`open_tab`/`resume_tab` に渡す。`MAX_TABS_PER_WEB_PROCESS = 4` は計測
環境のコア数に合わせた**チューニングノブであって計測で最適化した値では
ない**。

### 実測結果 (同一セッション内 before/after、`docs/memory-analysis.md` §10)

- **PSS**: 1/5/10/20 タブで -0.1% / -16.8% / -22.6% / -25.6%。1 タブあたり
  92.4 → 63.3 MiB/タブ (Chromium 9.7)。5 タブ以上では before/after の trial
  分布が重ならない。対照群の Chromium は同時測定 2 回で ±0.4% 以内。
- **プロセス数**: 4/8/13/23 → 4/5/6/8 (分散ゼロ)、計算どおり
  ⌈タブ数/4⌉ 個の content `WebProcess`。
- **回帰ゲート**: `cold_startup` / `tab_create` / `tab_switch` すべて総合
  判定 OK (D46、各 8 試行 × candidate 2 回)。
- **クロスオリジン遷移**: `file://` → `http://127.0.0.1` へ遷移してもプロセス
  は分裂しない (この構成では process swap on navigation は起きなかった)。
- **タブを全部閉じて開き直しても**共有は続く (生存 webview が無ければ新
  グループを作り、次のタブがそこに入る)。
- **統合テスト・単体テスト**とも通過 (`pick_process_group` に単体テスト追加)。

### T2 は未達 — 残りの超過は「プロセスの固定費」ではなく「ページ 1 枚あたりのコスト」

Chromium 比は 20 タブで +365.0% → +246.2% と大幅に縮んだが +10% には遠い。
案 A (全タブ 1 プロセス) でも 1 タブあたり 54.3 MiB 増えており、プロセスの
固定費を完全に消してもページ 1 枚あたり Chromium の 5.6 倍を使っている。
1 タブ時 (+45%) は共有相手が無く、D48 の「toolbar 用 2 個目の webview 1 個
分」がそのまま残る。

**トレードオフ (記録)**: 1 つの `WebProcess` に最大 4 タブが乗るため、
レンダラのクラッシュが同じプロセスの他のタブに波及する (Chromium のサイト
分離とは逆方向)。VeloX は現状クラッシュ復旧 (#25) を持たないので、実害は
「1 タブ分が 4 タブ分になる」に留まるが、#25/#89 で扱うときはこの共有を
前提にすること。

**Revisit condition**: (1) 残りの超過の切り分け — 候補は GPU 無し環境での
非表示タブのソフトウェアレンダリング用バッキングストア、または非表示タブ
の JS heap/DOM。どちらも未検証で、#63 (Adaptive Tab Suspension: 非表示
タブの webview を落とす) が最も効く可能性が高い。(2)
`MAX_TABS_PER_WEB_PROCESS` は実機 (GPU あり、コア数の異なる環境) で再評価
する。(3) wry が `webkit2gtk` 型を隠す API を提供したら
`with_related_content_view` をそれに置き換える。

## D55: PR の自動マージは自前の workflow (`auto-merge.yml`) で行う

**対象**: 「CI が全部通ったら PR を自動でマージしたい」という要望。
GitHub 標準の auto-merge (PR 画面の「Enable auto-merge」) はプライベート
リポジトリの Free プランでは使えない (ブランチ保護ルールが前提で、それが
有料機能) ため、GitHub Actions で同等の仕組みを自前で用意した。

**判断**:

- **判定基準は「この workflow 以外のチェックが全部 success / skipped /
  neutral」。** PR の head commit に付いた check-runs (GitHub Actions と
  GitHub App のチェック) と commit status (レガシー API) を両方集め、
  Auto Merge 自身の workflow 名 / job 名だけを除外する。1 つでも未完了
  (`status != completed` または `state == pending`) なら「待つ」、1 つでも
  失敗 (failure / cancelled / timed_out / action_required など) なら
  「見送る」。perf-gate の WARN 判定は job が成功終了するので、マージを
  妨げない (FAIL は job が失敗するので妨げる)。チェックがまだ 1 つも無い
  commit もマージしない (push 直後の一瞬をすり抜けさせないため)。
- **起動は `workflow_run` (CI / perf-gate / release-windows の完了時) +
  30 分ごとの `schedule` + `workflow_dispatch`。** `check_run` /
  `check_suite` の completed イベントは Auto Merge 自身の完了でも発火して
  無限ループになり得る (job を `if` でスキップしても skipped の check-run
  が生まれて再発火する) ため使わない。`workflow_run` は監視対象を明示列挙
  するので自己再帰しない。代わりに列挙漏れや、GitHub App のチェックが
  後から完了するケースを `schedule` の定期実行で拾う。新しい workflow を
  追加したら `workflows:` リストにも追加する (漏れても最大 30 分遅れで
  マージされるだけで、誤マージにはならない)。
- **`pull_request` トリガは使わない。** PR ブランチ側の workflow 定義で
  走るうえ、PR に "Auto Merge" のチェックが並んで判定対象から除外する
  手間が増える。Draft 解除直後などはスケジュール実行を待つ (≤ 30 分)。
- **PR ごとの走査で、イベントの payload に依存しない。** どのトリガで
  起動しても「`main` 向け open PR を全件見て、条件を満たすものをマージ」
  という同じ処理をする。イベント種別ごとの分岐が無いぶん単純で、
  `workflow_run` の payload から PR 番号を復元する不安定さも避けられる。
- **マージ方式は merge commit (`MERGE_METHOD: merge`)。** これまでの手動
  マージ (#123, #126, #129 など) と同じ。`--match-head-commit` で判定した
  commit から PR が進んでいたらマージせず、次回の実行で新しい commit の
  チェック結果を見る。マージ後はブランチを削除する。
- **対象外にする手段は `no-automerge` ラベルと Draft。** レビューを挟みたい
  PR は、ラベルを付けるか Draft にしておく。マージ失敗 (競合・ブランチ
  保護など) は warning を出して他の PR の処理を続け、workflow 自体は
  失敗させない。
- **同時実行は `concurrency` で直列化する。** 複数 workflow がほぼ同時に
  完了したとき、2 つの Auto Merge が同じ PR をマージしようとするのを防ぐ。

**既知の制約**: `GITHUB_TOKEN` で行ったマージは、その後の `main` への
push で他の workflow (`ci.yml` の `push: main`) を起動しない (GitHub の
仕様: GITHUB_TOKEN が起こしたイベントは新しい workflow run を作らない)。
PR 段階で同じ CI が通っているので実害は小さいが、必要なら repo 権限の
PAT を `AUTO_MERGE_TOKEN` シークレットに登録すると、そちらが優先して
使われる。

**Revisit condition**: リポジトリを public にする、または有料プランで
ブランチ保護ルール + 標準 auto-merge が使えるようになったとき。その場合は
標準機能に置き換えて本 workflow を削除できる。

## D56: Adaptive Tab Suspension — 3 シグナルの適応ポリシー、休止の単位はタブではなくプロセスグループ

**対象**: Issue #63 (Epic #57 Stage 3。D54 の Revisit condition が「残りの
超過に最も効く可能性が高い」と名指ししていた項目)。詳細な測定データ・再現
手順は `docs/memory-analysis.md` §11、目標との対応は
`docs/performance-targets.md` §12。

**背景**: D9 の自動休止は「一定時間アイドル」の 1 軸だけ (既定は無効) で、
メモリのために休止しているのに、メモリを一切見ていなかった。3 タブを 1 日
開いたままの利用者はスクロール位置を失い、5 分で 20 タブ開いた利用者は何も
救われない。D54 の時点で 20 タブの PSS は Chromium の +246% だった。

### 判断

- **3 つの独立したシグナルを純粋関数 `suspension::plan` で統合する**:
  アイドル時間 (`VELOX_AUTO_SUSPEND_AFTER_MS`、従来と同じ意味)、生存タブ
  上限 (`VELOX_MAX_LIVE_TABS`)、メモリ予算 (`VELOX_MEMORY_BUDGET_MB`、
  プロセスツリー PSS を `VELOX_MEMORY_CHECK_INTERVAL_MS` 間隔でサンプリング)。
  メモリ予算は「超過量 ÷ `ESTIMATED_BYTES_PER_TAB` (64 MiB、D54 の実測
  63.3 MiB/タブ)」個のタブを 1 回の sweep で要求する — 超過が大きいほど
  強く休止する「適応」部分。タブ数とメモリの要求は足し算せず大きい方を採る
  (1 タブ休止すれば両方の要求が 1 減る)。メモリサンプルは 1 サンプルにつき
  1 回だけ使う (`AppState::pending_memory_sample` を `take()`)。古い
  サンプルで毎パス休止し続けると、前の sweep の効果が数字に出る前に
  次々落としてしまう。
- **既定は全シグナル無効のまま** (D9 のオプトイン方針を維持)。既定設定
  では sweep は `is_enabled()` で即 return、サンプラスレッドも起動せず、
  `/proc` 走査も一切増えない。回帰ゲート (`cold_startup` / `tab_create` /
  `tab_switch`) は総合判定 OK、`tab_scaling.py` も before と誤差範囲で一致。
- **保護**: アクティブタブ (D9 どおり)、読み込み中のタブ (途中で落とすと
  やり直しになり復帰コストが最悪)、音声再生中のタブ (WebKitGTK の
  `is-playing-audio` を GLib プロパティ経由で読む
  `BrowserWindow::is_playing_audio`。新しい依存クレートは足さない、D6)。
  macOS/Windows では wry に相当する API が無く常に `false` — 音声保護は
  Linux のみ (記録)。
- **休止の単位はプロセスグループ** (`suspension::reclaim_order`)。下記の
  実測で、同一 `WebKitWebProcess` 内で webview を破棄しても解放された
  ヒープがプロセスに残り PSS がほとんど下がらない一方、プロセスが終了
  すれば全部返ることが分かった。タブ数/メモリの要求は「空にできるグループ
  (アクティブ・読み込み中・音声再生中のタブを含まない) を LRU グループ順に
  丸ごと休止」で満たす — 要求を超えてもグループ全体を落とす (超過分こそが
  目的)。空にできないグループのタブはその後に LRU で 1 つずつ (部分回収
  だが、無いよりまし)。グループの新しさは「最も最近使ったタブ」で測る。
- **計測用の追加**: `tab_suspend` perf イベント (`reason`)、休止タブへの
  切替を `tab_switch` から分けた `tab_resume` (`TabLatencyKind::Resume`、
  桁が違うので混ぜると `tab_switch_ms` が読めなくなる)、自動操作コマンド
  `suspend <index>`、`velox-bench` の `tab_resume` シナリオ、`VELOX_DEBUG`
  時のプロセスグループ配置ログ (`tab TabId(n) -> process group g`)。

### 実測で 3 回設計が変わった

| 版 | ポリシー | 20 タブ PSS (`VELOX_MAX_LIVE_TABS=4`) | 判定 |
| --- | --- | ---: | --- |
| v1 | タブ単位 LRU | 759.3 MiB | 効くが内訳が不審 (生存 4 タブが 4 プロセスに散る) |
| v2 | v1 + related view のバグ修正 | 903.5 MiB | **悪化** — 生存 3 タブのプロセスが 496 MiB を保持 |
| **v3** | **プロセスグループ単位** | **560.4 MiB** | **採用** |

- **v1 で見つかった D54 のバグ**: `open_tab` の related view 探索が「同じ
  `process_group` の最初の `ContentTab`」を取っていて、それが休止済み
  (`webview: None`) だと `related: None` → 既存グループの id を名乗った新
  プロセスが起動していた。休止タブが混ざったグループでしか起きず、D54 の
  計測には影響しない。生存 webview を持つタブだけを候補にするよう修正。
- **v2 の悪化が本 Issue の主要な知見**: 修正で生存タブが 1 プロセスに正しく
  集まると、延べ 15 ページを載せたそのプロセスが生存 3 ページで 496 MiB
  (新規プロセスなら 4 ページで 286 MiB) を保持した。settle 20 秒でも不変。
  **WebKit のプロセス内でページを破棄してもメモリは OS に返らない。返るのは
  プロセスが終了したとき**。v1 が良く見えたのはバグで休止対象が個別
  プロセスに散っていたからだった。これは D54 の「案 A (全タブ 1 プロセス)
  でもページ 1 枚あたり 54 MiB」とも整合する — 1 プロセスに集めるほど、
  そのプロセスは終了できなくなる。**今後のメモリ最適化はすべて「どの
  プロセスを終了させられるか」で考えること。**

### 実測結果 (同一セッション内、`docs/memory-analysis.md` §11)

- **PSS** (`tab_scaling.py`、3 試行の中央値): `VELOX_MAX_LIVE_TABS=4` で
  1/5/10/20 タブ 409.3 / 553.7 / 419.8 / 560.4 MiB (before 409.0 / 653.6 /
  990.6 / 1612.1)。Chromium 比は 20 タブで +245% → **+20%**、10 タブで
  **+14%**。`VELOX_MEMORY_BUDGET_MB=700` は 407 / 660 (予算内) / 476 / 615。
  タブ数に対して鋸歯状 (グループ単位で落とすため生存タブ数が 1〜4 の間を
  往復する)。
- **プロセス数**: 有効時は 10/20 タブで 4 個 (content `WebProcess` 1 個)。
- **復帰コスト** (`tab_resume`、8 試行): 2.7ms (中央値) + 再読み込み
  `page_load` 10.1ms (`minimal.html`)。既存プロセスへの related view なので
  新プロセスの起動を伴わない。
- **既定設定に回帰なし**: 回帰ゲート 3 シナリオ OK、`tab_scaling.py` も
  誤差範囲。
- **テスト**: ポリシーの単体テスト (各シグナル・保護・グループ順序・
  端数)、設定パース、`Tabs::suspension_candidates`、`tab_resume` スクリプト
  の Tabs 再生検証、統合テスト (`VELOX_MAX_LIVE_TABS=2` で実際に
  `tab_suspend` / `tab_resume` が記録される)。

### トレードオフ (記録)

- プロセス単位で落とすため、上限を 1 超えただけで最大 4 タブ分の状態
  (スクロール位置・フォーム入力・戻る/進む) が失われる。D9 の「休止は
  可逆な close」という位置付けは変わらないが、影響範囲が広がった。
  タブ単位 (v2) の方が失うものは少ないが、メモリはほぼ戻らない — 本 Issue
  の目的から後者を選んだ。
- `ESTIMATED_BYTES_PER_TAB` は `minimal.html` の実測値で、実サイトでは
  過小評価になる (収束が 1 sweep 遅れるだけで、間違いはしない)。
- WebKit 側のメモリ保持の詳細 (bmalloc の scavenger / JSC の GC タイミング)
  は未調査。「プロセスが終了しない限り戻らない」は本環境・settle 20 秒
  までの観測で、それ以上待てば戻る可能性は否定していない。

**Revisit condition**: (1) T2 の残り: 1 タブ時の +45% は toolbar 用
`WebProcess` で、休止では届かない (D48 §5)。(2) 実サイトでの評価 —
`minimal.html` では復帰が 13ms だが、実サイトでは再読み込みが支配的になる。
Epic #57 ルール 4 の観点で、上限 (`VELOX_MAX_LIVE_TABS`) の推奨値は実サイト
で決めるべき。(3) 既定を有効にするかどうか — 数値上は有効にする理由が
揃ったが、D9 の「ユーザが頼んでいない休止で状態を失う」懸念は残る。設定
画面 (#30) ができたら UI から選ばせるのが筋。(4) wry が macOS/Windows で
音声再生状態を公開したら保護を広げる。(5) macOS/Windows では
`process_group` が実プロセスに対応しないため、プロセス単位の順序は単に
「グループらしきもの」を先に落とすだけになる — 実機で挙動を確認する。

## D57: タブ生成・切替の律速は VeloX 側ではなく web プロセスの起動 — 既定の上限 4 は据え置き、ノブだけ公開する

**対象**: Issue #60 (Epic #57 Stage 2 の P0。依存する #59 は D43 で完了)。
測定データと再現手順は `docs/performance-targets.md` §13。branch
`claude/issue-check-next-phase-r86skp`。

**背景**: #60 は「タブ生成・切替を最適化する」ことと、その前提として
「1/5/10/20 タブ環境で計測し、ボトルネックを特定する」ことを求めている。
既存の `tab_create` / `tab_switch` シナリオはタブ数が固定で、タブ数を変えた
比較ができなかった。

### 先に結論

1. **VeloX 側 (Rust レイヤ) は律速ではない。** `tab_create_ms` は 1→20 タブで
   2.00→2.60ms、`tab_switch_ms` は 0.10→0.40ms。どちらもタブ数にほぼ比例せず、
   T4 の目標 (20 タブでタブ切替の中央値 100ms 以下) を 2 桁下回る。
2. **タブ生成のコストを決めるのは「新しい web プロセスを起こすかどうか」**で、
   タブ数そのものではない。既存プロセスに相乗りできる場合は
   `page_load_ms` 6.5〜9.2ms / `tab_create_ms` 2.0ms、起こす場合は
   14.6〜15.1ms / 2.7〜2.9ms と、**およそ 2 倍**になる。
3. **既定の上限 4 は据え置く。** 変更を正当化できるだけの根拠がこの環境だけ
   では揃わない (下記)。代わりに `VELOX_MAX_TABS_PER_PROCESS` で設定可能に
   したので、D54 が Revisit condition に挙げていた「実機での再評価」が
   ビルドし直さずにできる。

### 計測を成立させるために足したもの

タブ数 N での操作を測るシナリオは、必ず「N 個のタブを開く準備フェーズ」を
持つ。その準備で出る `tab_create` / `page_load` は 1 タブ時・2 タブ時…の値
であり、測りたい N タブ時の値と混ぜると中央値がどちらでもない数字になる。

- **`mark` 自動操作コマンド**: perf ログに `measure_start` を書く。
  `benchmark::aggregate_trials` は**最後のマーカーより後のイベントだけ**を
  集計する。マーカーの無い試行 (#60 より前の全シナリオ) は全イベントが対象の
  ままなので、既存の数値は変わらない。
- **`tab_create_N` / `tab_switch_N` シナリオ** (N は `Scenario::TAB_COUNTS`)。
  `tab_create_N` は N タブまで開いて `mark` し、以降「1 つ開いて閉じる」を
  8 回繰り返す — タブ数は常に N なので、サンプルはすべて同じ条件で採れる。

### ボトルネックの特定 — 反証可能な予測で確かめた

20 タブでの上限スイープが**非単調**だった (上限 4:15.8ms、6:7.3ms、8:7.8ms、
**10:16.1ms**、12:8.2ms)。プロセス数では説明できない (上限 10 と 12 はどちらも
2 プロセス)。

仮説: `pick_process_group` は「空きがあり読み込み中でないグループ」を探す。
20 タブが上限の倍数だと全グループが満杯になり、測定する `open` は毎回**新しい
`WebKitWebProcess` を起こす**。倍数でなければ既存プロセスに相乗りする。

予測と結果 (各 3 試行、20 タブ):

| 上限 | グループ構成 | 予測 | `page_load_ms` | `tab_create_ms` | プロセス数 |
| ---: | --- | --- | ---: | ---: | ---: |
| 5  | 5+5+5+5 (満杯) | 遅い | **15.1** | 2.90 | 8 |
| 7  | 7+7+6 (空きあり) | 速い | **9.2**  | 2.00 | 6 |
| 9  | 9+9+2 (空きあり) | 速い | **6.5**  | 2.00 | 6 |
| 10 | 10+10 (満杯)    | 遅い | **14.6** | 2.70 | 6 |

**4 件とも予測どおり。** 上限 9 と 10 はプロセス数が同じ 6 なのに 6.5ms と
14.6ms に分かれるので、**プロセス数ではなく「相乗りできるか」が効いている**と
確定できる。既定 (上限 4) のベースラインもこれで説明がつく — 1/5/10 タブは
空きがあり 6.0〜7.0ms、20 タブだけが満杯で 14.6ms。

### 既定値を 4 のままにした理由

上限 8 はこの環境では悪くない (メモリは 20 タブで 1652→1544 MiB、-6.6%、
プロセス 8→6。バースト系の `tab_switch` / `tab_resume` は上限 4 と誤差範囲)。
それでも既定を変えないのは:

- **20 タブでの速さの差は「20 が 4 の倍数」という条件の産物**であって、
  上限 8 が一般に速いわけではない。上限 8 でも 8/16/24 タブ目では同じ
  新規プロセス生成が起きる。効くのは「起こす頻度が 1/4 → 1/8 になる」ことだけで、
  1 回あたり約 7.5ms、タブ 1 つあたりの償却では 1ms 未満。
- **レンダラのクラッシュで巻き添えになるタブが 4 → 8 に倍増する** (D54 で
  記録したトレードオフ)。VeloX にはまだクラッシュ復旧 (#25) が無い。
- **この環境は代表的でない** (4 コア・GPU 無し、`docs/performance-targets.md`
  §1)。D54 が上限をコア数に合わせたのと同じ理由で、1 台の非代表環境の数字で
  既定を動かすのは Epic #57 のルール 5 (OS/環境ごとに結果を分ける) に反する。

**やったのは「ノブを公開したこと」**: `VELOX_MAX_TABS_PER_PROCESS`
(`Config::max_tabs_per_web_process`、既定 4)。D54 はこの値を「計測で最適化した
値ではないチューニングノブ」と明記しながらコンパイル時定数にしていたため、
比較にはビルドし直しが必要だった。1 バイナリで比較できるようにしたことで、
上記の測定自体が可能になった。

### 最適化を「しなかった」判断

Epic #57 のルール 1 (ベンチマークなしの最適化をしない) の裏返しとして、
**測って効果が見込めないものは入れない**。検討して見送ったもの:

- **ツールバーのタブ列更新の差分化**: `veloxSetTabs` は毎回
  `innerHTML = ""` で全タブを作り直し、`sync_tab_strip` は 8 か所 (各タブの
  読み込み開始・完了・タイトル・favicon を含む) から呼ばれるので O(N²) に
  見える。実測すると 20 タブのバースト全体で Rust 側は 100 回・合計 7.2ms、
  1 回あたり 0.05ms だった。差分更新は複雑さに見合わない。
- **`activate_and_refresh` の 5 本の `evaluate_script` を 1 本に束ねる**:
  タブ切替 1 回の合計が 0.4ms なので、削れる余地が測定限界以下。

### 残る限界 (正直な記録)

`tab_create_ms` / `tab_switch_ms` は**メインスレッドのハンドラが返るまで**を
測っており、ユーザが見る「実際に描画されるまで」ではない。`set_visible` も
`evaluate_script` も非同期で、描画は web プロセス側で起きる。この環境で
描画完了を外形から測る手段 (スクリーンショット差分など) は用意していない。
`page_load_ms` は「ページの load イベントまで」なので描画に近いが、同じではない。

**Revisit condition**: (1) 実機 (GPU あり、コア数の異なる環境) で
`VELOX_MAX_TABS_PER_PROCESS` を振り直し、既定値を決め直す。(2) クラッシュ復旧
(#25) が入ったら、巻き添え範囲のコストが下がるので上限を上げる判断がしやすくなる。
(3) 新規プロセス生成をタブ生成のクリティカルパスから外す案 (Chromium の spare
renderer のような事前起動) は、絶対値が 7.5ms でこの環境では割に合わないが、
実機で 1 プロセスの起動がもっと重いなら再検討する価値がある。

## D58: バックグラウンドタブの CPU は WebKitGTK が既に抑えている — VeloX 側の実装は足さず、計測手段だけ用意する

**対象**: Issue #64 (Epic #57 Stage 4。依存する #63 は D56 で完了)。測定
データと再現手順は `docs/performance-targets.md` §14。

**背景**: #64 は「バックグラウンドタブの CPU 消費を削減する」ことを求めて
いる。着手前は「非表示タブでも `requestAnimationFrame` やタイマーが回り
続けているのではないか」という前提だった。この前提を先に測った。

### 先に結論

**バックグラウンドタブは既に 99.4% 抑制されている。VeloX 側に足すべき実装は
無かった。** CPU を回し続ける固定ページ (`scripts/bench/pages/busy.html`)
を使った実測 (各 3 試行、16 秒窓):

| 状態 | CPU (1 コア比) |
| --- | ---: |
| busy がアクティブタブ | **100.0〜101.2%** |
| busy がバックグラウンドタブ | **0.4〜0.6%** |
| busy を休止 (#63) | 0.0〜0.2% |
| 静的ページ 2 タブ (対照) | 0.1% |

**機構**: VeloX はタブ切替で `set_visible(false)` を呼ぶ。wry の WebKitGTK
バックエンドはこれを GTK ウィジェットの `hide()` に落とし、WebKitGTK は
ウィジェットが unmap されたページを「隠れている」と扱う。結果として
`document.visibilityState` が `hidden` になり、エンジン側が
`requestAnimationFrame` を止め、タイマーを間引く。**VeloX が何かを実装した
から抑制されているのではなく、既存のタブ切替がそのまま効いている。**

### 「止まっている」のではなく「間引かれている」ことを確かめた

#64 は「音楽再生、WebSocket、WebRTC、通知などを不用意に停止しない」ことを
条件に挙げている。CPU 使用量は「少ない」ことしか示せず、0 と区別が付かない
ので、固定ページに `?beacon=1` を足して**外側 (ローカル HTTP サーバの
アクセスログ) から発火回数を数えた**:

| 状態 | ビーコン到達 | 元のタイマー間隔 |
| --- | ---: | --- |
| visible | 2.17 件/秒 | 500ms (= 2 件/秒、設計どおり) |
| hidden  | **1.10 件/秒** | 500ms → 約 1000ms に間引き |

**タイマーは止まっていない (約 1/2 の頻度で回り続ける) し、バックグラウンド
からのネットワークリクエストも通る。** ビーコンの `state=` パラメータが
`hidden` を報告していることが、上記の visibility 伝播の直接の裏付けでもある。
正当な背景処理を壊す変更を入れる余地は無い — むしろ入れてはいけない。

### 代わりに用意したもの (計測手段)

実装が要らないと**測ってから**言えるようにするため、再現手段を残した:

- **`scripts/bench/pages/busy.html`**: rAF ループ + 10ms タイマー + CSS
  アニメーションで実際に CPU を焼く固定ページ。`?idle=1` で全ループを止めた
  静的版に、`?beacon=1` で発火ごとに同一オリジンへ 1 本投げる版になる。
- **`scripts/profile/cpu_usage.py`**: VeloX の外から `/proc/<pid>/stat` の
  utime+stime を 2 点だけ読む。測定コストが被測定側に乗らないので、
  **絶対値**を出せる。
- **`cpu` perf イベントと `cpu_percent` メトリクス**: 既存の RSS サンプラが
  2 サンプル間の CPU 時間差から 1 コア比の使用率を計算して記録する。
  `MetricKey::CpuPercent` として回帰ゲートに掛けられる。
- **`background_cpu` シナリオ**: busy ページを開き、`?idle=1` 版を新しい
  タブで開いて busy 側をバックグラウンドに送り、`mark` してから測る。

**`cpu_usage.py` とシナリオの使い分け**: シナリオの `cpu_percent` にはサンプラ
自身が /proc を歩くコストが乗る (この環境で数 %)。同一シナリオの before/after
比較では両方に等しく乗って打ち消し合うので回帰検知には使えるが、「実際に何 %
か」を言うには外側から測る `cpu_usage.py` を使う。この違いは
`docs/performance-targets.md` §14.3 に記録した。

### 副産物として分かったこと

**フォアグラウンドでページがアニメーションしていると、`velox` 本体
(ブラウザプロセス) が 7.7〜7.9% の CPU を使う。** 内訳は WebProcess 92〜93%、
velox 本体 7.7〜7.9%。GPU の無いこの環境でのコンポジット経路のコストで、
バックグラウンドタブとは無関係だが、#67 (Browser State / Event Dispatch
Optimization) が見るべき数字として記録しておく。

### 最適化を「しなかった」判断

D57 と同じく、Epic #57 のルール 1 の裏返しとして、測って効果が見込めない
ものは入れない。検討して見送ったもの:

- **VeloX 側で明示的にページを一時停止する** (WebKitGTK の
  throttling API を叩く、`is-visible` を明示的に落とすなど): 既に 0.5% まで
  落ちているので削り代が無く、しかも #64 の「正当な背景処理を壊さない」条件に
  真っ向から反する。
- **バックグラウンドタブの描画抑制**: 上記のとおりエンジンが既に行っている。

**Revisit condition**: (1) macOS (WKWebView) / Windows (WebView2) では未検証。
どちらも「非表示の webview はスロットルされる」とされているが、VeloX の
タブ切替 (`set_visible`) がそれを引き起こすかは実機で確認が要る。(2) この
環境は GPU 無しでソフトウェアレンダリングのため、アクティブタブの CPU
(100%) は実機と乖離する。バックグラウンド側の結論 (0.5%) は描画が止まって
いる状態なので影響は小さいが、実機で再測定する価値はある。(3) 音声再生中の
タブ (#63 で保護対象にした) が本当にバックグラウンドでも再生を続けるかは、
この環境に音声デバイスが無いため未検証。


## D59: サブリソースブロック — D17 の再検証、Windows (WebView2) のみ実装できることが判明

**対象**: Issue #22 (依存する #21 は D17 で main-frame navigation blocking として
実装済み)。CLAUDE.md「対応 OS の優先度」により Windows を最優先して再検証した。

### D17 との差分（先に結論）

**D17 の結論のうち「wry 0.56 にサブリソースのリクエスト横取り API が無い」自体は
正しい。ただし D17 は `wry::WebViewBuilder` の `with_*` 系ビルダーメソッドしか
調べておらず、`WebView` を組み立てた**後**に呼べる拡張トレイト
(`WebViewExtWindows`) を見落としていた。** この拡張トレイトの
`webview()` メソッドは、wry が内部で保持している生の `ICoreWebView2`
COM オブジェクトをそのまま返す — 安定版・公開 API・`unsafe` 不要。ここから先、
`ICoreWebView2::AddWebResourceRequestedFilter` /
`add_WebResourceRequested` という WebView2 ネイティブのリクエスト横取り
フックを **wry の外から** 直接呼び出せることが分かった。D17 が「(b) `wry::WebView`
の先にある生プラットフォームオブジェクトに手を伸ばす」を「3 プラットフォーム
それぞれ `unsafe` 隣接の別実装が要る」と評価していたのに対し、少なくとも
Windows についてはその「手を伸ばす」経路が wry 自身によって公開 API として
既に用意されていたことになる。macOS (`WKContentRuleList`) / Linux
(`WebKitUserContentFilter`) に同等の拡張トレイトは無く、D17 の結論はこの 2
プラットフォームでは変わらない。

### 何を確認したか（根拠）

`~/.cargo/registry/src/*/wry-0.56.1/src/lib.rs` を実際に読んだ:

- `pub(crate) struct InnerWebView` ( `src/webview2/mod.rs` 61-63 行目)
  が `pub controller: ICoreWebView2Controller` / `pub webview: ICoreWebView2`
  / `pub env: ICoreWebView2Environment` を保持している。構造体自体は
  `pub(crate)` なのでフィールドはクレート外から直接は見えない。
- しかし `lib.rs` 2340-2348 行目に **`#[cfg(target_os = "windows")] pub trait
  WebViewExtWindows`** があり、`fn webview(&self) -> ICoreWebView2` /
  `fn controller(&self) -> ICoreWebView2Controller` / `fn environment(&self)
  -> ICoreWebView2Environment` を公開している。実装 (2378-2394 行目) は
  単に `self.webview.webview.clone()` — 上記の内部フィールドをクローンして
  返すだけ。
- wry 自身、`src/webview2/mod.rs` の `attach_custom_protocol_handler`
  (992-1110 行目) で `webview.AddWebResourceRequestedFilter(...)` /
  `webview.add_WebResourceRequested(...)` を呼んでいる — ただし
  `with_custom_protocol` 用のカスタム URI スキームだけにフィルタを絞っており
  (`work_around_uri_prefix` でスキーム名をパスに埋め込む回避策越し)、通常の
  `http`/`https` サブリソースには発火しない。D17 の「wry は
  webview2-com のイベントを内部で配線しているが `WebResourceRequested` は
  配線していない」という記述はこの内部利用に限れば正しいが、**外部から
  `WebViewExtWindows::webview()` 経由で同じ COM オブジェクトに自分の
  リスナーを登録するのを妨げるものではない** — wry のリスナーと VeloX の
  リスナーは独立に共存できる（WebView2 は 1 つの `ICoreWebView2` に対して
  複数の `WebResourceRequested` ハンドラを登録できる）。
- `webview2-com` 0.38.2 (wry が `Cargo.toml` で要求するのと同じバージョン)
  は `WebResourceRequestedEventHandler::create(Box<dyn FnMut(...) ->
  windows::core::Result<()>>)` という COM イベントハンドラ実装ヘルパーと
  `take_pwstr` (COM が返す `PWSTR` を `String` に変換しつつ
  `CoTaskMemFree` する) を `pub use` で公開している (`src/callback.rs`
  342-347 行目、`src/pwstr.rs`)。VeloX 側で改めて COM の vtable を
  組み立てる必要はない。
- `webview2-com-sys` 0.38.2 の生成バインディング (`src/bindings.rs`) で
  `ICoreWebView2WebResourceRequestedEventArgs::{Request, Response,
  SetResponse, GetDeferral, ResourceContext}` (37812-37905 行目)、
  `COREWEBVIEW2_WEB_RESOURCE_CONTEXT_{DOCUMENT,IMAGE,SCRIPT,STYLESHEET,
  FONT,MEDIA,XML_HTTP_REQUEST,FETCH,WEBSOCKET,...}` の全列挙値
  (928-961 行目)、`ICoreWebView2Environment::CreateWebResourceResponse`
  (14019-14043 行目) の存在を確認した — issue が求める「画像・スクリプト・
  XHR/fetch」の resource-type 判定と、ブロック応答 (空ボディ + 403) の生成に
  必要な API が揃っている。

### 実装したもの

- **`browser::subresource`** (`src/browser/subresource.rs`) — 純粋・
  エンジン非依存のロジック。`ResourceType` (Document/Stylesheet/Image/
  Font/Script/XhrOrFetch/Media/WebSocket/Other)、`SiteExceptions`
  (issue の「サイト単位の例外」— 完全一致ホスト名の許可リスト、
  `FilterList` のドメインサフィックス方式とは別軸)、そして
  `is_blocked_resource(list, exceptions, page_host, resource_type, url)`。
  `ResourceType::Document` は常にブロック対象外 — WebView2 の
  `WebResourceRequested` はメインフレームのナビゲーションでも発火するが、
  その判定は D17 の `with_navigation_handler` が既に行っており、ここで
  二重に判定すると「誤って表示中のページ自体をブロックする」事故になり
  得るため。`<iframe>` 自身のドキュメント読み込みも同じ理由 (トップ
  フレームか iframe かを確実に見分ける手段が `ResourceContext` 単体には
  無い) で意図的に対象外にした — 詳しくは「残っている制約」を参照。
  14 件のユニットテストがある (`cargo test subresource`)。
- **`ui::webview2_blocking`** (`src/ui/webview2_blocking.rs`,
  `#[cfg(windows)]`) — 上記の COM 呼び出し。`AddWebResourceRequestedFilter`
  で `"*"` (全リクエスト) にフィルタを登録し、`add_WebResourceRequested`
  ハンドラの中で `ResourceContext` → `ResourceType` に変換、
  `ICoreWebView2::Source` (現在表示中のページの URL、サイト例外の判定に
  使う) を取得し、`is_blocked_resource` の結果に従って
  `SetResponse` に空ボディ・403 の `ICoreWebView2WebResourceResponse` を
  差し込む。登録失敗は (D17/D18 と同じ流儀で) stderr にログして継続、
  ブラウザは落とさない。`BrowserWindow::new` と `open_tab` の両方 —
  つまり最初のタブ・新規タブ・休止からの復帰タブすべて — で
  `attach()` を呼ぶことで、content webview がいつどう生成されても
  同じブロック挙動になる (D17 の `content_webview_builder` と同じ設計原則)。
- **ブロック数集計**: `Tab::on_subresource_blocked` を追加し、既存の
  `Tab::blocked_count` (ツールバーのバッジ) を main-frame ブロックと共有する
  — ユーザ視点では「このタブでコンテンツブロックが何回働いたか」の 1 個の
  数字で十分なため。`UserEvent::SubresourceBlocked(TabId, String)` を追加し、
  `app.rs` で `UserEvent::NavigationBlocked` とほぼ同じ扱い方をするが、
  stderr へのログ出力だけは意図的に付けていない (busy なページでは 1 秒間に
  何十件もブロックが起こり得るため、`log_failure` 系の診断ログを埋もれさせる)。
- **サイト単位の例外**: `Config::content_blocking_site_exceptions`
  (`VELOX_CONTENT_BLOCKING_ALLOW`、カンマ区切りホスト名) →
  `browser::SiteExceptions` → `BrowserWindow` → `ui::webview2_blocking`。
  トグル用のツールバー UI (ボタン一つで今開いているサイトを許可する、等) は
  今回のスコープに含めていない — 環境変数での静的指定のみ。UI 化は follow-up。
- **依存クレート追加** (`Cargo.toml`, `[target.'cfg(windows)'.dependencies]`):
  `windows = "0.61"` (`Win32_System_Com` フィーチャのみ — `IStream` の
  `Option<&IStream>` 引数に必要) と `webview2-com = "0.38"`。**バージョンは
  wry 0.56.1 が要求するものと完全に一致させている** (wry の `Cargo.toml`
  参照) — ここがずれると `webview()` が返す `ICoreWebView2` と VeloX が
  import する `ICoreWebView2` が型として別物になり、コンパイルは通っても
  値を渡せない (あるいは cargo が 2 系統の `windows`/`webview2-com` を
  依存グラフに持ち込んで型が合わなくなる) 事故になり得るため。実際 `cargo
  tree` で見ると `tao` が独自に `windows 0.62.2` を使っており (VeloX の
  Windows ビルドの既存の依存)、VeloX 自身の `windows = "0.61"` はそれとは
  別系統として wry/webview2-com の 0.61.3 に解決される — Rust は同名クレートの
  複数バージョン共存を許すので問題にならないが、意図せず `windows 0.62` 系に
  解決されていないかは今後の `cargo update` のたびに確認が要る。

### 検証できたこと・できなかったこと（正直な記録）

**この開発環境は Linux のみで、Windows 実機は無い。** 確認できた範囲:

- `rustup target add x86_64-pc-windows-msvc` でターゲットを追加し、
  `cargo check --target x86_64-pc-windows-msvc --lib` および
  `cargo clippy --target x86_64-pc-windows-msvc --lib -- -D warnings` が
  **エラー・警告 0 件で通る**ことを確認した (型チェックのみ、リンクや実行は
  していない)。`--all-targets` (テストを含む) は `src/browser/downloads.rs`
  の既存のテストコード (`resolve_unix_download_dir` という存在しない関数名を
  参照している、本 Issue と無関係の pre-existing なバグ) で落ちる —
  `git stash` して確認したところ、この PR の変更を一切含まない `main` でも
  同じエラーで落ちることを確認済み。本 PR が原因ではないため直していない
  (別 Issue で扱うべき)。
- **実行時の動作 (実際にリクエストがブロックされるか、`SetResponse` が
  期待通り機能するか、`ICoreWebView2::Source` が想定したタイミングで
  正しい値を返すか) は一切確認できていない。** WebView2 ランタイムも
  Windows も無いため。

### 残っている制約 (revisit condition)

1. **iframe のドキュメント読み込みはブロック対象外**: `ResourceContext ==
   Document` を一律で除外しているため、広告 iframe そのもの (よくある
   `<iframe src="https://ads.example/...">` パターン) は現状素通りする。
   WebView2 にはトップフレームと iframe の文書リクエストを確実に見分ける
   単純な手段が `ICoreWebView2WebResourceRequestedEventArgs` 単体には無く
   (`AddWebResourceRequestedFilterWithRequestSourceKinds` の
   `RequestSourceKinds` は Document/ServiceWorker/SharedWorker/
   DedicatedWorker の区別であって top-level/iframe の区別ではない)、誤って
   トップフレームの表示中ページをブロックする事故のリスクを取ってまで
   実装する価値が今回のスコープでは無いと判断した。iframe 内の広告 URL が
   `FilterList` に載っていれば、その iframe が読み込む画像/スクリプトは
   別途ブロックされるため、影響は「iframe の空箱が残る」程度に留まる。
2. **実機未検証**: 上記の通り型チェックのみ。次に Windows 実機 (または
   Windows CI ランナー) が使えるようになったら、実際に広告ページで
   ブロック件数バッジが増えること、通常サイトが壊れないことを確認する。
3. **macOS/Linux は変更なし**: D17 の結論のまま。`WKContentRuleList` /
   `WebKitUserContentFilter` に相当する `*ExtWindows` 的な拡張トレイトが
   wry に無いかは今回改めて `wkwebview`/`webkitgtk` バックエンドの
   `lib.rs` 該当箇所も確認したが、`WebViewExtWindows` に相当するものは
   無かった (macOS 向けの拡張は `WebViewBuilderExtWebview2` のような
   ビルダー系のみで、ビルド後の `WebView` から `WKWebView` 本体を取り出す
   公開 API は無い)。CLAUDE.md の OS 優先度方針どおり、この 2 プラット
   フォームは「動作する (= 何もしない、壊さない)」最小実装のままで良いと
   判断し、今回は追わなかった。
4. **サイト例外は静的設定のみ**: `VELOX_CONTENT_BLOCKING_ALLOW` による
   起動時指定のみで、ツールバーからのトグル UI は無い。UI 化は follow-up。


## D60: サイト権限 — wry 0.56 の `with_permission_handler` は存在するが origin もカスタム UI も渡せない、origin 単位ストア + 安全側デフォルトの組み合わせで対応する

**対象**: Issue #24 (「サイト権限と権限要求UI」)。

**先に結論**: D17 (Issue #22、サブリソースブロック) と同様、まず「wry に
権限要求をフックする API があるか」を憶測せずに調べた。**今回は D17 と違い、
該当 API は実在する** (`WebViewBuilder::with_permission_handler`)。ただし
実装を進める中で、この API には設計上の制約が 2 つあり、それが Issue の
「Allow / Block UI」をそのままの形では実装させない — その制約と、代わりに
採った設計を記録する。

### 調査: wry 0.56.1 の `with_permission_handler`

ベンダー済みソース (`~/.cargo/registry/src/index.crates.io-*/wry-0.56.1/`)
を実際に読んだ。

- **API 定義**: `src/lib.rs` の
  `WebViewBuilder::with_permission_handler<F>(self, handler: F) -> Self`
  (`F: Fn(PermissionKind) -> PermissionResponse + Send + Sync + 'static`)。
  `src/permissions.rs` に `PermissionKind` (`#[non_exhaustive]`。
  `Camera`/`Microphone`/`Geolocation`/`Notifications`/`ClipboardRead`/
  `DisplayCapture`/`Midi`/`Sensors`/`MediaKeySystemAccess`/`LocalFonts`/
  `WindowManagement`/`PointerLock`/`AutomaticDownloads`/
  `FileSystemAccess`/`Autoplay`/`Other`) と `PermissionResponse`
  (`Allow`/`Deny`/`Default`) が定義されている。
- **Windows (WebView2)** — `src/webview2/mod.rs`: `attributes.permission_handler`
  が設定されていれば `ICoreWebView2::add_PermissionRequested` に登録。
  `COREWEBVIEW2_PERMISSION_KIND_*` を `PermissionKind` に変換してハンドラを
  呼び、`Allow`→`args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)`、
  `Deny`→`...STATE_DENY`、`Default`→ 何もしない (ソースコメント: "Do
  nothing, let WebView2 show default prompt")。doc コメントは「Windows:
  Fully supported via WebView2's PermissionRequested event」。
- **Linux (WebKitGTK)** — `src/webkitgtk/mod.rs`: `WebView::connect_permission_request`
  に登録。`UserMediaPermissionRequest` (カメラ/マイク/画面共有の複合要求) と
  `GeolocationPermissionRequest`/`NotificationPermissionRequest`/
  `PointerLockPermissionRequest` を型で判別してハンドラを呼ぶ。`Default` は
  そのシグナルハンドラが `false` (未処理) を返すことで WebKitGTK 自身の
  既定動作に委ねる — `with_permission_handler` の doc コメント
  (`src/lib.rs`) はこれを明記して「Linux: The default behavior is
  `Self::Deny`」としている。
- **macOS/iOS (WKWebView)** — doc コメントは「Fully supported via
  WKUIDelegate's requestMediaCapturePermission」だが、これは
  Camera/Microphone のみ。`PermissionKind` 各バリアントの doc コメントを
  読むと Geolocation/Notifications/ClipboardRead/Midi/Sensors/…はいずれも
  「macOS / iOS: Not yet supported by platform backends」と明記されており、
  実質サポートされるのはカメラ・マイクだけ。

### この API がもたらす 2 つの制約

1. **origin/URL がハンドラに渡されない**。`Fn(PermissionKind) ->
   PermissionResponse` の引数は `PermissionKind` のみで、どのフレーム・
   どの URL からの要求かという情報が一切無い。origin 単位の許可/拒否
   (受け入れ条件「許可/拒否がサイト単位で適用される」) を実現するには、
   呼び出し側 (VeloX) が別途「このタブは今どの origin を表示している
   か」を追跡し、ハンドラ呼び出し時にそれを引ければならない。
2. **同期・即時決定のみ、非同期のカスタム UI を挟めない**。
   `Fn(PermissionKind) -> PermissionResponse` は同期関数で、`wry`/
   プラットフォームは戻り値を待ってその場で許可/拒否を確定する
   (`PermissionResponse::Default` を返した場合のみプラットフォーム側が
   独自にネイティブプロンプトを出す)。VeloX 独自の「このサイトがカメラ
   へのアクセスを求めています。許可 / ブロック」という画面を表示して
   ユーザ操作を待ってから答える、という非同期フローはこの関数シグネ
   チャでは表現できない (`app.rs`「全状態変更はメインスレッドの
   `UserEvent` ディスパッチに集約」というイベントループ駆動の設計とも
   相性が悪い — 応答を待つ間イベントループを止めるわけにはいかない)。

### 採った設計

1. **`src/browser/site_permissions.rs`** — `wry`/UI に一切依存しない純粋
   ロジックとして origin 単位の権限ストア `SitePermissionStore` を実装
   (`bookmarks.rs`/`history.rs` と同じ形: プレーンな `Vec<PermissionRecord>`
   + 単体テスト)。永続化は `src/browser/persistence.rs` に既存パターン
   通りに追加 (`site_permissions.json`。壊れたファイル・欠けたフィール
   ドはいずれも空ストアへフォールバックし、起動不能にはならない)。
   - 扱う種類は issue の列挙どおり `Camera` / `Microphone` /
     `Geolocation` / `Notifications` / `ClipboardRead` の 5 つ。それ以外
     は `PermissionKind::Other` に丸められ、**レコードの有無に関わらず
     常に拒否** (`SitePermissionStore::resolve` が最初に弾く)。将来
     `wry` が新しい種類を追加しても自動的に安全側に倒れる
     (`#[serde(other)]` により、将来のバージョンが書いた
     `site_permissions.json` の未知の kind 文字列も `Other` として読め、
     ファイル全体のパース失敗にはならない)。
   - 「今後も許可」「今後も拒否」だけが `PermissionDecision::Allow`/
     `Block` としてディスクに残る唯一の状態。「一度だけ許可」は
     `PermissionDecision` のバリアントとしては存在させず、「`set` を
     一切呼ばない」こと自体として扱う (モジュール doc コメント参照) —
     ストアを 3 状態に増やすより、「保存しない」がそのまま「一度だけ」
     の意味になる方が安全側に倒しやすい。
2. **`src/ui/window.rs` の実配線** — `content_webview_builder` に
   `.with_permission_handler` を追加し、実際に本物の `wry::PermissionKind`
   を受け取って応答する。制約 1 (origin が来ない) への対処として、各
   タブの content webview ごとに `Arc<Mutex<Option<String>>>` で「直近の
   ナビゲーション成功時点の origin」(`site_permissions::origin_of`) を
   保持し、既存の `with_navigation_handler` (ブロックリスト判定のすぐ
   後、ブロックされなかった場合のみ) で更新する。`Mutex` を使うのは
   `with_permission_handler` のクロージャが `Send + Sync` を要求する
   ため — アプリの他の状態が守っている「メインスレッドの `UserEvent`
   ディスパッチに集約 (ロックなし)」の原則の例外だが、範囲はこの
   1 個の `String` キャッシュだけに閉じている (アプリの実データである
   `SitePermissionStore` 自体は起動時に読み込んだきり不変な
   `Arc` で共有しており、可変状態としての「ロック」はここには無い)。
   制約 2 (同期決定のみ、非同期カスタム UI 不可) への対処として:
   - ストアに明示的な `Allow`/`Block` が既にあれば、それをそのまま
     `PermissionResponse::Allow`/`Deny` として返す — カスタム UI 無し
     でも即答できる。
   - 記録が無い場合 (「未設定」) は `PermissionResponse::Default` を
     返す。前節の調査どおり、これは **Windows (WebView2) / macOS
     (WKWebView, カメラ・マイクのみ) ではプラットフォーム純正の
     Allow/Block プロンプトに処理を委ねる** — つまり VeloX が何も
     描画しなくても、CLAUDE.md の OS 優先度で最優先とする Windows では
     ユーザは明示的な許可 UI を見られる (受け入れ条件「権限要求を
     ユーザーに明示できる」を、Windows についてはネイティブ UI が
     満たす)。Linux (WebKitGTK) では `Default` は拒否に落ちる — これが
     そのまま受け入れ条件「不明な権限要求を安全側で処理する」の安全側
     デフォルトになる。
   - origin が取得できない要求 (まだ http(s) にナビゲートしていない、
     あるいは `file:`/`about:`/`data:` など — `origin_of` が `None` を
     返すケース) は `PermissionKind::Other` と同様、常に拒否
     (`resolve_permission`)。「サイト単位で保存された何か」を紐付ける
     先が無い以上、許可しようがないという判断。
   - マッピング関数 `map_permission_kind`/`resolve_permission` は純粋
     関数として切り出し、実際の webview を起動せずに単体テストしている
     (`src/ui/window.rs` の `tests` モジュール)。
3. **今回やらなかったこと (フォローアップ)**: プラットフォーム純正
   プロンプトでユーザが実際に何を選んだかを `wry` から観測する手段が
   無い (`with_permission_handler` の doc コメント自身も「一度永続的に
   許可/拒否されると、次回以降はこのハンドラ自体が呼ばれずプラット
   フォームの保存済み設定が使われる」と明記している) ため、その結果を
   `SitePermissionStore` に書き戻すことはできない。したがって受け入れ
   条件の「設定から権限変更」「現在サイトの権限状態表示」に対応する
   VeloX 独自の UI (ツールバーへの新しいパネル、`ToolbarCommand`/
   `UserEvent` の追加) は本 PR にはまだ無い。`SitePermissionStore` 自体
   は読み書き両方の API (`set`/`clear`/`clear_origin`/`records_for`) を
   備えているので、そうした UI を足す土台として設計してある。

### Revisit condition

(1) macOS/Windows 実機での動作は未検証 (この環境は Linux/WebKitGTK の
CI のみ) — 特に WebView2/WKWebView のネイティブプロンプトが実際に
origin 単位で永続化されるか、VeloX を再起動しても維持されるかは実機で
確認が要る (CLAUDE.md の OS 優先度どおり、確認するなら Windows が先)。
(2) `wry` が将来 origin 付き・非同期対応の権限 API を追加すれば、
VeloX 独自の Allow/Block プロンプト UI に切り替える価値が生まれる
(`wry` の CHANGELOG を継続的に見る — D17 の revisit condition と同じ
運用)。(3) 設定画面/ツールバーへの「現在サイトの権限」表示・変更 UI は
別途 Issue 化して積み残す。

## D61: CI に Windows ジョブを追加する — macOS は対象外、統合テストは実行しない

**対象**: Issue #33 (Epic #53)。当初の受け入れ条件「3 OS でビルド可能な状態を
検証できる」は、Epic #53 のスコープ見直し (CLAUDE.md「対応 OS の優先度」) に
より外れている。本 Issue でやるのは「Windows の CI 品質ゲートを整える」こと。

**判断**:

- **`ci.yml` に `check-windows` (windows-latest) ジョブを追加する。** 既存の
  `check` (Linux) ジョブは変更しない — `VELOX_INTEGRATION_REQUIRE_GUI` +
  `xvfb-run` + `dbus-run-session` の組み合わせは Issue #34/#72 の再発防止策
  そのものなので、触らない。追加ジョブは同じ `CI` workflow 内の別ジョブに
  するため、`auto-merge.yml` の `workflow_run.workflows` リスト
  (`CI` / `Performance Regression Gate` / `Release (Windows)`) は変更不要
  (workflow 単位のトリガであり、ジョブ追加では変わらない)。一方で
  auto-merge 自体は PR の head commit の check-runs を全件見て
  success/skipped/neutral を要求するため、`check-windows` の追加によって
  「Windows のビルド/テストが通らない PR は自動マージされない」が新たに
  効くようになる — これは本 Issue の目的 (Windows の品質ゲート) と合致する
  望ましい副作用であり、`auto-merge.yml` 側の追加対応は不要と判断した。
- **macOS ジョブは追加しない。** CLAUDE.md の「macOS / Linux は当面
  『最低限の整備』に留める」方針に明記されている通りで、macOS ランナーは
  Linux より高コスト (課金上の重み) なうえ、CI 時間とメンテコストが増える
  だけで Windows 優先方針には寄与しない。Linux は既存 CI と性能計測の
  実行環境として引き続き必要だが、macOS には今のところそのどちらの役割も
  無い。macOS の本格対応は Issue #33 の完了を待たず、3 OS の品質が
  「担保できた段階」(CLAUDE.md 該当節) で改めて着手する。
- **Windows ジョブは `cargo build` + `cargo test --lib` のみで、統合テスト
  (`tests/integration.rs`) は実行しない。** `tests/integration.rs` の
  `gui_skip_reason()` は Linux でのみ `DISPLAY`/`DBUS_SESSION_BUS_ADDRESS`
  を見てスキップ判定をし、macOS/Windows では常に `None` (スキップしない)
  を返す設計になっている — 「デスクトップ OS なら追加の下準備なしに GUI が
  起動できるはず」という前提のためだが、GitHub Actions の `windows-latest`
  ホストランナー (対話セッションはあるが CI 専用の仮想環境) で実際に
  `velox` (WebView2) のウィンドウ起動・イベントループが安定して成立するかは
  未検証・不確実。これを確かめずに `cargo test` (引数なし) をそのまま
  Windows ジョブで動かすと、(a) 実際に統合テストが GUI 起動に失敗して
  ジョブが赤くなり続ける、または (b) 何らかの理由で当たり障りなく通って
  しまい「Windows で検証できた」と誤認する、のどちらに転んでも本 Issue の
  目的に反する。特に (b) は Issue #34 がまさに防ごうとした「見かけ上は緑だが
  何も検証できていない」形そのものなので避けたい。そこで **確実に成立する
  範囲 (`src/browser/` 配下の純粋ロジックに対する `--lib` 単体テスト) だけを
  Windows ジョブの対象にし、GUI を要する統合テストは対象外であることを
  ワークフローのコメントに明記する**、という安全側の設計にした。
  「動いたことにする」のではなく「まだ検証していない」ことを明示している。
- **`cargo build --release` は通常の PR 向け CI には追加しない。**
  `release-windows.yml` が `workflow_dispatch` / `v*` タグ push で thin LTO
  付きの release ビルドをすでに検証しており (WebView2 のセットアップ含めて
  前例がある)、それを PR ごとに複製すると thin LTO のぶん CI 時間が伸びる
  だけで得るものが少ない。PR ゲートでは debug ビルドの `cargo build` で
  「ビルドが壊れていないか」だけを見れば十分と判断した。
- **fmt / clippy は Windows ジョブに複製しない。** どちらもソースコードの
  静的な整形・lint であり OS 依存の結果差が無いため、Linux ジョブで 1 回
  実行すれば足りる。Windows ジョブは「Windows 固有の懸念 (ビルド・実行時の
  単体テスト)」に絞った。
- **依存キャッシュは Linux ジョブと同じ `Swatinem/rust-cache@v2` を使う。**
  ランナー OS ごとにキーが分かれるため、Linux 用キャッシュと衝突しない。

**追加したジョブが初回実行で既存バグを 1 件検出した**: `check-windows` を
入れた最初の CI 実行で `cargo test --lib` が**コンパイルエラー**で落ちた。
`src/browser/downloads.rs` の `resolve_unix_download_dir` は
`#[cfg(not(any(target_os = "macos", target_os = "windows")))]` でガードされて
いるのに、それを呼ぶ 3 つのテスト (`unix_dir_*`) には同じ cfg が付いておらず、
Windows/macOS では「存在しない関数を呼ぶテスト」が残ってしまう、という
書き漏れである。同ファイルの `open_path_command_*` テストは最初から同じ cfg
を持っており、そこと不揃いだった。**この不整合は main に元からあったもので、
CI が Linux 専用だったために誰も気づけなかった** — Windows ジョブを足す価値が
そのまま出た形なので、本 PR のスコープ内 (追加したジョブを緑にする) として
同じ PR で修正した。テストを削除・スキップしたのではなく、テスト対象の関数と
同じ cfg をテスト側にも付けて対象プラットフォームを揃えただけであり、Linux
では従来通り 3 件とも実行される。

**Linux から Windows のコンパイルを事前検証できる**: 上記の切り分けの過程で、
`rustup target add x86_64-pc-windows-msvc` を入れれば Linux 上でも

```sh
cargo check --target x86_64-pc-windows-msvc --all-targets
```

が通ることを確認した。リンクを伴わない型チェックのみなので MSVC ツール
チェーンは不要で、`webview2-com` / `tao` の Windows 版まで検査される。実際、
修正前はこのコマンドが CI と同一の 3 エラーを再現し、修正後は解消した。
Windows 固有コードや cfg 分岐を触るときは、CI を一往復させる前にこれで
確認できる。ただし**リンクと実行を伴わないため、これが通っても
`cargo build` / `cargo test` が Windows で通る保証にはならない** — 実行時の
挙動を見るのは引き続き `check-windows` ジョブの役割である。この事情から、
このコマンドを CI に足すことはしない (Windows ジョブが上位互換であり、
Linux ジョブに足しても検査が重複するだけ)。開発者の手元での事前確認手段と
して CLAUDE.md に記載するに留める。

**検証の限界 (正直な記録)**: 本 Issue の実装は Linux 環境で行っており、
`check-windows` ジョブが `windows-latest` 上で最終的にグリーンになるかは
本 PR の CI 実行結果で確認する。上記の Windows ターゲット型チェック、YAML
構文の妥当性 (`yaml.safe_load`)、既存 Linux ジョブのコマンドがローカルで
通ることは確認済みだが、Windows ランナー上での実行時の挙動 (WebView2 を
含む) はこの環境では確かめられない。

**Revisit condition**: (1) `windows-latest` 上で `tests/integration.rs` の
GUI 起動 (WebView2) が安定して動くことを実際の CI 実行で確認できたら、
`check-windows` にも統合テスト (`cargo test` 全体、あるいは
`VELOX_INTEGRATION_REQUIRE_GUI` 相当の仕組み) を追加する。(2) 3 OS の
品質が担保できた段階 (CLAUDE.md「対応 OS の優先度」) で macOS ジョブの
追加を再検討する。(3) Windows ジョブが赤くなったときは、原因が CI 環境
固有の問題なのか実コードの Windows 対応不足なのかを切り分ける — 初回の
`resolve_unix_download_dir` は後者だった。

## D62: セキュリティ・入力値堅牢性 (#35) — スキーム許可リストは維持、IPC に
サイズ上限、JS 埋め込みに追加エスケープ、ブックマークの壊れた `folder_id` を
ロード時に自己修復

**対象**: Issue #35。外部入力の境界 (URL 正規化、トールバー IPC、Rust→JS の
文字列埋め込み、`history.json`/`bookmarks.json`/`input_history.json` の永続化
読み込み、`VELOX_AUTOMATION_SCRIPT`) を総点検し、攻撃パターンをテストで固定
化した。テストは 100 件以上追加 (`src/browser/navigation.rs`,
`src/ui/toolbar.rs`, `src/ui/window.rs`, `src/browser/persistence.rs`,
`src/browser/history.rs`, `src/browser/bookmarks.rs`,
`src/browser/input_history.rs`, `src/config/mod.rs`, `src/app.rs`)。

### 見つかったもの・直したもの

**クラッシュ (パニック) は見つからなかった。** `url`/`serde_json` はどちらも
不正入力に対して `Err` を返す設計で、本体コードもその `Err` を
`unwrap()`/`expect()` せず `Option`/`Result` で素通りさせる既存の書き方が
既に徹底されていた (`normalize_input`, `parse_command`,
`persistence::read_json` はいずれも失敗を吸収して `None`/`Err`/デフォルト値
に倒す)。`serde_json` 自体もパース時の再帰深度に上限を持つため、数万階層の
配列/オブジェクトのネスト ("JSON 爆弾") を IPC・永続化ファイルの双方に流し
込んでもスタックオーバーフローせず `Err` になることをテストで確認した
(`toolbar::tests::does_not_panic_or_hang_on_deeply_nested_json`,
`persistence::tests::deeply_nested_json_does_not_panic`)。

見つかった実際の問題は次の 3 点、いずれも修正済み:

1. **ブックマークの `folder_id` が壊れたまま読み込まれる。**
   `BookmarkEntry::folder_id` の「`None` か実在する folder id のどちらか」
   という不変条件は `BookmarkStore::edit`/`remove_folder` が能動的に守って
   いるだけで、`#[derive(Deserialize)]` によるファイル読み込みはこの不変条件
   を一切検証しない。手編集や部分的に壊れた `bookmarks.json` が存在しない
   `folder_id` を指すエントリを持っていた場合、そのブックマークは
   `entries_in(None)` (root) にも `entries_in(Some(壊れたid))` にも現れず、
   パネル/ブックマークバーのどちらからも永久に見えなくなる ("消えた"よう
   に見えるブックマーク)。`BookmarkStore::repair_dangling_folder_ids` を
   追加し、`persistence::load_bookmarks` がロード直後に必ず呼ぶようにした。
   壊れた `folder_id` は root に付け替えられ、次に保存されれば
   ファイル自体も修復される。
2. **不正/巨大な IPC メッセージをそのまま stderr に全文出力していた。**
   `app::handle_user_event` の `ToolbarMessage` 分岐は、パースに失敗した
   `body` を `{body:?}` でそのまま `eprintln!` していた。IPC にサイズ上限が
   無かった当時の設計では、数百万文字のアドレスバー貼り付けが弾かれた場合、
   その全文がそのままログに落ちる — クラッシュはしないが、ログを肥大化させ
   る/機微情報を丸ごと残すという別種の「サイズに比例したコスト」の穴だった。
   `app::log_preview` (最大 200 文字、`char` 境界で切り詰め) を追加し、常に
   これ経由でログに出すようにした。
3. **U+2028/U+2029 (LINE/PARAGRAPH SEPARATOR) が JS 文字列リテラルの終端に
   なり得る。** `ui::toolbar` の `set_*_script` 関数群は、以前から
   `serde_json` の文字列シリアライズ (`"`/`\`/制御文字のエスケープ) だけで
   `evaluate_script` に渡す JS を組み立てていた。RFC 8259 は U+2028/U+2029 を
   JSON 文字列中でエスケープ不要としているが、これらは ES2019 より前の
   ECMAScript 文法では文字列リテラルの内部でも行終端子として扱われていた
   — つまりエンジンによっては、タイトルや URL にこの 2 文字が混じるだけで
   文字列が意図せず終端し、後続の生 JS が別の文としてそのまま実行されかね
   ない。`escape_js_line_terminators` を追加し、`evaluate_script` に渡す
   すべての JSON 埋め込み (`set_url_script`, `set_focus_address_bar_script`,
   `set_tabs_script`, `set_candidates_script`, `set_history_script`,
   `set_bookmarks_script`, `set_bookmark_bar_script`,
   `set_downloads_script`) がこれを通るようにした。

### スキーム許可リストは変更しなかった

`ALLOWED_SCHEMES = ["http", "https", "file", "about", "data"]`
(`browser::navigation`、命名決定前からの既存コード) はそのまま維持した。
`javascript:`/`vbscript:`/`livescript:` などスクリプト実行系スキームは元々
拒否されており、大文字小文字・前後の空白・`javascript:alert(1)//`のような
コメント付与では回避できないことをテストで固定化した
(`rejects_dangerous_schemes_regardless_of_case_or_whitespace_tricks`)。
`data:`/`file:` は Issue #35 の対象ではなく、ダウンロード機能のテスト
(D28 関連) や `about:blank` 的な用途で既に前提にされている既存動作のため、
「危険そうだから」で新たに絞り込むことはしなかった — 制限を強めることが
今回のスコープではなく、CLAUDE.md の「正常系を壊さない」方針にも反する。

### IPC ペイロード上限をどう決めたか

`ui::toolbar::MAX_IPC_PAYLOAD_BYTES = 1 MiB`。トールバー Webview は VeloX
自身がバンドルする信頼済み HTML (`TOOLBAR_HTML`) であり、任意の外部 Web
コンテンツではないため、これは「攻撃者からの入力を弾く」ためというより
**多層防御**として入れた: 上限が無いと、アドレスバーへの巨大なクリップ
ボード貼り付けや (トールバー Webview 自体に将来何らかの脆弱性が入った場合
の) 悪意ある巨大メッセージが、`ToolbarCommand` のフィールド型チェックに
たどり着く前に `serde_json::from_str` へそのまま渡り、無制限に時間/メモリ
を消費し得る。1 MiB は「実用上あり得る最大の正当な入力 (アドレスバーへの
非常に長い URL や検索クエリ) に対して十分な余裕を残しつつ、明らかに
病的なサイズは弾く」という基準で選んだ — Chromium の URL 長上限が概ね
2MB 程度であることも参考にしたが、厳密にそれへ揃える理由はないため、
JSON のオーバーヘッドを差し引いても十分な余裕を持つ 1 MiB とした。
上限超過はパースを試みる前に `ParseCommandError::TooLarge` を返し、
`serde_json` には一切渡さない。

一方、**コンテンツ Webview → Rust のショートカット IPC
(`ui::window::parse_content_shortcut`)** はそもそも JSON を解釈しない —
固定の合言葉文字列 (`velox:new-tab` 等) との完全一致比較のみで、一致しな
ければ即座に無視する (D18/D23)。この経路は任意の Web ページ (信頼できない
入力) から届くため、こちらにこそサイズ上限が要ると思われるかもしれないが、
文字列の完全一致比較はサイズに比例したコストしかかからず (パース木を作ら
ない)、巨大な文字列を送っても最初のバイト不一致で早期に `None` へ落ちる
ため、明示的な上限を追加する必要はないと判断した。実際に 500 万文字の
入力でパニックしないことをテストで確認した
(`parse_content_shortcut_does_not_panic_on_hostile_content_webview_input`)。

### JS エスケープの方針

Rust → JS の文字列埋め込みは今後も **`serde_json` の文字列/値シリアライズ
を経由するのが唯一の方法** とする — 独自のエスケープ関数を書き足さない。
`"`/`\`/制御文字は `serde_json` が RFC 8259 通りにエスケープするため、URL
やタイトルにこれらがいくら含まれても JS 文字列リテラルの外へ抜け出すことは
ない (`url_script_escapes_quotes_and_backslashes` 等で既存)。今回追加した
`escape_js_line_terminators` は、その `serde_json` の出力に対する**後処理**
として U+2028/U+2029 だけを追加でエスケープするもので、JSON のパース結果を
変えない (エスケープ後の文字列も同じ JSON として解釈できる) ため、
`serde_json` を経由する既存の安全性の議論をそのまま維持できる。

`</script>` 等の HTML 的な文字列 (`url_script_neutralizes_script_closing_and_html_sequences`)
は `evaluate_script` が HTML パーサではなく JS エンジンへ文字列をそのまま
渡す API であるため、そもそも特別扱いする理由がない — これは今回のテストで
挙動を確認しただけで、コード変更はしていない。

### 永続化ファイルの壊れ方への方針

`browser::persistence::read_json` は元々「読めない/パースできない/型が
合わない」の区別をせず、すべて `None` (呼び出し側でデフォルト値) に丸めて
いた。この方針は変更していない — 部分的に読めたフィールドだけ救おうとする
部分復旧は複雑さの割に価値が低く (`#[serde(default)]` で吸収できる
フィールド追加は D27/D32 で既にその形になっている)、壊れたファイル全体を
安全に空として扱う方が事故が少ない。今回追加したのは
`BookmarkStore::repair_dangling_folder_ids` (上記) のみで、これは
「パース自体は成功するが、パースだけでは守れない構造的不変条件」という
別種の問題に対する追加のポスト処理であり、`read_json` 自体の方針変更では
ない。

### 対応しなかったもの (アドレス表示の見た目に関わる既知の限界)

以下はいずれも「クラッシュしない」ことは確認したが、意図的に**未対応**の
まま残した — URL 自体の解析/読み込みは正しく行われるが、アドレスバーに
表示される見た目が本来のホストと異なって見えうるという、実在するブラウザ
共通の課題であり、今回のスコープ (堅牢性テストの整備) を超える表示層の
設計判断が要るため:

- **userinfo によるホスト偽装** (`https://google.com@evil.com/` は
  `evil.com` が実ホストで `google.com` は捨てられる userinfo) —
  `userinfo_before_the_host_does_not_change_the_actual_host` で挙動を固定化
  したのみ。
- **Bidi override 文字によるパス偽装** (`\u{202E}` でファイル名の見た目を
  反転させる) — `does_not_panic_on_bidi_override_characters_in_a_url` で
  パニックしないことのみ固定化。
- **IDN ホモグラフ攻撃** (punycode 変換自体は `url`/`idna` クレートに委譲
  済みで正しく動くが、見た目が似た文字を使ったなりすましドメインをどう
  警告表示するかは対象外)。

**Revisit condition**: アドレスバーの表示ロジック自体に手を入れる Issue が
立ったら、上記 3 点をまとめて検討する。userinfo は本来「表示前に取り除く」
判断がしやすい (URL としての意味を変えずに済む) ので着手コストが低く、
bidi override / IDN ホモグラフは表示ポリシーの設計判断 (どこまで punycode
表示に倒すか) が要るため、着手コストが相対的に高い。

## D63: 依存関係・セキュリティ監査を CI 化 (#37) — cargo-deny 単体を採用し、PR は依存グラフを触った時だけブロッカーにする

**対象**: Issue #37。Rust 依存クレートの脆弱性・ライセンス・更新状況を CI で
継続監視する。実装前に `cargo install cargo-audit` / `cargo install
cargo-deny` を実際にこの環境で行い、VeloX の依存ツリー (Cargo.lock 286
クレート、`gtk = "0.18"` を含む Linux ターゲット cfg 依存も含む) に対して
両方を実際に走らせた結果に基づいて判断した (机上の一般論やよくある
allow-list のコピペではない)。

**判断**:

- **`cargo-audit` と `cargo-deny` の両方をローカルで実行して比較し、
  最終的に CI には `cargo-deny` だけを採用した。** `cargo audit` の結果は
  「既知脆弱性 (vulnerability) 0 件、warning 12 件」。12 件の内訳は
  `Cargo.toml` の `[target.'cfg(any(target_os = "linux", ...))'.dependencies]`
  にある `gtk = "0.18"` (wry の gtk バックエンドが Linux ビルドに必要と
  する gtk-rs GTK3 バインディング) が引き込む transitive 依存
  (`atk`/`atk-sys`/`gdk`/`gdk-sys`/`gdkwayland-sys`/`gdkx11`/
  `gdkx11-sys`/`gtk`/`gtk-sys`/`gtk3-macros` の unmaintained
  advisory 10 件、`proc-macro-error` の unmaintained 1 件、`glib` の
  unsound 1 件、RUSTSEC ID は deny.toml の `[advisories].ignore` に
  列挙) だけで、VeloX 自身のコードに起因するものは無い。`cargo deny
  check advisories` は同じ RustSec DB を使うため検知内容は同一だが、
  advisories に加えて licenses/bans/sources もカバーする上位互換であり、
  Issue の受け入れ条件にある「ライセンス監査」を別ツールで賄う必要が
  無くなる。CLAUDE.md / D6 の「依存クレートは必要最小限に保つ」は
  Rust クレートの話だが、CI ツールについても「同じ RustSec DB を見る
  ツールを 2 本併走させて設定ファイルを 2 つメンテする」意味は無いと
  判断し、`cargo-audit` は本 Issue の調査目的にのみ使い、CI には積まない。
- **ライセンス監査は実データに基づく allow-list にした。**
  `cargo deny init` の空 allow-list で `cargo deny check licenses` を
  走らせ、実際に拒否された全エントリの SPDX 式 (286 クレート分) を
  集計した結果、VeloX の依存ツリーに現れる atomic license は
  `0BSD` / `Apache-2.0` / `Apache-2.0 WITH LLVM-exception` /
  `BSD-3-Clause` / `CC0-1.0` / `MIT` / `MIT-0` / `MPL-2.0` /
  `Unicode-3.0` / `Unlicense` / `Zlib` の 11 種類のみで、GPL 系の
  copyleft ライセンスは一切無かった。`deny.toml` の `[licenses].allow`
  にはこの 11 種類だけを列挙している (「よくある allow-list」のコピペ
  ではなく実測値)。唯一の非パーミッシブ枠は `MPL-2.0` (wry →
  `dom_query` → `cssparser`/`cssparser-macros`/`selectors` 経由) で、
  ファイル単位の弱いコペレフト (バイナリ配布・リンクは制限しない) の
  ため許可した。VeloX 自身は MIT (Cargo.toml の `license = "MIT"`) で、
  MPL-2.0 のファイルを改変して再配布する予定は無い。
- **advisories の `unsound` スコープを既定の `"workspace"` から
  `"all"` に上書きした。** `unmaintained` の既定は `"all"` (transitive
  依存も検査) だが `unsound` の既定は `"workspace"` (自クレート自身が
  unsound advisory を持つ場合のみ) で、そのままだと `glib 0.18.5`
  (RUSTSEC-2024-0429, `glib::VariantStrIter` の Iterator 実装の
  unsound) のような transitive advisory を検査対象から外してしまう。
  見落としを防ぐため明示的に `"all"` にした。
- **例外ルールは `deny.toml` の `[advisories].ignore` に RUSTSEC ID +
  理由を 1 件ずつ書く運用にした。** 一括で `unmaintained = "allow"` に
  するような包括的な緩和はせず、個別 ID を列挙する。個別に列挙する
  ことで、将来 VeloX 自身が直接依存する別のクレートが新たに
  unmaintained/unsound になったときはちゃんと検知され (`ignore` に
  無い ID なので `advisories FAILED` になる)、今回把握済みの 12 件
  だけが素通りする。誤検知や「対応版が無い」既知の警告を握りつぶす
  のではなく、1 件ごとに `docs/decisions.md` (本項) への参照込みで
  記録した。
- **CI は新しい workflow `.github/workflows/dependency-audit.yml` を
  追加し、既存の `ci.yml` (#33 の成果物、`check-windows` を含む) には
  一切手を入れていない。** ジョブは `EmbarkStudios/cargo-deny-action@v2`
  (ビルド済みバイナリを取得して実行するため、ソースからの
  `cargo install cargo-deny` (ローカル検証で約 3 分) を CI 毎回走らせ
  ずに済む) で `cargo deny check` (advisories/bans/licenses/sources
  すべて) を実行する。
- **CI failure policy: `continue-on-error` は使わず、代わりに
  トリガーの `paths` フィルタでブロッカーの範囲を絞った。** 2 系統の
  トリガーを用意している。
  1. `pull_request` (`paths: ["Cargo.toml", "Cargo.lock", "deny.toml",
     ".github/workflows/dependency-audit.yml"]` に限定): 依存グラフ
     そのものを変更する PR に対してだけ、通常どおり (継続不可の)
     マージブロッカーとして働く。依存を一切触らない大多数の PR では
     `paths` に一致するファイルが無いためジョブそのものが起動せず、
     check-run も生成されない。
  2. `schedule` (毎日 1 回、`cron: "0 18 * * *"` = JST 03:00): 依存を
     まったく動かしていない期間に後から公表される advisory を拾う
     ための定期監視。PR の head commit に紐付かないので、失敗しても
     `auto-merge.yml` の判定には影響しない。
  この設計により「VeloX 側に非がなく突然公表される advisory で、依存を
  何も動かしていない無関係な PR まで巻き込んで開発が止まる」という
  Issue 本文の懸念を、`continue-on-error` で失敗を握りつぶすのではなく
  「そもそもその PR では検査が走らない/走っても PR 自身の変更が原因」
  という形で構造的に避けた。
- **`auto-merge.yml` への影響**: 上記の `paths` フィルタにより、依存を
  触らない PR ではこのジョブの check-run 自体が存在しないため、
  `auto-merge.yml` の「head commit の全 check-runs が success/skipped」
  判定には最初から数えられない (影響ゼロ)。依存を触った PR では
  他のジョブと同様に 1 つの check-run として扱われ、失敗すれば
  (継続不可なので) 従来どおりマージが止まる — これは意図した挙動
  (依存グラフを変えた張本人に対応してもらう)。`workflow_run.workflows`
  リストにも `Dependency Audit` を追加した (D55 のコメント「新しい
  workflow を追加したら追加する」に従う。追加漏れがあっても 30 分毎の
  `schedule` フォールバックがあるため誤動作にはならない)。
- **lockfile 監視・依存更新チェックは Dependabot (`.github/dependabot.yml`)
  を新規導入した。** `cargo` エコシステムと `github-actions` エコシステム
  の両方を対象にし、週次 (月曜) + `groups` で minor/patch 更新を 1 本の
  PR にまとめる (major はグルーピング対象外で個別 PR のまま — wry/tao/gtk
  のような描画スタック本体の major bump は挙動が変わりうるため一括
  マージしたくない)。Dependabot が作る PR には `no-automerge` ラベルを
  付与し、`auto-merge.yml` の対象から明示的に外した。理由は、
  `cargo fmt`/`clippy`/`cargo test` が通っても依存更新が WebView の
  実際の描画・IPC 挙動まで検証できるわけではなく、人間のレビューを
  必ず挟みたいため。

**検証の限界 (正直な記録)**: (1) `EmbarkStudios/cargo-deny-action@v2` を
実際に GitHub Actions 上で実行して確認したわけではない (この環境では
`cargo deny` をソースからインストールしてローカルで直接走らせて検証した)。
action 自体の配布バイナリ取得やキャッシュ挙動は、本 PR マージ後の実際の
CI 実行で確認する必要がある。(2) `deny.toml` の advisories ignore
(RUSTSEC-2024-0411/0412/0413/0414/0415/0416/0417/0418/0419/0420/0370/0429)
はすべて `gtk = "0.18"` (Linux 専用ターゲット依存) 由来で、CLAUDE.md の
「対応 OS の優先度」(Windows 最優先、Linux は最低限の整備) と整合する
判断だが、上流の gtk-rs が GTK4 版に移行しない限り、あるいは wry が
gtk4-rs 対応の新しい gtk backend を出さない限り解消しない — VeloX 単独
では直せない。(3) `dependabot.yml` の実際の PR 生成・grouping の挙動も
マージ後の初回実行を待って確認する必要がある。

**Revisit condition**: (1) wry が GTK4 (gtk4-rs) ベースの gtk backend を
リリースし、`gtk = "0.18"` を上げられるようになったら、`deny.toml` の
gtk-rs 関連の `ignore` エントリを削除する。(2) `EmbarkStudios/cargo-deny-action`
が実際の CI 実行で想定通り動くか (プラットフォーム互換のバイナリ取得・
キャッシュ) を確認し、問題があれば `cargo install cargo-deny --locked`
方式に切り替える。(3) Dependabot の週次 PR 頻度・grouping が実際に
運用してみて多すぎる/少なすぎると分かったら `interval`/`groups` を
調整する。(4) 新しい直接依存の追加で MPL-2.0 以外の copyleft ライセンス
(GPL 系など) が入りそうになったら、`deny.toml` の allow ではなく
依存追加自体を見直す。

## D64: EasyList/EasyPrivacy 対応 (#23) — 自前パーサを拡張、実データは同梱もダウンロードもしない

**対象**: Issue #23 (依存する #22 は D59 で Windows 限定のサブリソースブロックとして
実装済み)。既存の `browser::blocklist::FilterList` (Issue #21/#51、D17 で
`||domain^`/`@@||domain^` のみ対応) を拡張し、EasyList/EasyPrivacy が実際に
使う構文をどこまで解釈できるかを広げた。`browser::subresource`
(`src/browser/subresource.rs`) と `ui::webview2_blocking` は #22 の成果物
としてこの PR では一切変更していない。

### ライセンス調査（実際に確認した一次情報）

EasyList/EasyPrivacy のデータそのものをリポジトリに同梱するか判断するため、
公式ソースを実際に確認した:

- `https://easylist.to/pages/licence.html` — このプロキシ環境からは
  `EGRESS_BLOCKED` (easylist.to へのアクセスがネットワークプロキシで
  ブロックされている) で直接は開けなかった。
- `https://github.com/easylist/easylist` — リポジトリのルートに
  `LICENSE`/`LICENCE`/`COPYING` の類は存在しない (`README.md`,
  `CONTRIBUTING.md` 等のファイル一覧を実際に取得して確認した)。
  `README.md` はライセンスについて
  `Visit easylist.to/pages/licence.html.` と外部ページへの参照のみで、
  リポジトリ内には条文そのものが無い。
- Web 検索で easylist.to のライセンスページの実際の文言を確認したところ
  (検索結果に埋め込まれた引用): *"dual licensed under the GNU General
  Public License version 3 of the License, or (at your option) any later
  version, and Creative Commons Attribution-ShareAlike 3.0 Unported, or (at
  your option) any later version. ... 'The EasyList authors
  (https://easylist.to/)' should be attributed as the source of the
  material."* — Issue に書かれていた GPLv3 / CC BY-SA 3.0 のデュアル
  ライセンスという理解と一致する。
- `https://github.com/gorhill/uBlock/wiki/Filter-list-licenses` (uBlock
  Origin 側が主要フィルタリストのライセンスを一覧化している wiki) を
  実際に取得し確認: EasyList・EasyPrivacy はどちらも「GPL3」と
  「CC BY-SA 3.0」の両方が列挙されている一方、AdGuard 系フィルタは
  GPL3 のみで CC BY-SA を含まない、という比較が明記されていた —
  EasyList/EasyPrivacy がこの二重ライセンス構造を持つという事実の
  独立した裏付けとして扱った。

**判断**: **実データ（本物の EasyList/EasyPrivacy ファイル）はリポジトリに
同梱しない。ネットワークから自動ダウンロードもしない。** 根拠:

1. GPLv3 は同梱物が「一体として」配布される場合にコピーレフトの影響範囲
   (どこまでが「派生物」か) が争点になりやすく、CC BY-SA 3.0 は
   ShareAlike (同一ライセンスでの再頒布) を要求する — VeloX 本体は MIT
   であり、フィルタデータをリポジトリに静的に同梱すると「MIT ライセンスの
   リポジトリの一部として GPLv3/CC BY-SA 3.0 のデータを配布する」形になり、
   帰属表示 (attribution) 義務も含めてリリース物のライセンス整合性が
   複雑になる。この複雑さを本 Issue のスコープで精査しきる時間的余裕は
   ないため、同梱しない選択が安全側。
2. 自動ダウンロードも見送った。ネットワークから毎回 (または定期的に)
   フェッチする実装は、取得したデータをアプリの動作の一部として利用する
   点でライセンス上の扱いは同梱と大差なく、かつ D6 が明示的に除外している
   「HTTP クライアント依存の追加」(D6: "Notably absent on purpose: ...
   any HTTP client (the engine owns networking)") と、更新スケジューリング・
   キャッシュ・失敗時のフォールバックといった実装コストを新たに持ち込む。
   D17 の時点で既に「ネットワークからのフェッチは follow-up」と決めており、
   本 Issue でもその判断を維持する。
3. 採用したのは **「ユーザーが自分で easylist.to から手動ダウンロードした
   ファイルを、既存の `Config::extra_blocklist_path`
   (`VELOX_EXTRA_BLOCKLIST` 相当の起動時設定) で読み込ませる」** という
   D17 由来の pull 型・手動更新モデルの継続。VeloX 自身はどのライセンスの
   データも配布・複製しないため、VeloX 自体の頒布物 (MIT のソース +
   ビルド成果物) はライセンス上クリーンなまま — GPLv3/CC BY-SA 3.0 の
   遵守義務 (帰属表示など) は、そのファイルをダウンロードして使う
   ユーザー自身の利用行為に付随する (これは実際のリストファイル自身が
   ヘッダコメントに明記している内容でもある)。
4. **更新方式**: ユーザーが `https://easylist.to/easylist/easylist.txt` /
   `https://easylist.to/easylist/easyprivacy.txt` を任意の頻度 (EasyList
   自体はほぼ毎日更新されている) で再ダウンロードし、
   `extra_blocklist_path` の指すファイルを置き換えて VeloX を再起動する
   運用を前提とする。VeloX 側にホットリロードや自動更新チェックの機構は
   無い (`Config` は起動時に一度だけ解決される既存の設計と一貫)。
   バックグラウンドでの自動フェッチ・更新は、上記の理由により本 Issue
   のスコープ外として follow-up 送りとする。

### 既存クレート評価: `adblock` (Brave, crates.io)

crates.io/docs.rs (docs.rs 自体はプロキシで `EGRESS_BLOCKED`) を実際に
確認した:

- `https://crates.io/api/v1/crates/adblock` (JSON API): 最新バージョン
  `0.13.3`、**ライセンス `MPL-2.0`**、説明は "Native Rust module for
  Adblock Plus syntax (e.g. EasyList, EasyPrivacy) filter parsing and
  matching."、最終更新 2026-08-20、累計ダウンロード数 1,139,645
  (broadly 使われている、メンテナンスも継続中と判断できる)。
- ライセンス適合性: **MPL-2.0 は VeloX (MIT) と両立する** — MPL-2.0
  §3.3 は MPL 対象ファイルをより大きな作品 (Larger Work) の一部として
  別ライセンスで配布することを許しており (MPL 対象のソース自体は MPL の
  まま公開されていればよい)、多くの MIT/Apache プロジェクトが MPL-2.0
  クレートに依存する実例がある。**ライセンスは不採用の理由ではない。**
- 依存ツリー: `https://raw.githubusercontent.com/brave/adblock-rust/master/Cargo.toml`
  を実際に取得して `[dependencies]` を確認したところ、必須依存だけで
  `regex`, `flatbuffers`, `idna`, `itertools`, `cssparser`/`selectors`
  (コスメティックフィルタ用、feature gated), `seahash`, `rustc-hash`,
  `memchr`, `base64`, `arrayvec`, `bitflags`, `serde`/`serde_json`,
  `thiserror`, `percent-encoding` など (crates.io 側の依存一覧 API では
  必須 16 件 + オプション 3 件 (`addr`/PSL, `cssparser`, `selectors`))。
- **不採用の理由は D6 の依存最小化方針**: VeloX が実際に使える機能は
  「ネットワークレベルのドメイン/パターンブロック」だけで、`adblock`
  クレートが提供する正規表現マッチング・コスメティックフィルタ
  (`cssparser`/`selectors`)・PSL 付き third-party 判定・
  `flatbuffers` シリアライズといった機能の大半は VeloX には適用先が無い
  (DOM 操作フックが無い、レスポンス書き換えフックも無い)。D6 の
  「依存クレートは必要最小限」「1 クレート 1 役割」の原則に照らすと、
  使わない機能のために正規表現エンジンや CSS パーサをバイナリに含める
  コストに見合わない。D17 が同じ理由で `adblock` 系クレートを見送った
  判断を、実際にクレートの中身を確認したうえで踏襲した。
- 一方で、自前実装が「一部の構文しか解釈できない」ままでは Issue の受け
  入れ条件を満たせないため、単純なドメイン一致だけだった D17 時点の
  `FilterList` を、**ワイルドカード/セパレータ/アンカー付きの汎用パターン
  ルール**と**`$` オプション (リソースタイプ・`domain=`)**まで理解できる
  よう拡張した — 「自前実装で済ませる場合、どこまでのルール構文を
  サポートすれば実用に足りるか」を実際に手を動かして見極めた結果である。

### サポートしたルール構文 (`src/browser/blocklist.rs`)

モジュール冒頭のドキュメントコメントに正式な一覧があるが、要点:

- `||domain^` ドメインアンカー (既存、`$options` 付きも可)。
- **新規**: `||...` 以外の一般パターン — リテラル文字列、`*` (任意長
  ワイルドカード)、`^` (セパレータ — 英数字/`.`/`-`/`_`/`%` 以外の 1 文字、
  または URL 末尾)、先頭/末尾の `|` アンカー。Adblock Plus 自体が定義する
  非正規表現フィルタと同じアルゴリズム。バックトラックはしない
  (リテラルの最初の出現位置がセパレータ条件を満たさなければ、それ以降の
  出現位置は試さない) — ブロック漏れ方向の簡略化であり、誤ブロックの
  方向には倒れない。
- `@@` 例外 — ドメインアンカー/汎用パターンどちらにも付けられる。
- **新規**: `$` オプションのうちリソースタイプ (`script`/`image`/
  `stylesheet`/`xmlhttprequest`/`subdocument`/`font`/`media`/`websocket`/
  `ping`/`popup`/`document`/`other` とその別名) と `domain=a.com|~b.com`
  はパース・評価する (`MatchContext` 経由)。`third-party`/`important`/
  `match-case`/`all`/`empty`/`mp4` は認識するがマッチングには影響させない
  (後述)。それ以外の未知オプション (`$csp=`/`$redirect=`/`$rewrite=`/
  `$badfilter`/`$genericblock`/`$elemhide` 等) や否定リソースタイプ
  (`$~script`) は**ルール全体を安全に読み飛ばす** — 中途半端に適用する
  方が誤動作リスクが高いと判断した。
- **新規**: `/regex/` (パターン全体が `/` で始まり `/` で終わる)
  形式の正規表現フィルタは Adblock Plus の仕様どおり正規表現として認識し、
  VeloX は正規表現エンジンを持たない (意図的 — 依存追加を避ける) ため
  安全に読み飛ばす。
- コメント (`!`)、`[Adblock Plus 2.0]` 形式のヘッダ行、要素非表示/
  スクリプトレット行 (`##`/`#@#`/`#$#`/`#%#`/`#?#` を含む行) は既存どおり
  無視。
- **意図的に実装しなかったもの**: 正確な third-party 判定 (public suffix
  list 相当の実装が要り D6 に反するため、`domain=` の実装だけに留めた)、
  `$important` の優先度上書き、`$badfilter` のルール取り消しセマンティクス
  (取り消し対象ではなく通常ブロックとして誤適用するとむしろ危険なため
  ルールごと破棄)、コスメティック/スクリプトレット/レスポンス書き換え系
  オプション全般。

### 不正ルールへの耐性

- 1 行が `MAX_RULE_LINE_LEN` (8192 バイト) を超える場合はパースせず
  読み飛ばす — 巨大な 1 行によるパースコスト膨張を防ぐ。
- リスト全体は `MAX_RULES` (300,000 ルール — EasyList + EasyPrivacy を
  合わせても現状 15 万行程度なので十分な余裕を持たせた) で頭打ちにし、
  それ以降の行は解析せず無視する (エラーにはしない) — 異常に巨大な
  ファイルによる無制限のメモリ消費を防ぐ。
- ワイルドカードのみで構成される (リテラルを一切含まない) パターンは
  「実質すべてのリクエストをブロックする」事故になるため、明示的に
  ルールとして採用しない (`*` 単独や `****` のような行は無視)。
- 未対応の `$` オプション・否定リソースタイプ・不正な `domain=` 値・
  壊れた `||domain^` (ドメイン部分に許可されない文字を含む) は、いずれも
  そのルール 1 行を安全に無視するだけで、パニックや他ルールへの影響は
  無い。
- 単体テスト (`src/browser/blocklist.rs` の `tests` モジュール) に、
  上記すべてを固定化するケースに加え、コメント・ヘッダ・不正オプション・
  正規表現・巨大ファイルをランダムに混在させた 5,000 行のガベージ
  ファイルでパニックしないことを確認するテスト、`MAX_RULE_LINE_LEN`
  境界のテスト、`MAX_RULES` の頭打ちを 30 万件超のファイルで確認する
  テストを含む。Issue #35 が並行して進めている「クラッシュしないこと」を
  固定化する堅牢性テストと同じ発想。

### 検証できたこと・できなかったこと（正直な記録）

**ルールマッチングの単体テスト**: `cargo test` で 35 件
(`browser::blocklist::tests::*`、D17 時点の 10 件から追加) が
ライブラリテスト全体 582 件の一部として実行され、すべて成功。
ドメインアンカー・汎用パターン (リテラル/ワイルドカード/セパレータ/
アンカー)・例外・リソースタイプ/`domain=` オプション・未対応オプションの
ルール破棄・巨大ファイル/巨大行への耐性のいずれもここでカバーしている。

**「既存サイトで広告/トラッカーがブロックされる」は部分的にしか検証できて
いない**。この開発環境は Linux (WebKitGTK) のみで、D59 の結論どおり
サブリソースブロック自体が Windows (WebView2) 限定であり、Linux では
そもそも `browser::subresource`/`ui::webview2_blocking` が動作対象外。
検証できた範囲とできなかった範囲を分けて記録する:

- 検証できた: `FilterList::is_blocked`/`is_blocked_with_context` の
  単体テスト (実際のリクエスト URL 文字列に対する判定ロジック)。
  `src/ui/window.rs` の `content_webview_builder` が
  `content_blocking_enabled && blocklist.is_blocked(&url)` を
  変更なく呼び出しており (このシグネチャは #22/#59 と同じく維持)、
  main-frame ナビゲーションレベルのブロックは既存の統合テスト基盤
  (`tests/integration.rs`) の対象範囲内で壊れていないことを
  `cargo test`/`xvfb-run` で確認した。
- 検証できなかった: Windows 実機 (または WebView2 ランタイム) が
  この環境に無いため、拡張した `FilterList` を実際の広告/トラッカー
  リクエスト (例えば EasyList を読み込んだ状態でのブラウジング) に
  対して動かし、ブロック件数バッジが増える・広告が消えることを
  目視確認することはできていない。これは D59 が既に記録した制約
  (Windows 実機未検証) の範囲内であり、今回新たに増えた制約ではない。

### 残っている制約 (revisit condition)

1. **third-party/`$important`/`$badfilter` は未実装** — 上記のとおり
   意図的な見送り。third-party は PSL 相当の実装が必要になった時点で、
   `$important`/`$badfilter` は優先度付きマッチングモデルが必要になった
   時点で再評価する。
2. **`browser::subresource::is_blocked_resource` はリソースタイプ/
   `domain=` オプションを未だ利用しない** — #22 の成果物としてこの PR
   では変更していないため、`FilterList::is_blocked_with_context` は
   実装・テスト済みだが実際の呼び出し元からはまだ配線されていない。
   `browser::subresource::ResourceType` → `browser::RuleResourceType`
   の変換を追加する follow-up で接続できる (両者は語彙が完全一致しない
   ため素朴な `From` 変換にはならない点に注意)。
3. **実データの動作確認が Windows 実機でしかできない** — 上記のとおり。
4. **自動更新の UI/機構は無い** — `extra_blocklist_path` の手動再配置 +
   再起動のみ。ホットリロードや定期フェッチは follow-up。


## D65: タブセッション復元 (#25) — 保存は `sync_tab_strip` に相乗り、復元は休止/復帰機構をそのまま再利用、クラッシュ検知フックは wry 0.56 に存在しない

**対象**: Issue #25 (依存: #12)。起動時セッション読み込み・終了時タブ情報
保存・URL/title/favicon の保存・WebView 再生成・WebView/renderer クラッシュ
検知の調査・タブ単位の復旧・「前回のタブを復元」設定。

### 保存形式とタイミング

- **形式**: `browser::session::SessionSnapshot { tabs: Vec<SavedTab>,
  active_index: usize }`、`SavedTab { url, title: Option<String>, favicon:
  Option<String> }`。`history.json`/`bookmarks.json`/`input_history.json`
  と同じ 3 点構成 — 純粋なデータ型+ロジックは `browser::session`、IO は
  `browser::persistence::{load,save}_session` (`session.json`、同じ
  データディレクトリ、D10) — を厳密に踏襲した。持たせるのはタブ strip
  自身が描画に使っている情報 (`app::sync_tab_strip` の `TabSummary` と同じ
  3 フィールド) だけで、スクロール位置・フォーム入力・エンジン側セッション
  履歴は最初から対象外 — これは新しい割り切りではなく、タブ休止 (D9) が
  既に受け入れているのと同じ損失をセッション復元にも適用しているだけ。
- **タイミング**: `app::persist_session` を `app::sync_tab_strip` の内部から
  呼ぶ。`sync_tab_strip` はタブに影響する変更のほぼ全て (open/close/
  activate/suspend/load 完了/favicon 解決) が既に通る唯一の関数なので、
  ここに相乗りすれば専用の呼び出し箇所を各所に増やさずに済む。唯一の例外は
  `PageTitleResolved` (この方法内は元々 `sync_tab_strip` を呼んでいない) で、
  ここだけ個別に `persist_session` を呼ぶ。**終了時保存ではなく変更の都度
  保存**にしたのは、Issue が名指しした「クラッシュ復旧」がまさに
  `WindowEvent::CloseRequested` のような正常終了フックが実行され*ない*
  ケースだから — 終了時だけの保存ではクラッシュした瞬間の直前状態を
  再現できない。書き込みは `history`/`bookmarks` と同じ「その都度 best-
  effort、失敗は `log_io_failure` で stderr に流すだけで継続」パターン。
- **プライバシー**: `AppState::history_enabled` (D13/D14 の private browsing
  choke point) が `false` の間は `persist_session` は何も書かない —
  `Config::restore_previous_session` の値に関係なく、プライベートセッション
  が開いていたタブをディスクに残さない。復元側も `config.private` なら
  常にスキップする (後述)。

### 復元とタブ単位の復旧 — 休止/復帰機構との統合

Issue 自身が「WebView 再生成は休止/復帰機構と重複する可能性が高い」と
指摘していたとおり、`suspension.rs`/`TabState` を読んでからの設計判断:

- **`Tabs::restore(saved: &[SavedTab], active_index: usize) -> Tabs`**
  (`browser::tabs`) は、アクティブだったタブ 1 つだけを `Tab::new` (通常の
  `Active` 開始) で作り、**それ以外の全タブを新設の `Tab::new_suspended`
  (`pub(super)`) でいきなり `TabState::Suspended` として作る**。
  `Active -> Background -> Suspended` の通常遷移を経由しない直接コンストラクタ
  だが、理由は単純: 復元されたタブは一度も webview を持ったことがなく、
  「そこから休止する」遷移ではなく「最初から休止状態」でしかありえない。
  `TabState` の遷移テーブル (D20) 自体は変更していない — 遷移不能な状態を
  型で表現するのではなく、コンストラクタで直接その状態を作るところが
  新しい部分。
- **`ui::window::BrowserWindow` の休止/復帰そのものは 1 行も変更して
  いない**。`BrowserWindow::new` は元々「渡された 1 個の `TabId`」用の
  webview しか作らない — 復元後の `Tabs` からアクティブなタブの id を渡す
  だけで、それ以外の復元タブは `BrowserWindow::contents`(`HashMap<TabId,
  ContentTab>`) に一切エントリを持たない。ユーザがそのタブをクリックする
  (あるいは前のタブが閉じられて繰り上がる) と、`Tabs::activate` は既存の
  ロジックだけで `ActivationEffect::Resume` を返す (`TabState::Suspended`
  だから) — `app::activate_and_refresh` はそれをセッション中に休止された
  タブと**区別せず**同じ `BrowserWindow::resume_tab` (= `open_tab` +
  `activate_tab`) に渡す。つまり「WebView 再生成」という専用パスは実装
  していない — 休止/復帰機構をそのまま復元にも使っている。副作用として、
  10 タブ復元しても起動時に実際に張られる webview は 1 個だけ (アクティブ
  タブの分) で、残り 9 個はユーザが実際に開くまでプロセスもメモリも
  消費しない。
  **唯一見つけて直した既存のバグ**: `BrowserWindow::new` は最初のタブの
  webview を常に `config.homepage` で読み込んでいた — `Tabs::restore` が
  そのタブの `current_url` を正しく復元済みタブの URL にしていても、実際に
  表示される最初のページは無条件にホームページのままだった (この issue が
  無ければ気づかれなかったであろう、既存コードの潜在バグ)。`BrowserWindow::new`
  に `initial_url: &str` 引数を追加し、`app::run` から `tabs.active().
  current_url()` を渡すよう修正 — 通常時 (`Tabs::new(homepage)`) はその
  タブの `current_url` も `homepage` と同じ値なので、非復元時の挙動は
  1 バイトも変わらない。統合テスト
  `restoring_the_previous_session_reopens_its_tabs_across_a_real_relaunch`
  はこの修正が無いと (`VELOX_HOMEPAGE` に無関係な第三のページを渡した上で)
  確実に落ちる。
- **id は必ず採番し直す** (`Tabs::restore` は `next_id = 0` から
  `SavedTab` の並び順に割り当てる)。前回プロセスの `TabId` は元々永続化
  していない (`u64` の内部値に意味はなく、プロセスをまたいで再現する
  必要もない)。
- **起動時の配線** (`app::run`): `Config::restore_previous_session &&
  !config.private` のときだけ `persistence::load_session` →
  `SessionSnapshot::sanitize` を試み、`Some` なら `Tabs::restore`、それ
  以外 (設定 OFF・データディレクトリ無し・ファイル無し・壊れている・
  private mode) は従来どおり `Tabs::new(homepage)`。`data_dir` の解決を
  `Tabs`/`BrowserWindow` 構築より前に前倒しした以外、既存の起動シーケンス
  (history/bookmarks/input_history の読み込み、`AppState` 構築) は変えて
  いない。

### 「前回のタブを復元」設定

`Config::restore_previous_session: bool` (既定 `false`)、
`VELOX_RESTORE_SESSION` の有無で切り替え — `VELOX_PRIVATE`/
`VELOX_PERF_METRICS` と同じ「有無だけを見る」パターン。既定を OFF にしたのは
D9 (自動休止) と同じ理由: 今まで存在しなかった「前回のタブが勝手に開く」
挙動で驚かせるのは、機能を足さないより悪い既定。保存自体は
`restore_previous_session` の値に関係なく (private mode 以外) 常に行う
ので、後から設定をオンにした瞬間から直近のセッションを復元できる。設定
画面 (#30) はまだ無いので、当面は環境変数のみ。

### 破損データの扱い (最重要の受け入れ条件)

`SessionSnapshot::sanitize` が唯一の検証ゲート — アドレスバー入力に対する
`navigation::normalize_input` と同じ役割をセッションデータに対して果たす:

- 各タブの `url` を `normalize_input` に通し直す。拒否されたスキーム
  (`javascript:` 等)・空文字・パース不能な文字列を持つエントリはその 1 件
  だけを捨てる (セッション全体は破棄しない)。
- 生き残ったタブが 0 件になったら `None` を返し、呼び出し側は
  `Tabs::new(homepage)` にフォールバックする。
- `active_index` は信用せず、直前にアクティブだった URL を生き残った
  リストの中から位置で探し直す。見つからなければ (アクティブだった
  エントリ自体が捨てられた、または元の `active_index` が最初から範囲外)
  先頭のタブにフォールバックする — 添字を直接使い回さないので、範囲外
  インデックスによる panic は構造的に起こらない。
- `#[serde(default)]` を `SessionSnapshot`/`SavedTab` の全フィールドに
  付けたので、古いスキーマの (フィールドが足りない) ファイルも読める。
- JSON として壊れている場合 (truncated・型違い・配列/数値/文字列/null が
  トップレベルに来ている等) は `serde_json::from_str` がそのまま失敗し、
  `persistence::load_session` は `history`/`bookmarks` の既存ローダーと
  全く同じ「`Option::None` を返すだけ」という契約に従う — 起動を止める
  経路が存在しない。
- `Tabs::restore` 自体も `saved` が空・`active_index` が範囲外という
  想定外の入力に対して (`sanitize` を経由しない直接呼び出しに備えて)
  それぞれ「`about:blank` の 1 タブにフォールバック」「`0` に丸める」と
  自己防衛しており、`SessionSnapshot::sanitize` 頼みの単一障害点にしていない。
- テスト: `browser::session`・`browser::tabs`・`browser::persistence` の
  各層で、正常系に加えて「壊れた JSON」「truncated (書き込み途中で
  クラッシュした想定)」「型が違う (`tabs` が文字列、`active_index` が
  文字列)」「トップレベルが配列/数値/文字列/null」「2 万タブの巨大ファイル」
  「存在しないアクティブタブ・全滅した URL」を個別にケース化した — 本体
  コードでの `unwrap()`/`expect()` は使っていない (CLAUDE.md のルール通り)。

### クラッシュ検知の調査結果 (wry 0.56.1、`~/.cargo/registry/src/index.crates.io-*/wry-0.56.1/src`)

Issue #22 の教訓 (`WebViewBuilder` の `with_*` だけでなく、ビルド後の
プラットフォーム別拡張トレイトも確認する) に従い、`grep -rniE
"crash|terminat|render_process|process_fail|web.?process"` で `src/`
全体を機械的に走査した上で、ヒットした箇所を実際に読んだ。

| プラットフォーム | wry が公開する API | 場所 |
|---|---|---|
| macOS/iOS (WKWebView) | **あり**: `WebViewBuilderExtDarwin::with_on_web_content_process_terminate_handler(impl Fn() + 'static)` — `webView:webContentProcessDidTerminate:` (WKNavigationDelegate) をラップしたビルド後拡張トレイト。 | `src/lib.rs:1633-1660`、`src/wkwebview/navigation.rs:107-114`、`src/wkwebview/class/wry_navigation_delegate.rs:101-103` |
| Windows (WebView2) | **無し**。`WebViewExtWindows` (`src/lib.rs:2340-2375`) は `controller()`/`environment()`/`webview()`/`set_theme`/`set_memory_usage_level`/`reparent`/`hwnd` のみ — `ICoreWebView2::add_ProcessFailed` に対応するものは無い。`webview()` が生の `ICoreWebView2` COM インターフェースを返すので技術的には呼び出し側が `unsafe` な COM 呼び出しで直接登録することは可能だが、wry 自体はそれを一切ラップしていない。 | `src/lib.rs:2340-2375`。`grep` で `ProcessFailed`/`process_fail` は 0 件。 |
| Linux/BSD (WebKitGTK) | **無し**。`with_related_content_view`/`is_playing_audio` が使っているのと同じ「wry の `WebViewExtUnix::webview()` で生の `webkit2gtk::WebView` を取り、GLib の汎用プロパティ/シグナル API を叩く」という抜け道は存在しうる (libwebkit2gtk 自体には `WebKitWebView::web-process-terminated` という実在のシグナルがある) が、これは **wry のソースには一切現れない** — wry を読んで確認できる範囲を超え、`webkit2gtk`/GIR のドキュメントに頼ることになる。D9 が同じ理由 (wry を経由しない生 API) で `CacheModel` 調整を見送ったのと同じ扱いとし、本 Issue でも実装しなかった。 | `grep` で `terminat`/`crash`/`process_fail`/`web-process-terminated` は `src/webkitgtk/` に 0 件。 |
| 全プラットフォーム共通 | `WebView::clear_all_browsing_data` はクラッシュ検知ではなく Cookie/キャッシュ/ストレージの一括消去であり無関係。 | — |

**判断: 実装しない**。理由:

- **CLAUDE.md の OS 優先度**は Windows を最優先とし、「OS 別分岐を書く場合は
  Windows の実装を先に用意する」ことを求めている。ここで唯一実在する
  フックは macOS/iOS 専用であり、Windows には対応するものが無い。Windows に
  何も無いまま macOS だけにクラッシュ復旧の作り込みを入れるのは、この方針と
  正面から矛盾する。
- Windows 側の「技術的には可能」な道 (生の `ICoreWebView2` を取り出し
  `unsafe` な COM 呼び出しで `add_ProcessFailed` を自前で登録する) は、
  CLAUDE.md が原則禁止する `unsafe` の新規使用と、D6 が求める「新しい
  依存を足す理由の説明」を同時に要求する重い変更であり、本 Issue の主目的
  (セッション永続化) の付随作業として見合わない。
- Linux 側の GLib シグナル案も同様に、wry のソースだけでは実在も挙動も
  検証できず、「憶測で API があることにしない」という Issue 自身の指示に
  反する。
- 以上より、**セッション永続化と復元 (wry に依存せず実装できる部分) を
  確実に仕上げる** という Issue が示した代替方針を採用した。

### 受け入れ条件との対応

- [x] 再起動後に前回のタブを復元できる — `Tabs::restore` + 上記の起動時
  配線。`VELOX_RESTORE_SESSION=1` で有効化。単体テストの
  `Tabs::restore`/`SessionSnapshot` 往復に加え、統合テスト
  `restoring_the_previous_session_reopens_its_tabs_across_a_real_relaunch`
  (`tests/integration.rs`) が実際に `velox` バイナリを 2 回起動して
  (Xvfb + `dbus-run-session` 環境で) 検証: 1 回目でタブを 2 つ開いて
  終了 → `session.json` の内容 (URL・順序・active_index) を確認 →
  2 回目を `VELOX_RESTORE_SESSION=1` かつセッションと無関係な
  `VELOX_HOMEPAGE` で起動 → ホームページが一切読み込まれず、復元された
  アクティブタブが自分の URL を読み込み、もう一方の復元タブへの
  `switch` が `tab_resume` (= 本物の休止からの復帰) として記録される
  ことまで確認済み。
- [ ] 1 タブの WebView 障害でブラウザ全体が終了しない — wry 0.56 に
  Windows/Linux 向けの検知フックが無いため、能動的な検知・自動復旧は
  実装していない。構造的には各タブの `WebView` は `ui::window` の
  `TabId` ごとの独立したエントリであり、あるタブのエンジンコールバック
  が他のタブやイベントループ自体に触れる経路はコード上存在しないが、
  これは実機での検証が必要な主張であり、テストで固定化できていない。
- [ ] 復旧不能なページでもエラーUIを表示できる — 同上の理由でスコープ外。
  wry の `with_on_page_load_handler`/`with_navigation_handler` はページ内容
  レベルのナビゲーション失敗理由 (DNS/TLS 等) を判別可能な形で渡してこない
  ため、根拠のあるエラー UI をこの Issue の範囲で実装することを見送った。
  各エンジンの既定のエラーページ (WebKitGTK/WKWebView/WebView2 がナビゲー
  ション失敗時に自前で描画するもの) がある程度この役割を代替している。
- [x] セッション保存データが破損しても起動不能にならない — 上記
  「破損データの扱い」の節と `browser::session`/`browser::persistence` の
  テストで担保。

### Revisit condition

(1) Windows/WebView2 の `ICoreWebView2::add_ProcessFailed` を `unsafe` な
COM 呼び出しで直接ラップする道は、CLAUDE.md が Windows を最優先する以上
最初に検討すべき次の一手 — ただし `unsafe` 原則禁止の例外化を伴うので、
実装前に別途方針判断が要る。(2) wry の将来バージョンが
`WebViewExtWindows`/`WebViewExtUnix` にクラッシュ通知を追加したら、この
D65 を更新した上でそちらに乗り換える。(3) 「復旧不能なページのエラー UI」
は、まずナビゲーション失敗理由を判別できる hook の有無を別途調査してから
着手すべきで、本 Issue のスコープには含めなかった。(4) 設定画面 (#30) が
できたら `VELOX_RESTORE_SESSION` を UI トグルに昇格させる。(5) macOS/Linux
は CLAUDE.md の方針どおり「動作すれば十分」の最小実装 (`Tabs::restore`/
`persistence` は 3 OS 共通の純粋 Rust なので実質差分は無いが、実機検証は
Windows を優先し、macOS/Linux は未検証のまま)。

## D66: サイトデータ管理 (#26) — `wry::WebView::clear_all_browsing_data()` で全消去、origin 単位はエンジンごとに非対称で見送り

**対象**: Issue #26 (関連 #7 — プライベートブラウジング、D14/D15/D49 で
実装済み)。CLAUDE.md「対応 OS の優先度」により Windows を最優先して調査した
上で、Linux (CI・性能計測環境) で実装・実測している。

### 既存のデータ境界 (D14/D15/D49) — この Issue はその上に「削除」を足すだけ

この Issue に着手する前提として、通常モード/プライベートモードのデータ境界
は既に D14/D15/D49 で実装済みであることを確認した。新しい境界は導入して
いない:

- **通常モード**: `ui::window::BrowserWindow::context: Option<WebContext>`
  が `Some(WebContext::new(None))` — toolbar と全タブの content webview が
  この 1 つを共有する (D49)。永続ストアで、下記の実測どおり
  `$XDG_DATA_HOME/velox`/`$XDG_CACHE_HOME/velox` (Linux) 相当の場所に
  Cookie 以外のサイトデータが残る。
- **プライベートモード**: `context` は最初から `None`。toolbar・各タブの
  content webview はそれぞれ `.with_incognito(true)` で個別に
  `WebContext::new_ephemeral()` (WebKitGTK) /
  `nonPersistentDataStore()` (WKWebView) /
  `SetIsInPrivateModeEnabled(true)` (WebView2) を持ち、**互いに共有せず、
  ディスクにも残らない** (D15)。`ui/window.rs` 1361 行目以降の
  `WebviewIsolation` のドキュメントコメントが「private なら related view
  も張らない」ことまで含めて既にこの境界を明文化している。
- 両モードは同一プロセス内で排他 (D14: プロセス全体に効くフラグ、共存しない)
  なので、本 Issue の削除処理がモードを跨いでデータへ触れる経路はコード上
  存在しない。`clear_all_site_data`(下記) はこの前提の上に実装した。

### エンジンごとの永続化方式 (受け入れ条件「エンジンごとの永続化方式をdocsに
記録」の本体)

`~/.cargo/registry/src/*/wry-0.56.1` を実際に読み、`data_directory: None`
(VeloX が今使っている呼び出し方 — 上記) のときに何が起きるかを確認した:

- **WebKitGTK (Linux/BSD)** — `src/webkitgtk/web_context.rs` 30-49 行目:
  `data_directory` が `None` のときは `WebContext::builder()` に何も足さず
  `.build()` するだけで、`webkit2gtk::WebsiteDataManager` を明示的に作らない
  ため、**WebKitGTK 自身のデフォルト `WebsiteDataManager` が使われる**。
  `create_context` (65-74 行目) が `ApplicationInfo::set_name(env!(
  "CARGO_PKG_NAME"))` = `"velox"` を設定しており、これがデフォルトの保存先
  ディレクトリ名に使われる。実機で確認した実際の値は次項。**Cookie の永続化
  だけは `Some(data_directory)` 分岐 (37-43 行目) でしか
  `cookie_manager.set_persistent_storage(...)` を呼んでいない** — VeloX は
  常に `None` を渡しているため、今の実装では Cookie はメモリ内のみで
  **プロセスを再起動すると失われる** (下記「副次的発見」)。
- **WKWebView (macOS)** — D15 が既に確認済み: `WKWebsiteDataStore` の既定
  (persistent) ストアが `Library/WebKit/<bundle id>/WebsiteData/` 相当の
  場所に Cookie・キャッシュ・各種ストレージをまとめて持つ。ここは Cookie も
  含め既定で永続化される (WebKitGTK と異なり明示設定不要)。
- **WebView2 (Windows)** — `src/webview2/mod.rs` 288-296 行目:
  `data_directory` が `None` のときは空の `HSTRING` を
  `CreateCoreWebView2EnvironmentWithOptions` の `userDataFolder` 引数に渡す
  。WebView2 はこれを「既定値を使う」の意味に扱い、実行ファイルのパス由来で
  自動的に `<実行ファイルの場所>/<実行ファイル名>.exe.WebView2/` 相当を
  導出する (Microsoft のドキュメント記載の既定挙動) — **WebKitGTKと違い、
  何も指定しなくても実行ファイルごとに自然に分離される**ため、他アプリの
  データと混在するリスクは WebKitGTK より低い。Cookie を含め既定で永続化
  される。

### 実機で確認した Linux のディレクトリ配置 (推測ではなく実測)

`WebContext::builder().build()` の実際の保存先はライブラリのドキュメント
コメントだけでは確定できない (libwebkit2gtk C 実装依存) ため、`xvfb-run` +
独立した `HOME`/`XDG_*` の下で実際に VeloX (デバッグビルド) を起動し、
Cookie と `localStorage` をセットする自作ページを読み込ませて、起動前後の
ファイル差分を取った:

```
$XDG_CACHE_HOME/velox/CacheStorage/salt
$XDG_CACHE_HOME/velox/WebKitCache/Version 17/Records/<hash>/Resource/<hash>
$XDG_CACHE_HOME/velox/WebKitCache/Version 17/salt
$XDG_DATA_HOME/velox/history.json                      ← 既存 (D10)
$XDG_DATA_HOME/velox/hsts-storage.sqlite
$XDG_DATA_HOME/velox/localstorage/http_<host>_<port>.localstorage(-wal/-shm)
$XDG_DATA_HOME/velox/mediakeys/v1/salt
$XDG_DATA_HOME/velox/storage/salt
```

**わかったこと 3 点**:

1. `WebContext::new(None)` は「エンジンの共有デフォルト」ではなく、
   `ApplicationInfo` の `"velox"` という名前のおかげで **既に VeloX 専用の
   ディレクトリ**(`persistence::default_data_dir()` が history/bookmarks に
   使っているのと同じ `$XDG_DATA_HOME/velox`)に事実上分離されている。他
   アプリとの混在は無い — 当初懸念していた「WebKitGTK 全体の共有デフォルト
   を誤って触る」リスクは実測で否定された。
2. **サイトデータと VeloX 自身の永続化ファイル (`history.json` 等) が同じ
   ディレクトリに同居している。** ファイルシステムを直接 `rm -rf` する方式
   だと、`history.json`/`bookmarks.json`/`input_history.json` を巻き込んで
   消してしまう事故が起きうる — 「安全な削除」を素朴なディレクトリ削除で
   実装しなかった理由の実測的な裏付けである。
3. **内部レイアウトはバージョン依存で非公開**(`WebKitCache/Version 17/...`
   のように libwebkit2gtk のキャッシュフォーマットバージョンがパスに焼き
   込まれている)。ファイルシステムを直接操作する実装は将来の libwebkit2gtk
   更新で静かに壊れる — エンジン自身の API を使うべき理由がここにもある。
4. 前述のとおり Cookie 用のファイルは一切現れなかった — 実測でも
   「Cookie は永続化されていない」という上記のソース読解を裏付けた。

### 削除の実装: `wry::WebView::clear_all_browsing_data()` — 3 エンジンとも
public かつ安全な API が既にあった

Issue の指示 (Issue #22/D59 が Windows 側拡張トレイトの見落としで結論を
覆した前例) に倣い、`WebViewBuilder` の `with_*` 系だけでなく `WebView`
構築後に呼べるメソッド・拡張トレイトの双方を確認した。その結果、**`unsafe`
な COM/objc 呼び出しを VeloX 側で書く必要は無かった** — wry 0.56.1 自身が
`wry::WebView::clear_all_browsing_data(&self) -> Result<()>` という public
メソッドを、VeloX が対象とする 3 エンジン全てに実装済みで公開している:

- `src/lib.rs` 2251-2253 行目: `WebView::clear_all_browsing_data` は
  `self.webview.clear_all_browsing_data()` に委譲するだけの薄いラッパ。
- `src/webkitgtk/mod.rs` 920-931 行目: `context.website_data_manager()` を
  取り、`WebsiteDataManagerExtManual::clear(WebsiteDataTypes::ALL,
  TimeSpan::from_seconds(0), None, |_| {})` を呼ぶ。`unsafe` はこの関数の
  中に一切現れない (`webkit2gtk` クレート側が `unsafe` を内包している)。
- `src/wkwebview/mod.rs` 832-841 行目:
  `configuration().websiteDataStore()` から
  `removeDataOfTypes_modifiedSince_completionHandler(allWebsiteDataTypes,
  epoch, handler)` を呼ぶ。こちらは objc 呼び出しのため関数全体が
  `unsafe` ブロックだが、**wry 側の実装**であり VeloX 側で書く unsafe では
  ない。
- `src/webview2/mod.rs` 1808-1817 行目:
  `self.webview.cast::<ICoreWebView2_13>()?.Profile()?
  .cast::<ICoreWebView2Profile2>()?.ClearBrowsingDataAll(&
  ClearBrowsingDataCompletedHandler::create(Box::new(move |_| Ok(()))))` —
  Issue #22/D59 が発見した `WebViewExtWindows`/`ICoreWebView2_13::Profile()`
  と全く同じ経路を、**wry 自身が既にこの用途向けに実装済み**だった。ここも
  `unsafe` は wry 内部にあり、VeloX 側のコードには一切現れない。

いずれの実装も「全消去」(Cookie・キャッシュ・
local/session storage・IndexedDB・Service Worker 等をまとめて) であり、
`WebContext`/`WKWebsiteDataStore`/`ICoreWebView2Profile` という**共有ストア
単位**で効く。個々の `WebView` に対して呼んでも、その `WebView` が属する
ストアそのものを消すため、D49 で共有した `WebContext` を使う通常モードでは
どの webview から呼んでも同じ範囲が消える。

**実装**: `ui::window::BrowserWindow::clear_all_site_data(&self) ->
SiteDataClearResult` が toolbar の webview と全タブ (`ContentTab::webview`
が `Some` のもの全て) それぞれに対して `clear_all_browsing_data()` を呼び、
成功数・失敗数・最初のエラーを集計して返す。`ToolbarCommand::ClearSiteData`
(`clear_site_data` IPC) → `app.rs` の `clear_all_site_data` ヘルパー →
`browser::site_data::summarize(attempted, failed)` が結果を
`ClearOutcome::{Success, Partial, AllFailed, Nothing}` に判定し、`Partial`/
`AllFailed` のときだけ stderr にログする (完全成功時は無言 — 既存の
`persist_*`/`log_io_failure` と同じ流儀)。UI は履歴パネルに「サイトデータを
削除」ボタンを追加しただけ (`ui/toolbar.html`) — 履歴とは独立した操作で、
`ClearHistory` のように VeloX 自身の状態を触ることはない。

**通常/プライベート両モードで同じコードが正しく動く理由**: toolbar +
全タブへ「毎回」試みる設計にしたことで、`self.private` によるモード分岐を
一切書かずに済んでいる。通常モードでは全 webview が同じ `WebContext` を
共有しているため冗長 (無害) だが、休止中タブしか無くても常に生きている
toolbar 経由で必ず 1 回は成功する。プライベートモードでは toolbar と各タブ
がそれぞれ独立した ephemeral ストアを持つ (D15) ため、生きている webview
**全部**に対して個別に呼ばないと一部のタブの Cookie/ストレージが消し
残る — 全 webview を毎回試みる設計はこの両立を自動的に満たす。

### 安全性: 実行中ロックの危険性をどう避けたか

Issue が名指しした「実行中の WebView が掴んでいるファイルを消す危険性」は、
**ファイルシステムを直接操作する設計を採らなかったことで、そもそも発生し
ない**。`clear_all_browsing_data()` はエンジン自身の API 呼び出しであり、
ファイルのオープン・クローズ・ロック解放はすべてエンジン (WebKitGTK/
WKWebView/WebView2) 内部が担う — VeloX のコードはファイルパスを一切
知らない・触らない。加えて:

- 失敗しても呼び出し元 (`clear_all_site_data`) がパニックすることはない —
  `wry::Result<()>` を集計するだけで、`unwrap`/`expect` は使っていない。
- 1 つの webview の呼び出しが失敗しても残りへの試行は止めない (受け入れ
  条件「削除失敗時に安全にエラー処理される」) — `attempt` クロージャは
  エラーをカウントするだけで早期リターンしない。
- WebKitGTK 実装 (`clear`) はコールバックの結果を無視する
  (`|_| {}`/`move |_| Ok(())`) 非同期発火型で、呼び出し自体は即座に返る —
  VeloX 側で完了を待ち合わせるロジックを書いていないぶん、待機中に UI が
  固まる心配もない (ただし裏を返すと「呼び出しが返った時点でまだ消去が
  完了していない」余地はある。実測では `wait 500ms` 後には
  `localstorage`/キャッシュのレコードファイルは既に消えていたので実用上は
  問題ないが、「呼び出しが返った瞬間に完全に消えている」保証はしていない
  ことを明記しておく)。
- 実測 (`xvfb-run` 経由、自作の Cookie/localStorage セットページ → `wait
  1500ms` → 削除呼び出し → `wait 500ms` → 終了) で、`localstorage/*.
  localstorage` と `WebKitCache/.../Records/...` のレコードファイルが
  消え、同じディレクトリに同居する `history.json` はそのまま残ることを
  確認した — 「サイトデータだけを消し、VeloX 自身の履歴/ブックマークは
  巻き込まない」という設計目標を実測で裏付けた。

### 見送ったもの: origin 単位の削除

Issue の実装内容が挙げる「origin 単位のデータ管理」は、**危険な実装を避け
るため今回は見送り、設計だけ記録する**。3 エンジンの調査結果が非対称
だったことが理由:

- **WebKitGTK (Linux)**: `webkit2gtk::WebsiteDataManagerExtManual::fetch`/
  `remove` (`webkit2gtk-2.0.2/src/website_data_manager.rs` 14-116 行目) は
  `unsafe` を要求しない安全な Rust API で、`WebsiteData::name()`
  (`auto/website_data.rs` 19-35 行目) がレコードのドメイン名を返す —
  `fetch` して名前でフィルタし `remove` すれば origin 単位の削除ができる。
  到達経路は `wry::WebViewExtUnix::webview()` (`src/lib.rs` 2426-2427 行目)
  → `webkit2gtk::WebView` → (`webkit2gtk::WebViewExt` の)
  `website_data_manager()`。`v2_16` フィーチャが必要だが、VeloX (`wry`) は
  `webkit2gtk/v2_40` を要求しており `v2_16` を包含するので使える
  (`webkit2gtk-2.0.2/Cargo.toml` 55-70 行目、`v2_18`→…→`v2_16` の
  カスケード)。
- **WebView2 (Windows)**: `ICoreWebView2Profile2::ClearBrowsingData` /
  `ClearBrowsingDataInTimeRange` (`webview2-com-sys-0.38.2/src/bindings.rs`
  31371-31421 行目) は `COREWEBVIEW2_BROWSING_DATA_KINDS` というデータ種別
  でのフィルタしか持たず、**origin/ドメインでのフィルタは無い**。唯一
  ドメイン単位が効くのは Cookie だけ — `ICoreWebView2CookieManager::
  DeleteCookiesWithDomainAndPath` (同ファイル 11390-11409 行目)。つまり
  Windows では「origin 単位の全消去」は Cookie 以外 (localStorage/
  IndexedDB/キャッシュ) には存在しない。到達経路自体は #22/D59 と同じ
  `WebViewExtWindows::webview()` → `ICoreWebView2_13::Profile()` で問題
  ないが、`unsafe` な COM 呼び出しをこの実機テストができない環境で書いて
  「Windows 最優先」の看板の下に出すには、カバレッジが Cookie だけの
  中途半端な機能になってしまう。
- **WKWebView (macOS)**: `WKWebsiteDataStore` にも
  `fetchDataRecordsOfTypes:completionHandler:`/
  `removeDataOfTypes:forDataRecords:completionHandler:` という同種の
  record 単位 API がある (Apple のドキュメント記載) が、wry はこれを
  `WebViewExtMacOS` のような形で公開しておらず、VeloX 側で新たに objc
  呼び出しを書く必要がある。macOS は CLAUDE.md の方針上「最低限の整備」
  対象であり、動作確認もできない。

**結論**: 3 エンジンのうち安全に (`unsafe` を書かずに) origin 単位を実装
できるのは WebKitGTK だけで、しかも Linux は優先度の低い OS。Windows は
実装できても Cookie だけの部分的な機能になり、しかも実機で検証できない。
「消す対象が明確でないなら範囲を狭める」の原則に従い、**今回は全消去のみを
実装し、origin 単位はこの設計を土台にした将来の Issue に回す**。次に着手
する際の実装ポイントは上記の到達経路 (Linux: `WebViewExtUnix::webview()` +
`WebsiteDataManagerExtManual`、Windows: `WebViewExtWindows::webview()` +
`ICoreWebView2CookieManager` で Cookie のみ) としてそのまま使える。

### 副次的発見: 通常モードの Cookie は現状ディスクに永続化されていない (Linux)

本 Issue の対象ではないが安全性検討の過程で見つけたため記録する。上記の
とおり `wry` は `WebContext::new(None)` (VeloX が常に使う呼び方) のとき
`cookie_manager.set_persistent_storage(...)` を一切呼ばない
(`webkitgtk/web_context.rs` の `Some(data_directory)` 分岐にしか無い) ため、
**通常モードであっても Linux (WebKitGTK) では Cookie はメモリ内のみで
保持され、プロセスを再起動すると失われる**。実機実測でも `$XDG_DATA_HOME/
velox`/`$XDG_CACHE_HOME/velox` の下に Cookie ファイルは一度も現れなかった。
プライベートモードと違い「意図した」非永続化ではなく、`WebContext::new`
に `data_directory` を渡していないことの副作用と見られる。ログイン状態が
再起動のたびに失われるのは製品として望ましくない可能性が高いが、Cookie
永続化を有効にする変更 (`WebContext::new(Some(dir))` への切り替え) は本
Issue のスコープ (削除・データ境界) を超え、别のトレードオフ (どの
ディレクトリを使うか、既存の `persistence::default_data_dir()` と衝突しな
いか等) の検討を要するため、ここでは変更せず新規 Issue 化を推奨する
記録に留める。

### テスト・検証

- `browser::site_data::summarize` の判定ロジックはエンジン非依存の純粋
  関数として `src/browser/site_data.rs` に実装し、4 パターン
  (0 件/全成功/一部失敗/全失敗) を単体テストした。
- `ToolbarCommand::ClearSiteData` の IPC パース・`toolbar.html` に対応する
  ボタン/ハンドラが存在することを、既存の `ClearHistory` 系テストと同じ
  形で追加した。
- `BrowserWindow::clear_all_site_data` 自体 (wry 呼び出し) は自動テストの
  対象にしていない — この項目が使う `wry::WebView::clear_all_browsing_data`
  は wry 自身が実装・提供する API であり、`evaluate_script`/`load_url`
  など他の wry 呼び出しと同様、VeloX の統合テスト (`tests/integration.rs`)
  も個々の wry API の効果までは検証しない方針 (同ファイル冒頭のコメント)
  に合わせた。代わりに、`xvfb-run` 上で実際に VeloX を起動し Cookie/
  localStorage をセットしたページを読み込ませ、`clear_all_browsing_data`
  相当の呼び出し (一時的に `AutomationCommand::Mark` へ差し替えて確認し、
  検証後に元へ戻した — この差し替えはコミットに含めていない) 後に該当
  ファイルが消え `history.json` は残ることを手動で確認した。
- Windows 側は `cargo check --target x86_64-pc-windows-msvc --lib` で型
  レベルの整合は確認したが、実機での動作確認はできていない
  (`wry::WebView::clear_all_browsing_data` の WebView2 実装自体は wry 側の
  既存コードであり、VeloX が新規に書いた unsafe コードは無い)。

**Revisit condition**: (1) origin 単位の削除 (上記見送り分)。(2) Cookie の
永続化 (副次的発見、別 Issue 候補)。(3) `clear_all_browsing_data` の
WebKitGTK 実装が完了を待たない非同期発火型であることの影響 — 呼び出し
直後に webview を破棄する・ページを再読み込みする等のタイミングでは消去
の完了前に別の書き込みが走る余地が理論上ある。VeloX の現在の呼び出し方
(ボタン契機、その後は特に何もしない) では実害が無いと判断したが、将来
「削除 → 即座に別処理」のような使い方を足す場合は要再検討。(4) Windows/
macOS の実機検証 (`clear_all_browsing_data` が実際にファイルを消すこと)
は今回の環境では不可能だった。

## D67: 設定画面と永続設定基盤 (#30) — `browser::settings` に一元化し、
`Config::apply_settings`/`to_settings` で相互変換、Appearance のみ即時
反映・他は次回起動反映

**対象**: Issue #30 (関連 #10)。CLAUDE.md「対応 OS の優先度」により
Windows を最優先の判断基準としたが、実装・実測は本リポジトリの他 Issue
と同じく Linux (CI・性能計測環境) で行っている。

### 全体設計: 「起動設定 (`Config`)」と「永続ユーザ設定 (`Settings`)」を
明確に分離する

受け入れ条件の「ConfigとUIが適切に分離される」を、既存のアーキテクチャ
方針 (`docs/architecture.md` の 4 層分離、テスト可能なロジックは
`src/browser/` に置く) にそのまま乗せる形で解釈した:

- **`browser::settings::Settings`**(新規、`src/browser/settings.rs`) —
  UI が変更でき、ディスクに永続化される設定値そのもの。`General` /
  `Appearance` / `Search` / `Privacy` / `Performance` / `Downloads` /
  `Advanced` の 7 カテゴリを持つ、serde 駆動のプレーンな構造体群。
  `browser::session::SessionSnapshot` と同じ形 — UI/エンジン非依存、
  `sanitize()` で不正値を修復し、単体テストが主戦場。`Security` と
  `Shortcuts` は永続フィールドを持たず (後述)、設定画面には表示用の
  読み取り専用タブとしてのみ存在する。
- **`config::Config`** — 既存どおり「起動時に一度だけ解決される設定」の
  ままとし、新しいフィールドを増やさない代わりに `Settings` との相互
  変換を 2 つのメソッドとして持たせた:
  - `Config::apply_settings(&mut self, settings: &Settings)` — 永続化
    済みの `Settings` を `Config` の対応フィールドへ一方向にコピーする。
  - `Config::to_settings(&self) -> Settings` — その逆方向。`search_engine`
    は 5 つの組み込みプリセットの値と完全一致するかを比較し、一致しなけ
    れば `"custom"` + name/template として往復させる (プリセット名を
    保持するフィールドが `Config` 側に無いため、値の完全一致で復元する
    しかない — 起動時に一度 `VELOX_SEARCH_ENGINE=google` 等で選ばれた
    ものであれば問題なく `"google"` に戻る)。
- **`ui::toolbar`/`ui/toolbar.html`** — 設定画面の描画とフォーム入力は
  完全に UI 層の責務。`ToolbarCommand::UpdateSettings { settings:
  Box<Settings> }`(サイズの大きいバリアントを避けるため Box 化、
  clippy `large_enum_variant`)/`ResetSettings` の 2 コマンドで、
  カテゴリごとの粒度ではなく `Settings` ドキュメント全体を都度置き換える
  設計にした — 20 個近いフィールドそれぞれに専用 IPC コマンドを作るより、
  1 つの「保存」ボタンでフォーム全体を JSON にまとめて送る方が
  IPC プロトコルの複雑度を大きく減らせる (既存の `EditBookmark` 等、
  複数フィールドを 1 コマンドにまとめる先例に倣った)。
- **`browser::persistence`** — 既存の `history.json`/`bookmarks.json`
  などと同じパターンで `settings.json` を追加 (`load_settings`/
  `save_settings`)。読み込み失敗 (欠落・破損・型不一致) は
  `load_session` と同じ契約で `None` を返し、呼び出し側
  (`app::run`)が `Settings::default()`/`Config::to_settings()` に
  フォールバックする。

### 開発中に見つけた設計バグとその修正: `apply_settings` は
「`settings.json` が実在するときだけ」呼ぶ

実装の初期版では `app::run` が起動のたびに

```rust
let settings = load_settings(dir).unwrap_or_default().sanitize();
config.apply_settings(&settings);
```

という形で無条件に `apply_settings` を呼んでいた。これは**一見自然だが
致命的な後退バグ**だった: `settings.json` が存在しない (=まだ設定画面を
一度も使っていない、フレッシュチェックアウトやテスト環境) 場合、
`Settings::default()` の「オフ」「未設定」な値が `Config` の対応
フィールドへ無条件に上書きされ、`Config::from_env_and_args` が直前に
読んでいた `VELOX_RESTORE_SESSION`/`VELOX_MAX_LIVE_TABS`/
`VELOX_PERF_METRICS` などの環境変数がすべて無効化されてしまう。

これは本 Issue の統合テストスイート (`tests/integration.rs`) を実行して
初めて発覚した — 8 本中 6 本が失敗し (`restoring_the_previous_session_
reopens_its_tabs_across_a_real_relaunch` が `VELOX_RESTORE_SESSION` を、
`live_tab_cap_suspends_background_tabs_and_switching_back_resumes_them` が
`VELOX_MAX_LIVE_TABS` を、`startup_completes_and_records_a_startup_event`
ほか perf 系 3 本が `VELOX_PERF_METRICS`/`VELOX_PERF_FORMAT` をそれぞれ
握り潰されて失敗していた)、CLAUDE.md がテスト層を分けている理由
(D46/D47 — ユニットテストだけでは検出できない配線の問題) をまさに体現す
る形になった。

**修正**: `app::run` は `persistence::load_settings` が `Some` を返した
とき (=`settings.json` が実在し、パースにも成功したとき) だけ
`apply_settings` を呼ぶ。`None` のとき (未作成・破損・データディレクトリ
未解決) は代わりに `Config::to_settings()` で「今まさに有効な `Config`」
から `Settings` を逆算し、それを `AppState::settings` の初期値として使う
— 環境変数由来の値を設定画面が黙って上書きすることは無くなり、かつ
初めて設定画面を開いたときに「今動いている値」がそのまま表示される
(何も変更せず保存しても挙動が変わらない)。`config::tests::to_settings_
round_trips_back_through_apply_settings` で `Config -> to_settings ->
apply_settings` が恒等写像になることを、任意の (デフォルトでない)
`Config` に対して検証した。

### どのフィールドが「即時反映」でどれが「次回起動反映」か

9 タブすべてを即時反映にするコストは、`browser::blocklist::FilterList`・
`browser::suspension::SuspensionPolicy`・`PerfLog` 等が軒並み起動時に
一度だけ構築されウィンドウ/タブのクロージャに焼き込まれている
(`ui::window::content_webview_builder` 等) 現在のアーキテクチャ全体の
可変化を要求し、本 Issue のスコープを大きく超えると判断した。代わりに
以下の 2 段構えとした:

- **`Appearance` タブ (`theme`/`show_bookmark_bar`) だけは即時反映**。
  どちらも `Config` を経由しない — `theme` は `ui::window::BrowserWindow::
  set_theme` が `veloxSetTheme(...)` を `evaluate_script` するだけの
  純粋な CSS 変数切り替え (`:root[data-velox-theme]`、Web ページ本体の
  `prefers-color-scheme` には一切触れない — wry 0.56 にはその
  per-webview 上書き手段が無い)。`show_bookmark_bar` は既存の
  `BrowserWindow::set_bookmark_bar_visible` (Issue #19 由来、
  `Cell<bool>`) をそのまま呼ぶだけで済んだ。ついでに
  `docs/architecture.md` に残っていた「ブックマークバーの表示状態は
  セッションのみで再起動すると消える (#30 待ち)」という既知の制約も、
  この Issue で実際に解消した (`Settings::appearance::show_bookmark_bar`
  として永続化し、起動時に `window.set_bookmark_bar_visible` へ反映)。
- **それ以外 (General/Search/Privacy/Performance/Downloads/Advanced) は
  次回起動時に反映**。`Config::apply_settings` が起動時に一度だけ呼ばれ、
  以降そのプロセスの生存期間中は変わらない。設定画面のフッターに
  「一部の変更は次回起動後に反映されます」という注記を出し、ユーザに
  誤解させないようにした。

### Downloads タブは実際に配線した (当初は保留を検討したが撤回)

`download_dir_override` は当初「永続化はするが実際の効果は無い」まま
出す案も検討したが、実装コストが小さく既存パターン (`content_blocking_
enabled` を `ContentPolicy` 経由で `content_webview_builder` に運ぶのと
同型) で機械的に済むと分かったため撤回し、実際に配線した:
`Config::download_dir_override` → `ui::window::BrowserWindow`(生成時に
1 回コピー) → `ContentPolicy`/トップレベルの `with_download_handlers`
呼び出し 2 箇所 → `browser::downloads::resolve_download_dir_with_override`
(新規、`resolve_download_dir` のラッパ。空白のみ/`None` なら既存の
`VELOX_DOWNLOAD_DIR`/プラットフォーム既定にフォールバック)。
`app::open_downloads_folder`(「フォルダを開く」ボタン) も同じ関数を
経由するよう変更した。次回起動後に反映される点は他の非 Appearance
フィールドと同じ。

### Security タブ・Shortcuts タブは読み取り専用 (永続フィールドを持たない)

Issue の実装内容一覧には Security・Shortcuts も含まれるが、両者は
「編集可能な新しい永続設定」を追加するのではなく、**既存の状態を見せる
読み取り専用ビュー**として設定画面に組み込んだ:

- **Security** — `browser::site_permissions::SitePermissionStore`
  (Issue #24/D60 で実装済み) の内容を、`app::AppState` にも
  `Arc::clone` した参照を持たせて表示するだけ。D60 が既に指摘している
  とおり、wry 0.56 の `with_permission_handler` は「オリジン単位の
  事前登録された allow/block を読む」ことしかできず、動作中に新しい
  決定を書き込む経路(カスタム許可 UI)自体が無い。設定画面から
  書き込む口を新設するには、この `Arc`(現在は不変のスナップショット)
  を可変化した上で `with_permission_handler` 側のクロージャとも
  共有し直す必要があり、`unsafe` なしで安全にやるなら
  `Arc<Mutex<...>>` 化が要る — 本 Issue のスコープでは見送り、
  「表示のみ」に留めた。
- **Shortcuts** — `browser::settings::shortcut_reference()` という
  静的なテーブル (アクション名 + キーの組、11 件) を返すだけの純粋関数
  を追加し、設定画面はそれを一覧表示するのみ。キーバインドの再割り当て
  機能自体はこの Issue では実装していない — `ui/toolbar.html` の
  keydown リスナーと `ui::window::ContentShortcut`(D18/D23) の 2 経路に
  ハードコードされたショートカットをユーザ定義可能にするには、キー
  コンフリクト検出・両チャンネル (信頼済み toolbar / 非信頼 content)
  への設定反映など、それ自体で 1 つの Issue になる規模のため。

### テスト・検証

- `browser::settings`: 27 件 (デフォルト値、JSON 往復、前方/後方互換性
  ―フィールド欠落・未知フィールド・スキーマバージョン、`sanitize` の
  各カテゴリごとの修復規則、ショートカット一覧の非空性・重複無し
  チェック、巨大/Unicode 入力でパニックしないこと)。
- `browser::persistence`: `settings.json` の読み書き往復・欠落・破損・
  切り詰め・旧バージョン (フィールド欠落) からの読み込みを既存の
  `history.json` 等と同じテスト形状で追加 (6 件)。
- `browser::downloads`: `resolve_download_dir_with_override` の
  上書き優先・トリム・空/未設定時のフォールバックを 3 件追加。
- `config`: `apply_settings`/`to_settings` の相互変換 (デフォルト値が
  恒等写像になること、各カテゴリのコピー、サスペンションポリシーの
  オーバーライド、検索エンジンのプリセット判定・カスタムエンジン往復)
  を 15 件追加。
- `ui::toolbar`: 新規 `ToolbarCommand`(`update_settings`/
  `reset_settings`)のパース、`Panel::Settings` のスクリプト生成、
  `SettingsView` の JSON シリアライズ、`Theme` の JS 側スクリプト生成を
  5 件追加、既存の `toolbar_html_declares_expected_hooks` に設定画面の
  フック文字列を追加。
- 統合テスト (`tests/integration.rs`) は変更していない (8 本のまま) が、
  上記の `apply_settings`/`to_settings` バグ修正の検証そのものとして
  全数グリーンであることを確認した — この Issue が壊しかけた既存機能
  (env var 駆動の起動設定) を統合テストが実際に検出した実例として、
  D46/D47 の「ユニットテストだけでは検出できないクラスの回帰がある」
  という主張を追加で裏付けている。

### 満たせなかった/見送った点

- **キーボードショートカットの再割り当て自体は未実装**(上記 Shortcuts
  タブの節を参照)。表示のみ。
- **Security タブからのサイト権限の変更・削除は未実装**。表示のみ。
- **Appearance 以外のカテゴリはプロセス再起動なしに反映されない**。
  UI 上はフッターの注記で明示しているが、「保存した瞬間にすべて反映
  される」ことを期待するユーザには驚きになりうる — 将来的に
  `browser::blocklist::FilterList`/`SuspensionPolicy` 等を `Cell`/
  `Arc<AtomicXxx>` 化して真の即時反映にする余地はあるが、既存の
  `ui::window` の設計 (タブごとのクロージャへ値を焼き込む) を広範に
  触ることになるため別 Issue とした。
- **`Settings.performance.auto_suspend_after_ms`(ミリ秒)** のような
  一部フィールドは UI 上ミリ秒単位のまま表示しており、分/秒単位への
  変換など UX 上のこなれた表現は行っていない。
- **Windows/macOS の実機確認は未実施**。`cargo check --target
  x86_64-pc-windows-msvc --all-targets` による型チェックのみ。設定画面
  自体は `toolbar.html`(全プラットフォーム共通の HTML/CSS/JS) の変更が
  主体で、Windows/macOS 固有分岐は今回のフィールド追加 (`ContentPolicy`
  への `download_dir_override` 追加等) で増えていないため、リスクは
  低いと考えているが未検証であることは明記する。

**Revisit condition**: (1) Appearance 以外の即時反映化 (上記見送り分)。
(2) キーボードショートカットの再割り当て機能 (別 Issue 相当の規模)。
(3) Security タブからのサイト権限編集 (D60 の恒久的な制約 — wry の
`with_permission_handler` にカスタム UI 差し込み口が無い問題を再調査
した上でないと着手できない)。(4) Windows/macOS 実機での設定画面の動作
確認 (Issue #33 の「3 OS リリースビルド検証」の一部として)。

## D68: 複数ウィンドウ対応 (#29) — `browser::WindowId`/`Windows` を新設、`TabId` はウィンドウ内でのみ一意という前提のまま `UserEvent` に `WindowId` を明示的に付与する

**対象**: Issue #29。依存関係として挙げられている #11 (タブ管理)・#12
(タブ状態) は実装済み。関連 Issue #27 (プライベートブラウジング) の
「Private Window を別ウィンドウとして開く」という将来要件を見据え、拡張点を
D14 の記述と整合する形で残した (詳細は本項最後の節)。

### 設計判断: `Tabs` 自体は変更せず、その上に `Windows` (複数の `Tabs` の集合) を足す

`docs/architecture.md`/D20 が確立した層分離 — `browser::`(UI/エンジン非依存の
純粋ロジック、単体テストの主対象) / `app.rs`(配線) / `ui::`(wry/tao) — を
そのまま踏襲した。既存の `browser::tabs::Tabs`(1 ウィンドウぶんのタブ集合+
アクティブ index、既に十分にテストされ、`app.rs`・`browser::automation` の
他、`velox-bench` のシナリオ生成コードまで広く依存している) には一切手を
入れず、その上に「複数の `Tabs` を管理する」新しいレイヤーを 1 つ足す設計に
した:

- `src/browser/window_id.rs` — `WindowId(u64)`。`browser::tab::TabId` と
  全く同じ形 (`Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord`
  derive、`From<u64>`、`get()`)。
- `src/browser/windows.rs` — `Windows { entries: Vec<{id: WindowId, tabs:
  Tabs}>, next_id: u64 }`。`open_window`/`open_restored_window`/
  `close_window`/`tabs`/`tabs_mut`/`contains`/`ids`/`len`/`is_empty` のみを
  公開する薄いラッパーで、`Tabs` の中身には一切踏み込まない。

この 2 ファイルは `wry`/`tao` は疎か `ui::` にも依存しない純粋 Rust なので、
`browser::tabs` と同じく単体テストの主対象にした (`windows.rs` に 12 件:
一意な id 発行・id の非再利用・ウィンドウ間のタブ隔離・
`close_window`/`open_restored_window` の挙動など)。

**なぜ `Tabs` 自身にウィンドウの概念を混ぜ込まなかったか**: `Tabs` は
`browser::automation`(Issue #112)・`velox-bench` のシナリオ生成
(`generate_bench_script`)・既存の数十件の単体テストまで、「1 つのタブ集合」
を前提にした API (`new(initial_url)`、`open`/`open_at` が `self.next_id`
から連番を払い出す、等) にかなり広く依存されている。ここに
`Vec<Windows>`/`WindowId` を混ぜ込む変更は影響範囲が大きく、動作実績のある
既存コード・既存テストを壊すリスクが高い。「既存の `Tabs` は 1 ウィンドウの
状態機械として完成している」という前提を守ったまま、複数ウィンドウは
「`Tabs` を複数個持つ」という一段上のレイヤーで表現する方が、変更を
`browser::windows.rs`(新規ファイル) と `app.rs`(配線の書き換え) に閉じ込め
られ、レビュー可能なサイズに収まると判断した。

### 副作用: `TabId` はウィンドウをまたいで一意ではない

上記の設計の直接の帰結として、**`TabId` はもはやプロセス全体で一意ではない**。
`Windows::open_window` は毎回 `Tabs::new`(または `Tabs::restore`) を呼ぶが、
`Tabs::new` は `next_id: u64 = 0` から数え始める実装のままなので、2 つの
ウィンドウがそれぞれ `TabId(0)` を持つのは正常な状態である
(`browser::windows` のテスト
`each_window_has_its_own_independent_tab_id_space` で明文化)。
`docs/architecture.md`/`TabId` のドキュメントコメントが謳う「一度発行された
`TabId` は使い回されない」という保証は **1 つの `Tabs` インスタンス内でのみ**
成立し、ウィンドウをまたいだ一意性は最初から要求しない設計にした。

これを `Tabs` 側 (`Windows` がグローバルなカウンタを注入する等) で解決する
選択肢も検討したが、`Tabs::new`/`Tabs::restore`/`Tabs::open_at` のシグネチャ
変更が波及する既存コード・既存テストの量に対して得られる利益
(「`TabId` 単体でウィンドウを逆引きできる」) が見合わないと判断し、見送った。
代わりに次の節の通り、ウィンドウをまたいで届くイベント側に `WindowId` を
明示的に持たせることで曖昧さを解消した。

### `UserEvent` への `WindowId` の付与 — `TabId` からの逆引きに頼らない

`app::UserEvent` のうち、特定のタブ・ウィンドウの webview から送られる
バリアント (`ToolbarMessage`・`NavigationStarted`・`NavigationBlocked`・
`SubresourceBlocked`・`LoadStarted`・`LoadFinished`・`PageTitleResolved`・
`FaviconResolved`・`OpenDevtoolsRequested`・`ContentShortcut`・
`NewTabRequested`) はすべて `WindowId` を新たに (または追加で) 持つように
した。`ui::window::BrowserWindow` は自分自身の `WindowId`(構築時に
`browser::Windows::open_window`/`open_restored_window` が発行したものを
そのまま受け取る) を `id: WindowId` フィールドとして保持し、上記イベントを
送るすべてのクロージャに `own_id`(`Copy` なので複数クロージャへのキャプチャ
は既存の `TabId` キャプチャと同様に安全かつ低コスト) として焼き込む。

上の副作用の節で書いた通り `TabId` 単体ではどのウィンドウの話か分からない
以上、`app.rs` の `handle_user_event` が `state.windows.window_of(tab_id)`
のような逆引きをする設計は成立しない (2 つのウィンドウが同じ `TabId` を
持ちうるため)。イベントの送信元 (`ui::window::BrowserWindow`) が最初から
自分の `WindowId` を知っているので、逆引きさせず素直に持たせるのが最も
単純かつ安全な設計だった。

**`DownloadStarted`/`DownloadCompleted` にも `window_id` を追加した**が、
`browser::DownloadStore` 自体は D28 の設計のままプロセス全体で 1 つの
共有ストアである — ダウンロード一覧そのものをウィンドウごとに分割する変更
ではない。`window_id` はあくまで「どのウィンドウのダウンロードパネルを
即座に再描画するか」を決めるためだけに使う (次の「見送ったもの」節を参照)。

一方、`AutomationCommand`(`Mark`/`Quit`/`Wait` を除く各コマンド) と
`MemorySampled` には `WindowId` を追加していない。理由はそれぞれ以下の
「自動化スクリプトの複数ウィンドウ対応」「自動休止ポリシー」の節に譲る。

### `ui::window::BrowserWindow` — `id`/`tao_id()` の追加、`SitePolicies: Clone`

- `BrowserWindow::new` は `event_loop: &EventLoopWindowTarget<UserEvent>` を
  既に引数に取っていた (`app::run` からの呼び出し1箇所のみが前提だった
  だけで、シグネチャ自体はイベントループ実行中の追加ウィンドウ生成にも
  そのまま使える形だった) ため、新規ウィンドウの生成自体に構造変更は不要
  だった。追加したのは `id: WindowId` 引数と、`Self` に生えた 2 つの
  アクセサ: `id() -> WindowId`(自分自身の id) と
  `tao_id() -> tao::window::WindowId`(`tao` 自身が
  `Event::WindowEvent { window_id, .. }` で報告してくる方の id — 名前が
  同じ "WindowId" でも別の型なので、実装側で明確に呼び分けている)。
- `SitePolicies`(`blocklist`/`site_exceptions`/`site_permissions`、いずれも
  `Arc`) に `#[derive(Clone)]` を追加した。新しいウィンドウを開くたびに
  最初のウィンドウと全く同じ 3 つの `Arc` を複製 (実体は refcount のみ増加)
  して渡すためで、複数ウィンドウが同じ `FilterList`/`SiteExceptions`/
  `SitePermissionStore` インスタンスを共有する — フィルタ設定やサイト権限は
  「アプリ全体で 1 つ」のままにする、という選択を明示した。

### `app.rs` — `AppState.tabs: Tabs` → `AppState.windows: Windows`、`ui_windows: HashMap<WindowId, BrowserWindow>`

`app::run` は今までただ 1 つの `window: BrowserWindow` 変数を
イベントループのクロージャにキャプチャしていたが、これを
`ui_windows: HashMap<browser::WindowId, ui::window::BrowserWindow>` に
置き換えた。`AppState`(browser 層寄りの純粋な状態) は `windows: Windows`
を持ち、UI 層の実体 (`ui_windows`) とは別に管理する — `AppState` 自体は
引き続き `wry`/`tao` の型を一切知らない (`window_event_proxy` を
`AppState` のフィールドにしなかった理由も同じで、後述)。

- **イベントディスパッチ**: ほぼ全てのハンドラ関数
  (`handle_toolbar_command`・`handle_content_shortcut`・
  `handle_automation_command`・`open_new_tab`・`close_tab`・
  `activate_and_refresh`・`sync_tab_strip`・… ) が、既存の
  `window: &mut BrowserWindow` パラメータに加えて `window_id: WindowId` を
  並行して受け取るようになった。関数本体内の `state.tabs.foo()` は
  `tabs_of(state, window_id).foo()` (新設のヘルパー、
  `state.windows.tabs_mut(window_id).expect(...)`) に機械的に置き換わって
  いる。`expect` を使っているが、これは「`window_id` が指すウィンドウは
  呼び出し時点で `ui_windows` に実在することを呼び出し元が既に確認済み」
  という不変条件に基づくものであり (`ui_windows`と`state.windows`は
  `open_new_window`/`close_window_by_tao_id` の 2 箇所でのみ、常に両方
  同時に変更される — シングルスレッドのイベントループなのでこの不変条件は
  常に成立する)、CLAUDE.md が禁じる「乱用」ではなく 1 箇所に集約した
  ドキュメント付きの invariant-backed `expect` である。
- **`tao::window::WindowId` → `browser::WindowId` の解決**:
  `Event::WindowEvent { window_id, .. }`(`CloseRequested`/`Resized`) は
  `tao` 自身の id しか持たないため、`window_by_tao_id_mut`/
  `close_window_by_tao_id` が `ui_windows.values()` を線形探索して対応する
  `BrowserWindow`(`tao_id()` で比較) を見つける。同時に開くウィンドウ数は
  現実的には数個〜十数個程度であり、イベントの都度線形探索しても実用上
  問題にならないと判断し、逆引き用の別マップは追加しなかった。
- **ウィンドウを閉じる = そのウィンドウのリソース解放**
  (受け入れ条件「終了時のリソース解放が正常」): `close_window_by_tao_id`
  が `ui_windows.remove(&id)` で `BrowserWindow`(`tao::window::Window` と
  その全 webview を所有) を drop し、`state.windows.close_window(id)` で
  対応する `Tabs` も破棄する。**最後の 1 枚を閉じたらプロセスを終了する**
  (`state.windows.is_empty()` を見て `ControlFlow::Exit`) — これは macOS の
  慣習 (最後のウィンドウを閉じてもアプリは常駐し続ける) ではなく
  Windows/Linux の慣習を採用したもので、CLAUDE.md の「Windows を最優先」
  方針に従った判断である。macOS 向けにこの挙動を変える対応は行っていない
  (docs/decisions.md の他の D と同様、意図的に見送った OS 差分として
  ここに記録する)。

### Ctrl/Cmd+N の配線 — 新しいコマンド 3 つが同じ `app::open_new_window` に集約する

既存のタブ操作 (Ctrl/Cmd+T 等) が `ToolbarCommand`(信頼された toolbar
webview 発の構造化コマンド) と `ContentShortcut`(信頼されない content
webview 発の固定センチネル文字列、D18/D23 の trust boundary) の 2 経路を
持つのと全く同じパターンで、`ToolbarCommand::NewWindow`/
`ContentShortcut::NewWindow` を追加した (`velox:new-window` センチネル、
`tab_shortcut_script`/toolbar.html 双方の keydown リスナに `Ctrl/Cmd+N` を
追加)。加えて自動化スクリプト向けに `AutomationCommand::NewWindow`
(`new_window` コマンド、引数なし) も追加し、3 経路すべてが
`app::open_new_window` という 1 つの実装に集約する — 既存の
`open_new_tab`/`close_tab` が全トリガーの集約点になっているのと同じ設計
方針である。

`open_new_window` は `browser::Windows::open_window` で新しい `WindowId`+
`Tabs` を確保し、`ui::window::BrowserWindow::new` を
`state.site_policies.clone()` と (`EventLoopProxy` の複製)
`window_event_proxy` で呼び出して実際の OS ウィンドウを構築、成功したら
`ui_windows` に挿入する。`BrowserWindow::new` が失敗した場合 (実運用では
起こらないはずだが、最初のウィンドウと全く同じ構築処理なので理論上は
同じ失敗モードを共有する) は `state.windows` 側に作った空のエントリも
`close_window` で巻き戻し、プロセス全体は落とさずログだけ出す —
既存の `?` を使わない `log_failure` パターンと同じ思想。

**`ToolbarCommand::NewWindow`/`ContentShortcut::NewWindow` は
`handle_toolbar_command`/`handle_content_shortcut` の中では処理しない**、
`handle_user_event` の時点で横取りする、という実装上の制約がある: これらの
関数は既に `window: &mut BrowserWindow`(`ui_windows.get_mut(&window_id)` の
借用) を受け取っており、新しいウィンドウを開くには `ui_windows` 全体への
`&mut` が必要で、両方を同時に借用することは Rust の借用規則上できない。
そのため `handle_user_event` は `ToolbarCommand`/`ContentShortcut` を
パースした直後、`ui_windows.get_mut` する前に `NewWindow` かどうかを見て
先に `open_new_window` を呼ぶ。`handle_toolbar_command`/
`handle_content_shortcut` 自身の `match` にも `NewWindow` の腕は残して
あるが (`enum` を網羅する必要があるため)、中身は空で「ここには来ない」旨の
コメントのみを置いた。

### 自動化スクリプト (`VELOX_AUTOMATION_SCRIPT`) の複数ウィンドウ対応 — 「現在のウィンドウ」を切り替えるだけの最小拡張

`browser::automation` は Issue #112 の時点で「1 つのタブ集合」を前提に
`open`/`switch`/`close`/`suspend` をタブ strip 上の 0-based 位置で指定する
設計になっており、これを本格的に複数ウィンドウ対応させる (例:
`switch <window> <index>` のような構文にする) のは本 Issue のスコープを
大きく超える。代わりに **`new_window` という引数なしコマンドを 1 つ追加し、
「以降の `open`/`switch`/`close`/`suspend`/`navigate` は新しく開いたウィンドウを
対象にする」** という最小の拡張にとどめた。

`app::run` は `automation_window: WindowId`(可変、初期値は最初のウィンドウ)
をイベントループのクロージャにキャプチャしており、
`UserEvent::Automation(AutomationCommand::NewWindow)` を受けたら
`open_new_window` を呼んで返ってきた新しい id で `automation_window` を
更新する。他のすべての `AutomationCommand` は
`ui_windows.get_mut(automation_window)` で得た「現在のウィンドウ」に対して
実行される。**この設計により `AutomationCommand` 自体に `WindowId` を
追加する必要がなかった** — 「今操作対象になっているウィンドウ」という
1 個のグローバルな可変状態を `app::run` 側に持つだけで済んだ。

この拡張の統合テストとして `tests/integration.rs` に
`new_window_retargets_automation_and_shuts_down_cleanly` を追加した:
`new_window` の後の `open`/`switch` が実際に新しいウィンドウの `Tabs` に
届いていること (`tab_create`/`tab_switch` perf レコードの件数で検証) と、
2 枚のウィンドウが開いたままの状態で `quit` してもプロセスが正常終了する
こと (受け入れ条件「終了時のリソース解放が正常」) を、実際に `velox` を
起動して確認している。

### 見送ったもの・既知の制約 (次の Issue で拾うべきもの)

複数ウィンドウ対応は影響範囲が広く、CLAUDE.md
の「無理に1 PRに詰め込まず、レビュー可能なサイズを保つ」方針に従い、
以下は明示的に本 Issue のスコープ外とした:

1. **セッション復元 (#25) は最初のウィンドウのみが対象**。
   `AppState::primary_window`(起動時に開いた最初のウィンドウの
   `WindowId`) を新設し、`persist_session` は
   `window_id == state.primary_window` のときだけ書き込む。Ctrl/Cmd+N で
   開いた 2 枚目以降のウィンドウのタブは `session.json` に一切残らず、
   次回起動時は常に最初のウィンドウ 1 枚(+その復元されたタブ)から始まる。
   複数ウィンドウのセッション復元は `SessionSnapshot` のスキーマ自体を
   「ウィンドウの配列」に変える必要があり、D65 の設計を拡張する形の
   別 Issue が必要と判断した。
2. **タブ自動休止ポリシー (#63) はウィンドウごとに独立して評価する**。
   `app::sweep_tabs` は `state.windows.ids()` の各ウィンドウに対して
   個別に `suspension::plan` を呼ぶ — `max_live_tabs`/メモリ予算は
   「ウィンドウごと」の上限になり、複数ウィンドウ合計に対するグローバルな
   予算にはなっていない。また `pending_memory_sample`(1 回のメモリ
   サンプルにつき 1 回だけ消費される) は最初に評価されたウィンドウの
   sweep でしか消費されないため、あるサンプルが複数ウィンドウの休止判断に
   同時に反映されることはない (次のサンプルが来れば他のウィンドウにも
   順番に反映される)。ウィンドウをまたいだグローバルな予算共有は
   `suspension::plan`/`Candidate` の設計をウィンドウ横断に拡張する必要が
   あり、見送った。
3. **ダウンロードパネルの即時反映は操作したウィンドウのみ**。
   `browser::DownloadStore` はプロセス全体で共有の 1 つのストアのままだが
   (D28 のまま変更なし)、`DownloadStarted`/`DownloadCompleted` が
   `refresh_downloads_panel` を呼ぶのは、その通信が発生した
   (`window_id` が指す) ウィンドウのパネルのみである。別のウィンドウを
   開いていても、そちらのダウンロードパネルはそのウィンドウ自身で何か
   操作する (パネルを開閉する等) まで最新の一覧に更新されない。全ウィンドウ
   への即時反映は、`window: &mut BrowserWindow`(`ui_windows.get_mut` の
   借用) を保持したまま `ui_windows` 全体を不変イテレートする必要があり、
   上記の「Ctrl/Cmd+N の配線」節と同種の借用の競合を、頻度の高いパスに
   対して都度回避する実装が必要になる。ホットパスではない (ダウンロードの
   開始/完了は頻度が低い) ため対応コストに対して価値が低いと判断し
   見送ったが、実装ポイントとして記録しておく。
4. **サイトデータ削除 (#26/D66) はトリガーしたウィンドウの webview のみ**。
   `clear_all_site_data(window: &BrowserWindow)` は変更しておらず、複数
   ウィンドウ環境では「サイトデータを削除」ボタンを押したウィンドウの
   toolbar + 全タブの webview にしか `clear_all_browsing_data()` を
   呼ばない。D66 の実測 (`WebContext::new(None)` は `ApplicationInfo` の
   アプリ名 (`"velox"`) 由来で全ウィンドウが実質同じ保存先ディレクトリを
   指す) を踏まえると、Cookie/キャッシュ等の永続データ自体は 1 つの
   ウィンドウから消せば (エンジンが同じ保存先を指している限り) 他の
   ウィンドウの分もまとめて消えている可能性が高いが、**別ウィンドウの
   生きている webview がメモリ上に保持している状態(その後の書き込みで
   復活しうる)までは消せない**。「全ウィンドウのサイトデータを確実に
   消す」ボタンにする場合は (3) と同じ借用の課題を解決する必要があり、
   別 Issue に切り出す方が安全と判断した。
5. **`Config::private`(プライベートブラウジング) は引き続きプロセス全体で
   1 つ**。今回変更していない。D14 が既に書いていた「複数ウィンドウが
   実現したときの拡張路線」がそのまま使える形で残っていることを本 Issue
   で確認した:
   - `Config::private` は今も `ui::window::BrowserWindow::new` の 1 呼び
     出しあたりの構築時パラメータであり (`app::open_new_window` は
     `config: &Config` を丸ごと渡している)、これを「プロセス全体の
     `Config`」から「ウィンドウごとに異なりうる値」に変えるには、
     `AppState`/`open_new_window` の呼び出し元が
     `config.private` の代わりに「この新規ウィンドウは Private にする
     か」という個別のフラグを渡すように変えるだけでよい — `BrowserWindow`
     側のコード (`.with_incognito(config.private)` 等) は変更不要。
   - `AppState::history_enabled`(bool 1 個) は D14 が予告した通り、
     「ウィンドウ(または `Windows` の各エントリ)ごとの値」に変える必要が
     ある。本 Issue で `Windows` という「ウィンドウごとの状態を持つ場所」
     が既に存在するようになったので、次に着手する Issue は
     `WindowEntry`(`browser::windows.rs`) に `private: bool`
     (または同義の値) を足し、`record_visit_if_enabled`/
     `record_input_history_if_enabled`/`persist_session` の
     `state.history_enabled` 参照を `state.windows.tabs(window_id)` 経由の
     ウィンドウごとの値に置き換える、という具体的な道筋が見えている。
   - toolbar のプライベートバッジ/ウィンドウタイトルの `— プライベート`
     接尾辞は既に `BrowserWindow::new` の構築時に `config.private`(1 個の
     bool) から計算されているだけなので、ウィンドウごとの bool さえ届けば
     そのまま動く。
   - **Issue #27 が実装すべきこと (本 Issue が明示的に残した宿題)**:
     (a) `WindowEntry`/`AppState` にウィンドウごとの private フラグを追加、
     (b) `open_new_window`(または新しい `open_private_window`) が
     `Config::private` の代わりにそのフラグを見て `BrowserWindow::new` を
     呼ぶ、(c) `Ctrl/Cmd+Shift+N` 相当の「新しい Private Window」トリガーを
     Ctrl/Cmd+N と同じ 3 経路 (`ToolbarCommand`/`ContentShortcut`/
     `AutomationCommand`) に追加。`WindowId`/`Windows`/
     `ui_windows: HashMap<WindowId, BrowserWindow>` という土台そのものは
     本 Issue で完成しているため、#27 は「新しい種類のイベント配線」を
     1 つ足すだけで済むはずである。

### テスト・検証

- `browser::windows`(新規): 12 件 — id の一意性・非再利用、ウィンドウ間の
  タブ隔離、`close_window`(最後の 1 枚を閉じられる/2 回閉じても安全)、
  `open_restored_window`、`len`/`is_empty` の整合性。
- `browser::automation`: `new_window` のパース (`parses_
  new_window_and_rejects_arguments_on_it`) を追加。
- `ui::window`: 新しいセンチネル `velox:new-window`/
  `ContentShortcut::NewWindow` を、既存の「全センチネルを網羅する」
  テスト (`tab_shortcut_script_captures_expected_combos_in_capture_phase`/
  `parse_content_shortcut_matches_every_sentinel_exactly`) に追加。
- `ui::toolbar`: `{"cmd":"new_window"}` のパース、および
  `toolbar_html_declares_expected_hooks` に `new_window` の存在確認を追加。
- `tests/integration.rs`(新規 1 件):
  `new_window_retargets_automation_and_shuts_down_cleanly` — 実際に
  `velox` を起動し、`new_window` → `open`/`switch` が新しいウィンドウの
  タブ操作として実行されること (perf レコード件数で検証) と、2 枚のウィンドウ
  が開いたまま `quit` してもプロセスが正常終了 (exit code 0) することを
  確認した。
- テスト件数: `cargo test --lib` は本 Issue 着手前 700 件 → 713 件
  (`browser::windows` 12 件 + `automation::new_window` 1 件)。
  `cargo test --test integration`(`xvfb-run` + `dbus-run-session` 経由) は
  8 件 → 9 件。既存テストの削除・スキップ化は行っていない。
- `cargo fmt --check`/`cargo clippy --all-targets -- -D warnings` は
  警告ゼロ。`cargo check --target x86_64-pc-windows-msvc --all-targets`
  (`ui::webview2_blocking` への `WindowId` 引数追加を含む) も型レベルで
  通過を確認したが、CLAUDE.md D61 の通りリンク・実行はしていないため、
  Windows 実機での動作確認はできていない。

**Revisit condition**: 上記「見送ったもの」5 点、特に (1) 複数ウィンドウの
セッション復元と (5) Private Window (#27) は、両方とも独立した Issue として
着手可能な状態にある。(2)(3)(4) はいずれも「頻度の低いパス、または
グローバル予算/即時反映という追加要件が実際に必要になったら」拾えばよい
優先度の低い改善として記録するに留める。

### 追記: 複数ウィンドウ × ページ内検索 (#43, D69) の統合

**経緯**: 本 PR (#29) を `main` に merge する時点で、Issue #43(ページ内検索、
D69、PR #147)が既に `main` にマージ済みだった。#43 は本 Issue が存在しない
前提 (単一ウィンドウ) で実装されており、`AppState::find: Option<
browser::find::FindState>` という**プロセス全体で 1 個だけのグローバルな
検索セッション**を持つ設計だった。`browser::find::FindState` 自体は
`tab_id: TabId` しか保持していないため、これをそのまま複数ウィンドウ環境に
持ち込むと、本 D68 が明示している「`TabId` はウィンドウ内でのみ一意」という
前提により、以下 3 箇所で実際にバグになることが判明した:

1. ウィンドウ B のタブ 0 をナビゲートすると、`state.find.as_ref().is_some_and(|f| f.tab_id() == id)` の `id` 比較がウィンドウ A のタブ 0 と衝突し、
   無関係なウィンドウ A の検索バーが閉じられる。
2. `UserEvent::FindMatchesUpdated { tab_id, total }` が `WindowId` を持たない
   ため、DOM 検索の結果イベントがどのウィンドウ宛てかを区別できず、
   `window.set_find_status`/`highlight_find_match` が別ウィンドウの
   `BrowserWindow` に適用されうる。
3. タブ切替時の `close_find_bar` 呼び出し (`activate_and_refresh`) が
   `state.find.is_some()` というグローバル判定のため、ウィンドウ B での
   タブ切替がウィンドウ A の検索バーを閉じてしまう。

**決定: 検索セッションをウィンドウ単位の状態にする** — 本 D68 が既に確立した
「`browser::Windows` の各 `WindowEntry` がその窓固有の状態を持つ (`tabs:
Tabs` がその筆頭)」という設計パターンをそのまま踏襲し、`find:
Option<browser::find::FindState>` を `WindowEntry` に追加した
(`src/browser/windows.rs`)。`AppState::find` フィールド自体は削除し、
`Windows` に `find`/`find_mut`/`set_find`/`take_find` という 4 つのアクセサ
(`tabs`/`tabs_mut` と対になる形) を追加して、`app.rs` からは
`state.windows.find(window_id)` のように必ず `WindowId` 付きで参照する形に
した。`FindState` 自体 (`src/browser/find.rs`) は無改修— `TabId` しか
知らなくてよい、という D69 の設計は変えていない。「global な `Option`
1 個」ではなく「ウィンドウごとに独立したセッション」を選んだのは、実際の
ブラウザの挙動 (ウィンドウ A で "foo" を検索している間に、ウィンドウ B で
別に "bar" を検索できる) に合わせるためで、global な 1 個にすると
「後から開いた方が必ず前のウィンドウの検索を強制終了させる」という
D68 の設計原則にもそぐわない挙動になっていた。

`UserEvent::FindMatchesUpdated` にも `window_id: WindowId` を追加した
(発火元は `ui::window::BrowserWindow::search_in_page` — `self.id` を
`evaluate_script_with_callback` のクロージャにキャプチャするだけで済んだ)。
これで上記 3 箇所はすべて次のように解消される:

1. `state.windows.find(window_id).is_some_and(|s| s.tab_id() == id)` —
   `window_id` が一致する `WindowEntry` の中でしか `tab_id` を比較しない。
2. `UserEvent::FindMatchesUpdated { window_id, tab_id, total }` を受けた
   `handle_user_event` がまず `window_id` で `BrowserWindow`/`Tabs` を
   解決してから `state.windows.find_mut(window_id)` を見るため、結果が
   別ウィンドウに漏れることがない。
3. `activate_and_refresh` の判定を `state.windows.find(window_id).is_some()`
   に変更し、その `window_id` 自身の検索セッションだけを見るようにした。

`open_find_bar`/`close_find_bar`/`update_find_query`/`step_find`
(いずれも `handle_toolbar_command`/`handle_content_shortcut` から
`window_id: WindowId` を既に受け取っている呼び出し元を持つ) は全て
`window_id: WindowId` を追加の引数として受け取るように変更した — 配線
自体は本 Issue (#29) の側で既に `window_id` を全ハンドラに通していたため、
届いていなかったのは「`state.find`(グローバル)を見る」というロジックの
部分だけだった。

**テスト**: `src/browser/windows.rs` に、2 つのウィンドウが偶然同じ
`TabId` を持つ状況 (`each_window_has_its_own_independent_tab_id_space` と
同じ前提) で検索セッションが独立していることを検証する単体テストを
6 件追加した:
`a_new_window_has_no_find_session`、
`find_sessions_are_independent_per_window`(本題 — 一方の `set_find` が
他方に漏れないこと)、
`taking_one_windows_find_session_never_closes_anothers`(上記 3 番の
バグの再現)、
`find_mut_edits_only_the_targeted_windows_session`、
`set_find_take_find_and_find_mut_are_noops_for_an_unknown_window`、
`closing_a_window_drops_its_find_session_without_a_panic`。
`app.rs` 側の統合 (`UserEvent::FindMatchesUpdated`/`activate_and_refresh`
の分岐) 自体は既存の統合テスト方針 (D47: wry 呼び出し自体は統合テストの
対象にしない) に従い、`browser::windows` の単体テストでロジックを、
実際の 2 ウィンドウでの目視相当の検証は行っていない — 見送った検証として
下記に記録する。

**見送った検証**: 実際に 2 つの `BrowserWindow` を開いて同時に別々の
検索語で検索し、互いのハイライト/件数表示が混線しないことを実機
(または統合テスト) で確認することはしていない。`tests/integration.rs`
は `VELOX_AUTOMATION_SCRIPT` 経由の駆動のみで、ページ内検索の
`ToolbarCommand`(`OpenFindBar`/`FindQuery`/...) は自動化コマンドの
対象になっていないため、既存の自動化の仕組みだけでは統合テスト化でき
ない。将来 #43 側で検索コマンドを自動化スクリプトに追加する機会があれば、
その時に複数ウィンドウの統合テストも追加するのが自然と考える。

### 追記: 複数ウィンドウ × 設定画面 (#30, D67) の統合

**経緯**: 本 PR (#29) を `main` に再度取り込んだ時点で、Issue #30(設定画面と
永続設定基盤、D67、PR #148)も `main` にマージ済みだった。#30 も単一ウィンドウ
前提で実装されており、`AppState::settings: Settings` はプロセス全体で
1 個だけの共有ドキュメントである点は元々ウィンドウの概念に依存していない
ため問題ないが、**設定を変更した際の即時反映経路 (`app::apply_updated_
settings`) が単一の `window: &mut BrowserWindow` しか受け取らない**設計
だったため、複数ウィンドウ環境で次の 2 点が問題になることが判明した:

1. 設定画面で Appearance (テーマ・ブックマークバー表示) を変更して保存
   すると、**保存操作をしたウィンドウの chrome にしか反映されず**、
   他のウィンドウは次にそのウィンドウ自身で何か操作するか再起動するまで
   古いテーマ/表示のままになる。
2. 同様に、他のウィンドウで設定画面を開いていた場合、そちらの表示 (フォーム
   の内容) が保存内容を反映せず古いままになる。

**決定**: `app::apply_updated_settings` のシグネチャを `window: &mut
BrowserWindow` から `ui_windows: &mut HashMap<WindowId, BrowserWindow>`
(呼び出し元が持つマップそのもの) に変更し、`ui_windows.values()` を
全走査して **開いている全ウィンドウ**に対して `set_theme`/
`set_bookmark_bar_visible`/`refresh_settings_panel` を適用するようにした。
`ToolbarCommand::UpdateSettings`/`ResetSettings` は (`NewWindow` と全く同じ
理由 — `window: &mut BrowserWindow` という 1 エントリの借用と
`ui_windows` 全体への `&mut` は同時に成立しない) `handle_toolbar_command`
の中では処理せず、`handle_user_event` が `window_id` を解決する前に
横取りするように変更した。

`state.settings`(D67 のプロセス全体で共有という設計)自体は変更していない
— 「ウィンドウごとに異なる設定を持てるようにする」のではなく、「1 つの
共有設定を、開いている全ウィンドウの chrome に一貫して反映する」ことを
選んだ。これは D68 が D14(プライベートブラウジングも whole-app)で
確立した「アプリ全体で 1 つの値を全ウィンドウが等しく参照する」という
既存の設計方針とも整合する。

さらに、**新しく開いたウィンドウ (Ctrl/Cmd+N, `app::open_new_window`) にも
現在の設定 (テーマ・ブックマークバー表示) を construction 直後に適用**する
ようにした — 起動時に最初のウィンドウへ行っている初期化 (`app::run` の
「apply initial bookmark bar visibility」)と全く同じ処理を、2 枚目以降の
ウィンドウにも行う。これがないと、Ctrl+N で開いた新しいウィンドウだけが
常に既定のテーマ/非表示状態で始まってしまい、既存ウィンドウと見た目が
食い違う。

`Config::download_dir_override` (ダウンロード先の上書き) はこの変更の対象外
とした — D67 が元々「Appearance 以外は次回起動まで反映されない」と明記して
おり、ダウンロード先はウィンドウ構築時に `ContentPolicy`/`with_download_
handlers` へ焼き込まれる値なので、既存の単一ウィンドウ時代から変わらず
「保存後、次に開いたタブ/ウィンドウから新しい値が効く」という挙動のままで
一貫している(今回複数ウィンドウ対応で新たに劣化させた挙動ではない)。

**見送った検証**: 実際に複数ウィンドウを開いた状態で設定画面から
Appearance を変更し、全ウィンドウの chrome が同時に切り替わることを
実機/統合テストで確認することはしていない。ページ内検索の統合と同じ理由
(`tests/integration.rs` は `VELOX_AUTOMATION_SCRIPT` 経由の駆動のみで、
`ToolbarCommand::UpdateSettings`/`ResetSettings` は自動化コマンドの対象に
なっていない) で、既存の自動化の仕組みだけでは統合テスト化できなかった。
`browser::settings`(`Settings`/`sanitize`)自体は本統合で変更しておらず、
既存の単体テストがそのまま有効。`app::apply_updated_settings`/
`app::open_new_window` の全ウィンドウ反映ロジックは実際の
`wry::WebView` 呼び出し (`set_theme`/`set_bookmark_bar_visible`) を伴うため、
D47 の方針 (wry 呼び出し自体は統合テストの対象にしない) に従い自動テストの
対象にしていない。

## D69: ページ内検索 (#43) — 3 エンジンとも自前 JS 実装、ネイティブ find API は Windows を優先する限り使えないと判明

**対象**: Issue #43。Ctrl/Cmd+F・検索 UI・次/前へ移動・件数表示・Esc 終了・
大文字小文字の扱い。依存関係として挙げられている Issue #38 (キーボード
ショートカット管理) はまだ着手されていないため、今回は既存の D18/D23 と
同じ「固定の Ctrl/Cmd+F 割り当て」で最小実装し、後から #38 の仕組みに
載せ替えやすい形にした (後述)。

### 調査: wry 0.56 経由でネイティブ find API に届くか

Issue の指示どおり、自前 JS 実装に踏み切る前に 3 エンジンそれぞれで
ネイティブの「ページ内検索」API に wry 経由で安全に届くかを実際のソース
(`~/.cargo/registry/src/.../wry-0.56.1`,
`webview2-com-sys-0.38.2`, `webkit2gtk-2.0.2`) で確認した — D25/D59/D66 で
確立した「issue の指示に頼らず実ソースを読む」調査スタイルをそのまま踏襲。

- **WebKitGTK (Linux) — 安全なネイティブ API が実在した。**
  `webkit2gtk::WebView::find_controller() -> Option<FindController>`
  (`webkit2gtk-2.0.2/src/auto/web_view.rs` 1019 行目) から
  `FindControllerExt::{search, search_next, search_previous,
  count_matches}` と `counted-matches`/`failed-to-find-text` シグナル
  (`webkit2gtk-2.0.2/src/auto/find_controller.rs`) に届く。到達経路は
  D66 と全く同じ `wry::WebViewExtUnix::webview()`
  (`wry-0.56.1/src/lib.rs` 2427/2444 行目) → `webkit2gtk::WebView`。
  呼び出し側に `unsafe` は要求されない (webkit2gtk クレートが内部で
  `unsafe extern "C"` 呼び出しをラップ済み — D66 の
  `clear_all_browsing_data`/`website_data_manager` と同じ形)。**しかし
  Linux は CLAUDE.md の OS 優先度で最も低い** — ここだけネイティブ実装を
  作っても Windows/macOS には使えず、後述のとおり Windows 側は全く別の
  実装 (JS) が必要になるため、1 機能に 2 系統の実装を抱える非対称さが
  生まれる。
- **WebView2 (Windows) — API 自体は存在するが、実運用には使えないと判断した。**
  `webview2-com-sys-0.38.2/src/bindings.rs` に
  `ICoreWebView2Find`/`ICoreWebView2FindOptions`/
  `ICoreWebView2FindStartCompletedHandler` 一式が確認できた
  (`ICoreWebView2_28::Find() -> ICoreWebView2Find`、42531 行目)。しかし
  これは D59/D66 がこれまで使ってきた `ICoreWebView2_13` (Profile 系) より
  はるかに新しいインターフェース番号であり、対応する WebView2 Runtime も
  相応に新しいバージョンを要求する — Evergreen ランタイムは自動更新
  される前提とはいえ、企業配布端末やオフライン環境では更新が遅れることが
  珍しくなく、`.cast::<ICoreWebView2_28>()` が失敗しうる実機を否定できない。
  加えて `Start`/`FindNext`/`FindPrevious`/`Stop` はいずれもコールバック
  ベースの COM API で、`ICoreWebView2FindStartCompletedHandler`
  相当のハンドラ実装 (D66 の `ClearBrowsingDataCompletedHandler` より
  複雑 — マッチ数変化・アクティブマッチ変化の 2 種類のイベント購読も
  追加で必要) を新たに書く必要があり、**この環境には実機の Windows が無く
  検証もできない**。「Windows 最優先」という方針は「Windows で動く実装を
  最初に作る」ことを求めているのであって、「検証できない可能性のある
  最新 API に賭けて Windows 版だけ作る」ことではないと判断し、見送った。
- **WKWebView (macOS) — wry のデスクトップ実装に find 相当の公開 API は無い。**
  `wry-0.56.1/src/wkwebview/mod.rs` (macOS/デスクトップ本体) には
  find/search 系のメソッドが一切無い。`findString(_:withConfiguration:
  completionHandler:)` 相当のバインディングは
  `wry-0.56.1/src/wkwebview/ios/WKWebView.rs` に存在するが、これは **iOS
  専用ファイル**であり、しかも該当メソッドを囲うフィーチャフラグ
  (`WKFindConfiguration`/`WKFindResult`) がコメントアウトされたまま
  ビルドされていない。デスクトップ版 wry から呼べる経路は存在しない。

**結論**: 3 エンジンのうち安全に実装できるのは WebKitGTK (最低優先度) だけ、
Windows (最優先) は理論上の経路はあるが実機検証不能な最新 API のみ、
macOS はそもそも経路が無い。「Windows を優先し、そこで動く方式を先に選ぶ」
という CLAUDE.md の方針に従い、**3 エンジンとも同じ JS ベースの自前実装に
統一した** — 1 つの実装を 3 OS 共通でテストでき、Windows 版から着手しても
崩れない。WebKitGTK のネイティブ `FindController` は将来 Linux 向けの
個別最適化を検討する際の実装ポイントとして下記の Revisit condition に残す。

### 実装: JS インジェクション + Rust 側の純粋な位置管理

- **`browser::find::FindState`** (`src/browser/find.rs`, 新規) が
  UI/エンジン非依存の純粋ロジックを持つ: クエリ正規化
  (`normalize_query` — trim して空なら `None`)、現在の検索クエリ・
  大文字小文字区別フラグ、DOM から報告されたヒット総数、アクティブな
  マッチの 0-based インデックス、`next_match`/`previous_match` の巡回
  ロジック (0 件は常に `None`、1 件なら自分自身に留まる、末尾から先頭・
  先頭から末尾へラップする)。`docs/architecture.md` の 4 層分離のとおり
  ここは webview を一切知らず、単体テストの主対象 (`src/browser/find.rs`
  のテストモジュール、17 ケース)。
- **`ui::window::BrowserWindow`** が実際の DOM 操作を担う 3 つのメソッド:
  - `search_in_page(tab_id, query, case_sensitive)` — 生成した JS
    (`find_search_script`) を `evaluate_script_with_callback` でその
    タブの content webview に流し、`document.body` 配下のテキストノードを
    `TreeWalker` で走査して一致箇所を `<span class="velox-find-hl">` で
    包み、件数を `JSON.stringify({ total: N })` として返す。件数は
    `UserEvent::FindMatchesUpdated { tab_id, total }` で非同期に返る
    (`fetch_page_title`/`fetch_favicon` と同じ fire-and-forget パターン、
    D12)。
  - `highlight_find_match(tab_id, index)` — 直前のアクティブマッチの
    ハイライトを外し、指定インデックスのマッチに `velox-find-hl-active`
    クラスを付けて `scrollIntoView({block:"center"})` する。
  - `clear_find_highlights(tab_id)` — 挿入した `<span>` をすべて元の
    テキストノードに戻す (`replaceChild` + `normalize()`)。
  三つとも「未知/休止中タブは黙って no-op」という `fetch_page_title` と
  同じ契約。
- **クエリのエスケープ (Issue の指示どおり必須)**: `find_query_literal`
  が `serde_json::Value::String(query).to_string()` で JSON 文字列化した
  うえで、`ui::toolbar::escape_js_line_terminators` (D62 で追加済みの
  U+2028/U+2029 対策) を **そのまま再利用** して `evaluate_script` に渡す
  — 独自の再実装はしていない。マッチングは常に「リテラル部分文字列」
  であり、クエリを正規表現として解釈することは絶対にない:
  `query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")` (MDN 推奨のエスケープ
  スニペット) でメタ文字を全てエスケープしてから `new RegExp` に渡す。
  `find_search_script`/`find_query_literal` には、`"`/`\`/U+2028/U+2029/
  正規表現メタ文字を含むクエリで壊れないことを確認する単体テスト
  (`find_search_script_neutralizes_quotes_and_script_closing_sequences`,
  `find_search_script_neutralizes_regex_metacharacters_in_the_query` 等)
  を追加した — D62 が `set_url_script` 等に対して敷いた「インジェクション
  耐性をテストで固定化する」流儀をそのままなぞっている。
- **検索 UI はブックマークバー方式 — `Panel` ではなく独立した加算コンポーネント**。
  History/Bookmarks/Downloads/Omnibox の `Panel` は `config.panel_height`
  (既定 320px) 分だけ伸びる大きなドロップダウンで、1 行の検索バーには
  過大。D35 のブックマークバーが確立した「`toolbar_height` に独立して
  加算される固定高さの帯」という形をそのまま複製し、`find_bar_height`
  (既定 34px) と `find_bar_visible: Cell<bool>` を `BrowserWindow` に追加、
  `effective_toolbar_height` にも第 4 の加算項として組み込んだ (D35 の
  ときと同じく「パネルやブックマークバーと同時に出ていても構わない」)。
- **UI ↔ Rust の往復**: `ToolbarCommand` に `OpenFindBar`/`FindQuery{query,
  case_sensitive}`/`FindNext`/`FindPrevious`/`FindClose` を追加。検索語の
  入力は 120ms のクライアント側デバウンス (`toolbar.html`) の後に
  `find_query` を送る (Enter/Shift+Enter を押した瞬間はデバウンスを
  flush してから `find_next`/`find_previous` を送るので、直前のキー入力
  が反映されないまま巡回することはない)。DOM 検索が返す件数は
  `veloxSetFindStatus(total, active)` でトールバーに戻り、`アクティブ+1
  /総数` の "N/M" 表示になる。
- **セッションは 1 個・アクティブタブに紐付く (MVP の意図的な単純化)**。
  `AppState::find: Option<find::FindState>` は同時に 1 タブ分しか持たない
  — Ctrl/Cmd+F を押した瞬間のアクティブタブに固定され、別タブに切り替える
  (`activate_and_refresh` 経由のすべてのタブ切替) か、そのタブ自身が
  ナビゲーションを開始する (`NavigationStarted`/`LoadStarted`) と
  `close_find_bar` が呼ばれてハイライトを消し検索バーを閉じる —
  ページが変われば一致位置も無効になるため。バックグラウンドタブごとの
  独立した検索セッションは持たない (Chrome 等はタブごとに保持するが、
  今回はスコープ外とし、`FindState` 自体はいつか `HashMap<TabId,
  FindState>` に載せ替えられる形のまま残している)。

### Ctrl/Cmd+F の割り当てと Issue #38 への申し送り

D18/D23 と全く同じ二重配送 (content webview は固定センチネル文字列
`"velox:open-find-bar"` → `ContentShortcut::OpenFindBar`、trusted な
toolbar webview は構造化コマンド `{"cmd":"open_find_bar"}` を直接送信) を
再利用した。Issue #38 (キーボードショートカット管理) が今後この 2 経路
すべてに乗ってくる設計になる予定のため、今回は次の点を意識して実装した:

- センチネル文字列・`ContentShortcut::OpenFindBar` 列挙子・
  `ToolbarCommand::OpenFindBar` はどれも他のショートカット
  (`ToggleBookmark`, `FocusAddressBar` 等) と全く同じ形で追加しており、
  キーの割り当てを変える設定層を後から差し込む際、割り込む場所は
  1 か所 (JS 側でどのキーを監視するか) だけで済む。
- `app::open_find_bar`/`close_find_bar`/`update_find_query`/`step_find`
  はショートカットの発火経路 (トールバー/コンテンツ/将来の設定 UI) を
  一切知らない、純粋な「find バーを開く/閉じる/更新する」関数として
  切り出してある — #38 が新しいキーバインド管理層を追加しても、
  呼び出し先はこれらの関数のままで変わらない。

### 既知の制限 (意図的に見送ったもの)

- **一致はテキストノード単位**: `<b>` 等のインライン要素をまたいで
  分割されたテキストは 1 つの一致として検出できない (ナイーブな
  テキストノード走査の一般的な制約)。ネイティブブラウザの find は
  DOM 全体をフラット化して検索するため、この制約を持たない。
- **非表示要素の除外なし**: `display:none` 等で隠れたテキストも
  `SCRIPT`/`STYLE`/`NOSCRIPT`/`TEXTAREA`/`INPUT` 以外は検索対象になる
  (ネイティブブラウザは可視テキストのみを対象にすることが多い)。
  可視性判定はコストが高いため、スクリプトを単純に保つことを優先した。
- **正規表現/単語単位検索は無し** — Issue の「大文字小文字/一致方式の
  検討」に対する結論として、大文字小文字の切替のみを実装し、リテラル
  部分文字列一致に固定した (前述のとおりセキュリティ上の理由もある)。
- **Esc は検索入力にフォーカスがあるときのみ閉じる**。ページ本体に
  フォーカスがある状態からの Esc では閉じない — content webview 側で
  常時 Escape を捕捉すると、ページ自身が Esc を使う機能 (モーダルを
  閉じる等) を壊しかねないため、今回は見送った。
- **検索セッションはタブ 1 つに固定** (前述)。

### テスト

- `src/browser/find.rs`: `FindState`/`normalize_query` の単体テスト
  12 件 (正規化、巡回、0/1/複数件、リセット挙動)。
- `src/ui/toolbar.rs`: 新しい `ToolbarCommand` 5 種の IPC パーステスト、
  `set_find_bar_visible_script`/`set_find_status_script` のテスト。
- `src/ui/window.rs`: `effective_toolbar_height` の find bar 加算分の
  テスト、`ContentShortcut::OpenFindBar` のセンチネル解析テスト、
  `find_search_script`/`find_activate_script`/`find_clear_script`/
  `find_query_literal` のインジェクション耐性テスト。
- 単体テスト件数: 700 → 724 (+24、`cargo test --lib -- --list` で計測)。
  減少なし。
- 統合テスト (`tests/integration.rs`) は今回変更していない (8 件のまま、
  全て pass) — find 機能は既存の統合テストが検証する「実プロセス起動・
  実タブ管理・実ファイル永続化」のいずれとも直接関係しないため、新規の
  統合テストは追加していない。
- `cargo check --target x86_64-pc-windows-msvc --all-targets` で型
  レベルの整合は確認したが、実機の Windows/WebView2 での動作確認は
  できていない (この環境に Windows 実機が無いため) — 特に Ctrl+F が
  WebView2 自身のネイティブ既定アクセラレータ (Chromium ベースのため
  存在しうる) と衝突しないかは未検証。F12 (D18) も同種の既定
  アクセラレータだが `event.preventDefault()` だけで問題なく上書き
  できている実績があるため恐らく同様に機能すると考えているが、万一
  WebView2 自身の find UI が併せて出てしまう場合は
  `ICoreWebView2Settings3::AreBrowserAcceleratorKeysEnabled(false)`
  (D59/D66 と同じ `WebViewExtWindows::webview()` 経由) で無効化する
  のが次の一手になる。

**Revisit condition**: (1) Linux 向けにネイティブ `WebKitFindController`
を使う個別実装 (前述の到達経路がそのまま使える) — ただし優先度は低い。
(2) WebView2 の `ICoreWebView2Find` (`ICoreWebView2_28`) — 対象ランタイムの
普及が進み、実機検証できる環境が揃った段階で再検討。(3) テキストノード
境界をまたぐ一致・非表示要素の除外・正規表現/単語単位検索 — 前述の
「既知の制限」。(4) タブごとに独立した検索セッションを保持する
(現在はアクティブタブの 1 セッションのみ)。(5) Ctrl/Cmd+F の割り当てを
Issue #38 のキーバインド管理層に載せ替える。(6) content webview に
フォーカスがある状態からの Esc 対応。


## D70: リリースパッケージング (#41) — Windows は tag/version 整合チェックを追加、Linux は最小 tarball を新設、macOS は明示的に見送り

**対象**: Issue #41 の受け入れ条件 4 点 (3 OS の release artifact 生成 /
tag からの再現可能なビルド / GitHub Release への自動公開 / 配布手順の
docs 記録) を、既存の `release-windows.yml` (D51) と突き合わせて棚卸しした。

### 棚卸し結果 (着手前)

| 受け入れ条件 | 状態 |
|---|---|
| 3 OS の release artifact | 未達 — Windows のみ |
| tag からの再現可能なビルド | 部分達成 (Windows) — `--locked` は使われているが、push したタグと `Cargo.toml` の `version` が一致することを検証していない |
| GitHub Release への自動公開 | 部分達成 (Windows) — tag push で `softprops/action-gh-release` により実施済み |
| 配布手順の docs 記録 | 達成 (Windows) — README に手動/tag push の手順あり |

`release-windows.yml` 自体は D51 の設計 (分離した専用 workflow、
`workflow_dispatch` + `v*` タグ push の 2 起動経路、`--locked`、SHA-256
チェックサム、スモークテストは「実行ファイルの存在とサイズ」) を既に
満たしており、**作り直す必要はなかった**。

### 判断: Windows を最優先に直す、Linux は最小 tarball を追加、macOS は見送る

CLAUDE.md の OS 優先度方針 (Windows 最優先、macOS/Linux は「ビルドが通り
既存機能を壊さない」最低限の整備に留め、3 OS 同時対応を完了条件に据えない)
と、この Issue が依存する #33 (「3 OS でビルド可能な状態を検証できる」を
含む CI 品質ゲート epic) が **macOS を含めないまま完了 (closed) 済み**で
あるという既成事実の 2 点を踏まえ、以下の粒度に決めた。

1. **Windows (`release-windows.yml`)**: 既存の仕組みは維持しつつ、
   「tag と `Cargo.toml` の version が一致しない状態で Release が
   作られてしまう」抜けを塞いだ。タグを打つ前に `Cargo.toml` の
   version を上げ忘れると、GitHub Release のタグ名と zip 内の
   ファイル名が食い違ったまま公開されてしまう — 「tag からの再現可能な
   ビルド」の一貫性を損なう実害のある抜けと判断し、tag push 時のみ
   走る検証ステップ (`tagVersion -ne $cargoVersion` なら `throw`) を
   ビルド前に追加した。既存のビルド・パッケージ・アップロード・
   Release 作成ロジックには手を入れていない。
2. **Linux (`release-linux.yml`, 新設)**: `ci.yml` が既に
   `libwebkit2gtk-4.1-dev` を入れた `ubuntu-latest` で `cargo build`
   (debug) を実行しており、release ビルドまでの追加コストが低いこと、
   かつ CLAUDE.md が Linux を CI/性能計測の実行環境として明示的に
   引き続き使う対象としていることから、**AppImage/deb 等のネイティブ
   パッケージ化はせず**、Windows の zip と同じ構成 (velox, velox-bench,
   README.md, LICENSE) を tar.gz + SHA-256 に固めるだけの最小 workflow を
   新設した。トリガー・tag/version 整合チェック・成果物検証・Release
   添付の流れは `release-windows.yml` と揃えた (同じ `v*` タグで両方の
   workflow が起動し、同じ GitHub Release に zip と tar.gz が並んで
   添付される)。パッケージスクリプト (バージョン取得 →
   ディレクトリ構成 → tar.gz → sha256sum) はこのブランチの Linux 環境で
   実際に `cargo build --release --locked` した成果物を使って手動で
   一度実行し、生成物の展開・チェックサム検証まで確認済み。
3. **macOS**: 今回は着手しない。理由は (a) #33 が macOS を含めずに
   「完了」と判定されており、プロジェクトとして現時点でその判断を
   覆す情報がないこと、(b) macOS の release ビルド (署名なし `.app`/
   `.dmg` の作成、`actions/upload-artifact`・`softprops/action-gh-release`
   との組み合わせ) を検証できる実機/CI 実行環境がこのセッションには
   無く、動かないワークフローを「動く」体で追加するのは D61 が避けた
   ("素通りさせて緑にする") のと同じ失敗パターンになること。README に
   「macOS の release workflow は無い」ことを明記し、将来
   `release-windows.yml`/`release-linux.yml` と同じパターンで追加できる
   ことだけ示した。

### 見送ったもの・未検証のもの

- **AppImage / deb** (Issue 本文が調査対象として挙げていたもの):
  検討の結果、現段階では tar.gz で十分と判断し、実装しなかった。将来
  ディストリビューションパッケージが必要になった時点で別 Issue とする。
- **macOS の `.app`/`.dmg` パッケージング、コード署名・notarization**:
  未着手。コード署名は Windows 分も含め Issue #42 のスコープ
  (README ロードマップにも「Packaging, code signing and notarization for
  macOS / Windows」として記載済み)。
- **実際にタグを打っての公開テスト**: 本 PR ではタグ push を行っていない
  ため、`release-windows.yml`/`release-linux.yml` が実際に GitHub Release
  を作成・添付する一連の流れ (2 つの workflow が同じタグで同時に
  `softprops/action-gh-release` を呼ぶ際の競合を含む) は GitHub Actions
  上で未検証。YAML の構文チェックと、Linux 側はローカルでのビルド・
  パッケージスクリプトの動作確認のみ行った。
- **2 workflow が同じタグに対して同時に Release 作成 API を呼ぶ際の
  競合**: `softprops/action-gh-release` は対象タグの Release が既に
  あれば追記する挙動だが、Windows/Linux 両 workflow がほぼ同時に初回
  作成を試みると、両方が「Release が無い」と判断して作成しに行き、
  片方が失敗し得る。**対策として、タグ push のときだけ両 workflow が同じ
  `concurrency.group` (`release-tag-<github.ref>`) を共有し、
  `cancel-in-progress: false` で直列化した。** キャンセルではなく
  キューイングさせるため、片方の完了後にもう片方が走り、後発は
  「既存 Release への添付」になる。group にタグ名 (`github.ref`) を
  含めているので、別タグのリリース同士は従来どおり並列に走る。
  なお PR / `workflow_dispatch` では Release を作らないため直列化する
  理由が無く、むしろ両 workflow の CI が不必要に待たされる (実際に
  PR #146 で Linux 側の release ジョブが Windows 側の完了待ちになった)。
  そのため group 名を `startsWith(github.ref, 'refs/tags/v')` で分岐させ、
  タグ以外では workflow ごとに別 group (`release-windows-*` /
  `release-linux-*`) にして並列に走らせている。
  なお GitHub の concurrency は「実行中 1 件 + 待機 1 件」しか保持せず
  3 件目以降は待機中のものがキャンセルされる仕様だが、同一タグで走る
  release workflow は 2 つだけなので問題にならない。**この直列化自体は
  実際のタグ push で未検証**であり、初回リリース時に確認すること。

**Revisit condition**: (1) #33 の macOS 除外判断が変わり、macOS の
release workflow 追加に着手できる環境が整ったとき。(2) 実際にタグを
打って Windows/Linux 両方の Release 公開フローを検証したとき (特に
上記の同時実行競合)。(3) ディストリビューション向けパッケージ
(AppImage/deb) の要望が具体化したとき。(4) コード署名 (#42) 着手時に
`release-windows.yml`/`release-linux.yml` の署名ステップを追加する。

## D71: ダークモードとブラウザ UI テーマ (#31) — #30 の資産の棚卸しを行い、
「明示的な Light/Dark がネイティブウィンドウ枠と Private Window 配色に
届いていなかった」2 点のギャップだけを埋める

**対象**: Issue #31 (依存: #30、D67 で実装済み)。CLAUDE.md「対応 OS の
優先度」により Windows を最優先の判断基準としたが、開発・実測は他の
Issue と同じく Linux (CI・性能計測環境) で行っている。

### 棚卸し: #31 の受け入れ条件は #30 (D67) の時点で大半が実装済みだった

着手前に `browser::settings::Theme`(`System`/`Light`/`Dark`)・
`ui::toolbar.html`・`ui::window::BrowserWindow::set_theme`・`app.rs` の
`Ready`/`UpdateSettings` ハンドラを読んだところ、Issue #31 の 4 つの
受け入れ条件のうち 3 つは **#30 で既に実装済み**だったと確認できた
(ゼロから作る要素ではない):

- **「手動で Light/Dark を切り替えられる」**: `AppearanceSettings::theme`
  (`Theme::System`/`Light`/`Dark`) が既にあり、設定画面から選ぶと
  `ToolbarCommand::UpdateSettings` → `BrowserWindow::set_theme` →
  `veloxSetTheme(...)` → `toolbar.html` の `:root[data-velox-theme]` が
  即座に切り替わる (D67)。
- **「再起動後も設定が維持される」**: `Settings` は `settings.json` に
  永続化され (`browser::persistence`)、起動時 `app::run` が
  `ToolbarCommand::Ready` ハンドラで `window.set_theme(state.settings.
  appearance.theme)` を一度push している (`app.rs:1171` 付近) ため、
  明示的な選択は次回起動後も toolbar chrome に正しく反映される。
- **「UI 全体でテーマが統一される」(toolbar/tab strip/bookmark bar/
  dialogs/settings)**: これらは全て `ui/toolbar.html` という 1 枚の
  HTML/CSS に同居しており (別ファイルに分かれていない)、CSS 変数
  (`--bg`/`--fg`/`--field-bg`/`--border`/`--tab-bg` 等) 経由で統一済み。
  `grep` で `#[0-9a-f]{3,6}` のハードコード色を全数確認したが、`:root`/
  `@media`/`[data-velox-theme]` の変数宣言以外に地の色コードは無かった。
  なお VeloX 自身が生成する HTML はこの `toolbar.html` のみ (新規タブ
  ページ相当の専用 HTML は存在せず、ホームページは通常の URL 読み込みで
  済ませている) ため、この Issue の「VeloX 自身が生成する HTML もテーマ
  に追従させる」という注意点についても、追加で対応すべき別ファイルは
  無かった。

未達だったのは実質 1 点、**「OS テーマに追従できる」**の一部 (後述の
ネイティブウィンドウ枠) と、棚卸し中に見つけた **2 つの具体的なバグ**
だった。

### 調査: OS ダークモードの取得は Windows を含む 3 OS とも `tao` 0.37 の
標準 API だけで足りる (追加実装は不要、既に「タダで」動いている部分がある)

指示どおり Windows を最優先に、`tao` 0.37.0 の実ソース
(`~/.cargo/registry/.../tao-0.37.0/src/`) を確認した:

- `tao::window::Window::theme() -> Theme`(`Theme::Light`/`Dark` の 2値)
  — 現在の実効テーマを取得できる。3 OS 全てで実装あり
  (`platform_impl/{windows,macos,linux}/window.rs`)。
- `tao::event::WindowEvent::ThemeChanged(Theme)` — OS 側でテーマが
  変わった瞬間に発火するイベント。**Windows**:
  `platform_impl/windows/event_loop.rs` が `WM_SETTINGCHANGE` を捕捉し
  `try_window_theme` で再判定して発火 (ポーリング不要)。**macOS**:
  `platform_impl/macos/window_delegate.rs` が
  `AppleInterfaceThemeChangedNotification` を購読。**Linux**:
  `platform_impl/linux/event_loop.rs` が GTK のテーマ変更通知を捕捉。
- `tao::window::Window::set_theme(Option<Theme>)` —
  `None` を渡すと「OS に追従したままにする」、`Some(Light|Dark)` を
  渡すと明示的に固定する。**重要な発見**: `WindowBuilder` の
  `preferred_theme` はデフォルトで `None` であり、
  `platform_impl/windows/window.rs` の `try_window_theme` はこれを
  `self.event_loop.preferred_theme.lock()` にフォールバックさせた上で
  現在の OS テーマを都度解決する。つまり **VeloX が一切コードを書かなく
  ても、ネイティブウィンドウ枠 (タイトルバー等) は #31 着手前から既に
  OS テーマに追従していた** (`window.set_theme(...)` を一度も呼んで
  いなかったため)。ドキュメントコメントいわく `set_theme` の効果は
  Windows/Linux はウィンドウ単位、macOS はアプリ全体。
- toolbar chrome 側 (`:root` の `@media (prefers-color-scheme: dark)`)
  も、WebView2/WebKitGTK/WKWebView いずれも OS のダーク設定を自前で
  監視して `prefers-color-scheme` を再評価する一般的なブラウザエンジン
  機能であり、これも VeloX 側のコード無しに OS 変更へ追従する
  (D67 のコメントが既にこの前提に立っている)。

結論: **「OS テーマに追従できる」自体は、toolbar chrome についても
ネイティブウィンドウ枠についても、#31 着手前から (意図せず) 概ね
成立していた。** `tao`/wry の制約で「できない」ことにはならなかった —
むしろ「明示的に何もしていないことが、たまたま正しい」状態だった。

### 見つけた実際のギャップ 1: 明示的な Light/Dark 選択がネイティブ
ウィンドウ枠に届いていなかった

`BrowserWindow::set_theme`(D67 実装) は `toolbar.evaluate_script(...)`
だけを呼び、`tao::window::Window::set_theme` を一度も呼んでいなかった。
このため「OS がダークモードのときに設定画面で明示的に Light を選ぶ」と、
toolbar webview の中身 (アドレスバー・タブストリップ等) は Light に
なるが、**OS が描画するタイトルバーはダークのまま**という食い違いが
起きる — この Issue の「UI 全体でテーマが統一される」という受け入れ
条件に反する具体的なバグだった。

**修正**: `browser::settings` に純粋関数 `native_window_theme(theme:
Theme) -> Option<ResolvedTheme>` を追加した (`ResolvedTheme` は
`tao::window::Theme` の 2 値だけを写した browser 層のミラー型 —
`browser::` は `tao` に依存できないため、変換は `ui::window` 側の 1 箇所
[`tao_theme_of`] だけで行う、アーキテクチャの 4 層分離を維持)。
`Theme::System` は `None` (= tao 自身の OS 追従に委ねる、上記調査の
「タダで動く」経路をそのまま活かす) に、`Theme::Light`/`Dark` は
`Some(Light|Dark)` にマップするだけの、状態を持たない全域関数。
`BrowserWindow::set_theme` はこれを使って
`self.window.set_theme(...)` を toolbar への `evaluate_script` と
同じタイミング (Ready / UpdateSettings) で呼ぶよう変更した。
`WindowEvent::ThemeChanged` 自体のハンドリングは追加していない —
`System` 時は `set_theme(None)` により tao が自分で追従を続け、
明示選択時は OS 変更を無視するのが正しい挙動なので、VeloX 側で
イベントを拾って何かする必要が無かった。

### 見つけた実際のギャップ 2: Private Window の配色が明示的なテーマ
選択に追従していなかった

`toolbar.html` の `--private-bg`/`--private-fg`/`--private-field-bg`/
`--private-border` は `:root` と `@media (prefers-color-scheme: dark)`
にしか定義されておらず、`:root[data-velox-theme="light"]`/`"dark"`
(D67 で追加された明示的上書きブロック) には無かった。つまり
「OS はダーク、VeloX の設定は明示的に Light」という状況で Private
Window を開くと、toolbar 全体は Light になるのに Private Window の
バッジ・背景だけがダーク色のまま残る — Issue 本文が名指しした
「Private Window の視覚的テーマ拡張に備える」を先取りする形で見つかった
バグ。

**修正**: 上記 4 変数を両方の `data-velox-theme` 上書きブロックにも追加
した。`--private-badge-bg`/`--private-badge-fg` は元々ライト/ダークで
同じ値 (`#6b3fa0`/`#ffffff`) なので上書きブロックには追加していない
(上書きの必要が無い)。

### なぜこれ以上は広げなかったか

- **`Theme::System` 時に toolbar へ実際に解決した Light/Dark を push
  する案は見送った**。CSS の `@media (prefers-color-scheme: dark)` は
  各 WebView エンジンが OS 設定を直接見て判定するのに対し、
  `tao::window::Window::theme()` は `tao` 自身の OS 判定ロジックを経由
  する — 3 OS × 2 エンジンの組み合わせで両者が理論上ズレる余地があり
  (今回 Windows 実機・macOS 実機のどちらでも検証できていない)、
  今まで安定して動いていた CSS 経路を、検証できない Rust 側の解決に
  置き換えるのはリスクに見合わないと判断した。ネイティブウィンドウ枠
  向けの `Option<ResolvedTheme>` (System→`None`) はこの問題が起きない
  — `None` を渡すことは「tao に丸投げする」ことであり、VeloX 自身が
  OS テーマを解決する必要が無いため。
- **`WindowEvent::ThemeChanged` を明示的に処理するコードは追加して
  いない**。理由は上記のとおり、`System` 時の追従は tao/WebView エンジン
  側が自律的に行い、明示選択時はそもそも OS 変更を無視するのが仕様
  だから。`app.rs` の `event_loop.run` の `match event` は既存の
  `_ => {}` に自然に落ちるため、これを追加してもコンパイル上・実行上の
  問題は生じない。

### テスト・検証

- `browser::settings`: `native_window_theme` の 3 状態
  (System→None、Light→Some(Light)、Dark→Some(Dark)) と純粋性を確認する
  テストを 3 件追加。
- `ui::window`: `tao::window::Theme` への変換関数 `tao_theme_of` の
  単体テストを 1 件追加 (`BrowserWindow` 自体の GUI 依存メソッドは
  既存方針どおりユニットテスト対象外 — 実ウィンドウでの検証は下記の
  「検証できていないこと」参照)。
- `ui::toolbar`: `toolbar.html` の `data-velox-theme="light"`/`"dark"`
  各ブロックに 4 つの `--private-*` 変数が実際に含まれることを確認する
  回帰テストを 1 件追加 (ギャップ 2 の修正の検証)。
- `cargo test`(xvfb-run + dbus-run-session): 既存の統合テスト 8 本は
  変更なしで全数グリーン。ユニットテストは着手前 776 件→着手後 781 件
  (+5、上記の追加テストと一致)、減少なし。
- `cargo check --target x86_64-pc-windows-msvc --all-targets`: 型検査
  のみ通過 (リンク・実行はしていない — この開発環境は Linux のみ)。

### 満たせなかった/検証できていない点

- **「OS テーマに追従できる」の実機検証は行っていない**。この開発環境は
  Linux (Xvfb) のみで、実際に OS のダーク/ライト設定を切り替えてタイトル
  バー・toolbar 双方が追従するかを目視確認することはできなかった。上記の
  `tao` 0.37 のソースコード解析に基づく推論であり、特に Windows 実機
  (最優先 OS) での確認は今後の課題として残る。
- **`WindowEvent::ThemeChanged` の実発火・実挙動は未検証**(Xvfb 環境に
  は「OS のダークモード設定」という概念自体が無く、切り替えて発火させる
  ことができない)。
- **Linux (WebKitGTK) での `prefers-color-scheme` の実際の追従確認も
  未実施**。CLAUDE.md の OS 優先度方針により最低限の整備に留めている。
- **Private Window 機能自体 (Issue 本文の「Private Windowの視覚的
  テーマ拡張に備える」の主題) はまだ実装されていない**— 現状の
  `--private-*` 変数は「今後 Private Window 機能が入ったときにテーマと
  矛盾しないよう備える」という位置づけの CSS 変数であり、この Issue の
  スコアではその機能自体は追加していない (別 Issue の領域)。
- **設定画面 UI 上、「システム (現在: ダーク)」のように解決済み OS テーマ
  を表示する機能は追加していない**。受け入れ条件には無いため見送った。

**Revisit condition**: (1) 実際に Windows/macOS 実機で OS テーマ切替を
目視検証できる環境が整ったとき (現状 Linux 専用の開発環境という制約に
よる)。(2) Private Window 機能そのものを実装する Issue に着手すると
き、本 Issue で用意した `--private-*` の明示的テーマ対応が実際に使われる
ことを確認する。(3) toolbar chrome の `System` 解決を CSS 依存から
`tao::window::Window::theme()` 起点の明示解決に置き換える方が有利だと
判明したとき (現状は上記のとおりリスク回避のため見送っている)。

## D72: View Source (#45) — `document.documentElement.outerHTML` を取得し、HTML エスケープ済みテキストとして新規タブに `data:` URL で表示する

**対象**: Issue #45 の受け入れ条件 3 点 (ページソースを表示できる /
ショートカット (Ctrl/Cmd+U) から起動できる / 現在ページを壊さず新規タブ等で
表示できる)。依存として挙げられている Issue #38 (キーボードショートカット
管理) は D69 の時点と同じくまだ着手されていないため、今回も既存の D18/D23/
D69 と同じ「固定の Ctrl/Cmd+U 割り当て」で最小実装し、後から #38 の仕組みに
載せ替えやすい形にした (D69 の「Ctrl/Cmd+F の割り当てと Issue #38 への申し
送り」節と全く同じ構造 — 詳細は後述)。

### この機能で最優先すべきセキュリティ設計: 取得したソースは「テキスト」としてしか描画しない

View Source は「ページの HTML ソースをそのまま画面に出す」機能である以上、
入力（ページの生ソース）は本質的に信頼できない — ソース自体が
`<script>` タグや `onerror` 属性を含んでいて当然で、それこそが表示したい
内容そのものである。**もしこのソースを一度でも「HTML マークアップ」として
別ページに挿入してしまえば、View Source は事実上「そのページをもう一度
VeloX の（表示上は新しい、しかし技術的には同じ特権を持つ）タブで実行する」
機能に成り下がり、閲覧者に "ソースを見せている" つもりが実際にはスクリプト
を実行させてしまう深刻な脆弱性になる。**

対策は単純かつ徹底している: 取得したソースの**すべてのバイト**を
`browser::view_source::escape_html` (`&` `<` `>` `"` `'` の 5 文字を実体
参照に変換する、標準的な HTML テキストエスケープ) に通してから、生成した
ドキュメントの `<pre>` 要素の**テキストコンテンツとしてのみ**埋め込む。
これにより、ソースの中身が丸ごとの `<script>...</script>` ブロックであれ、
`</pre>` を使って囲みタグから抜け出そうとする試みであれ、タグの途中で
ぶつ切りになった不完全なソースであれ、ブラウザの HTML パーサーには
「解釈不能な地の文」としてしか見えない。生成するドキュメント自体に
`<script>` 要素を一切含めていない (実行すべき JS がそもそも無い) ことも
重ねての設計上の防御になっている — D69/D62 の `escape_js_line_terminators`
はコンテンツが JS 文字列/正規表現リテラルの中に埋め込まれる前提のエスケープ
だが、本機能にはそのコンテキストが存在しないため、標準的な HTML エスケープ
だけで足りる (この違いは `browser::view_source` のモジュールドキュメントに
明記した)。

**この設計を検証する単体テストを `src/browser/view_source.rs` に用意した**
(抜粋、全て pass):

- `build_view_source_document_never_reproduces_a_live_script_tag` —
  ソースに `<script>alert(document.cookie)</script>` を含めても、
  生成ドキュメントに生の `<script>alert` が現れないこと、代わりに
  `&lt;script&gt;alert(document.cookie)&lt;/script&gt;` という
  エスケープ済みテキストとして現れることを確認。
- `build_view_source_document_is_safe_when_source_is_cut_off_mid_tag` —
  ソースが `<scr` のようにタグの途中で切れていても、`<` が生のまま
  残らないことを確認 (Issue の指示にある「閉じタグの途中で切れたソース」
  のケース)。
- `build_view_source_document_escapes_an_attempted_pre_closing_tag` —
  ソースが `</pre><img src=x onerror=alert(1)>` のように、こちらが
  ソースを包んでいる `<pre>` 自体を早期に閉じて隣に要素を注入しようと
  しても、生成ドキュメント中の実際の `<pre>`/`</pre>` ペアが 1 組のまま
  であることを確認。
- `build_view_source_document_escapes_the_page_url_in_title_and_header` —
  ページ URL 自体 (ヘッダーと `<title>` に埋め込む) にも同じエスケープを
  適用していることを確認 (実際の呼び出し元は必ず
  `navigation::normalize_input` を通過済みの URL しか渡さないが、
  多重防御として)。
- `truncate_source_utf8_never_splits_a_multibyte_character` — 打ち切り
  位置が UTF-8 のマルチバイト文字の途中に来ても panic せず、文字境界まで
  後退することを確認 (日本語ページのソースを想定)。

### ソース取得方法: `document.documentElement.outerHTML` を JS で取得（生 HTTP 再フェッチはしない）

検討した選択肢は 2 つ:

1. **`document.documentElement.outerHTML` を `evaluate_script_with_callback`
   で読む**（採用）。`BrowserWindow::fetch_page_title`/`fetch_favicon`
   (D12) と全く同じ「fire-and-forget な JS 評価 → `UserEvent` で非同期に
   結果を受け取る」パターンを再利用するだけで済み、3 エンジン
   (WebKitGTK/WKWebView/WebView2) すべてで無条件に動く — wry 0.56 は
   `evaluate_script_with_callback` を全プラットフォームでサポートして
   いるため、D69 のネイティブ find API 調査のような「Windows だけ賭けに
   出る」判断すら不要だった。**CLAUDE.md の「Windows を最優先」を素直に
   満たす** (Windows で動く実装を最初に選び、そのまま 3 OS 共通で使える)。
2. **ページの元 HTTP レスポンスバイト列を再取得する** (見送り)。ブラウザの
   ネイティブ「View Source」に近い挙動 (JS 実行前の生の応答をそのまま
   見せる) だが、wry 0.56 はレスポンスボディを取り出せる汎用のネットワーク
   API を公開していない — D17/D59 で確認済みの「wry はビルダーレベルの
   main-frame ナビゲーションフックとリクエストブロッキングは持つが、
   レスポンス本文を読めるフックは持たない」という制約がそのまま当てはまる。
   別途 HTTP クライアント (例えば `reqwest`) を追加してページを独立に
   再フェッチする案も検討したが、(a) Cookie・認証状態・User-Agent
   などをブラウザのセッションと二重管理する必要が生じる、(b)
   同一 URL に対して 2 回目のリクエストを飛ばすことになり、副作用のある
   POST 送信後のページ等では意味が変わってしまう、(c) 依存クレートが
   増える (D6) — というコストに見合わないと判断した。

**採用した方式の既知の限界**: `outerHTML` は「今この瞬間の DOM」のスナップ
ショットであり、ページ自身の JS が `document.write`/DOM 操作でサーバの
応答から書き換えた後の状態を返す。つまり「サーバが実際に送ってきたバイト
列」とは一致しないことがある (SPA 等では顕著)。ブラウザの devtools の
「View Page Source」相当ではなく「Inspect Element の outerHTML」相当の
挙動である。実務上ほとんどのページ検証用途 (レイアウト崩れの原因調査、
メタタグの確認など) は現在の DOM を見たいことが多く、この差異は許容できる
簡略化と判断したが、正直に記録しておく。

### 表示先: 既存の「新規タブを開く」経路 (`open_new_tab`) にそのまま乗せる `data:` URL

新しいタブとして開くこと自体は Issue の指示どおりで迷いは無かったが、
「そのタブに何を読み込ませるか」に選択肢があった:

- **カスタム URL スキーム/プロトコルハンドラ (`view-source:` 相当) を実装
  する** (見送り)。Chrome 等の `view-source:https://example.com/` を模倣
  できればアドレスバー表示は理想的になるが、wry 0.56 に「エンジンが
  ロードしようとした任意の URL に対して VeloX 側が代わりにレスポンスを
  返す」ようなカスタムプロトコルハンドラの公開 API は無く (D18/D59/D69 が
  積み重ねてきた「wry の実ソースを読んでから機能を選ぶ」調査スタイルの
  結論)、`browser::navigation::normalize_input` の URL 検証・タブの
  `current_url`・セッション永続化 (`SessionSnapshot::sanitize`) など
  複数箇所が前提にしている「`current_url` は実際にエンジンがロードした
  URL と一致する」という不変条件を壊さずに擬似スキームを割り込ませるのは、
  P2 の機能 1 つのために見合わないコストと判断した。
- **`data:text/html` URL として、既存の `app::open_new_tab` にそのまま
  渡す** (採用)。エスケープ済みドキュメントを組み立てたら、それを
  `data:` URL にエンコードし、`ToolbarCommand::NewTab`/`Ctrl+T`/
  `target="_blank"` などが最終的に必ず通る `app::open_new_tab` に、
  他の呼び出し元と全く同じ形で渡すだけで済む。`data:` は既に
  `browser::navigation::ALLOWED_SCHEMES` に含まれるスキームであり、
  タブのプロセス配置 (D54)・アクティブ化・タブ作成レイテンシ計測 (D19)
  などを一切新設せずにそのまま享受できる。

**この方式が生む、正直に記録すべき既知の制限**:

- **アドレスバーには `view-source:https://example.com/` のような読みやすい
  疑似 URL ではなく、`data:text/html;charset=utf-8;base64,....` という
  長い文字列がそのまま表示される。** Issue の受け入れ条件「現在ページの
  URL を正しく扱う」は、(a) ソース取得元のタブ・URL を取り違えない
  (`BrowserWindow::fetch_page_source` が `page_url` を要求時点で捕捉し、
  非同期の結果に一貫して紐付ける — `fetch_favicon` の `page_url` 引数と
  同じ設計)、(b) 取得元の URL を生成ドキュメントのヘッダー/`<title>` に
  明示表示する、の 2 点では満たしているが、**アドレスバー表示の見た目に
  関しては満たせていない**。`Tab::current_url`/`on_navigation_started`/
  `on_load_finished` は「エンジンが実際にロードした URL」を無条件に
  正としてタブストリップ・セッション永続化に反映する設計であり (D20)、
  ここに「表示用の別 URL」を割り込ませるには `Tab` に新しいフィールドを
  足すか、ナビゲーションイベントハンドラに view-source 用の特別扱いを
  複数箇所へ差し込む必要がある — D20 が守ってきた「`current_url` は常に
  真実」という前提を、この 1 機能のためだけに壊すコストに見合わないと
  判断し、見送った。**この受け入れ条件は文字どおりには満たせていない**
  ことをここに明記する。
- **`data:` URL はそのタブの `current_url` としてタブの生存期間中
  保持され続け、`app::sync_tab_strip` が触れるたびに (`TabSummary`/
  `veloxSetTabs` の JSON として) トールバー webview へ再送され、
  `app::persist_session` が呼ばれるたびに `session.json` にも書き出される。
  1 回きりのペイロードではなく、以後のほぼ全イベントで繰り返し
  シリアライズされる「アンビエントな状態」になる。** これが
  `browser::view_source::MAX_SOURCE_BYTES` を意図的に 300,000 バイトと
  かなり保守的な値に抑えた理由そのものである — 数 MB 級のソースをそのまま
  許すと、無関係なタブの開閉やタブ切替のたびに数 MB の `evaluate_script`
  呼び出しとセッションファイル書き込みが発生しかねない。300 KB であれば
  base64 化後 (約 1.33 倍) でも数百 KB に収まる。上限を超えたソースは
  `browser::view_source::truncate_source_utf8` が文字境界を尊重して
  切り詰め、生成ドキュメントに切り詰め済みである旨の通知
  (`.velox-notice`) を表示する。
- **セッション復元・履歴への影響**: `data:` URL は
  `browser::navigation::ALLOWED_SCHEMES` に含まれるため、
  `SessionSnapshot::sanitize` は View Source タブの `current_url` を
  そのまま (拒否せず) 通す — 次回起動時のセッション復元が有効なら、
  そのタブは古いソースのスナップショットのまま復元される (実害はないが、
  やや直感に反する)。一方、履歴 (`HistoryStore`) には**意図的に**記録
  されないよう `app::handle_user_event` の `LoadFinished` 処理に
  `url.starts_with("data:")` の除外を 1 箇所追加した — でなければ、
  ページを閲覧するたびに数百 KB の base64 文字列が visit history と
  オムニボックスの候補に紛れ込むことになり、これは実用上明確な UX
  劣化だと判断したため。タイトル/favicon の取得 (`fetch_page_title`/
  `fetch_favicon`) は除外していない — `fetch_page_title` が読む
  `document.title` は生成ドキュメント自身の `<title>ソースを表示: ...`
  なので、タブストリップに「ソースを表示: https://example.com/」という
  読める見出しが出るのはこの経路によるものであり、あえて残した。
- **再読み込み (リロード)** はスナップショット時点の内容を再表示する
  だけで、元ページを再取得しない (`data:` URL はエンジンにとって
  「その場で完結した」ページであるため)。
- **閉じたタブの再オープン (Ctrl/Cmd+Shift+T)**: `browser::tabs::
  ClosedTabs` (D24) は URL 文字列をそのまま LIFO に積むだけなので、
  View Source タブを閉じた直後に再オープンすると理屈の上では元の
  `data:` URL が復元されるはずだが、専用のテストは追加していない
  (優先度の低いエッジケースと判断)。

### ショートカット (Ctrl/Cmd+U) の実装: D18/D23/D69 と全く同じ二重配送

`ContentShortcut::ViewSource` (固定センチネル文字列
`"velox:view-source"`、`ui::window::tab_shortcut_script` に追加) と
`ToolbarCommand::ViewSource` (`{"cmd":"view_source"}`、`toolbar.html` の
キーダウンリスナーに追加) の 2 経路を、D18/D23/D69 と寸分違わぬパターンで
追加した。どちらも `app::request_view_source` という 1 つの共有関数に
収束する — Issue #38 (キーボードショートカット管理) が今後この 2 経路
すべてに乗ってくる設計になったとき、変更が必要な箇所は「JS 側でどのキーを
監視するか」の 1 か所だけで済むよう、D69 と同じ配慮を踏襲した。
`browser::settings::shortcut_reference` (Issue #30/D67 の Shortcuts タブ)
にも「ページのソースを表示 — Ctrl/Cmd+U」の行を追加し、発見可能性を確保
した (なお Ctrl/Cmd+F は D69 実装時にこの一覧への追加が漏れていたことに
気づいたが、本 Issue のスコープ外のため今回は手を付けていない)。

### 実装の全体像

- **`browser::view_source`** (`src/browser/view_source.rs`、新規) —
  UI/エンジン非依存の純粋ロジック一式:
  `escape_html`/`truncate_source_utf8`/`build_view_source_document`/
  `base64_encode`/`to_data_url`。依存クレートを増やさず (D6)、base64
  エンコーダは RFC 4648 の固定アルゴリズム (~20 行、セキュリティ上の
  難しい判断を要さない) を自前実装し、RFC のテストベクタで単体テスト
  済み。`docs/architecture.md` の 4 層分離のとおりここは webview を
  一切知らず、単体テストの主対象 (24 ケース)。
- **`ui::window::BrowserWindow::fetch_page_source`** — 実際の DOM 読み取り
  を担う唯一のメソッド。`fetch_page_title`/`fetch_favicon` と同じ
  「不明/休止中タブは黙って no-op」「fire-and-forget、結果は
  `UserEvent` で非同期に返る」契約 (D12)。
- **`app::request_view_source`**/**`app::open_view_source_tab`** —
  前者がショートカット発火時にアクティブタブの `page_url` を捕捉して
  取得をキックし、後者が `UserEvent::ViewSourceReady` を受けて
  ドキュメントを組み立て `data:` URL 化し、`open_new_tab` に渡す。

### テスト

- `src/browser/view_source.rs`: 24 件 (エスケープ・打ち切り・ドキュメント
  組み立て・XSS 耐性・base64・data URL のそれぞれ)。
- `src/ui/toolbar.rs`: `ToolbarCommand::ViewSource` の IPC パーステスト
  1 件を追加。
- `src/ui/window.rs`: `parse_content_shortcut`/`tab_shortcut_script` に
  `ViewSource`/`VIEW_SOURCE_MESSAGE` を追加した既存テストの拡張、および
  `VIEW_SOURCE_FETCH_SCRIPT` の内容検証テスト 1 件を追加。
- 単体テスト件数: 776 → 799 (+23、`cargo test --lib -- --list` で計測)。
  減少なし。
- 統合テスト (`tests/integration.rs`) は今回変更していない (8 件のまま、
  全て pass) — D69 のときと同じ判断で、View Source は既存の統合テストが
  検証する「実プロセス起動・実タブ管理・実ファイル永続化」のいずれとも
  直接関係しないため、新規の統合テストは追加していない。
- `cargo check --target x86_64-pc-windows-msvc --all-targets` で型
  レベルの整合は確認したが、実機の Windows/WebView2 での動作確認は
  できていない (この環境に Windows 実機が無いため) — 特に
  `evaluate_script_with_callback` が数百 KB 級の文字列を問題なく
  往復できるか、Ctrl+U が WebView2 自身の既定アクセラレータと衝突しないか
  (D69 の F12/Ctrl+F と同じ懸念) は未検証。

**満たせなかった／部分的にしか満たせなかった受け入れ条件**:
「現在ページの URL を正しく扱う」— 取得元 URL の取り違え防止と
ドキュメント内表示は満たしているが、**アドレスバーの表示** (生の `data:`
URL が見える) は満たせていない。上記「表示先」の節に理由を記録した。

**Revisit condition**: (1) Issue #38 のキーバインド管理層への Ctrl/Cmd+U
の載せ替え。(2) `Tab`/`TabState` にビュー専用の表示 URL を持たせる設計が
別の必要性 (例えば他の内部ページ) から生まれた場合、View Source の
アドレスバー表示もそれに乗せる。(3) 巨大ページの全文表示が本当に必要に
なった場合、`data:` URL 方式を専用スキーム/プロトコルハンドラに置き換える
(前述のとおり現状は見送り)。(4) 実機 Windows での動作確認
(`evaluate_script_with_callback` の大きな文字列、Ctrl+U のアクセラレータ
衝突)。(5) 閉じた View Source タブの再オープン専用のテスト追加。

## D73: コード署名 (#42) — 証明書が無いため「有効化可能な仕組み」に留め、実際の署名は見送り

**対象**: Issue #42 の受け入れ条件 4 点 (macOS 署名/notarize、Windows 署名、
秘密鍵をリポジトリに置かない、リリース手順の docs 記録) の棚卸しと対応。

### 前提となる制約

このプロジェクトは Windows 用コード署名証明書 (OV/EV) も macOS の Apple
Developer Program の Developer ID も保有しておらず、リポジトリの Secrets
にも登録されていない。証明書の新規取得には認証局への費用支払いや (Azure
Trusted Signing の場合は) 組織の 3 年以上の事業実績または米国/カナダ在住の
個人という適格性要件があり、このセッション内では取得できない。したがって
**「署名された成果物を実際に生成する」ことは今回のスコープ外**とし、
証明書が用意でき次第すぐに有効化できる仕組みと、証明書が無くても今できる
配布信頼性の改善に絞って実装した。調査の詳細と出典 (公式ドキュメントを
直接確認できたもの/検索結果からの要約に留まるものの区別を含む) は
[docs/windows-code-signing.md](windows-code-signing.md) に記録した。

### 実装したもの

1. **`release-windows.yml` への署名ステップ追加 (opt-in)**: 以下 6 つの
   Secrets/Variables が全て設定されている場合のみ、`azure/login` →
   `azure/artifact-signing-action@v2` (Azure Trusted Signing。2024年前後に
   Artifact Signing へ改称) で `velox.exe`/`velox-bench.exe` に
   Authenticode 署名し、`Get-AuthenticodeSignature` で結果を検証する。
   1 つでも未設定なら (現状は全て未設定) 署名ステップ一式をスキップし、
   これまでどおり無署名の zip を作る — 既存のリリースフローを壊さない
   ことを最優先した。
   - `secrets.WINDOWS_CODESIGN_AZURE_CLIENT_ID` /
     `WINDOWS_CODESIGN_AZURE_TENANT_ID` /
     `WINDOWS_CODESIGN_AZURE_SUBSCRIPTION_ID`
   - `vars.WINDOWS_CODESIGN_ENDPOINT` / `WINDOWS_CODESIGN_ACCOUNT_NAME` /
     `WINDOWS_CODESIGN_CERT_PROFILE_NAME`
   - 認証は GitHub Actions の OIDC (`permissions: id-token: write` +
     `azure/login`) によるフェデレーション認証とし、長期のクライアント
     シークレットや証明書そのものを Secrets に置かない設計にした
     (秘密鍵は Azure 側の HSM に留まり CI には渡らない)。
   - `if:` で `secrets.*` を直接比較する条件分岐は job 単位では使えず
     step 単位でも挙動が不安定という調査結果を踏まえ、判定用の
     `Check Windows code signing configuration` ステップを 1 つ設け、
     env 経由でシェル変数に落としてから `enabled=true/false` を
     `GITHUB_OUTPUT` に出す方式にした (以降のステップはその出力だけを
     参照する、より確実な分岐)。
2. **なぜ Azure Trusted Signing を選んだか**: 2023 年 6 月の CA/Browser
   Forum の要件変更により、新規発行のコード署名証明書は EV/OV を問わず
   秘密鍵がハードウェア HSM に閉じ込められエクスポート不可になった
   (複数の独立したソースで確認)。そのため「`.pfx` を base64 化して
   Secrets に入れて `signtool` で署名する」という古典的な GitHub Actions
   パターンは新規証明書では成立しない。クラウド HSM 型のリモート署名
   サービスが必要になるが、その中で GitHub 公式相当の Action
   (`azure/artifact-signing-action`) が公開されており GitHub Actions との
   統合コストが最も低いと判断した Azure Trusted Signing を採用した。
   他ベンダー (DigiCert KeyLocker, SSL.com eSigner 等) への切り替えも
   構造 (判定ステップ → ログイン/認証 → 署名 → 検証) は流用できる。
3. **証明書なしでの配布信頼性改善**: README に SHA-256 チェックサムの
   検証手順 (`Get-FileHash` / `sha256sum -c`) を追記し、GitHub Release
   本文に署名済み/未署名を自動で明記するようにした
   (`release-windows.yml` の `Create GitHub Release` ステップ)。
4. **macOS**: release workflow 自体が D70 の判断により未着手のため、
   署名・notarization の実装は行わず、`docs/windows-code-signing.md` に
   Developer ID 署名 → notarytool による公証 → stapler での staple、
   という一般的な流れの概要のみ記録した。

### 見送ったもの・未検証のもの

- **実際の署名の実行**: 証明書/Azure サブスクリプションが無いため、
  `azure/login`/`azure/artifact-signing-action` のステップは一度も
  実行されておらず (`enabled=false` で常にスキップ)、**Azure 側の設定と
  繋げた動作確認はできていない**。証明書取得後、最初のタグ push 前に
  必ず `workflow_dispatch` で手動実行して確認すること
  (docs/windows-code-signing.md の「証明書を取得した後に行う作業」)。
- **Azure Trusted Signing の適格性審査そのもの**: 組織の 3 年実績、
  個人の米国/カナダ居住のいずれの要件もこのプロジェクトの現状では
  満たせないため、申請自体を行っていない。
- **macOS の署名・notarization の実装**: 概要調査のみで、
  `codesign`/`notarytool`/`stapler` を実際に呼ぶ workflow は書いていない
  (macOS の release workflow 自体が D70 により未着手のため)。
- **SmartScreen レピュテーションの実際の蓄積過程**: 署名を有効化しても
  即座に警告が消えるわけではなく、ダウンロード実績に応じて徐々に
  蓄積されるとされる (複数ソースで確認) が、具体的な閾値は Microsoft から
  公開されていない。実際に署名した初回リリース以降、様子を見て
  docs/windows-code-signing.md に追記する。

**Revisit condition**: (1) Windows 用コード署名証明書 (Azure Trusted
Signing の適格性を満たす、または他ベンダーのクラウド署名サービスを契約
する) が用意できたとき — `docs/windows-code-signing.md` の手順に従い
Secrets/Variables を設定するだけで `release-windows.yml` の署名が有効化
される。(2) macOS の release workflow (D70 の Revisit condition (1)) に
着手するとき、合わせて Developer ID 署名 + notarization を実装する。
(3) Azure Trusted Signing の適格性要件 (地域・事業年数) が緩和されたとき。
(4) 他ベンダーのクラウド署名サービスへの切り替えを検討するとき。


## D74: プライベートブラウジング、残りスコープ (#27) — Private Window を別ウィンドウとして開けるようにする。分離は D14/D15 の既存メカニズムのまま、per-window 化だけを行う

**対象**: Issue #27。Epic #53 の言葉を借りれば「大半は #7 (D14) で実装済み。
残りは『Private Window を別に開く』のみで、#29 (D68, PR #150) が前提」。
その #29 は本 Issue 着手時点で既に `main` にマージ済みで、D68 の末尾
「Issue #27 が実装すべきこと」の節に (a)(b)(c) の 3 点として実装方針が
具体的に書き残されていた。本項はその棚卸しの確認と、実際の実装・検証の
記録である。

### 棚卸し: #7 (D14/D15) と #29 (D68) で既に何ができていたか

着手前に `docs/decisions.md` の D14/D15/D68/D71、`src/config/mod.rs`
(`Config::private`)、`src/ui/window.rs`、`src/browser/windows.rs`、
`src/app.rs` の `open_new_window`/`AppState` を読んだ。Issue #27 の受け入れ
条件 4 点のうち、**ゼロから作る必要があったのは 1 点目だけ**だった:

- **「Private Windowを通常ウィンドウとは別に開ける」— 未実装だった。** 本 PR
  の主題。詳細は後述。
- **「閲覧履歴がアプリ側に残らない」— D14 のロジック (`record_visit_if_enabled`
  などの `history_enabled` ゲート) はそのまま使えたが、`history_enabled` が
  プロセス全体で 1 個の `bool` だったため、複数ウィンドウが混在する状況
  (通常ウィンドウ + Private Window が同一プロセス内に共存) では**そのまま
  では正しく動かない**ことが分かった。D68 が既に予告していた通り、
  per-window 化が必要だった (後述)。
- **「通常セッションのCookie/Storageを共有しない」— D14/D15 の
  `.with_incognito(true)` + `context: None` の分離メカニズムは
  `ui::window::BrowserWindow::new` 内で完結しており、ウィンドウが複数に
  増えても仕組み自体は無改修で使い回せることを D68 が確認済み
  (「D14 が既に書いていた『複数ウィンドウが実現したときの拡張路線』が
  そのまま使える形で残っている」)。今回の実装で実際にその通りだったことを
  確認した (後述の「データストア分離の検証」)。
- **「Private状態を明確に識別できる」— `--private-badge`・ウィンドウ
  タイトルの `— プライベート` 接尾辞・D71 で修正済みの `--private-*` CSS
  変数のテーマ追従は、いずれも「1 個の `bool` を `BrowserWindow::new`
  構築時に渡す」形で既に実装されており、その `bool` の出どころを
  `config.private` からウィンドウごとの値に差し替えるだけで済んだ。

### 実装したもの: D68 が示した (a)(b)(c) をそのまま実装

D68 の該当節がほぼ実装レシピそのものだったので、方針を変える理由は
見当たらず、そのまま採用した:

1. **(a) `browser::windows::WindowEntry` にウィンドウごとの `private: bool`
   を追加**。`Windows::new`/`open_window` は互換維持のため据え置き (常に
   `private: false` に委譲)、新たに `Windows::new_with_privacy`/
   `Windows::open_window_with_privacy` を追加して呼び分ける形にした
   (`Windows` 自身のテストを 1 件も壊さずに済む形を優先した)。新設の
   `Windows::is_private(id) -> Option<bool>` が `app.rs` 側の唯一の参照点。
2. **(b) `app::open_new_window` が `Config::private` の代わりにそのフラグを
   見て `BrowserWindow::new` を呼ぶ**。実際には「新しい
   `open_private_window`」を別関数として作るのではなく、`open_new_window`
   自体に `private: bool` 引数を 1 つ追加する形にした — 呼び出し元が
   `config.private`(既存の Ctrl/Cmd+N 系 3 経路) か `true`(新設の
   Ctrl/Cmd+Shift+N 系 3 経路) のどちらを渡すかだけが違い、実装は 1 箇所の
   ままで済む。`ui::window::BrowserWindow::new` にも同じ `private: bool`
   引数を追加し、関数内部の 7 箇所の `config.private` 参照をすべて
   この引数に差し替えた — `config: &Config` はプロセス全体で 1 個の
   共有参照のため、これを直接見ている限り「通常ウィンドウとプライベート
   ウィンドウが同一プロセスに共存する」ことは原理的に表現できなかった。
3. **(c) Ctrl/Cmd+Shift+N を、#29 が作った 3 経路と同じパターンで追加**。
   `ToolbarCommand::NewPrivateWindow`(`{"cmd":"new_private_window"}`)/
   `ContentShortcut::NewPrivateWindow`(センチネル `velox:new-private-window`)/
   `AutomationCommand::NewPrivateWindow`(`new_private_window` コマンド、
   引数なし) の 3 つを、既存の `NewWindow` 系と完全に同じ場所・同じ理由
   (「`ui_windows` 全体への `&mut` が要る = `handle_toolbar_command`/
   `handle_content_shortcut` の中では処理できず `handle_user_event` で
   横取りする」という D68 の借用上の制約) で追加した。`toolbar.html`/
   `tab_shortcut_script`(content webview 側) の両方の keydown リスナに
   Ctrl/Cmd+Shift+N を追加している。

`AppState::history_enabled`(プロセス全体で 1 個の `bool`) は完全に廃止し、
`record_visit_if_enabled`/`record_input_history_if_enabled`/
`persist_session` はすべて新設のヘルパー `window_is_private(state,
window_id)`(`state.windows.is_private(window_id).unwrap_or(true)` —
不明なウィンドウは安全側の `true` = 記録しない扱い) を通す形に書き換えた。
`ToolbarCommand::Ready` が押していた `window.set_private(config.private)`
も `window.set_private(window.is_private())`(ウィンドウ自身が構築時に
覚えている自分の `private` フラグ — 新設の `BrowserWindow::is_private()`)
に差し替えた。`config.private` を直接見ていたこの 1 箇所を放置すると、
通常ウィンドウの Ready ハンドラがプロセス起動時の `config.private`(= 常に
`false`、通常起動の場合) をそのまま押してしまい、逆に `--private` 起動中に
開いた「通常」の 2 枚目のウィンドウ (Ctrl+N, 後述) のバッジも常に
`config.private` の値になってしまうところだった。

**`--private`/`VELOX_PRIVATE` 起動フラグ (D14) 自体は変更していない**:
`app::run` は最初のウィンドウを `Windows::new_with_privacy(homepage,
config.private)` で開き、`BrowserWindow::new` にも `config.private` を渡す
— プロセス全体を Private にして起動する挙動は従来どおり。**Ctrl/Cmd+N
(無印) は今回も `config.private` を渡す**ようにした — つまり
`--private` で起動したプロセスで Ctrl+N を押すと、今までどおり
新しいウィンドウも Private になる (D68 が「今回は変更していない」と
書いていた挙動をそのまま維持)。Ctrl/Cmd+Shift+N だけが常に `true` を渡す
新経路で、`--private` 起動でも通常起動でも「明示的に Private Window を
1 枚追加する」という一貫した意味を持つ。

### データストア分離の検証: wry 0.56.1 の実ソースを 3 プラットフォームぶん確認した

指示の通り、「分離されているつもりで実は分離されていない」を最も警戒す
べき点として、`~/.cargo/registry/src/.../wry-0.56.1/src/` を実際に読んで
確認した (以下、確認した具体的なコード箇所を引用する)。

- **WebKitGTK (Linux, CI 環境)** — `src/webkitgtk/mod.rs` の
  `new_gtk`:
  ```rust
  let web_context = if attributes.incognito {
    default_context = WebContext::new_ephemeral();
    &mut default_context
  } else { /* ... 共有 WebContext ... */ };
  ```
  `.with_incognito(true)` を付けた `WebViewBuilder::build_*` 呼び出しは
  **呼ばれるたびに** `WebContext::new_ephemeral()`(インメモリ、非永続) を
  新規に作る。VeloX 側は toolbar/content 双方の webview 構築に
  `.with_incognito(private)` を渡しており (`ui::window::BrowserWindow::new`)、
  Private Window とは別に開いた通常ウィンドウの `WebContext::new(None)`
  (D66 の実測どおりアプリ名ベースの永続ディレクトリを指す) とは完全に別の
  オブジェクトになる。**確認できたこと**: Private Window の webview が
  通常ウィンドウの永続ストアに触れることはない。**同時に判明した限界
  (D15 が既に書いていた事実の再確認)**: `.with_incognito(true)` は呼ぶ
  たびに新しい ephemeral context を作るため、同じ Private Window 内の
  toolbar と各タブ、さらに複数の Private Window どうしも、互いに
  Cookie を共有しない (実ブラウザの「同一シークレットセッション内の
  タブはセッションを共有する」という一般的な期待からは外れる)。これは
  #7/D15 の時点から存在する制約で、本 Issue が新たに悪化させたものでは
  ない — 通常ウィンドウとの分離という本質的な要件は満たしている。
- **WKWebView (macOS)** — `src/wkwebview/mod.rs`:
  `(true, _, _) => WKWebsiteDataStore::nonPersistentDataStore(mtm)` —
  Apple のドキュメント上 `nonPersistentDataStore()` は呼び出すたびに新しい
  非永続ストアのインスタンスを返す (`.default()` が返す永続シングルトンとは
  対照的)。WebKitGTK と同じ「呼ぶたびに新規・非永続」という構造。
- **WebView2 (Windows, 最優先 OS)** — `src/webview2/mod.rs`:
  `controller_opts.SetIsInPrivateModeEnabled(incognito)` は
  `ICoreWebView2ControllerOptions3` 経由で個々の `Controller` に対して
  設定される。ここで **1 点、実機検証できていない構造上の懸念**を記録して
  おく: `env`(`ICoreWebView2Environment`) 自体は `create_environment` が
  `attributes.context.as_deref().and_then(|c| c.data_directory())` から
  導いた `data_directory` で作られ、VeloX は Private Window 用に
  `context: None` を渡す (`ui::window.rs` の `context` 変数) ため、
  Private Window の `env` は `data_directory` 未指定 (空文字列、
  `CreateCoreWebView2EnvironmentWithOptions` の既定 = 実行ファイル隣接の
  既定フォルダ) で作られる。これは Microsoft の公開ドキュメントが述べる
  「`IsInPrivateModeEnabled` の Controller はインメモリの非永続プロファイル
  を使う (基になる `Environment`/`data_directory` が何であれ、書き込みは
  ディスクに永続化されない)」という仕様に依拠しており、wry のソース自体
  からは「Private Window どうし・通常ウィンドウとの間でディスク上の
  `data_directory` が数値として同じ既定値に揃いうる」ことまでしか確認
  できない (＝ディスクに何か書かれるかどうかの最終防御線は WebView2 側の
  InPrivate 実装そのものに委ねている)。**この開発環境は Linux 専用
  (D61) で WebView2 を実行できないため、Windows 実機でこの分離を目視/
  ファイルシステム上で検証することはできていない** — CLAUDE.md の
  「Windows 最優先」の判断基準に従い、実装 (`.with_incognito(private)` を
  toolbar/content 双方に渡す、D15 の時点から変更なし) はそのまま維持しつつ、
  この限界を正直に記録する。

**アプリ自身のデータ (履歴/入力履歴/セッション) の分離は、統合テストで
実際に検証した** (wry/webview2 のようなブラックボックスに頼らない、
VeloX 自身が書き込むファイルでの検証): `tests/integration.rs` に新設した
`a_private_windows_page_visit_never_reaches_history_json` が、実際に
`velox` プロセスを起動し、通常ウィンドウで 1 ページ訪問した後
`new_private_window` で Private Window を開いて別の 1 ページを訪問し、
`quit` 後の `history.json` を `persistence::load_history` で読み返して
「通常ウィンドウの 2 件だけが記録され、Private Window の訪問は一切
含まれない」ことをアサートしている。ユニットテストの
`record_visit_if_enabled` 単体の正しさに加えて、`app::open_new_window`
(`private: true` での `BrowserWindow` 構築) → 実際のページ読み込み →
`UserEvent::LoadFinished` → `record_visit_if_enabled` という配線全体が
実機 (Xvfb 上の WebKitGTK) で意図通り動くことを確認できた、数少ない
「見送っていない」実地検証である。

### 見つけた/見送った既知のギャップ

- **オムニボックスの候補が通常ウィンドウの履歴を Private Window に
  漏らす**: `ToolbarCommand::OmniboxInput` は `state.history`/
  `state.bookmarks`/`state.input_history` をウィンドウの private 状態に
  関わらず読む (D39 の「書き込みは止めるが読み込みは妨げない」という
  既存方針をそのまま踏襲)。D14 の「プロセス全体が Private」という前提
  では、同時に走っている通常ウィンドウが存在しえないためこれは無害
  だったが、本 Issue で通常ウィンドウと Private Window が同一プロセスに
  共存できるようになった結果、**Private Window のアドレスバーに、
  同じセッション中に通常ウィンドウで実際に訪問した URL や打った検索語が
  候補として出うる**、という新しいギャップが生まれた。これは履歴が
  「ディスクに残る」問題ではなく「同一プロセス内の別ウィンドウに一時的に
  見える」問題であり、Issue #27 の 4 つの受け入れ条件には直接該当しない
  が、実ブラウザの Private/incognito モードの直感 (通常ウィンドウの閲覧を
  Private Window から見えなくする) には反する。修正には
  `omnibox::build_candidates` 系に `window_id`(または呼び出し元の
  private フラグ) を通す設計変更が必要で、かつ「Private Window の候補は
  ブックマークだけ見せるべきか、何も見せないべきか」という仕様判断も
  要るため、本 Issue のスコープでは修正せず、ここに明示的に残す。
- **`DownloadStore` はプロセス全体で 1 個の共有ストアのまま
  (D28/D68 変更なし)**: Private Window でのダウンロードも同じダウンロード
  パネル一覧に載る。ダウンロードしたファイル自体は (実ブラウザでも同様)
  ディスクに残る操作なので Private モードでも隠しようがないが、
  「そのダウンロードが Private Window で行われた」という一覧上の記録は
  通常ウィンドウのパネルからも見えてしまう。D68 が既に「頻度の低いパス」
  として見送っていたスコープで、本 Issue でも同様に見送った。
- **`SitePermissionStore`/`FilterList`/`SiteExceptions` は D68 の設計通り
  全ウィンドウ共有のまま**: Private Window で許可したサイト権限
  (カメラ/位置情報など) は通常ウィンドウにも残る。実ブラウザでも
  Private/incognito のサイト権限は「そのセッションの間だけ」揮発する
  実装が多いが、VeloX は元々 D60 の設計で「サイト権限はアプリ全体で
  1 つ」となっており、本 Issue はこれを変更していない — 変更するには
  `SitePermissionStore` 自体をウィンドウ (または private/non-private)
  スコープに分割する設計が必要で、スコープ超過と判断した。

### テスト・検証

- `browser::windows`: 7 件追加 (`a_window_opened_by_new_is_not_private`、
  `new_with_privacy_marks_the_first_window_private`、
  `open_window_marks_the_new_window_non_private`、
  `open_window_with_privacy_marks_the_new_window_private`、
  `is_private_returns_none_for_an_unknown_window`、
  `open_restored_window_is_never_private`、そして本題の
  `a_normal_and_a_private_window_sharing_the_same_tab_id_keep_independent_privacy`
  — #29 統合時に実際に 3 件のバグが見つかった「同じ `TabId` を持つ 2 つの
  ウィンドウが干渉しないか」という指示を、`private` フラグについて検証)。
- `browser::automation`: `new_private_window` のパース
  (`parses_new_private_window_and_rejects_arguments_on_it`) を追加。
- `ui::toolbar`: `{"cmd":"new_private_window"}` のパース、
  `toolbar_html_declares_expected_hooks` に `new_private_window` の
  存在確認を追加。
- `ui::window`: `tab_shortcut_script_captures_expected_combos_in_capture_phase`/
  `parse_content_shortcut_matches_every_sentinel_exactly` に新センチネル
  `velox:new-private-window`/`ContentShortcut::NewPrivateWindow` を追加。
- `app`: 2 件追加。
  `a_private_window_records_no_visit_while_a_normal_window_with_the_same_tab_id_still_does`
  (本題 — 同じ `TabId` を共有する通常/Private ウィンドウで
  `record_visit_if_enabled`/`record_input_history_if_enabled` が
  正しく独立して動くことの単体テスト) と
  `persist_session_skips_a_private_primary_window_but_writes_a_normal_one`
  (セッション永続化側の同型テスト、実ファイルシステムに対して)。
- `tests/integration.rs`(新規 1 件):
  `a_private_windows_page_visit_never_reaches_history_json` — 上述の
  「データストア分離の検証」参照。実際に `velox` を起動し、Private Window
  経由の訪問が `history.json` に一切現れないことを確認する、本 Issue の
  最も重要な受け入れ条件に対する実地証拠。
- テスト件数: `cargo test --lib` は着手前 800 件 → 着手後 810 件 (+10)。
  `xvfb-run` + `dbus-run-session` 経由の `cargo test --test integration`
  は着手前 9 件 → 着手後 10 件 (+1)。既存テストの削除・スキップ化は
  行っていない。
- `cargo fmt --check`/`cargo clippy --all-targets -- -D warnings` は
  警告ゼロ (`ui::window::BrowserWindow::new` の引数が 8 個になった分は
  `#[allow(clippy::too_many_arguments)]` を明示的に付与 — `app.rs` の
  `handle_user_event` 等、既存の同種関数と同じパターン)。
  `cargo check --target x86_64-pc-windows-msvc --all-targets` も型検査の
  みだが通過を確認した — CLAUDE.md D61 の通りリンク・実行はしておらず、
  上記「データストア分離の検証」の WebView2 に関する懸念は Windows 実機
  でのみ最終確認できる。

### 満たせなかった/検証できていない点 (正直な棚卸し)

- **Windows (WebView2) 実機でのデータストア分離の目視/ファイル検証は
  行っていない** — 開発環境が Linux 専用のため。上記のとおり、
  wry のソースと Microsoft の公開仕様からの推論に留まる。CLAUDE.md の
  「Windows 最優先」の判断基準は、実装の設計判断 (D15 から変更なしの
  `.with_incognito` 呼び出し) には反映したが、実機検証そのものは今回も
  できていない。
- **macOS 実機でのデータストア分離の検証も同様に未実施**(D71 と同じ理由)。
- **オムニボックス候補の cross-window リーク**(上記) は既知のまま未修正。
- **`DownloadStore`/`SitePermissionStore`/`FilterList`/`SiteExceptions`
  の全ウィンドウ共有は D68/D60/D28 の設計を維持したまま**、Private
  Window 導入後の具体的な意味合い (上記) を記録したのみで、分割は
  行っていない。
- **Private Window 内の複数タブ/複数 Private Window 間で Cookie が
  共有されない**(WebKitGTK/WKWebView の `.with_incognito(true)` が
  呼び出しごとに新規ストアを作る構造上の制約、D15 由来) — 実ブラウザの
  一般的な incognito 挙動 (同一シークレットセッション内では共有) との
  差異だが、#7 の時点からの既存の制約であり本 Issue のスコープでは
  修正していない。

**Revisit condition**: (1) Windows/macOS 実機でデータストア分離を検証
できる環境が整ったとき (D61/D71 と同じ制約)。(2) オムニボックス候補の
cross-window リークを修正する Issue に着手するとき — `window_id` を
`omnibox::build_candidates` 系まで通す設計と、「Private Window の候補に
何を出すか」という仕様判断が必要になる。(3) ダウンロード/サイト権限/
フィルタ設定をウィンドウ (または private/non-private) スコープに分割する
価値が実際に求められたとき (D68 の「見送ったもの」と同じ優先度判断)。
(4) WebKitGTK/WKWebView 側で「Private Window 内はタブ間で共有し、通常
ウィンドウとは分離する」ような、呼び出し単位でない共有 ephemeral
context を wry が公開するようになったとき。


## D75: 印刷・PDF保存 (#40) — 印刷は `wry::WebView::print()` (unsafe 不要)、
PDF直接書き出しは Windows のみ `ICoreWebView2_7::PrintToPdf`

**対象**: Issue #40。CLAUDE.md「対応 OS の優先度」により Windows を最優先し、
D59/D66/D69 と同じ「issue の指示に頼らず、ビルダーメソッドだけでなく拡張
トレイト経由の生インターフェースまで実ソースを読む」調査スタイルを踏襲した。

### 調査: wry 0.56 経由で印刷・PDF出力 API に届くか

`~/.cargo/registry/src/.../wry-0.56.1`、`webview2-com-sys-0.38.2/src/
bindings.rs`、`webview2-com-0.38.2/src/callback.rs` を実際に読んだ。

**まず、D59/D66/D69 が調べてこなかった場所に答えがあった**:
`wry::WebView` 自身が `pub fn print(&self) -> Result<()>` という、
`WebViewBuilder` の `with_*` 系でも `WebViewExtWindows` のような
プラットフォーム限定拡張トレイトでもない、**3 OS 共通・`cfg` 無しで常に
コンパイルされる素の `impl WebView` メソッド** (`wry-0.56.1/src/lib.rs`
2119-2122 行目) を既に公開している。中身はプラットフォームごとに全く
別物:

- **WebKitGTK (Linux)** — `src/webkitgtk/mod.rs` 772-776 行目:
  `webkit2gtk::PrintOperation::new(&self.webview)` を作って
  `run_dialog(None::<&gtk::Window>)` を呼ぶだけ。GTK のネイティブ印刷
  ダイアログが開き、その中に「ファイルに印刷」→ PDF という出力先が
  標準で存在する。
- **WKWebView (macOS)** — `src/wkwebview/mod.rs` 879-919 行目:
  `respondsToSelector(printOperationWithPrintInfo:)` (macOS 11+ でのみ
  真) を確認した上で `NSPrintInfo.sharedPrintInfo()` から
  `printOperationWithPrintInfo` → `runOperationModalForWindow_delegate_
  didRunSelector_contextInfo` という正規の `NSPrintOperation` モーダルを
  開く。macOS 10 以前では `can_print` が偽になり、**エラーにならず何も
  起きずに `Ok(())` を返す** (下記「既知の制約」参照)。
- **WebView2 (Windows)** — `src/webview2/mod.rs` 1801-1806 行目:
  `self.eval("window.print()", None)` — ページの JS コンテキストで
  `window.print()` を実行するだけ。WebView2 は Chromium ベースなので、
  これは Chrome/Edge の Ctrl+P と全く同じ印刷プレビュー UI (「Microsoft
  Print to PDF」を含む) を開く。COM を一切経由しない。

**この 1 メソッドで `unsafe` ゼロ・新規依存クレートゼロ**（`wry`/
`webview2-com`/`windows` は既存の D59 由来の依存のみ）で 3 OS 共通の
「印刷ダイアログを開く」が実現でき、そのダイアログ自身が (Windows/macOS/
Linux いずれも) PDF への出力先を持つ。D4 が back/forward をエンジンの
セッション履歴に任せて自前実装しなかったのと同じ理由 — **エンジンが既に
持っている機能を、ラッパー越しに安全に呼べるなら、VeloX 側で作り直さない**
— で、Ctrl/Cmd+P はこの `print()` 一本に決めた。

### 生 COM API (WebView2) も調べた — 世代差の結論

Issue の指示どおり、上記に落ち着く前に生の `ICoreWebView2` 系 API にも
実際に手を伸ばして確認した（D59 が D17 の結論をビルダーメソッドの外側で
覆した前例があるため、`WebViewExtWindows` 経由の生インターフェースまで
必ず見る）:

| API | インターフェース世代 | 到達可能性 |
|---|---|---|
| `ICoreWebView2_7::PrintToPdf`（ファイルへ直接、ダイアログ無し） | `ICoreWebView2_7` | **到達可能** — D59/D66 で実績のある `ICoreWebView2_13` より**古い**世代 |
| `ICoreWebView2Environment6::CreatePrintSettings` | `ICoreWebView2Environment6` | 到達可能（`_7` と同じ理由） |
| `ICoreWebView2_16::Print`（印刷ダイアログ相当、印刷完了コールバック付き） | `ICoreWebView2_16` | 理論上到達可能だが**未採用**（下記） |
| `ICoreWebView2_16::ShowPrintUI` | `ICoreWebView2_16` | 同上 |
| `ICoreWebView2_16::PrintToPdfStream` | `ICoreWebView2_16` | 同上 |

`webview2-com-sys-0.38.2/src/bindings.rs` を実際に読み、`ICoreWebView2_7`
(43213-43312 行目) が `PrintToPdf(resultfilepath: PCWSTR, printsettings:
Option<ICoreWebView2PrintSettings>, handler:
ICoreWebView2PrintToPdfCompletedHandler) -> Result<()>` を、
`ICoreWebView2Environment6` (15549-15596 行目) が
`CreatePrintSettings() -> Result<ICoreWebView2PrintSettings>` を持つことを
確認した。**WebView2 のインターフェース番号は累積的**（`_N` は常に `_N-1`
の上位互換のスーパーセットで、対応する WebView2 Runtime のバージョンが
新しいほど番号が大きい）ため、D59/D66 が既に「実績あり」と結論づけた
`ICoreWebView2_13` (D59 のサブリソースブロック、D66 の
`ClearBrowsingDataAll`) が届く実行環境なら、それより**古い** `_7`/
`Environment6` も届く。つまり `PrintToPdf` は D66 の `_13` キャストより
**むしろ安全側に倒れた賭け**であり、採用に足る根拠があると判断した。

一方 `Print`/`ShowPrintUI`/`PrintToPdfStream` はいずれも `ICoreWebView2_16`
(`webview2-com-sys-0.38.2/src/bindings.rs` 40358-40667 行目) が要求され、
これは D69 が「対応する WebView2 Runtime も相応に新しいバージョンを要求し、
この環境には実機の Windows が無く検証もできない」という理由で見送った
`ICoreWebView2_28` (Find API) ほどではないにせよ、D59/D66 で実績のある
`_13` より**新しい**世代であり、同じ「Windows 最優先は検証できない
最新 API に賭けることではない」という D69 の判断の型がそのまま当てはまる。
加えて `Print`/`ShowPrintUI` は `wry::WebView::print()` が既に (COM 抜きで)
同じユーザー体験 (印刷ダイアログを開く) を提供できてしまうため、わざわざ
`unsafe` な COM 呼び出しへ切り替える理由も無い。**よって `ICoreWebView2_16`
系は見送り**、Ctrl/Cmd+P は前述のとおり `wry::WebView::print()` に統一した。

### 実装したもの

1. **印刷 (Ctrl/Cmd+P、受け入れ条件「現在ページを印刷できる」)** —
   `ui::window::BrowserWindow::print_tab(tab_id) -> wry::Result<()>` が
   アクティブタブの content webview に対して `WebView::print()` を呼ぶ。
   D18/D23/D69/D72 と同じ二重配送: 信頼された toolbar webview からは
   構造化コマンド `ToolbarCommand::Print`（キー入力またはツールバーの
   印刷ボタン）、非信頼の content webview からは固定センチネル文字列
   `"velox:print"` → `ContentShortcut::Print`。どちらも
   `app::print_active_tab` に合流する。Issue #38 (ショートカット管理) が
   まだ無いための暫定固定割り当てである点も D69 と同じ。
2. **PDFとして保存（受け入れ条件「PDFとして保存できる」）** —
   全 OS 共通の一次手段は (1) のダイアログ自身が持つ PDF 出力先
   （Linux の GTK 印刷ダイアログの「ファイルに印刷」、macOS の印刷パネルの
   「PDF として保存」、Windows の Chromium 印刷プレビューの「Microsoft
   Print to PDF」）。加えて **Windows のみ**、ツールバーの「PDFとして
   保存」ボタン (`ToolbarCommand::SaveAsPdf`) からダイアログを介さない
   直接書き出しを提供する:
   - `browser::print`（`src/browser/print.rs`、新規、UI/エンジン非依存の
     純粋ロジック）— `Orientation`/`PaperSize`（A4/Letter/Legal、インチ
     単位。`ICoreWebView2PrintSettings::PageWidth`/`PageHeight` がインチ
     単位のため）/`Margins`/`PdfExportSettings`（scale・
     print_backgrounds を含む）と、それぞれの `sanitize()`（範囲外・
     非有限値をクランプ/デフォルトへフォールバック）。設定画面には
     まだ載せていない（このIssueのスコープ外、後述）ため現状は常に
     `PdfExportSettings::default().sanitize()` を使うが、将来 UI から
     値を受け取る際も同じ検証済みの経路を通せるようにしてある。
     `suggest_pdf_filename(title, url)` はページタイトル（あれば）→
     URL のホスト → 固定の汎用名、という順で保存ファイル名を決める純粋
     関数。25 件のユニットテストがある。
   - `ui::webview2_print`（`src/ui/webview2_print.rs`、新規、
     `#[cfg(windows)]`）— `wry::WebViewExtWindows::webview()`/
     `environment()`（D59/D66/D69 と同じ入口）から
     `ICoreWebView2Environment6::CreatePrintSettings` →
     設定を `browser::print::PdfExportSettings` から適用 →
     `ICoreWebView2_7::PrintToPdf` を呼ぶ。完了コールバックは
     `webview2-com` が既に用意している `PrintToPdfCompletedHandler`
     ヘルパー（D66 の `ClearBrowsingDataCompletedHandler` と全く同じ
     `#[completed_callback]` マクロ由来のヘルパーで、VeloX 側で COM
     vtable を組み立てる必要はない）を使い、成功可否を
     `UserEvent::PdfExportFinished { window_id, tab_id, destination,
     success, error }` として非同期に返す。`unsafe` はこのファイルの
     数箇所（`.cast::<T>()` 自体は安全だが、その後の COM プロパティ
     セッターと `PrintToPdf` 呼び出し自体が `unsafe fn`）にとどまり、
     すべて「このモジュールが生成・保持している生きた COM 参照に対する
     プレーンな COM 呼び出しであり、有効性を保証できる」という理由を
     コメントで明記した（D59/D66 と同じ形）。保存先ディレクトリ・
     ファイル名の決定は `browser::downloads` の既存資産をそのまま
     再利用した（新規ロジックを増やさない）:
     `resolve_download_dir_with_override`（Issue #16/#30 の
     `Config::download_dir_override`/`VELOX_DOWNLOAD_DIR`/OS既定値の
     解決をそのまま流用）と `prepare_destination`（ファイル名の
     サニタイズ・ディレクトリ作成・`report (1).pdf` 方式の重複回避を
     そのまま流用）。
   - **macOS/Linux**: `BrowserWindow::export_tab_as_pdf` の
     `#[cfg(not(windows))]` 側は `PdfExportRequest::UnsupportedPlatform`
     を返すだけで、ファイル書き込みは一切試みない — 「未実装」ではなく
     「呼べる安全な API が無い」ことを D75 の調査で確認した結果であり、
     CLAUDE.md の「Windows の実装を先に用意し、macOS/Linux は動作する
     ことを優先した最小実装で構わない」という方針どおりの意図的な
     見送り。呼び出し側 (`app::save_active_tab_as_pdf`) はこの場合、
     印刷ダイアログ (Ctrl/Cmd+P) から PDF を保存するよう促す
     ステータスメッセージを表示する。
3. **エラー表示（受け入れ条件「印刷失敗時にエラーを表示」）** —
   ツールバーに新設した `#print-status`（find bar/bookmark bar のような
   高さ加算式の帯ではなく、`#toolbar` 行内の 1 個の `<span>` —
   長いメッセージは CSS の `text-overflow: ellipsis` で省略表示しつつ
   `title` 属性に全文を保持、クリックまたは 8 秒で自動的に消える）に
   `veloxSetPrintStatus(message)` で表示する。`print_tab`
   の COM 非依存の失敗と `UserEvent::PdfExportFinished` の成功/失敗の
   両方がここに合流する。**ただし正直に書くと、この受け入れ条件は
   `print_tab`（Ctrl/Cmd+P）経路では実質的にほとんど満たせない**:
   `wry::WebView::print()` は 3 OS いずれも「ダイアログを開く/JS を
   実行する」呼び出し自体が失敗したかどうかしか `Result` に反映せず、
   実際の印刷ジョブの成否（ユーザーがダイアログをキャンセルした、
   プリンタが無い、ドライバエラー等）は wry 0.56 の公開 API からは一切
   観測できない（WebKitGTK 版は `run_dialog` の戻り値を捨てて常に
   `Ok(())`、macOS 版は古い macOS で無条件に `Ok(())`、Windows 版は
   `window.print()` という fire-and-forget な JS 呼び出し）。**この条件を
   本当の意味で満たせるのは Windows 限定の PDF 直接書き出し経路だけ**
   （`PrintToPdfCompletedHandler` が本物の成功/失敗を返す）。

### 見送ったもの・意図的なスコープ外

- **`ICoreWebView2_16::Print`/`ShowPrintUI`/`PrintToPdfStream`**:
  上記のとおり世代が新しすぎて実機検証できないため（D69 と同じ判断）。
- **ページ範囲の指定**: `ICoreWebView2PrintSettings`（`PrintToPdf` が
  受け取る基底インターフェース）には存在せず、`PageRanges` は
  `ICoreWebView2PrintSettings2`（`Print`/`ShowPrintUI` 側でのみ使う、
  物理プリンタ向けの拡張）にしかない。今回採用した `PrintToPdf` 経路では
  ページ範囲を指定する API 自体が無いため、PDF書き出しは常に全ページ
  出力になる。
- **設定画面 UI（用紙サイズ/向き/余白/背景印刷のユーザー選択）**:
  `browser::print::PdfExportSettings` は値の形と検証ロジックだけを
  用意し、`browser::settings::Settings`（Issue #30/D67）には今回フィールド
  を追加していない — 常に `PdfExportSettings::default()` を使う。この
  Issue のスコープ（「印刷・PDF保存が動く」）を超えるため、UI 化は
  follow-up とする。
- **macOS の `print_with_options`（余白カスタマイズ）**:
  `wry::WebViewExtDarwin`/`WebViewExtMacOS::print_with_options(&PrintOptions)`
  はマージン (`PrintMargin`) だけを受け取れる macOS 限定 API だが、
  CLAUDE.md の OS 優先度（macOS は最低限の整備）に従い、`print_tab` は
  3 OS とも引数無しの `print()` に統一し、この macOS 限定の余白調整には
  乗らなかった。
- **PDF書き出しのファイル名/ページ設定を選ぶダイアログ**: 「PDFとして
  保存」ボタンは常に既定のダウンロード先へ既定設定で書き出す
  （場所を選ばせない）。これは D28 の「保存先を選ばせず既定ディレクトリに
  即保存する」ダウンロードの流儀に合わせた意図的な単純化。

### 検証できたこと・できなかったこと（正直な記録）

**この開発環境は Linux のみで、Windows/macOS 実機は無い。**

- `cargo test`（Linux, `browser::print` 25 件 + `ui::toolbar` の新規
  print 系テスト + `ui::window` のセンチネル/スクリプトテスト）と
  `cargo clippy --all-targets -- -D warnings`（Linux）はエラー・警告
  0 件。単体テスト件数: 800 → 815（+15、減少なし）。統合テスト
  (`tests/integration.rs`) は 9 件のまま全て pass（この Issue は既存の
  「実プロセス起動・実タブ管理・実ファイル永続化」シナリオと直接
  関係しないため、新規の統合テストは追加していない）。
- `cargo check --target x86_64-pc-windows-msvc --all-targets` はエラー
  0 件（型チェックのみ、リンク・実行はしていない）。
  `ui::webview2_print` 内の 2 件の unit test（`orientation_to_native`
  の分岐網羅、D59 の `resource_type_from_context` テストと同じ位置づけ）
  はこの Linux 環境では `#[cfg(windows)]` によりビルドにすら含まれず
  一度も実行されていない — Windows 実機でのみ実行される。
  なお `cargo clippy --target x86_64-pc-windows-msvc --all-targets --
  -D warnings` は本 PR と無関係な**既存の**警告
  (`src/ui/webview2_blocking.rs` の `handle_request` が clippy の
  `too_many_arguments`(8/7) に抵触、D59 由来) で失敗する — `git stash`
  して確認したところ、この PR の変更を一切含まない `main` でも同じ理由で
  失敗することを確認済み。本 PR の要求検証コマンドには含まれておらず
  （CLAUDE.md 上も `cargo check --target ... --all-targets` のみが必須）、
  本 PR が原因でもないため直していない。
- **実行時の動作（実際に印刷ダイアログが開くか、PDF が書き出されるか、
  Chromium の印刷プレビューに Microsoft Print to PDF が実在するか、
  `ICoreWebView2Environment6`/`ICoreWebView2_7` へのキャストが実機の
  WebView2 Runtime で本当に成功するか）は一切確認できていない** —
  WebView2 ランタイムも Windows も macOS も無いため。

**Revisit condition**: (1) `ICoreWebView2_16` 系 API — 対象ランタイムの
普及が進み、実機検証できる環境が揃った段階で `Print`/`ShowPrintUI` への
切り替え（本物の印刷完了コールバック、物理プリンタへの直接印刷、ページ
範囲指定）を再検討する。(2) 設定画面 (#30) に印刷設定タブを追加する際、
`browser::print::PdfExportSettings` をそのままフォームの検証層として
再利用する。(3) Issue #38 のキーバインド管理層が実装されたら、
Ctrl/Cmd+P の固定割り当てをそこに載せ替える。(4) macOS の余白カスタム
(`print_with_options`) — macOS 本格対応 (#33) に着手する段階で検討する。


## D76: 名前を付けて保存 (#46) — Windows は WebView2 の `CallDevToolsProtocolMethod` で MHTML 保存 + ネイティブ Save-As ダイアログ、macOS/Linux は outerHTML の素の保存に留める

**背景 (Issue #46, 依存 #16)**: 現在のページをユーザーが指定した場所へ保存
できるようにする。受け入れ条件は (1) 保存先を選択できる、(2) ページを
保存できる、(3) 同名ファイルを安全に扱える、(4) エラー時に原因を表示
できる、の 4 つ。PR #153 (Issue #45 View Source) の申し送りにあった
「`outerHTML` スナップショットとは別の取得経路が必要になる可能性が高い」
という懸念を最初に検証した。

### 保存形式の選択: Windows は MHTML、macOS/Linux は outerHTML

**WebView2 (Windows) の到達可能性を実ソースで確認した**
(`webview2-com-sys` 0.38.2 のバインディング、および `windows` 0.61.3 を
実際にフェッチして調査):

- `ICoreWebView2::CallDevToolsProtocolMethod` は **ベースの `ICoreWebView2`
  インターフェース**(`ICoreWebView2_NN` の世代を問わない、WebView2 の
  最初期の安定版から存在する)のメソッドで、Chromium DevTools Protocol の
  任意のメソッドを呼び出せる。ここに `Page.captureSnapshot`
  (`{"format":"mhtml"}`) を渡すと、ページの HTML に画像・CSS・サブフレーム
  などのサブリソースを base64/quoted-printable でインライン化した
  **MHTML 文書一式**が `{"data": "...mhtml テキスト..."}` という JSON で
  返ってくる。D59/D66 が到達できた `ICoreWebView2_13` はおろか、世代の
  縛りが一切ないベースインターフェースなので、D69 が経験した「世代が
  新しすぎて見送る」という制約に一切当たらない。
- 一方、WebView2 には `ShowSaveAsUI`/`SaveAsUIShowing`
  (`COREWEBVIEW2_SAVE_AS_KIND_COMPLETE`/`_HTML_ONLY`/`_SINGLE_FILE` を含む、
  ブラウザ本体の「名前を付けて保存」に相当するネイティブ機能一式)も存在
  することが分かったが、これらは **`ICoreWebView2_25`** で初めて追加された
  メソッド/イベントで、D69 が F12/Ctrl+F 相当の判断で見送った前例
  (「世代が新しすぎる」)と全く同じ理由で、今回も採用を見送った。
  Evergreen ランタイムは自動更新されるとはいえ、`_25` のような非常に新しい
  世代を前提にすると、更新が遅れた実機で機能ごと動かなくなるリスクが高い。
  `CallDevToolsProtocolMethod` で同じ目的 (MHTML 保存) を世代非依存で
  達成できる以上、あえて `_25` に依存する理由がない。

結論: **Windows は `CallDevToolsProtocolMethod("Page.captureSnapshot",
{"format":"mhtml"})` で MHTML を取得し、そのまま `.mhtml` として保存する**
(`src/ui/save_dialog_windows.rs::capture_and_write_mhtml`)。これにより
Windows では画像・CSS も含めた「完全なページ」の保存が実現できる。

**macOS/Linux は CLAUDE.md の OS 優先度方針どおり最低限の実装に留めた**:
`document.documentElement.outerHTML` を `evaluate_script_with_callback` で
取得し(`fetch_page_title`/`fetch_favicon` と同じ fire-and-forget パターン)、
`<!doctype html>\n` を前置しただけの単一 HTML ファイルとして保存する
(`browser::save_page::wrap_outer_html_as_document`)。**この経路では画像・
外部 CSS・その他のサブリソースは一切取得・埋め込みされない** —
ページの見た目を完全に再現した保存にはならない、既知の制限として明記する。
View Source (#45, D72) の「HTML エスケープ済みテキスト」とは異なり、
こちらは実際にブラウザで開ける生のマークアップをエスケープなしでそのまま
書き出す(表示専用ではなく保存が目的のため)。

Windows と macOS/Linux で保存経路がここまで非対称になったのは、
`CallDevToolsProtocolMethod` が(現時点で調査した範囲では)WebView2 固有の
到達手段であり、WebKitGTK/WKWebView 側で同等に手軽な「エンジンに生の
MHTML を作らせる」公開 API を wry 0.56 経由で見つけられなかったため。
CLAUDE.md の OS 優先度方針(Windows 最優先、macOS/Linux は動作維持を優先)
に沿って、macOS/Linux 側の追加調査(たとえば独自の DOM 巡回によるリソース
インライン化)はこの Issue のスコープ外とした。

### 保存先の選択: Windows はネイティブ `IFileSaveDialog`、macOS/Linux はダウンロードフォルダ固定

「保存先を選択できる」を満たすため、まずクロスプラットフォームのファイル
ダイアログクレート (`rfd`) の採用を検討したが、`cargo fetch` で実際に
依存グラフを確認したところ Linux ビルドだけで gtk3/wayland/wasm-bindgen
系のクレートを大量に引き込むことが分かり、「依存クレートは必要最小限に
保つ」という方針(CLAUDE.md、D6)に反すると判断して見送った。

代わりに **Windows のみ、Win32 の Shell Common Item Dialog API
(`IFileSaveDialog`/`IShellItem`、`windows` クレートの `Win32_UI_Shell`/
`Win32_UI_Shell_Common` フィーチャ)を素の COM 呼び出しで実装した**
(`src/ui/save_dialog_windows.rs::show_save_dialog`)。これは WebView2 固有
の API ではなく、あらゆる Windows ネイティブアプリの「開く/保存」ダイアログ
が使う古典的な仕組みであり、`ui::webview2_blocking` (D59) が確立した
「wry の外側で素の COM を呼ぶ」パターンをそのまま踏襲している。
`FOS_OVERWRITEPROMPT` を設定しているため、選んだパスに既存ファイルが
あれば **OS 標準の「上書きしますか?」確認**が出る — これが「同名ファイル
を安全に扱える」の Windows での答えであり、VeloX 側で衝突検出ロジックを
実装する必要がない。

**macOS/Linux はダイアログなし**: 設定画面の「ダウンロード先フォルダ」
(Issue #30/D67, `Config::download_dir_override`)で解決したディレクトリに
自動保存する。同名ファイルの衝突は既存の
`browser::downloads::unique_filename`/`prepare_destination` をそのまま
再利用し、`report.html` → `report (1).html` の方式で回避する(ダウンロード
機能 #16 と全く同じ挙動)。「保存先を選べる」という受け入れ条件は、
macOS/Linux では今回満たせていない既知の制限として記録する。

### ファイル名サニタイズ: ページタイトル向けに新しい防御層を追加した

**保存するファイル名はページのタイトル(または URL のホスト名)由来であり、
ページ自身が `document.title` を通じて完全に制御できる、信頼できない
入力である**(D62 の「入力値堅牢性」の脅威モデルそのもの)。

`browser::downloads::sanitize_filename` (Issue #16, D28) がすでに
パストラバーサル・NUL/制御文字・Windows 予約デバイス名 (`CON`/`PRN`/
`AUX`/`NUL`/`COM1`-`COM9`/`LPT1`-`LPT9`)・末尾のドット/空白・長さ超過を
一通り防いでいるため、これをそのまま再利用した。ただし
`sanitize_filename` は「ダウンロードの提案ファイル名」(`Content-Disposition`
やダウンロード URL 由来、パスらしい文字列であることが多い)向けに設計
されており、パストラバーサル対策が「最後の `/`/`\` 区切りセグメントだけ
残す」という方式になっている。ページの**タイトル**は自由なテキストであり
`:`/`/`/`|`/`?` を単なる句読点として含むことが日常的にある
(例: "Breaking: Top Story"、"Tips \& Tricks (Q\&A)") ため、この方式を
そのまま適用するとタイトルの大部分を意図せず失ってしまう。

そこで `browser::save_page::suggested_file_name` に新しい前処理層を
追加した: **Windows で禁止されているファイル名文字
(`< > : " / \\ | ? *`)を `sanitize_filename` に渡す前にすべて `_` へ
置換する**(`replace_forbidden_filename_chars`)。これにより `/`/`\\` を
含め一切のパス区切り文字が残らない(＝トラバースする対象が存在しない)
ことを保証しつつ、タイトルの可読性を極力保つ。処理順序は:
`default_file_stem`(タイトル、または URL のホスト、またはフォールバック
`"page"`)→ `replace_forbidden_filename_chars` → `downloads::sanitize_filename`
→ 最後に `.mhtml`/`.html` を付与、の 4 段階。

**単体テストでの検証** (`src/browser/save_page.rs`、全 21 件):

- `neutralizes_path_traversal_in_the_title` — タイトルが
  `"../../etc/passwd"` でも、生成されたファイル名に `/`/`\\` が一切残らず
  `Path::components().count() == 1`(ディレクトリ成分が存在しない)ことを
  検証。
- `neutralizes_an_absolute_windows_path_in_the_title` — タイトルが
  `r"C:\Windows\System32\evil.exe"` でも同様。
- `escapes_a_windows_reserved_device_name_title` /
  `does_not_flag_a_title_that_merely_starts_with_a_reserved_prefix` —
  `CON`/`con`/`LPT9` は `_CON`/`_con`/`_LPT9` に、`CONSTITUTION` は
  誤検知されないことを検証(`sanitize_filename` 側の既存ロジックの再確認)。
- `trims_trailing_dots_and_spaces_from_the_title` /
  `strips_control_characters_from_the_title` — Windows が許さない末尾の
  ドット・空白、および NUL 等の制御文字が除去されることを検証。
- `falls_back_to_a_safe_name_when_the_title_is_only_dot_or_dotdot` —
  タイトルがそのまま `"."`/`".."` の場合、`sanitize_filename` の
  フォールバック名 (`download`) に落ちることを検証。
- `stays_non_empty_when_the_title_is_entirely_forbidden_characters` —
  タイトルが `":::"` のように禁止文字だけで構成されていても
  (`"___"` のように)空文字列にはならず、`download.html` への意図しない
  衝突を避けられることを検証。
- `truncates_an_extremely_long_title` / 各種 Unicode・空タイトルのケースも
  網羅。

`extract_mhtml`(`Page.captureSnapshot` の JSON 応答パース)も、実際の
WebView2 なしで検証できる範囲(正常系・欠損フィールド・不正 JSON)を
単体テストでカバーした。

### エラー時に原因を表示できる: 新しい UI を作らず Downloads パネルを再利用した

保存の進行状況・失敗理由を表示する専用 UI を新設する代わりに、Issue #16
(D28)の `DownloadStore`/Downloads パネルをそのまま再利用する設計にした。
`UserEvent::SavePageStarted`/`SavePageFinished` を新設し、
`DownloadStarted`/`DownloadCompleted` と全く同じ形で `DownloadStore` に
登録・完了させる。`SavePageFinished` は `error: Option<String>` を
そのまま人間可読な日本語メッセージとして運べるため、
`DownloadCompleted` が失敗時に常に固定文言("ダウンロードに失敗しました")
しか出せないのと異なり、**実際の失敗理由**(保存先ダイアログの COM
エラー、ファイル書き込みエラー、MHTML 取得エラー等)がそのまま
Downloads パネルの該当行に表示される。

保存先がまだ決まっていない段階の失敗(タブが休止中で webview が無い、
ネイティブダイアログの生成に失敗、ダウンロードフォルダの作成に失敗、
等)も、`SavePageFinished` 単体ではなく必ず `SavePageStarted` →
`SavePageFinished(error)` の順で 2 つのイベントを送るようにした
(`ui::window::report_save_page_failure`)。理由:
`DownloadStore::resolve_completion` は `InProgress` のエントリを url +
destination で解決する仕組みのため、対応する `Started` を送らずに
`Finished` だけ送ると解決先が無く、失敗が Downloads パネルに一切
現れない(エラーを"表示できる"はずが実際には無言で消える)という
バグになる。この 2 つは同じプレースホルダ (`PathBuf::new()`)
destination を共有するため、`resolve_completion` の厳密一致経路で
確実にペアが解決される。

ユーザーが Windows のネイティブ保存ダイアログを**キャンセル**した場合は
唯一の例外で、これはエラーではないため `SavePageStarted`/`Finished` の
どちらも送らない(Downloads パネルに何も残らない、キャンセル操作として
自然な挙動)。

### ショートカット: D18/D23/D69/D72 と同じ二重配送パターン

Ctrl/Cmd+S を、既存の全ショートカットと同じ二重配送で実装した:

- 非信頼の content webview 側は固定センチネル文字列
  `velox:save-page` (`ContentShortcut::SavePage`)。
- 信頼された toolbar 側は構造化コマンド `{"cmd":"save_page"}`
  (`ToolbarCommand::SavePage`)。

どちらも `app::request_save_page` に収束し、アクティブタブの
`Tab::current_url()`/`Tab::title()` を読んで
`BrowserWindow::request_save_page` に渡すだけの薄い関数になっている。
Issue #38(キーボードショートカット管理)が着手されたら、変更が必要な
箇所は「JS 側でどのキーを監視するか」の 1 か所
(`tab_shortcut_script`/`toolbar.html` のキーダウンリスナー)だけで済む。

**Save Page の導線は今回 Ctrl/Cmd+S のみ**とした(View Source, D72 と同じ
スコープ判断)。ツールバーへのボタン追加や右クリックメニューからの起動
(Issue #39)は見送った — 既存のツールバーに保存専用のボタン/メニューが
一切無く、新設するには CSS・レイアウトの検討が別途必要になるため。

### 複数ウィンドウ対応 (#29/D68)

`SavePageStarted`/`SavePageFinished` は他の per-tab `UserEvent` と同じく
`WindowId` を明示的に持つ(`TabId` はウィンドウ内でのみ一意という前提は
崩していない)。`DownloadStore` 自体はウィンドウ横断で共有される既存の
設計(D28)をそのまま踏襲し、Downloads パネルの更新はイベントが由来する
`window_id` のウィンドウにのみ反映する(`DownloadStarted`/`DownloadCompleted`
と全く同じ扱い)。

### 追加した依存クレート

新規追加なし。`windows`(既存の Windows 専用依存, D59)に
`Win32_UI_Shell`/`Win32_UI_Shell_Common` フィーチャを追加しただけ
(Cargo.toml に理由を記載)。`rfd` は上述の理由で見送った。

### 検証できていないこと・満たせなかった受け入れ条件

- **実機 Windows/WebView2 での動作確認はできていない**(この開発環境が
  Linux のみのため)。`cargo check --target x86_64-pc-windows-msvc
  --all-targets` で型レベルの整合は確認したが、リンク・実行はしていない。
  特に以下は未検証:
  - `IFileSaveDialog::Show` を tao のイベントループ内(モーダル呼び出し)
    から呼んだ際の実際の挙動(理論上は Win32 のネイティブモーダルとして
    問題なく動くはずだが、実機での確認はできていない)。
  - `CallDevToolsProtocolMethod` の完了ハンドラが実際にどのスレッド/
    タイミングで呼ばれるか、および数百 KB〜数 MB 級の MHTML 文字列を
    問題なく往復できるか。
  - WebView2 が Ctrl+S を独自のアクセラレータとして先取りしてしまわないか
    (D69 が Ctrl+F について残した懸念と同種)。
- **macOS/Linux では「保存先を選択できる」を満たせていない** —
  ダウンロード先フォルダに固定保存される(上述)。
- **macOS/Linux では保存されたページに画像・外部 CSS が含まれない** —
  `outerHTML` のみの保存のため(上述)。
- **Windows の保存ダイアログはオーナーウィンドウに紐付けていない**
  (`Show(None)`)。`raw-window-handle` 経由で HWND を取得する追加実装を
  見送ったため、ダイアログがブラウザウィンドウの背後に隠れる可能性が
  理論上ある。
- **同一ページを複数回連続で保存する際の同時実行**は特に考慮していない
  (通常の操作フローでは起こりにくいと判断)。

**Revisit condition**: (1) Issue #38(キーボードショートカット管理)に
着手するとき、Ctrl/Cmd+S の割り当てをその仕組みに載せ替える。(2) 実機
Windows での検証ができるようになったとき、上記の未検証事項を確認する。
(3) macOS/Linux 側でも本格的な「完全なページ」保存(リソースの取得・
埋め込み)が必要になったとき、独自の DOM 巡回実装を検討する。(4) Issue
#39(コンテキストメニュー)に着手するとき、右クリックからの保存導線を
追加する。


## D77: キーボードショートカット管理 (#38) — 既存ショートカットの棚卸しと
`browser::shortcuts::SHORTCUT_TABLE` への集約、衝突検出、信頼境界は不変

**対象**: Issue #38 の受け入れ条件 4 点 (主要ショートカットの一元定義、
OS 別 modifier 表示、キー衝突検出、テストでの主要 mapping 保証)。依存
Issue #11 (タブ管理ショートカット)/#30 (設定基盤) は着手済み。

### 前提: この Issue は「ゼロから作る」ではなく「集約」

着手前に既存実装を棚卸しした結果、VeloX には既に **21 個**のキーボード
ショートカットが実装済みで、`src/ui/window.rs`(`ContentShortcut`/
`tab_shortcut_script`/`parse_content_shortcut`、content webview 向けの
センチネル文字列チャネル)、`src/ui/toolbar.rs`/`toolbar.html`
(`ToolbarCommand`、toolbar 自身の trusted チャネル)、`src/app.rs`
(`handle_content_shortcut`/`handle_toolbar_command`)、
`src/browser/settings.rs`(`shortcut_reference`、設定画面 Shortcuts タブの
表示専用リスト、Issue #30/D67) の**最大 4 箇所**に定義が散在していた:

| # | 操作 | キー | 導入 Issue |
|---|------|------|-----------|
| 1 | 新しいタブ | Ctrl/Cmd+T | #11 |
| 2 | タブを閉じる | Ctrl/Cmd+W | #11 |
| 3 | 閉じたタブを再度開く | Ctrl/Cmd+Shift+T | #11 |
| 4 | 次のタブ | Ctrl/Cmd+Tab | #11 |
| 5 | 前のタブ | Ctrl/Cmd+Shift+Tab | #11 |
| 6-13 | 1〜8番目のタブに切り替え | Ctrl/Cmd+1〜8 | #11 |
| 14 | 最後のタブに切り替え | Ctrl/Cmd+9 | #11 |
| 15 | アドレスバーにフォーカス | Ctrl/Cmd+L | #15 |
| 16 | ブックマークの追加/削除 | Ctrl/Cmd+D | #19 |
| 17 | ブックマークバーの表示切替 | Ctrl/Cmd+Shift+B | #19 |
| 18 | 新しいウィンドウ | Ctrl/Cmd+N | #29 |
| 19 | ページ内検索を開く | Ctrl/Cmd+F | #43 |
| 20 | ページのソースを表示 | Ctrl/Cmd+U | #45 |
| 21 | DevTools を開く | F12 (macOS: Cmd+Option+I) | 初期実装 (D18) |

棚卸し中に見つかった実害のあるドリフト: `settings::shortcut_reference`
(#30 が D67 で作った設定画面の一覧) は #29 (`NewWindow`) と #43
(`OpenFindBar`) を欠いたままだった — この 2 つは `ui::window`/
`ui::toolbar` には実装済みなのに、設定画面には一度も表示されていなかった。
定義が 1 箇所に無いと起きる、まさに本 Issue が解消すべき問題の実例。

エディタ内 (`urlInput`/`findInputEl`/`historySearchEl` 等) の Enter/Esc/
Shift+Enter は対象外とした: これらは特定 UI 要素にフォーカスがあるときだけ
意味を持つウィジェット固有の挙動で、OS 別 modifier も衝突検出も本質的に
関係しない (Ctrl/Cmd を伴わない、対象範囲外)。

### 設計: `browser::shortcuts` を唯一のテーブルにする

新設 `src/browser/shortcuts.rs`(UI/エンジン非依存、`browser::` 配下 —
アーキテクチャの 4 層分離を維持) に以下を実装した:

- **`Platform`**(Windows/MacOs/Linux) — `Platform::current()` のみが
  `cfg(target_os)` を読み、他の関数はすべて `Platform` を引数に取る
  purely な形にした。CI が Linux でしか動かない (CLAUDE.md の OS 優先度
  方針) 環境でも、3 OS 分のラベル生成ロジックを全て単体テストできる。
- **`Key`/`Modifiers`/`KeyChord`** — 物理キーと修飾キーの組。
  `Modifiers::primary` は「Ctrl-or-Cmd を両方受け付ける」という*表示上*の
  概念であり、実際のキー判定 (`event.ctrlKey || event.metaKey`、D23) は
  一切変更していない — `KeyChord::label(platform)` は表示文字列
  (`"Ctrl+T"`/`"Cmd+T"`) を組み立てるだけの純粋関数。
- **`ShortcutId`** — 21 個の操作それぞれに対応する安定な識別子。
  `serde(rename_all = "snake_case")` を付け、将来 `Settings` に
  `HashMap<ShortcutId, KeyChord>` 的な上書きテーブルを足す際の鍵として
  そのまま使える形にした (「将来のユーザーカスタマイズを考慮した定義形式」
  という受け入れ条件への回答)。
- **`SHORTCUT_TABLE: &[ShortcutDef]`** — 21 行のテーブル。各行は
  `{ id, label (日本語表示名), chords }`。これが**唯一の**定義箇所。
- **`find_conflicts(&[ShortcutDef]) -> Vec<ShortcutConflict>`** — 同一
  `KeyChord` に 2 つ以上の異なる `ShortcutId` が結び付いていないかを
  検出する純粋関数。`SHORTCUT_TABLE` 自体に衝突が無いことを回帰テスト
  (`default_table_has_no_conflicts`) として固定した — #27/#40/#46 が
  Ctrl/Cmd+Shift+N・+P・+S を追加する際、既存の割り当てと衝突すれば
  この 1 テストが red になる。
- **`parse_sentinel(&str) -> Option<ShortcutId>`** — センチネル文字列 →
  `ShortcutId` の逆引き。`SHORTCUT_TABLE` を線形走査して厳密一致のみを
  見る、コンパイル時に閉じた集合に対する検索であり、JSON 化やパターン
  マッチの類推は一切行わない。

### 信頼境界 (D18/D23) は一切変更していない

**この Issue が変えたのは「同じ文字列がどこで宣言されているか」だけで、
「content webview から何が送れるか」は 1 文字も変えていない。**

- content webview の IPC ハンドラは今までどおり、`window.ipc` から届いた
  生文字列を**厳密一致でのみ**比較する。`ui::window::parse_content_shortcut`
  は `browser::shortcuts::parse_sentinel` に処理を委譲するようになったが、
  `parse_sentinel` 自体も「コンパイル時に固定された文字列の集合との厳密
  一致」以外の何もしない — JSON デコードや構造化データの解釈は一切ない。
  これは D18/D23 が定めた「content webview の IPC チャネルは
  `ToolbarCommand` 型の構造化コマンドパーサに成長させてはいけない」という
  制約をそのまま維持している。
- **`ShortcutId::OpenDevtools` は `ContentShortcut` に写像されない**
  (`parse_content_shortcut` が明示的に `None` を返す) — DevTools は今までと
  同じ、独立した `OPEN_DEVTOOLS_MESSAGE`/`devtools_shortcut_script`
  経由の配送を維持している (`devtools` フィーチャ/`debug_assertions` の
  gating も含め D18 のまま)。`SHORTCUT_TABLE` に載っているのはドキュメント
  化と衝突検出のためであり、配送経路を統合したわけではない。
- toolbar (信頼済み webview) 側は今までどおり `ToolbarCommand` という
  実 enum を送り続ける。`SHORTCUT_TABLE` はこの enum を生成しない —
  生成してしまうと「データ駆動のコマンド」という、まさに D18 が禁じている
  形に近づいてしまうため、意図的に手動のまま残した (下記参照)。
- **新規テスト** `content_webview_cannot_smuggle_an_unknown_command_
  through_the_shortcut_channel`(`src/ui/window.rs`) で、`ToolbarCommand`
  を偽装した JSON ペイロードや近似文字列を含む未知のコマンド文字列が
  すべて `None` になることを明示的に検証した。既存の
  `parse_content_shortcut_rejects_anything_not_an_exact_known_sentinel`/
  `parse_content_shortcut_does_not_panic_on_hostile_content_webview_input`
  (Issue #35 由来) は変更せずそのまま維持している。

### 新しいショートカットを 1 つ追加するとき、実際に触る箇所

目標としていた「1 行足せば済む」は content webview 側の配送コードに限って
達成できた。全体としては以下の通り (#27/#40/#46 が Ctrl/Cmd+Shift+N・+P・
+S を統合する際の実際の作業量):

- **`browser::shortcuts::SHORTCUT_TABLE` に 1 行追加**(`ShortcutId` に
  バリアントを 1 つ追加、`sentinel()` に 1 アーム追加) — これだけで
  ①衝突検出のスコープに入る、②設定画面 Shortcuts タブに表示される
  (`settings::shortcut_reference` がテーブルを読むだけになったため)、
  ③content webview 向け JS (`tab_shortcut_script`、
  `tab_shortcut_branches` が `SHORTCUT_TABLE` を読んで `if`/`else if`
  チェーンを自動生成する) にも自動的に反映される。
- **`ui::window::ContentShortcut` に 1 バリアント追加、
  `parse_content_shortcut` に 1 アーム追加** — content webview 経由でも
  発火させたい場合のみ。型安全な enum を保つための必須の手作業で、
  データテーブルには生成させない (信頼境界の節を参照)。
- **`ui::toolbar::ToolbarCommand` に 1 バリアント追加、`toolbar.html` の
  `keydown` リスナーに 1 分岐追加** — toolbar (アドレスバー/パネルに
  フォーカスがあるとき) 経由でも発火させたい場合。同じ理由で手作業のまま。
- **`app.rs` の `handle_content_shortcut`/`handle_toolbar_command` に
  1 アームずつ追加** — 実際の処理を呼ぶ。

4 箇所が 2 箇所 (テーブル 1 箇所 + toolbar 側の enum/JS/dispatch) に減った。
toolbar 側が残るのは、D18 が「toolbar は構造化コマンドを送ってよい信頼済み
チャネル」と定めていることの直接の帰結であり、それ自体を自動生成に
置き換えることは信頼境界の設計そのものに触れるため見送った (下記参照)。

### 見送ったこと・後続 Issue に切り出したこと

- **設定画面からの再割り当て UI は実装していない**。`ShortcutId`/
  `KeyChord` は serde 対応済みで `Settings` に上書きテーブルを足す土台は
  あるが、実際に `Settings` へフィールドを追加し、`toolbar.html`
  の Shortcuts タブを編集可能にし、`ToolbarCommand::UpdateSettings` 経由で
  検証・永続化する UI 実装は行っていない。理由: 本 Issue の主目的である
  「既存ショートカットの集約・衝突検出・OS 別 modifier 表示」だけでも
  実装・テストの規模が大きく、UI までスコープに入れると本 PR の変更範囲が
  過大になると判断した。後続 Issue #156 として切り出し、
  `cost:medium`/`benefit:3` を付与した。
- **toolbar 側 (`ToolbarCommand`/`toolbar.html`) の JS/enum の自動生成**は
  行っていない。content webview 側と同じ生成パターンを適用することも
  技術的には可能だが、`ToolbarCommand` は `serde` の構造化コマンドであり
  (フィールド付きバリアントもある)、汎用的なコード生成にすると
  「データテーブルが実質的にコマンドディスパッチを決める」形に近づき、
  D18 が意図的に避けた設計に踏み込むリスクがあるため見送った。
  `find_conflicts`/`shortcut_reference` は toolbar 側の割り当ても
  (`SHORTCUT_TABLE` に含めているので) カバーしている。
- **macOS 実機での動作検証はできていない** (CLAUDE.md の OS 優先度方針
  どおり、Linux で実装・テストし、`cargo check --target
  x86_64-pc-windows-msvc` で Windows のコンパイルのみ確認した)。
  `Platform::label`/`Platform::current` の macOS 分岐は単体テストで
  網羅しているが、実機の Cmd キー入力そのものは D23 から変更していない
  ため新規リスクではないと判断した。

**Revisit condition**: (1) 上記の再割り当て UI を実装する後続 Issue に
着手するとき — `ShortcutId`/`KeyChord` の serde 形式を土台にした
`Settings` 拡張から始める。(2) #27/#40/#46 のショートカットを実際に
`SHORTCUT_TABLE` へ統合するとき — 本 D77 の「触る箇所」の手順に従う。

## D78: コンテキストメニュー (#39) — ネイティブ API ではなく JS 描画を採用、content webview からの入力は専用の境界付き第 3 チャネルとして扱う

**対象**: Issue #39 の受け入れ条件 4 点 (右クリックでメニュー表示 /
リンク上の操作が対象 URL を正しく扱う / テキスト選択時の検索 /
DevTools 導線との統合)。依存に挙がっている #11 (タブ管理) は実装済み、
#28 (DevTools 統合) は D18 で実装済みのうえ `#duplicate` としてクローズ
済みで、本 Issue が求める「Inspect 項目」はその `BrowserWindow::
open_devtools` をそのまま再利用するだけで満たせる。#27 (プライベート
ウィンドウ)・#40 (印刷)・#46 (名前を付けて保存)・#38 (ショートカット
管理) は並行進行中 (#27 は本 PR 作成中にマージされ、後述のとおり
origin/main を取り込んで統合済み) — 「メニュー項目を 1 つ足すのに
最小の変更で済む」設計にした理由と、後述の切り出し Issue を参照。

### 調査: wry 0.56 でネイティブのコンテキストメニューに届くか

D25/D59/D60/D66/D69 と同じ「issue の指示に頼らず実ソースを読む」調査
スタイルを踏襲し、3 エンジンそれぞれを実際のベンダーソース
(`~/.cargo/registry/src/.../wry-0.56.1`, `webview2-com-sys-0.38.2`) で
確認した。

- **WebView2 (Windows) — `ICoreWebView2_11::add_ContextMenuRequested` が
  実在し、しかも D59/D66 が既に実績のある `ICoreWebView2_13` より**さらに
  古い**インターフェース世代だった** (`webview2-com-sys-0.38.2/src/
  bindings.rs` 39492 行目、`ICoreWebView2ContextMenuRequestedEventArgs`
  一式も 8095 行目に確認)。D69 (ページ内検索) が `ICoreWebView2Find`
  (`ICoreWebView2_28`) を「新しすぎて実機検証できない」という理由で
  見送ったのとは対照的に、こちらは Windows 最優先の方針にとって
  むしろ有望な選択肢に見えた。しかし採用しなかった。理由は 2 点:
  1. **メニュー項目 (`ICoreWebView2ContextMenuItemCollection`) の追加/
     削除/`Kind` (Command/CheckBox/Submenu/Separator) の扱い、および
     `ICoreWebView2ContextMenuTarget` から `HasLinkUri`/`LinkUri`/
     `HasSourceUri`/`SourceUri`/`SelectionText` 等を読み出す一式は、
     D59 の `WebResourceRequested` (COM オブジェクト 1 個からのプロパティ
     読み出しのみ) よりもはるかに大きい COM 表面積で、しかも
     `GetDeferral`/非同期完了ハンドラまで絡む。
  2. **macOS (WKWebView) と Linux (WebKitGTK) には全く別の API 系統
     (`webView:contextMenuConfigurationForElement:completionHandler:`と
     `WebKitWebView::context-menu` シグナル + `WebKitContextMenu`) しか
     存在せず**、しかも wry 0.56 のソース (`src/wkwebview/mod.rs`、
     `src/webkitgtk/mod.rs`) にはどちらの委譲/シグナルへのフックも
     公開されていない。仮に Windows だけネイティブ実装を作っても、
     macOS/Linux 向けには結局 JS ベースの別実装が要る — 1 機能に
     全く異なる 3 系統の「メニュー項目」表現 (COM オブジェクト /
     Objective-C ブロック / GTK ウィジェット) を抱えることになり、
     「メニュー項目を 1 つ足すときに触る箇所を最小にする」という
     設計目標そのものと真っ向から矛盾する。D69 が「Windows は理論上の
     経路はあるが検証不能な最新 API のみ、macOS は経路無し」という
     非対称な結論から「3 エンジンとも JS で統一」を選んだのと、今回は
     根拠は違う (Windows 側は経路が"ある") が結論の形は同じになった。
- **WKWebView (macOS) — ネイティブ相当の API 自体は存在するが wry
  未公開。** `webView(_:contextMenuConfigurationForElement:
  completionHandler:)` (`WKUIDelegate`) がネイティブの右クリックメニュー
  をカスタマイズする唯一の経路だが、wry 0.56 の `WryWebViewDelegate`
  (デスクトップ版 `src/wkwebview/mod.rs`) はこのデリゲートメソッドを
  実装/公開していない。
- **WebKitGTK (Linux) — `WebKitWebView::context-menu` シグナル自体は
  存在するが、これも wry からは配線されていない**。D69 の
  `find_controller()` のように `wry::WebViewExtUnix::webview()` 経由で
  生の `webkit2gtk::WebView` は取れる (`connect_context_menu` は
  `webkit2gtk` クレートが安全にラップ済み) ものの、上記 2 点の理由
  (COM 側の表面積・3 系統の非対称性) がそのまま当てはまるため、
  「Linux だけ個別対応する」価値はないと判断した (CLAUDE.md の OS
  優先度どおり、Linux は「動けば十分」であり、ここに独自実装を割く
  理由がない)。

**結論**: 3 エンジンとも、**content webview 内で `contextmenu` イベントを
捕捉し `event.preventDefault()` で既定メニューを止め、VeloX 側で
メニューを描画する** JS ベースの方式に統一した (issue が挙げた選択肢の
2 番目)。`event.preventDefault()` は DOM 標準として 3 エンジンすべてが
尊重する (自前の右クリックメニューを持つ Web アプリが日常的に使っている
挙動そのもの) ため、D69 のように「Windows は動くが他 2 つは経路が無い」
という非対称な妥協を選ぶ必要すらなかった。Windows だけ追加で
`wry::WebViewBuilderExtWindows::with_default_context_menus(false)`
(`lib.rs` 1810 行目、`#[cfg(windows)]`) も掛けている — これは
`preventDefault()` が何らかの理由で効かない場合の多層防御であり、
CLAUDE.md の Windows 最優先方針を「保険を厚くする」形で反映したもので、
JS 側の対応が本質的に不十分だからではない。

### 最優先で守った設計: content webview からの入力は信頼できない — D18/D23 の二重配送とは別の、第 3 の境界

D18/D23 が確立した「トールバー webview は信頼済み (構造化コマンド) /
content webview は信頼できない (固定センチネル文字列のみ)」という原則は、
今回**そのままの形では使えない**。コンテキストメニューは本質的に
「クリック位置に何があったか」という**データ**(リンク URL・画像 URL・
選択テキスト・座標)を content webview から受け取らなければならず、
これは D18/D23 が扱ってきた「固定の合言葉が来たか来ないか」だけの
判定には収まらない。かといって、D18 が禁じた「content 由来の入力を
`ToolbarCommand` のような構造化パーサに流し込む」を素直にやってしまうと、
任意の Web ページが好きな JSON を送り込める通路が toolbar の信頼済み
パーサと同格になってしまう。

採った設計は次の通り:

- **`ui::window` に、`ToolbarCommand`/`ContentShortcut` のどちらとも
  独立した第 3 のパーサを新設した**(`parse_context_menu_open`/
  `parse_context_menu_action`)。content webview の `with_ipc_handler`
  は 1 つのクロージャのままだが、その中で試す候補が
  「固定センチネル文字列との完全一致 (`OPEN_DEVTOOLS_MESSAGE`/
  `parse_content_shortcut`)」→「`"velox:context-menu-open:"` プレフィックス
  + JSON」→「`"velox:context-menu-close"` 完全一致」→「`"velox:
  context-menu-action:"` プレフィックス + 小さな整数」の順に増えた
  だけで、**`toolbar::parse_command` (トールバー専用の信頼済みパーサ) は
  一切呼ばれない** — D18 が「2 番目の `ToolbarCommand` 風パーサを生やす
  くらいなら専用のセンチネル/バリアントを追加せよ」と書いた指針どおり、
  専用の第 3 チャネルを追加する形にした。
- **メッセージサイズは事前チェック**: `MAX_CONTEXT_MENU_MESSAGE_BYTES`
  (32 KiB — D62 の `MAX_IPC_PAYLOAD_BYTES` (1 MiB、トールバー専用) より
  大幅に小さい。1 回のクリック情報でしかないため) を `serde_json::
  from_str` を呼ぶ**前**にチェックし、超過は即座に `None` — D62 の
  「パースを試みる前に弾く」パターンをそのまま踏襲。
  `parse_context_menu_action` 側も同じ思想で、桁数上限 (3桁) と
  先頭ゼロ拒否を数値パース前に行う (D23 の「近似一致を許さない完全一致」
  を数値入力に拡張したもの)。
- **JSON をデシリアライズできたことは「安全」を何一つ意味しない**。
  `ContextMenuOpenMessage` (ui::window, ワイヤ形式) →
  `browser::context_menu::RawMenuContext` (まだ untrusted) →
  **`browser::context_menu::sanitize`** (ここで初めて信頼できる
  `MenuContext` になる) という 3 段階を必ず経由する。`sanitize` が行う
  検証:
  - **リンク/画像 URL はスキームを `http`/`https` のみに絞った**
    (`sanitize_menu_url`)。`browser::navigation::ALLOWED_SCHEMES`
    (`http`/`https`/`file`/`about`/`data`) より**意図的に狭い** —
    アドレスバーの `file:`/`data:` はユーザ自身が能動的に入力した
    ものだが、コンテキストメニューのリンク/画像 URL はページが
    埋め込んだ `<a href>`/`<img src>` をユーザが右クリックしただけの
    ものであり、「リンクを新しいタブ/ウィンドウで開く」がページ側の
    `javascript:`/`data:`/`file:` を無条件に実行・表示する経路に
    ならないようにするための多層防御。`navigation::normalize_input`
    自体は再実装せず再利用し (`javascript:`/`vbscript:` 等は既存の
    スキーム許可リストで既に弾かれる — D62 で固定化済みの挙動)、
    その結果に対して追加で `http`/`https` チェックを重ねる形にした
    ので、URL パース自体の正しさは D62/既存テストの資産をそのまま
    引き継いでいる。
  - **選択テキストは制御文字を除去し、長さを上限
    (`MAX_SELECTION_LEN` = 4,000 文字) で切り詰める** (UTF-8 の文字境界を
    尊重、D72 の `truncate_source_utf8` と同じ配慮)。トリム後に空になる
    場合は `None` 扱い (「選択なし」と区別しない)。
  - `is_editable` は真偽値なのでサニタイズ不要だが、"Paste" の
    有効/無効判定にのみ使う。
- **メニュー内容の決定はすべて `browser::context_menu::build_menu`
  という 1 つの純粋関数に集約した**(この Issue の設計の核 — 後述の
  「拡張性」節を参照)。`ui::window`/`app.rs` はこの関数の戻り値
  (`Vec<MenuEntry>`) をそのまま描画・実行するだけで、どの項目を出すか/
  有効にするかの判断ロジックを一切持たない。`src/browser/context_menu.rs`
  に 40 件超の単体テストがあり、webview なしで検証できる
  (`docs/architecture.md` の 4 層分離のとおり)。
- **クリックされた項目は「番号」でしか content webview から戻ってこない**。
  メニューを描画した時点で `browser::context_menu::OpenContextMenu`
  (対象 `TabId` + `Vec<MenuEntry>`) を `browser::Windows`
  (ウィンドウごとに 1 個、D69 の `FindState` と全く同じ「ウィンドウ単位で
  1 セッション」の形) に保存し、`"velox:context-menu-action:<N>"` で
  戻ってきた `N` を **その保存済みリストに対してのみ** 解決する
  (`OpenContextMenu::resolve`)。これにより:
  - ページ側は「VeloX が実際に提示した項目」以外のアクションを
    絶対に選べない (存在しないインデックス・無効化された行のインデックス
    はどちらも `resolve` が `None` を返す — `resolve` 自身が
    `enabled` を再チェックする単体テストあり、
    `resolve_rejects_a_disabled_entry`)。
  - リンク URL/選択テキストといった実際に使われる値は、**最初の
    `sanitize` 時点で確定した文字列がそのまま `MenuAction` の enum
    ペイロードとして保持される** — 2 回目のメッセージ (クリック) で
    ページから再度 URL やテキストを送らせる必要が無く、そもそも
    そのための入力欄も存在しない。
  - ウィンドウをまたぐ取り違え防止 (#29/D68 の要請): `OpenContextMenu`
    は `WindowId` ごとに独立して保存され (`browser::windows::
    WindowEntry::context_menu`)、`UserEvent::ContextMenuRequested`/
    `ContextMenuActionSelected`/`ContextMenuClosed` はすべて `WindowId`
    **と** `TabId` の両方を明示的に運ぶ。クリック確定時は
    「そのウィンドウの現在のメニューが、まさにそのタブ向けに開かれた
    ものか」を `take_context_menu` する**前に**照合するため
    (`handle_user_event` のガード節)、あるウィンドウで開いた別タブ向けの
    メニューを誤って上書き/消費することはない — D69 の find セッションが
    同じ理由で `Windows::find_mut` を `tab_id()` チェック付きで使うのと
    同じパターン。
- **自前 HTML メニューの描画に、選択テキスト/リンク URL を生 HTML として
  挿入していない**。`ui::window::context_menu_render_script` が生成する
  JS は、行ラベルを `row.textContent = item.label` という **DOM API**
  で設定する — `innerHTML` は一度も使わない。`textContent` は代入した
  文字列をそもそも HTML として解釈しないため、選択テキストに
  `<script>`/`<img onerror=...>` 等が含まれていても、それが「生きた
  マークアップ」として現れることは構造的に起こり得ない。D72 (View
  Source) が `escape_html` で HTML エスケープを行っているのとは
  **異なる防御レイヤ**であることをモジュールのコメントに明記した — D72
  は「文字列としてのHTMLドキュメント」を組み立てる必要があったため
  エスケープが要ったが、今回は生きた DOM 操作なのでその手順自体が
  不要になる。
- **残る注入経路は「JS 文字列リテラルからの脱出」であり、これは D62/D69
  の資産をそのまま再利用して塞いだ**。`item.label` (唯一ページ由来の
  文字列を含みうるフィールド — `MenuAction::SearchSelection` の選択
  プレビュー) を含む `items` 配列全体を `serde_json::Value::to_string()`
  で JSON 化し、その結果に対して `ui::toolbar::escape_js_line_terminators`
  (D62 で追加済み、U+2028/U+2029 対策) を適用してから
  `const items = <ここ>;` として埋め込む — D69 の `find_query_literal`
  と寸分違わぬパターン。`"`/`\`/制御文字は `serde_json` が RFC 8259 通り
  エスケープするため、選択テキストに引用符やバックスラッシュが
  含まれていても JS 文字列リテラルの外へ抜け出すことはない。

### これらをどうテストしたか

- **`src/browser/context_menu.rs`** (40 件超):
  `javascript:`/`vbscript:`/`data:`/`file:` スキームのリンク/画像 URL が
  `sanitize` 後に `None` になること (大文字小文字・空白・コメント付与
  トリックを含む)、空/巨大な URL の拒否、選択テキストの制御文字除去・
  マルチバイト境界を尊重した切り詰め、`build_menu` の決定表 (プレーンな
  ページ/リンク/画像/選択あり/編集可能ターゲットそれぞれで正しい項目
  集合と有効/無効になること)、`OpenContextMenu::resolve` が無効な行・
  範囲外インデックスを拒否すること、ホスティルな入力 (NUL・bidi
  override 文字など) でパニックしないこと。
- **`src/ui/window.rs`** (20 件超): `parse_context_menu_open`/
  `parse_context_menu_action` の正常系・異常系 (不正 JSON・上限超過・
  非数値・先頭ゼロ・巨大整数でのパニック無し)、座標のクランプ、
  `context_menu_render_script` が `"`/`\`/U+2028/U+2029/
  `document.body.innerHTML = ...` 型の JS 文字列脱出試行を無害化すること
  (D69 と同型のテスト)、選択テキストに `<script>alert(...)</script>` を
  含めても生成スクリプト中に「解釈されるマークアップとしての
  `<script>`」が現れず、`items` 配列内の JSON 文字列値としてのみ現れる
  ことを確認するテスト、`context_menu_script`
  (contextmenu イベントリスナ) が `preventDefault`/`window.ipc.postMessage`
  /`.href`・`.src` (IDL 経由の絶対 URL 読み取り) を含むことの内容検証。
- 単体テスト件数: **823 → 869 (このブランチ単独の変更で +46)**。
  その後 #27 (プライベートウィンドウ、並行merge) を取り込んだことで
  さらに +10 され、最終的に **879**。いずれの段階でも減少なし
  (`cargo test --lib -- --list` で計測)。
- 統合テスト (`tests/integration.rs`) は今回変更していない (#27 マージ後
  10 件、全て pass) — D69/D72 と同じ判断で、コンテキストメニューは
  既存の統合テストが検証する「実プロセス起動・実タブ管理・実ファイル
  永続化」のいずれとも直接関係しないため、新規の統合テストは追加して
  いない。
- `cargo check --target x86_64-pc-windows-msvc --all-targets` で
  `#[cfg(windows)]` の `disable_default_context_menus`
  (`WebViewBuilderExtWindows::with_default_context_menus`) を含め型
  レベルの整合は確認したが、実機の Windows/WebView2 での動作確認は
  できていない (この環境に Windows 実機が無いため) — 特に
  `event.preventDefault()` が WebView2 の既定コンテキストメニューを
  実際に抑止するか (D69/D72 の F12/Ctrl+U と同種の「アクセラレータ/
  既定動作との衝突は理論的には対処済みだが未検証」という限界) は
  次の一手として記録する。

### メニュー項目を 1 つ足すときに触る箇所 — 意図した設計目標

`browser::context_menu::build_menu` の呼び出し 1 箇所だけが「どの項目を
出すか/有効にするか」を決める。新しい項目を足す最小手順:

1. `context_menu::MenuAction` に 1 バリアント追加。
2. `MenuAction::label()` に 1 アーム追加 (静的な日本語文字列、または
   D72/D69 と同じパターンで動的プレビューを足す)。
3. `build_menu` に 1 行 (`MenuEntry { action: ..., enabled: ... }`)
   追加。
4. `app::handle_context_menu_action` (または、新規ウィンドウが絡む場合は
   `handle_user_event` の割り込み節、D68 が `NewWindow` 系で既に確立した
   パターン) に 1 アーム追加して実際の処理を書く。

**ui::window 側のレンダリング/IPC コードは一切変更不要** — `MenuEntry`
の `label`/`enabled` を読んで描画し、`index` をクリックで送り返すだけの
汎用的な仕組みだからである。実際、#27/#40/#46 が本 PR 未反映のまま
メニューに次のように載せられるはずである (将来 Issue 化、後述):

- **#46 (名前を付けて保存)**: `MenuAction::SaveLinkAs(String)`/
  `SaveImageAs(String)` を追加し、`handle_context_menu_action` から
  #46 が用意する保存ダイアログ相当の関数を呼ぶだけで済む。
- **#40 (印刷)**: ページ全体を対象にした `MenuAction::Print` を足し、
  #40 の印刷起動関数を呼ぶだけ。
- **#27 (プライベートウィンドウ)**: 実は本 PR で既に
  `OpenLinkInNewWindow` がソースウィンドウのプライバシーを継承するよう
  配線済み (下記「#27 との統合」)。「新しいプライベートウィンドウで
  開く」を明示的な別項目にしたい場合も、`MenuAction::
  OpenLinkInNewPrivateWindow(String)` を 1 つ足すだけで済む形になって
  いる。

既存の View Source (`app::request_view_source`) とページ内検索
(`ToolbarCommand::OpenFindBar`/`ContentShortcut::OpenFindBar`) も
このテーブルに乗せられる候補だったが、**あえて今回のスコープに含め
なかった**: どちらもコンテキストメニューの対象 (右クリックした場所) に
依存しない「ページ全体」に対する操作であり、`MenuContext` に新しい
フィールドを増やす必要が無い最も足しやすい部類の項目である。issue 本文の
実装内容リストにも直接の記載が無く、受け入れ条件 4 点をまず確実に
満たすことを優先し、追加候補として次節の後続 Issue に切り出した。

### #27 (プライベートウィンドウ) との統合

本 PR は #27 のマージ後に origin/main を取り込んで書かれているため、
`MenuAction::OpenLinkInNewWindow` の実行 (`handle_user_event` の
`ContextMenuActionSelected` 節) は `state.windows.is_private(window_id)`
でメニューを開いた**元のウィンドウ**のプライバシーを読み取り、
`open_new_window` に引き継ぐようにした — プライベートウィンドウで
右クリックした場合に「リンクを新しいウィンドウで開く」が非プライベート
ウィンドウを開いてしまう (D74 が守ろうとした分離を素通りする抜け穴に
なる) のを防ぐ。`ToolbarCommand::NewWindow`/`NewPrivateWindow` のような
「常に private/常に non-private」の 2 択ではなく、「呼び出し元に合わせる」
という 3 つ目の扱いを導入した唯一の箇所であり、その理由をここに明記する。

### 実装しなかったもの・切り出した後続 Issue

- **Save (ページ/リンク/画像を保存)・Print (印刷)** は issue の実装内容に
  挙がっているが、#46 (名前を付けて保存)・#40 (印刷) がまだ未マージの
  ため、`MenuAction` に含めていない。受け入れ条件 4 点には含まれない
  ため今回のスコープからは除外したが、それぞれがマージされ次第、上記
  「触る箇所」の手順で追加できるよう設計してある。→ 後続 Issue #161 を
  切り出した。
- **View Source/ページ内検索をメニュー項目として追加すること** — 上記の
  とおり、対象非依存で最も足しやすいが、受け入れ条件外のため今回は
  見送った。
- **Back/Forward の有効/無効判定**: D20 が確立した「VeloX はエンジンの
  セッション履歴を複製しない」という前提により、「これ以上戻れない/
  進めない」を判定する手段が無く、常に有効として表示している (トールバー
  自身の Back/Forward ボタンも同じ制約ですでに常時有効)。
- **Paste (`document.execCommand("paste")`) の信頼性は未検証**。
  Chromium 系エンジンはセキュリティ上の理由でスクリプトからの
  `execCommand("paste")` を無効化していることがある (WebView2 が
  これに該当するかは実機が無く未確認)。エラーにはならないことは
  型として保証しているが、実際に貼り付けが起こるかは検証できていない。
- **選択範囲がテキストノードをまたぐ場合の扱い**は D69 と同じ制約
  (`window.getSelection().toString()` 自体はブラウザ標準 API なので
  D69 のようなテキストノード単位の制約は無いが、`<input>`/`<textarea>`
  内部の選択は `window.getSelection()` では取得できず、常に
  "選択なし" 扱いになる — フォーム内テキストの「選択して検索」は
  今回のスコープ外の既知の制限として記録する)。
- **1 ウィンドウにつき 1 メニューセッションのみ** (D69 の `FindState` と
  同じ MVP 簡略化)。バックグラウンドタブが偽の `contextmenu` イベントを
  発火させて同じウィンドウの別タブのメニューを消す、といった攻撃は
  タブ切り替え/ナビゲーション時に確実にメニューを破棄する対策
  (`handle_user_event` の `NavigationStarted`/`LoadStarted`、
  `activate_and_refresh`) と、`resolve` 時の `tab_id` 突合ガードで
  実害が出ない設計にしてあるが、専用の統合テストは追加していない。

**満たせなかった／部分的にしか満たせなかった受け入れ条件**: 4 点とも
機能としては満たしている。ただし「DevTools 導線と統合できる」は
`BrowserWindow::open_devtools` (D18) の既存の「アクティブタブに対して
開く」契約をそのまま再利用しており、メニューを開いたタブがその後
非アクティブになっていた場合 (通常は起こり得ないが理論上) はアクティブ
タブの DevTools が開く — D18/D23 が `ContentShortcut` 全般について
既に許容している限界と同じものを引き継いだだけで、本 Issue で新たに
生じた制約ではない。

**Revisit condition**: (1) Issue #38 のキーバインド管理層が導入する
「ショートカット/操作の一元管理」にコンテキストメニューの項目定義
(`browser::context_menu::build_menu`) を統合する。(2) #46 が着地したら
Save 系の `MenuAction` を追加する (上記「触る箇所」の手順どおり)。
(3) #40 が着地したら Print を追加する。(4) 実機 Windows での動作確認
(`preventDefault()` が WebView2 の既定メニューを実際に止めるか、
`execCommand("paste")` が動くか)。(5) `WebKitFindController` 同様、
Windows の `ICoreWebView2_11::ContextMenuRequested` を将来
「Windows だけ本格的にネイティブ化する」判断が下ったときの実装ポイント
として残す (今回は 3 エンジン非対称のコストが見合わないと判断し見送った
だけで、経路自体は本項で調査・記録済み)。(6) `<input>`/`<textarea>`
内部の選択テキストを拾えるようにする。(7) メニューセッションをタブごとに
複数持てるようにする。

## D79: メモリ/リソースライフタイム監査 (#62) — `page_load_timers` の無制限成長を修正、共有 WebProcess の retention は「有界」と確認して見送り

**対象**: Issue #62 の受け入れ条件 (1 時間以上の連続利用シナリオ / タブ開閉の
反復での memory growth 測定 / resource lifetime のコード上の追跡 / 検出した
leak・retention の修正 / 修正後の再測定)。Epic #57 の性能最適化の絶対ルール
(ベンチマーク駆動、WebView はブラックボックス、OS ごとに結果を分ける — この
節の計測はすべて Linux/WebKitGTK 上) に従う。前提: #61 (D48/D49)・#63
(D54/D56)・#64 (D58) がタブ数固定時の PSS 内訳・`WebContext`/`WebProcess`
共有・バックグラウンド CPU をそれぞれ切り分け済み。**本 Issue が新しく測った
のはそのどれとも違う軸 — 「タブ数を一定に保ったまま開閉だけを繰り返す」
churn シナリオ**で、これは #61/#63/#64 のどの計測にも存在しなかった。

### 監査の対象と方法

Issue が挙げた 8 領域 (WebView ownership / tab close / event listener
cleanup / IPC subscriptions / timers / async tasks / caches /
handles・resources) をコードレビューで辿った。対象は主に `src/app.rs`
(イベントディスパッチ・`AppState`)、`src/ui/window.rs`
(`BrowserWindow`/webview 生成・破棄)、`src/browser/tabs.rs`
(`Tabs`/`closed_tabs` スタック)、`src/ui/toolbar.html` (タブストリップの
JS 再描画)、`src/ui/webview2_blocking.rs` (Windows の COM イベント
ハンドラ)。async ランタイムは無く (`Cargo.toml` に tokio 等の依存なし)、
`async tasks` の懸念は該当しない。`std::thread::spawn` の呼び出し箇所は
3 箇所のみ (`spawn_rss_sampler`/`spawn_memory_pressure_sampler`/
`spawn_automation`) — いずれもプロセス寿命 or 1 回のスクリプト実行の
寿命で終了し、タブごとに増殖しないことをコードで確認した。`closed_tabs`
スタックは既存のテスト (`closed_tabs_stack_drops_the_oldest_entry_once_
over_capacity`) で上限があることを確認済み。`toolbar.html` のタブ
ストリップ (`window.veloxSetTabs`) は毎回 `innerHTML = ""` で全消去して
から再構築しており、イベントリスナーは DOM ノードごと GC される設計 —
リスナーの累積は無い。

### 見つけたもの (1): `page_load_timers` が閉じたタブのエントリを永久に
### 保持していた (修正済み)

`app::run` の `page_load_timers: HashMap<(WindowId, TabId),
metrics::PageLoadTimer>` (Issue #13 のタブ latency 計測用、
`config.perf_metrics` が有効なときだけ書き込まれる) は、
`UserEvent::NavigationStarted` でエントリを作り、`UserEvent::
LoadFinished` では `get_mut` で中身 (`started_at`) を読むだけで **エントリ
自体を一度も `remove` していなかった**。`WindowId`/`TabId` はどちらも
単調増加でタブが閉じても再利用されない
(`browser::tabs::Tabs::take_id`/`browser::windows::Windows::push_window`)
ため、**タブを閉じても既存のエントリは永久にマップに残り続け、開いた
タブの延べ数に比例して無制限に増え続ける** — Issue が挙げた「caches」
「handles/resources」に該当する、本物のリソースライフタイムのバグだった。
タブを閉じる際もこのマップからは何も削除されていなかった (`close_tab`/
`close_window_by_tao_id` はこのマップの存在を知らなかった) ため、
読み込み途中でタブを閉じた場合 (`LoadFinished` が届かない) はさらに
確実にエントリが残る。

**影響範囲**: `config.perf_metrics` が無効な既定設定では、このマップは
一度も書き込まれず空のままなので実害は無い。しかし `VELOX_PERF_METRICS=1`
(または `--perf-metrics`) は `velox-bench`・本 Issue が追加した churn
計測・実際のユーザが `docs/performance-targets.md`/`docs/profiling.md` の
手順で長時間計測するときに使う、正規のサポート対象の実行モードであり
— **まさに Issue #62 が要求する「1 時間以上の連続利用シナリオ」を計測
しようとするときに限って確実に踏むバグ**だった。

**修正 (`src/app.rs`)**:
1. `record_perf_event`'s `LoadFinished` 節を `get_mut` → `remove` に変更。
   ロード完了後のエントリには何も有用な情報が残らない (`PageLoadTimer::
   finish` が `started_at` を `take()` で消費する) ので、丸ごと削除して
   問題ない。次の `NavigationStarted` が `.entry().or_default()` で作り
   直す。
2. `close_tab`/`close_window_by_tao_id` に `page_load_timers: &mut
   PageLoadTimers` を追加し、タブ/ウィンドウを閉じる際に該当エントリを
   `remove`/`retain` で明示的に落とす — ロードが完了する前にタブが
   閉じられたケース (1 だけでは救えない) をカバーする。この配線のため
   `handle_toolbar_command`/`handle_content_shortcut`/
   `handle_automation_command`/`handle_user_event` のシグネチャに同じ
   引数を通した (呼び出し元は `run()` の event loop 1 箇所)。
3. `type PageLoadTimers = HashMap<(WindowId, TabId), metrics::
   PageLoadTimer>;` を新設し、この寿命規約をドキュメントコメント 1 箇所
   にまとめた。

**検証**: `src/app.rs` の単体テストに `load_finished_removes_the_page_
load_timer_entry`/`load_finished_without_a_matching_start_leaves_no_
entry` を追加し、`record_perf_event` を直接呼んでマップが空に戻ることを
確認した (表示なしで実行できる — `record_perf_event` は `wry`/`tao` に
依存しない純粋なロジック)。加えて `tests/integration.rs` に
`repeated_tab_open_close_cycles_exit_cleanly_and_record_every_page_load`
を追加: 実バイナリを `VELOX_AUTOMATION_SCRIPT` で 6 ラウンドの
open→close サイクルにかけ、プロセスがハングも panic もせずに終了し、
`tab_create`/`page_load` の記録数が期待どおり (それぞれ 6 件・7 件) に
一致することを確認する — このシグネチャ変更が dispatch チェーンの
どこかで壊れていれば、記録が欠落するか、最悪ハング/panic として顕在化
するはずのテスト。

**PSS レベルでの計測について、正直に書く**: `PageLoadTimer` 1 エントリは
`Option<Instant>` (数バイト) + タプルキー + `HashMap` のバケットオーバー
ヘッドで、1 件あたりせいぜい 100 バイト程度と見積もられる。本 Issue の
churn 計測 (下記) でも 1 セッションあたり数十〜100 件程度のタブ開閉しか
流していないため、**この修正の効果を `/proc` 経由の PSS/RSS 計測で直接
検出することはできなかった** — smaps のページ粒度 (4 KiB) およびこの
コンテナで観測されているセット間ノイズ (`docs/memory-analysis.md` §7、
0.1〜8.6%) の両方に対して、この修正が動かすバイト数は 3〜4 桁小さい。
#61 (D45) は同じ性質の疑問に `heaptrack` (malloc 単位で追跡できる) で
答えていたが、**このコンテナには `heaptrack` が導入されておらず**
(`which heaptrack` はゼロ件、`docs/profiling.md`/`docs/memory-analysis.md`
が前提にしている環境と本セッションの環境は異なる)、独立した byte 単位の
再現はできなかった。したがってこの修正の正しさの根拠は「コードレビュー +
上記の単体/統合テストによる直接的な動作確認」であり、「PSS 計測で改善を
実測した」わけではない — 捏造を避けるため、できなかったことをそのまま
書く。一方で、この種の per-tab エントリが無制限に残る設計上のバグである
ことと、それを消す修正で消えることは、テストが直接に (マップの中身を
覗いて) 証明している。

**回帰確認 (`velox-bench gate`、修正前後、各シナリオ baseline 8 試行 +
candidate 2×8 試行、同一セッション内、warn>20%/fail>60%)**:

| シナリオ | 総合判定 |
| --- | --- |
| `cold_startup` | OK |
| `tab_create` | OK |
| `tab_switch` | OK |

いずれも Fail/Warn なし。`pss_total_bytes`/`rss_total_bytes` を含む全指標が
ゲート内— この修正が既存の起動・タブ操作性能に悪影響を与えていないことを
確認した。

### 見つけたもの (2): タブ開閉を繰り返すと共有 `WebKitWebProcess` の PSS が
### 上がるが、有界で頭打ちになる (バグではないと判断・修正見送り)

**新設のベンチマーク**: `scripts/bench/tab_churn.py` (`docs/benchmarking.md`
にあるとおり `browser::automation` のスクリプト形式をそのまま使う)。
「K 個のタブを開く → 落ち着かせる → K 個とも閉じて 1 タブに戻す → 落ち
着かせる」を 1 ラウンドとして R 回繰り返し、**各ラウンドの「閉じ終わって
タブ数が常に同じ 1 に戻った瞬間」** に `tab_scaling.py` と同じ `/proc` の
読み方でプロセスツリー全体の PSS/RSS を採る。タブ数を毎回同じに揃えて
いるので、点が右肩上がりなら「タブ数は変わっていないのに増えている」=
retention の直接的な証拠になる。`--extrapolate-minutes` (既定 60、Issue
本文の「1 時間以上」に対応) で、実測した区間の平均ラウンド所要時間から
外挿した推定値も出す — **この外挿はあくまで実測ではない推定であること
を出力・本節の両方で明記する** (Issue の指示どおり)。

**実測 (この環境、`minimal.html`、`VELOX_PERF_METRICS` オフ = 純粋に
WebKit/プロセス側の挙動だけを見る)**:

1. 4 タブ/ラウンド、3 ラウンドの短い試し撃ち: 483.0 → 557.8 → 588.9 MiB
   (単調増加に見える)。
2. 4 タブ/ラウンド、15 ラウンドに伸ばすと: 447.6 → 560.7 → 578.0 →
   687.5 MiB (round 4) の後は 581〜736 MiB の範囲で**増加が止まり横ばい
   になる** (round 5〜15 の最小二乗傾き成分はほぼゼロ、全体の傾きは
   +12.9 MiB/round だが round 1〜4 の立ち上がりに引っ張られているだけ)。
3. 6 タブ/ラウンド、12 ラウンド (最終確認、のべ 72 回の open/close、
   42.1 秒): round 1 が 558.3 MiB、round 2 で 638.7 MiB に跳ねた後は
   581〜693 MiB の範囲で**増加も減少もしない** (round 3〜12 の最小二乗
   傾きは **-0.756 MiB/round** — むしろ僅かに右肩下がり)。

再現コマンド:

```sh
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  python3 scripts/bench/tab_churn.py --velox target/release/velox \
    --page minimal.html --rounds 12 --tabs-per-round 6 \
    --output results/tab-churn.json
```

**プロセス数は全ラウンドを通じて一定 (`velox`+`NetworkProcess`+toolbar
`WebProcess`+content `WebProcess` = 4)** — #63 (D54) のプロセスグループ
共有 (既定 `max_tabs_per_web_process=4`) により、ホームタブと同じ
グループに入りきらない分だけ一時的な 2 個目の content プロセスが
生まれるが、そのグループはラウンド終了時に空になり (ホームタブは
含まれないため) プロセスごと終了する。**ホームタブを含む方のグループ
だけが全ラウンドを通じて生き続け、そこに毎ラウンド新しいタブが相乗り
しては閉じられる**、というのが観測された挙動の実体である。

**解釈**: これは #63 (D56/`docs/memory-analysis.md` §11.1 の v2) が
「休止 (suspend) でも解放されたヒープはプロセス内に残り、プロセスの
終了だけが確実にメモリを OS に返す」と結論した現象と同根で、対象が
「休止」ではなく「本当のタブ close (相乗りしているプロセスの一部だけ
閉じる)」に広がっただけである。ただし本 Issue で新たに分かったのは
**この retention は無制限には増えない — round 1〜2 で急に増えたあとは
頭打ちになり、以降は開閉を繰り返してもほぼ横ばい (ノイズの範囲でむしろ
減ることさえある)** という点。これは glibc malloc/`bmalloc`/JSC の GC
ヒープのような世代・アリーナ型アロケータが典型的に見せる挙動 —
「そのプロセスの過去のピーク相当まで一度大きくなったアリーナは、以後
同程度の作業量のリクエストに対しては新たに OS へメモリを要求せず、
アリーナ内で使い回す」— と整合する。**Issue #62 の懸念していた「無制限
に増え続ける leak」ではなく、「共有プロセスの初回ウォームアップに相当
する、有界な 1 回きりのコスト」であると判断する。**

**修正を見送った理由**: (1) 上記のとおり無制限成長ではなく有界 — 「leak」
の定義に当てはまらない。(2) 唯一考えられる対策 (ホームタブと同じグループ
に一定回数以上乗せたら退役させ、以後の新規タブは別グループに送る、という
「プロセスグループの寿命ベース recycle」) は、#63 が v1→v2→v3 の 3 回の
実装・計測を経てようやく安全な設計 (グループ単位の丸ごと休止) に至った
のと同種の、**新規の設計変更**であり、Epic #57 のルール 1
(ベンチマーク駆動、Hypothesis→Profile→Baseline→Optimize→Benchmark→
Regression Check) とルール 4 (メモリと速度のトレードオフを見る —
グループを退役させれば新規プロセス起動が増え、#124 (D54) の§10.4 が
示した「burst オープン時のページロード直列化」と同種の速度リスクを
背負う) の両方を満たす検証には、この Issue の残り時間では届かない規模の
作業になる。(3) 効果自体、頭打ちになる時点の絶対値 (この計測では
~600〜690 MiB 程度) をどこまで削れるかは実装して計測するまで分からない
— 憶測で着手すべきではない。

### 「1 時間以上の連続利用」の外挿について、正直に書く

CI・この検証環境で実際に 1 時間 (churn なら 1000 ラウンド超) を回すのは
非現実的なため、**実際に流したのは最大 15 ラウンド (60 回の open/close、
約 39 秒) までである。** `tab_churn.py` の `--extrapolate-minutes` は
実測区間の平均ラウンド所要時間から単純な線形外挿を行うが、**上記のとおり
実測データそのものが「round 1〜2 で頭打ちになる非線形な形」を示しており、
線形外挿 (例: 60 分 ≈ 1000+ round として傾きを掛けるとGiB級の数字になる)
は明らかに実態と乖離する。** スクリプトの出力・本節の両方に「これは外挿
であり実測ではない」旨を明記し、**外挿された数値そのものは信頼できる
予測として使わないこと** — 実際に確認できた事実は「短い区間 (最大 15
ラウンド) では頭打ちになる」ということだけであり、それ以上長い時間で
何が起きるかは本 Issue では実測していない。

### Revisit condition

(1) `page_load_timers` と同じ「`WindowId`/`TabId` は再利用されない
キーで無制限に増え得るマップ」というパターンが将来別の場所に増えない
よう、新しい per-tab キャッシュ/マップを足すときはタブ close の経路で
明示的に破棄することをレビューの chuck リストに入れる。(2) 共有
`WebProcess` のプロセスグループ寿命ベース recycle (本節「修正を見送った
理由」) は、実際にその挙動が問題になる規模の長時間利用データが得られた
場合に、#63 と同じ v1→v2→v3 型の反復検証で着手する — 新しい Issue を
切ってから始めること。(3) `heaptrack` がこのコンテナに存在しないため、
Rust 側の小さな malloc の増減を独立に検証する手段が今は無い — 導入する
か、代替手段 (例: `mallinfo2`/`jemalloc` の統計を `velox` 自身が perf
ログに出す) を検討する余地がある。(4) macOS (WKWebView)/Windows
(WebView2) では churn 時の挙動が未検証 — 3 エンジンとも同じ「複数
webview で 1 プロセスを共有する」設計を持つか自体が異なるため、この
節の結論を他 OS にそのまま適用しないこと。
## D80: バックグラウンドタブのネットワーク活動 — WebKitGTK は 1 秒未満のタイマーだけをクランプする。ネットワーク要求そのものは止まらず、完全な抑制は既存の Adaptive Tab Suspension (#63) 頼み

**対象**: Issue #65 (Epic #57 Stage 4。依存する #64 は D58 で完了)。測定
データと再現手順は `docs/performance-targets.md` §15。

**背景**: #64 (D58) は「バックグラウンドタブの CPU は WebKitGTK の可視性
連動でほぼ止まる (0.5%)」ことを確かめたが、CPU が下がることと**ネットワーク
要求そのもの**が止まることは別の主張であり、D58 の `?beacon=1` 実験は
「タイマーは止まっていない (2.17→1.10 件/秒)」ことしか示していなかった。
#65 はこれを正面から測る。

### 先に結論

1. **典型的なポーリング間隔 (数秒に 1 回) は、バックグラウンド化しても
   まったく間引かれない。** WebKitGTK の隠しページタイマー節流は
   **1 秒未満のタイマーだけをクランプ**しており、それより遅いタイマーは
   素通しになる。
2. **1 秒未満の高頻度タイマー (D58 の `busy.html` と同種) は約 1/4 に
   間引かれる**が、要求はゼロにはならない — D58 の CPU 側の結論
   (「止まっているのではなく間引かれている」) がネットワーク要求にも
   そのまま当てはまる。
3. **WebSocket は完全に無傷。** 接続はバックグラウンド化後も維持され、
   心拍メッセージも届き続ける — #65 の「壊してはいけないもの」の条件を
   満たしている。
4. **WebKitGTK 2.52.6 はこの環境で `<link rel="prefetch">` を一切実行
   しない** (JS 自体は実行されている、後述)。「対応していないので壊しようが
   ない」という消極的な意味で Web 互換性への影響はゼロ。
5. **VeloX 側でネットワーク要求を完全にゼロにできる唯一の方法は既存の
   Adaptive Tab Suspension (#63、既定 OFF) だった。** 有効にすると、
   タブが休止した瞬間から WebSocket を含む全通信がゼロになる —
   ただし webview ごと破棄する荒い手段であり、#65 で新しく実装したもの
   ではない。

### 15.1 典型的なポーリング間隔 (2 秒) は素通し (各 3 試行、16 秒窓)

負荷源は `scripts/bench/pages/network_activity.html` (新規、Issue #65)。
`poll` = 2 秒おきの `fetch`/`XHR`、`pixel.gif` = 3 秒おきの `<img>` 差し替え、
WebSocket 心拍 = 2 秒おき。

| 状態 | polling (件/16s) | background resource (件/16s) | websocket (件/16s) |
| --- | ---: | ---: | ---: |
| アクティブタブ | 5, 5, 5 | 4, 4, 4 | 6, 6, 6 |
| バックグラウンドタブ | 5, 6, 5 | 4, 4, 4 | 6, 7, 6 |

**有意差なし。** 3 試行のばらつきの範囲内で、アクティブとバックグラウンドの
件数は事実上同じ。#64 (D58) が CPU について確かめた「hide() で
`visibilityState` が `hidden` になり、rAF が止まりタイマーが間引かれる」
という機構は、1 秒以上の間隔を持つタイマーには効いていない。

### 15.2 高頻度ポーリング (200ms) は約 1/4 に間引かれる (各 3 試行、8 秒窓)

`?poll_ms=200` で `poll` の間隔だけを D58 の `busy.html` の 10ms タイマーに
近い高頻度にした場合:

| 状態 | polling (件/8s) | 換算レート |
| --- | ---: | ---: |
| アクティブタブ | 31, 31, 31 (3 試行とも同一) | 3.9 件/秒 |
| バックグラウンドタブ | 8, 8, 8 (3 試行とも同一) | 1.0 件/秒 |

**約 74% 減 (3.9→1.0 件/秒)。** 200ms の理論値 5 件/秒に対しアクティブでも
3.9 件/秒に留まるのはブラウザ全体のスケジューリングオーバーヘッドだが、
バックグラウンドでの 1.0 件/秒は「1 秒未満のタイマーを 1 秒付近まで
クランプする」という WebKitGTK の既知の節流(D58 が CPU 側で見た「500ms→
約1000ms」と符合する)で説明がつく。**同じ `network_activity.html` 内で
`background_resource_loading` (3 秒間隔) と `websocket_protected` は
このケースでも間引かれていない** (各試行で 2 件・4〜5 件のまま) —
クランプの境界が 1 秒付近にあるという説明と整合する。

### 15.3 prefetch — WebKitGTK 2.52.6 はこの環境で発火させない

`network_activity.html` は `<link rel="prefetch" href="prefetch-target">`
を挿入した直後に無条件で `GET /prefetch-armed` を送る (「JS 自体が動いた」
ことと「エンジンが実際に prefetch した」ことを区別するため)。全試行を
通して `prefetch-armed` は毎回 1 件記録された一方、`prefetch-target`
(prefetch 自体がフェッチされたときだけ増える) は**一度も観測されな
かった**。VeloX 側の実装の問題ではなく、この WebKitGTK バージョンが
動的に挿入した `<link rel=prefetch>` を処理していないと考えられる。
壊しようのないものは壊れていない、という消極的な結果として記録する。

### 15.4 Adaptive Tab Suspension (#63) との連携 — 唯一の完全な抑制手段

`VELOX_AUTO_SUSPEND_AFTER_MS=3000` を設定し、タブをバックグラウンドへ
送ってから 8 秒待って (idle_after を十分に超えてから) 16 秒間観測:

| 状態 | 総イベント数 (16s窓) |
| --- | ---: |
| バックグラウンド (suspension 無効、§15.1 参照) | 15, 17, 15 |
| バックグラウンド (suspension 有効、休止後) | **0, 0, 0** |

**休止後は WebSocket を含む全通信がゼロになる。** `browser::suspension`
がタブの webview を丸ごと `drop` する (D56) ため、プロセスごと消えて
ネットワーク接続も残らない。より短い窓 (27 秒、`--settle-secs 0.2`) で
全体を観測すると、休止が効くまでの最初の数秒だけ通常どおりの通信が記録され
(`websocket_protected=4` 等)、以降はぴたりと止まる — 「間引く」のではなく
「消す」ことで初めて背景ネットワークをゼロにできることが確認できた。

**この結果は #65 の受け入れ条件を「抑制できた」で終わらせるものではない
ことに注意。** Adaptive Tab Suspension は既定で無効 (D9 の方針どおり)
であり、#65 のために新しく実装したものでもない。有効にするかどうかは
ユーザ (または将来のデフォルト変更判断) 次第で、#65 が単体で追加した抑制
機能ではない。

### 分類 (Issue #65 の「対象」リストに対応)

| Issue #65 の分類 | この環境での観測 | 抑制 |
| --- | --- | --- |
| polling / periodic fetch・XHR | ≥1 秒間隔は素通し (§15.1)、<1 秒間隔は ~1 秒にクランプ (§15.2) | エンジンが部分的に (WebKitGTK 既存動作)。VeloX 側の追加実装なし |
| prefetch | この環境では発火しない (§15.3) | 該当なし (壊れようがない) |
| background resource loading (`<img>` 定期差し替え) | 素通し (§15.1/§15.2 とも間引かれず) | なし |
| 不要な network-triggered IPC | VeloX 自身の `UserEvent` はページ読み込み/タイトル/favicon 単位で発火し、`fetch`/XHR 単位のイベントは元から存在しない (D17/D59 — サブリソース単位のフックが無い) | 該当する VeloX 内部 IPC が無い |
| WebSocket / WebRTC / 音楽再生 / 通知 (保護対象) | WebSocket は §15.1/§15.2 で無傷を確認。WebRTC・通知はこの環境に該当する API/デバイスが無く未検証 (Revisit condition) | 保護 (触っていないので壊れない) |

### なぜリクエスト単位の抑制を実装しなかったか

`browser::subresource::is_blocked_resource` はリクエスト単位の判断ロジック
を既に持つが、それを実際の要求に結びつけるフックが無ければ何も止められない。
D17/D59 が確認したとおり、**wry 0.56 でリクエストを横取りできるのは
`ICoreWebView2::WebResourceRequested` 経由の Windows (WebView2) だけ**で、
Linux (WebKitGTK)/macOS (WKWebView) には同等の口が無い。この開発環境は
Linux のみであり、Windows 版を書いても一度も実行して確かめられない。
Epic #57 のルール 1 (ベンチマークなしの最適化をしない) は「効果を確かめず
に最適化を入れない」という意味だが、**検証手段が原理的に存在しない
Windows 専用コードを新たに書き足すのは、それよりさらに悪い「一度も動かして
いないコードを本番の優先 OS に入れる」ことになる**ため、今回は見送った。

代わりに `src/browser/network_activity.rs` として実装したのは、**エンジン
非依存の分類ロジックだけ**:

- `NetworkActivityClass` — `ResourceType` を「保護対象
  (WebSocket/Media/Document)」と「抑制候補になり得る (それ以外)」に分ける
  `classify()`。`browser::suspension` が音声再生タブを保護する既存の判断
  と矛盾しないよう、Media の扱いを合わせてある。
- `is_polling()` — リクエスト時刻の列から「規則的な間隔で繰り返している
  (ポーリングらしい)」かどうかを判定する純粋関数 (変動係数ベース)。§15.2
  で見た「タイマーが節流されても比率としては規則的なまま」という事実
  (D58 の CPU 測定と一致) を前提に、節流後でも判定が揺れないよう設計し、
  テストでその前提そのものを検証している。

どちらも 100% 純粋な Rust で、Windows 実機が無くても `cargo test` だけで
正しさを保証できる。将来 Windows 側で `webview2_blocking.rs` と同じ経路
(`WebViewExtWindows::webview()`) を使ってリクエストを実際に横取りする
実装が必要になったとき、判断ロジックはここに既に用意されている。

### 検討して見送ったもの

- **`webview2_blocking.rs` を拡張してバックグラウンドタブの
  XHR/fetch を実際にブロックする**: 上記のとおり実機で一度も検証
  できないため見送った。仮に実装しても「型チェックは通るが動作は未確認」
  という、既存の `webview2_blocking.rs` (D59) と同じ限界を持つコードが
  増えるだけで、Issue #65 が求める「最適化前後を比較」ができない
  (比較する実測値が存在しない)。
- **VeloX 独自の JS 挿入によるタイマー節流** (すべてのページに
  `setInterval`/`fetch` をラップするスクリプトを注入し、バックグラウンド
  時に間引く): WebKitGTK/WKWebView/WebView2 いずれでも「全ページに
  無条件で JS を注入する」ことは `content_webview_builder` の既存の設計
  (D17/D59 が挙げた「エンジンの判断を横取りしない」原則) と衝突するうえ、
  サイトが自前の `setInterval` を上書き検知して壊れる可能性がある
  (#65 の「正当な通信を壊さない」条件に反するリスクが高い)。既存の
  Adaptive Tab Suspension (§15.4) の方が安全に同じ効果 (むしろ完全な
  停止) を達成できる。

**Revisit condition**: (1) wry が Linux/macOS 向けのリクエスト横取り
フックを追加した場合 (D17/D59 の revisit condition と同一)。(2) 実機
Windows で `webview2_blocking.rs` の動作を確認できるようになったら、
同じ経路で `network_activity::classify`/`is_polling` を使った計測
(まずは計測のみ、ブロックはさらに後) を追加する。(3) WebRTC・通知は
この環境にカメラ/マイク/通知バックエンドが無く未検証 — 実機での確認が
要る。(4) `?poll_ms=` のクランプ境界 (1 秒付近) を二分探索的に測れば、
WebKitGTK の実際の閾値を特定できる。今回は 200ms/2000ms の 2 点比較に
留めた。
## D81: IPC 計測基盤 (#66) — `PerfRecord::Ipc` で JS ↔ Rust を両方向計測し、実測に基づいて「タブストリップ全件再送信は意図的」「履歴パネルの無条件再送信は不要」と切り分けた

Issue #66 (Epic #57 Phase 3)。「WebView ↔ Rust の IPC コストを計測し、
不要な通信と payload を削減する」という課題に対して、**まず継続的に
計測できる仕組みを作り、その実測データだけで削減判断をした** — Epic #57
ルール 1 (ベンチマークなしの最適化をしない) を、#60/#64 と同じやり方で
守った。数値・再現手順は `docs/performance-targets.md` §18、使い方は
`docs/benchmarking.md` §6 を参照。ここには設計判断とその理由だけを残す。

### 計測をどこに追加したか — 既存の1本の choke point ずつに載せた

新しい IPC チャネルや新しいラッパー層は作らず、**既存の「全メッセージが
必ず通る場所」2 箇所にそれぞれ 1 行ずつ計測を差し込んだ**:

1. **JS → Rust (`direction=in`)**: `window.ipc.postMessage` は
   `ui::window::BrowserWindow`の `with_ipc_handler` → 唯一の
   `UserEvent::ToolbarMessage(window_id, body)` を経由し、
   `app::record_perf_event` がこれを既に (メトリクス ON 時のみ) 見ている。
   ここに `metrics::PerfRecord::ipc(IpcDirection::In, name, body.len(),
   started)` を 1 回書くだけで、**すべての `ToolbarCommand` を計測対象に
   できた** — 個々のコマンドの型ごとに計測コードを足す必要はない。
   `name` は `ui::toolbar::command_name` という新関数が担う: `body` を
   `serde_json::Value` として浅くパースし `"cmd"` フィールドだけ読む
   (実際のバリデーションは既存の `parse_command` に任せる)。**あえて
   `parse_command(body).ok().map(...)` にしなかった理由**: `parse_command`
   が失敗するボディ (未知の `cmd`、型不一致) でも `command_name` はタグを
   読めるため、「壊れたメッセージが飛んできたことそのもの」が計測ログに
   残る — サイレントに欠測させない設計を優先した。
2. **Rust → JS (`direction=out`)**: `ui::window::BrowserWindow` の
   `set_*`/`focus_address_bar`/`set_panel` はすべて最終的に
   `self.toolbar.evaluate_script(...)` を呼んでいた (19 箇所)。これを
   1 つの private メソッド `eval_toolbar(&self, name: &'static str,
   script: &str)` に集約し、19 箇所すべてをこの呼び出しに置き換えた。
   計測 (`Instant::now()` を挟んで `evaluate_script` を呼び、
   `IpcLog::record` へ渡す) は `eval_toolbar` の中の 1 箇所だけに書いた。
   **`duration` が測れるのは Rust 側のコスト (JSON 文字列の構築 + FFI
   呼び出し) だけ**であり、`evaluate_script` はコールバックを取らない
   fire-and-forget 呼び出しなので、生成された JS の実行時間・DOM 更新
   コストは測れない (Epic #57 ルール 3、WebView をブラックボックスとして
   扱う、を計測設計そのものに反映した)。content webview 向けの
   `evaluate_script` (find/view-source/favicon 取得など) はここに含めて
   いない — トールバー IPC チャネル (`docs/architecture.md` が
   "IPC protocol (JSON)" として明示している対象) にスコープを絞った。

### `BrowserWindow` への配線 — 新しい `pub` 型を 1 つだけ増やした

`app::PerfContext` (既存、`Arc<PerfLog>` + `process_start` のペア、
`app.rs` 内 private) と同じ形をもう 1 つ `browser::perf_log::IpcLog`
として `pub` で用意した。**`PerfContext` を `pub` にして使い回さなかった
理由**: `docs/architecture.md` の層構造 (`ui::` は UI ツールキット層、
`app.rs` はそれより上のイベントディスパッチ層) では `ui::window` が
`app` に依存できない。`PerfContext`はその依存を逆転させてしまうため、
構造的に同じでも別の `pub` 型として `perf_log.rs` (`browser::` 側、
既に IO を担う「意図的に汚れている」モジュール) に置いた。
`BrowserWindow::new` に `Option<IpcLog>` を追加パラメータとして渡す
(`config.perf_metrics` が off なら `None` — 既存の
`Option::is_none` 一発チェックで済むパターンを踏襲)。呼び出し元は 2 つ:
`app::run`(最初のウィンドウ) と `app::open_new_window`(Ctrl/Cmd+N)。
前者のために `perf_log`/`ipc_log` の構築を `AppState` 構築より前
(元は後、RSS サンプラを起動する直前だった) に前倒しした。後者は
`state.perf`(既存フィールド) から `PerfContext::to_ipc_log()` という
1 メソッドで作る — `state.perf` と新しいウィンドウの `ipc_log` が
別々の `PerfLog`/`process_start` を指してしまう (ログが分裂する) 事故を
型で防ぐためのアダプタメソッドである。

### 集計・可視化 — `browser::benchmark::summarize_ipc` + `velox-bench ipc-summary`

計測イベントを吐くだけでは「継続的に見える化」にならないため、
`benchmark.rs` (純粋 Rust、既存の `aggregate_trials`/`compare`/
`evaluate_gate` と同じファイル) に `summarize_ipc` を追加した:
`ipc` イベントを `(direction, name)` ごとにグルーピングし、件数・合計
バイト数・`duration_ms` の `Stats` (既存の `compute_stats` を再利用) を
返す。`total_bytes` 降順ソートなのは「このチャネルの通信量を支配して
いるのは何か」が Issue #66 の"高頻度イベントを特定"の核心だから。
`velox-bench` に `ipc-summary` サブコマンドを追加し、IO 層 (`--input`
複数ファイル読み込み、表示、`--output` への JSON 保存) を担わせた。
**`aggregate`/`gate` のように `BenchmarkResult`/回帰ゲートは作らなかった**
— IPC トラフィックは「シナリオの 1 メトリクス」ではなく「セッション
全体の通信内訳」を見る診断ツールという性格が異なるため、既存の
シナリオ前提の型に無理に押し込めるより独立コマンドにする方が素直だと
判断した。後続 Issue が回帰ゲートに載せたくなった場合は、`IpcSummary`
の特定の `name` (例えば `set_tabs` の `total_bytes`) を新しい
`MetricKey` として追加すれば `evaluate_gate` の枠組みにそのまま乗る。

### 実測して分かったこと・下した判断 (数値は §18)

- `set_tabs` (タブストリップ全件再送信) が量・回数とも最大だが、20 タブ
  という最も重いケースでも Rust 側コストは sub-millisecond (中央値
  0.000ms、120 件中の最悪値でも 3.5ms)。タブストリップは常時表示 UI
  であり、1 回のタブ操作につき最大 3 回 (読込開始/読込完了/favicon
  解決) 送るのも実際に変化した状態を反映しているだけ — **削減もバッチ
  化もしなかった。** 計測上のボトルネックが無い状態でタイマー/デバウンス
  ロジックを持ち込むことは Epic #57 ルール 1 に反する。
- `set_history` (履歴パネル全件再送信、既定 200 件上限) は
  `LoadFinished`/`PageTitleResolved`/`FaviconResolved` の 3 箇所から
  **履歴パネルが閉じていても**無条件に呼ばれていた — 唯一実測で見つかった
  「本当に不要なイベント」。`app::refresh_history_panel_if_open`
  (`window.open_panel() == Some(Panel::History)` を確認してから
  `refresh_history_panel` を呼ぶ薄いラッパー) を追加し、上記 3 箇所を
  これに差し替えた。パネルを開く操作 (`TogglePanel`) は既存のまま
  無条件に更新するので、**パネルを開いた瞬間の表示内容は変わらない**。
  同一の自動操作スクリプトでの before/after 実測 (§18.4): 20 タブ
  セッションで `set_history` 67→1 件 (-98.5%)、15,723→555 bytes
  (-96.5%)、ipc イベント総数は 502→430 件 (-14.3%)。
- **batching は導入しなかった。** 上記 2 点とも、削減判断は「送るか
  送らないか」で完結しており、複数イベントを 1 回の `evaluate_script`
  呼び出しにまとめる必要が生じる規模のボトルネックが見つからなかった。
- **`direction=in` の実測範囲には限界がある。** `browser::automation`
  の `open`/`switch`/`navigate`/`close`/`suspend` は `AutomationCommand`
  として `app.rs` のハンドラを直接呼ぶ設計 (Issue #112, D44) であり、
  実際の `window.ipc.postMessage` を経由しない。そのため自動操作
  スクリプトで `in` 側を測ると `ready`/`script_started` (起動時
  ハンドシェイク、1 回だけ) しか出てこない。オムニボックスの
  1 キー入力ごとの `omnibox_input` 往復のような、実際のキー入力でしか
  発火しない高頻度 `in` パスは **本 Issue では未計測のまま残した**。
  `ui::toolbar::ToolbarCommand` の定義から、typical な `in` メッセージが
  数十バイトの固定形状 JSON であることはソースコード上明らかであり、
  `out` 側で実測した「同程度サイズは sub-millisecond」から類推して
  ボトルネックである可能性は低いと考えているが、これは実測ではなく
  推論であることを明記する。

### なぜ `unwrap`/`expect` を避けつつ計測コードを 2 箇所（`app.rs`/`window.rs`）に分散させたか

計測は「失敗しても機能に影響してはならない」という既存方針
(`PerfLog::write` がエラーを `eprintln!` に落とすだけで伝搬しない、D16)
をそのまま受け継いだ。`IpcLog::record`/`PerfRecord::ipc` はどちらも
`Result` を返さない (失敗しうる処理が無い — シリアライズ失敗は
`PerfRecord::to_json_line` 側の既存フォールバックが吸収する) ため、
呼び出し側に `unwrap`/`expect` は一切増えていない。

### Revisit condition

(1) `browser::automation` に `type`(オムニボックス入力) 相当のコマンドを
足す機会があれば、`omnibox_input`/`omnibox_close` の `in` 側実測を追加
する。(2) `IpcSummary` の特定行を `MetricKey` に昇格し、`gate` の回帰
検知に載せる (#68 が対象にしうる)。(3) content webview 側の
`evaluate_script`(find/view-source/favicon 取得など) は今回計測対象外に
した — 頻度・サイズとも `ToolbarCommand` チャネルより明らかに小さいと
判断したが、実測はしていない。差が疑わしくなったら同じ `eval_toolbar`
パターンで計測を足せる。(4) Windows (WebView2) での実測は未実施 — この
節の数値はすべて Linux/WebKitGTK。`evaluate_script`/`with_ipc_handler`
は wry の共通 API だが、実際のディスパッチコストは OS ごとの WebView
実装に依存するため、Windows 実機での再計測が必要。
## D82: Performance Dashboard (#71) — `velox-bench`/perf-gate の出力形式をそのまま保存し、比較は「明示的に同一とマークしたセッション」の中でしか許可しない

**対象**: Issue #71 の受け入れ条件 4 点 (過去結果と比較できる / OS ごとに
分離できる / commit・PR 単位で性能差を確認できる / 測定条件を結果と一緒に
保存する)。設計・実装の詳細は
[docs/performance-dashboard.md](performance-dashboard.md) を、回帰ゲート
(#72) との役割分担は `docs/performance-targets.md` §17 を参照。ここでは
決定そのものと、他の選択肢を採らなかった理由を記録する。

### 決定

1. **新しい計測手段は作らない。** `velox-bench run`/`aggregate`/`gate` が
   生成する `BenchmarkResult`/`GateReport` JSON (#14/#72 で確定済み) を
   そのまま消費する。ダッシュボード側の独自フォーマットは、それを 1 エン
   トリの `"result"` フィールドに包んだ薄いラッパー
   (`schema_version`/`session_id`/`source`/`branch`/`pr_number`/`note`)
   だけで、`BenchmarkResult` 自体は無変換。**理由**: perf-gate.yml の出力
   形式とダッシュボードの保存形式を分岐させると「別々の形式を2つ持つと
   必ず腐る」(Issue #71 本文) — 変換ステップそのものを無くすのが最も確実
   な予防策。
2. **保存はリポジトリ内への追記 (JSON Lines)、レポートは静的
   HTML/Markdown 生成。** `results/history/<os>/<scenario>.jsonl` に 1 行
   1 エントリで追記し、`scripts/dashboard/report.py` がそれを読んで
   レポートを生成する。GitHub Pages やサーバは使わない (プライベートリポ
   ジトリであり、それらを前提にできるとは限らない — Issue #71 本文の
   スコープ指針どおり)。
3. **グラフは matplotlib 等を使わず、素の SVG を文字列で組み立てる。**
   `scripts/dashboard/report.py` の `render_svg_chart`。新規依存クレート/
   パッケージはゼロ (Rust・Python とも標準ライブラリ + 既存の `serde`/
   `serde_json` のみ)。
4. **比較の単位を「セッション」に限定し、それ以外は自動比較しない。**
   `session_id` は記録のたびに既定で一意に自動生成される
   (`common.generate_session_id`) — つまり**何もしなければ 2 回の記録は
   別セッション扱い**になる。複数の記録を比較可能な系列として扱いたい
   場合、呼び出し側が同じ `session_id` を明示的に指定しなければならない。
   `report.py` は同一 `session_id` の隣接エントリ同士だけを折れ線でつなぎ、
   `velox-bench gate` (CI と同一ロジック) で差分の重大度 (OK/WARN/FAIL)
   を計算する。セッションが変わる境界では、点は表示するが線ではつながず、
   差分バッジも出さない。
5. **閾値判定は独自実装せず `velox-bench gate` を呼び出す。** `report.py`
   は一時ディレクトリに `BenchmarkResult` を書き出し、`velox-bench gate
   --baseline <前のエントリ> --candidate <このエントリ>` をサブプロセスで
   呼んで `GateReport` を得る。`velox-bench` バイナリが手元に無い場合のみ、
   絶対差フロアを持たない簡易フォールバック (既定 warn=20%/fail=60%) に
   切り替わり、その旨をレポートに明記する。
6. **`results/history/**/*.jsonl` はコミットする。レンダリング結果
   (`report.html`/`report.md`) はコミットしない** (`.gitignore` に追加)。
   前者は `results/baseline/` と同じ「再生成できない生データ」、後者は
   `target/` と同じ「いつでも再生成できるビルド出力」という整理。

### 検討したが採らなかった選択肢

- **固定 baseline ファイルとの単純な時系列比較** (「最新の結果を、履歴上の
  1 つ前の結果と常に比較する」): §10/D46 が実測した「同一バイナリでも
  セッションを跨ぐと最大 +78.9% 動く」というノイズの大きさの前では、
  session の概念なしにこれをやると、ノイズを回帰の証拠であるかのように
  表示してしまう。**この「セッションを跨いだ比較を許さない」制約こそが
  本 Issue で最も設計判断を要した部分** (Issue #71 本文の指摘どおり) であり、
  4 の決定はそれへの直接の回答である。
- **CI (`perf-gate.yml`) からの自動記録**: `record.py` はそのまま呼べる
  形にしてあるが、実際にワークフローへ組み込むと「誰が
  `results/history/` への commit を作るか」(fork からの PR は push 権限が
  無い等) という運用設計が追加で必要になり、本 Issue のスコープ (保存
  形式を固める) を超える。**Revisit condition (1)** として送る。
- **matplotlib 等のグラフ描画ライブラリ**: Issue #71 本文が明示的に
  「重い依存を安易に足さないこと。素の SVG 生成で足りるならそちらを選ぶ」
  と指示しており、実際に足りた (折れ線 + 点 + ツールチップ程度で十分)
  ため導入しなかった。
- **`RunEnvironment`/`BenchmarkResult` (`src/browser/benchmark.rs`) への
  フィールド追加 (`session_id` 等を Rust 側に持たせる)**: 検討したが、
  (a) `benchmark.rs`/`velox-bench` は #72 の回帰ゲートが CI で直接依存する
  コードであり、Issue #71 のためにここへ手を入れると変更範囲が
  perf-gate.yml の挙動にまで及ぶリスクがある、(b) ダッシュボードが必要と
  するメタデータ (session/source/branch/PR) は「計測」ではなく「記録」の
  文脈でのみ意味を持つため、Rust 側の `BenchmarkResult` (計測結果そのもの)
  に持たせるより、Python 側の保存レイヤーに持たせる方が責務が素直に
  分かれる。そのため本 Issue では **Rust コードは一切変更していない**
  (新規依存クレートもゼロ)。

### 動作確認

`results/baseline/cold_startup-linux-xvfb.json` (#58 で実際に計測された
real data) を履歴に記録し、レポートを生成できることを確認した。加えて、
この dev/agent コンテナ上で `cargo build` した debug ビルドの
`velox`/`velox-bench` を使い、Xvfb + `dbus-run-session` 上で実際に
`cold_startup` を同一セッション内で 2 回、`tab_switch` を別セッションで
1 回計測し、記録・レポート生成の一連の流れが動くこと、同一セッション内の
差分が `velox-bench gate` 経由で正しく重大度付きで計算されること、
セッション境界で線が途切れ差分が計算されないこと、`--os`/`--scenario`
フィルタや履歴が空の場合・`velox-bench` バイナリが無い場合のフォールバック
が例外を出さずに動作することを確認した (詳細は
`docs/performance-dashboard.md` §8)。この確認用の計測データ自体は debug
ビルドの数値であり正式な baseline ではないため、`results/history/` には
コミットしていない — コミットしたのは `results/baseline/` 由来の 1 エント
リのみ。

### Revisit condition

(1) `.github/workflows/perf-gate.yml` から `record.py` を自動で呼び、
`results/history/` への commit 運用を設計する。(2) Issue #136 (Windows
実機での性能計測) が着地したら、その結果を `record.py` に渡すだけで
`results/history/windows/` に自然に載ることを実データで確認する。(3) 実機
(ノイズの小さい環境) が使えるようになった段階で、「セッションを跨いでも
許容誤差内なら緩やかにつなぐ」といった、比較の単位を広げる拡張を再検討
する — 現時点ではこの環境のノイズの大きさ (D46) がそれを許さない。

## D83: auto-merge の Closes キーワード自動クローズ (#168) — GITHUB_TOKEN マージでは GitHub 標準の自動クローズが効かないため、auto-merge.yml がマージ成功後に自分で `gh issue close` する

**対象**: Issue #168。`.github/workflows/auto-merge.yml` が
`secrets.AUTO_MERGE_TOKEN || secrets.GITHUB_TOKEN` で `gh pr merge` している
ところ、`AUTO_MERGE_TOKEN` が未登録のため実質的に常に `GITHUB_TOKEN` でマージ
していた。GitHub の仕様上、`GITHUB_TOKEN` によるマージ・push は GitHub 側の
それ以上の自動処理 (`Closes #NN` キーワードによる Issue 自動クローズや、
push トリガの他 workflow 起動) を誘発しない。これにより、CLAUDE.md
「Issue と PR の紐付け」節どおりに `Closes #NN` を書いても Issue が open の
まま残る事故が、実測で複数件 (#132/#131/#133/#163/#165/#166/#167 など)
確認された (詳細は Issue #168 本文の調査表)。

### 決定

1. **Issue #168 の案 A (auto-merge.yml が明示的に Issue を閉じる) を採用し、
   案 B (`AUTO_MERGE_TOKEN` という repo 権限 PAT の登録) は採らない。**
   PAT の発行・保管はリポジトリ設定側の作業であり、このセッションの権限では
   行えない。案 A は追加 secret 不要でリポジトリ内だけで完結し、案 B が後から
   登録されても支障が無い (下記 3)。
2. **抽出ロジックは workflow の YAML にベタ書きせず、
   `.github/scripts/extract_closing_issues.py` に切り出す。** 理由は
   テスト可能性: workflow は実際にマージするまで本番検証できないが、
   「PR 本文からどの Issue 番号を拾うか」は純粋な文字列処理なので、
   `.github/scripts/test_extract_closing_issues.py` (標準ライブラリの
   `unittest`。新規パッケージ依存を増やさないため `pytest` は使わない) で
   手元で確実に検証できる。GitHub 本体の挙動に合わせ、`close(s/d)` /
   `fix(es/ed)` / `resolve(s/d)` の全変化形 (大小区別なし) をサポートし、
   フェンスコードブロック (```` ``` ````) と引用行 (`>` 始まり) の中は
   除去してから走査する。`owner/repo#123` のような他リポジトリ参照や、
   キーワードを伴わない `#123` 単独・PR 自身の番号は、そもそも
   「キーワード + 空白 + `#数字`」という正規表現の形に合致しないため
   追加の除外処理なしで自然に無視される。auto-merge.yml 側は
   `gh pr view --json body` で取得した本文をこのスクリプトに標準入力で渡し、
   1 行 1 Issue 番号の出力を受け取るだけの薄いラッパーに留める。
3. **冪等性は「クローズ前に現在の state を見る」だけで確保し、専用の
   フラグや記録は持たない。** 対象 Issue が既に `CLOSED` なら
   `gh issue close` 自体を呼ばずに skip する。これにより、将来
   `AUTO_MERGE_TOKEN` が登録されて GitHub 標準の自動クローズが本当に効く
   ようになった場合 (D55/本 Issue 案 B) でも、この処理が動く頃には対象
   Issue は既に closed になっているため何もせず、二重クローズやエラーの
   心配なく共存できる。存在しない Issue 番号は `gh issue view` が失敗する
   ので `::warning::` を出して次の Issue 番号の処理に進む (Issue 単位の
   失敗が PR 単位・workflow 全体の失敗に波及しないよう、既存の
   マージ失敗時と同じ「`::warning::` を出して続行する」流儀に合わせた)。
4. **追加するのはマージ成功後の後処理のみとし、既存のマージ可否判定
   ロジック (check-runs/commit status の集計、`no-automerge` ラベル判定など)
   には一切触れない。** Issue #168 の要求どおり、影響範囲を「マージが
   成功した PR の後処理」に限定した。
5. **既存の open Issue (#60/#63/#64 など) を遡って機械的にクローズする処理は
   入れない。** Epic #57 が「計測は完了、残件あり」として意図的に未完了
   扱いにしているため、本対応は今後マージされる PR にのみ適用する
   (Issue #168 本文の指示どおり)。

### 動作確認

抽出ロジック (`extract_closing_issues.py`) は Issue #168 に列挙された全ケース
(単一キーワードの全変化形・大小文字違い、1 行併記、複数行併記、コードブロック
内、引用行内、他リポジトリ参照、キーワード無しの `#123` 単独、キーワードが
1 つも無い本文、重複排除) を含む 26 件の `unittest` で検証し、全件 pass する
ことを確認した。**一方、`auto-merge.yml` 側の後処理 (`gh issue view`/
`gh issue close` の呼び出し部分) は、実際に auto-merge 経由で PR がマージ
されるまで本番環境で検証できていない** (workflow の実行そのものは GitHub
Actions 上でしか起きないため)。この PR がマージされた際の挙動 (対象 Issue が
実際にクローズされるか) を確認し、問題があれば追って対応すること。

### Revisit condition

(1) この PR 自身がマージされた時点で、本文の `Closes #168` により Issue #168
が実際に自動クローズされるかを確認する — 最初の実地検証になる。(2) 将来
`AUTO_MERGE_TOKEN` (PAT) が登録された場合、GitHub 標準の自動クローズと本対応
が同時に動くことになるが、決定 3 の冪等性により害はない想定。実際に PAT が
登録された際は、二重クローズやエラーが出ていないかログで一度確認するとよい。
## D84: 統合テストの固定 wait を根治する — `AutomationCommand::WaitLoad` を「main スレッドの状態機械 + 通知チャネル」で実装し、D44 の枠内 (新規制御チャネルなし) に収める

**対象**: Issue #169。`tests/integration.rs` の各テストは「ページの読み込みが
終わったはず」を `wait <ms>` という実時間スリープだけで表現しており、CI で
3 回 flake した (PR #162/#163/#166、いずれも「待ち時間を伸ばす」対症療法で
対処済み — Issue 本文の表を参照)。#60/D57 が実測したとおり、タブを開く/
休止タブを復帰させるコストは新しい `WebKitWebProcess` を起こすかどうかで
約 2 倍変わり (`page_load_ms` 6.5〜9.2ms vs 14.6〜16.1ms)、環境負荷でさらに
広がる。「何 ms 待てば十分か」は環境依存の量であり、定数化できる性質のもの
ではなかった。

**追加したコマンド**: `wait_load [timeout_ms]`。`browser::automation::
AutomationCommand::WaitLoad { timeout_ms: u64 }` として追加し、
`parse_script` に対応するパーサ (`parse_wait_load`) と単体テスト
(引数なし/あり/不正値/上限超過/上限ちょうど) を追加した。`timeout_ms` 省略
時は `automation::DEFAULT_WAIT_LOAD_TIMEOUT_MS` (10000ms)、指定時も上限は
既存の `wait` と同じ `MAX_WAIT_MS` (120000ms) — 新しい上限定数は増やして
いない。意味は「アクティブタブの進行中のページロードが終わるまで待つ。
既に終わっていれば即座に次へ進む」。`velox-bench`
(`browser::automation::generate_bench_script`) はこの Issue で一切変更して
いない — 既存シナリオの生成スクリプトに `wait_load` が混ざることはなく、
挙動・計測値は変わらない (下記「動作確認」参照)。

**設計 — D44 の枠内に収める (新規制御チャネルなし)**: D44 が明記している
とおり VeloX の自動操作はファイル駆動のスクリプトであり、待ち受け
ソケット/RPC サーバは意図的に採用していない。`wait_load` もこの制約の中で
実装した — スクリプトの書式・`VELOX_AUTOMATION_SCRIPT` という 1 つの
入力経路は変えていない。実行時の課題は「イベントループはメインスレッドの
`UserEvent` ディスパッチに集約されておりロックを持たない」
(docs/architecture.md) という制約の中で、自動操作スレッド
(`spawn_automation`) をブロックして `LoadFinished` を待つとデッドロックする
(待っている間に `LoadFinished` イベント自体を処理できない) ことだった。
解決策は Issue が示唆したとおり「メインスレッド側に状態機械を持たせる」:

1. `app::AutomationWaitState { pending: Option<AutomationWait>, notify:
   mpsc::Sender<()> }` を `run()` の中で 1 つだけ作り (`page_load_timers`
   と同じ流儀で `&mut` を各関数に通す)、`spawn_automation` には対応する
   `mpsc::Receiver<()>` を渡す。`pending` は「今どのタブの読み込みを
   待っているか (`window_id`/`tab_id`/`deadline`)」を持つ — 自動操作
   スクリプトは 1 度に 1 つの `wait_load` しか実行しない (後述) ので
   `Option` 1 枠で足りる。
2. 自動操作スレッド (`spawn_automation`) は `WaitLoad` に出会うと、他の
   コマンドと同じくイベントを `proxy.send_event` で送った**あと**、
   `automation_wait_rx.recv()` でブロックする。メインスレッドは通常どおり
   `LoadFinished`/`ToolbarMessage`/... を処理し続けられる — ブロックして
   いるのは自動操作スレッドだけ。
3. メインスレッドの `handle_automation_command` は `WaitLoad` を受けると
   `Tabs::active().is_loading()` を見る。`false` (既に読み込み完了) なら
   即座に `notify.send(())` して次へ進ませる。`true` なら `pending` に
   `window_id`/`tab_id`/`Instant::now() + timeout_ms` を書き込んで戻る —
   ここではブロックしない。
4. `pending` を解消する経路は 2 つだけ、どちらも「解消したら必ず
   `pending = None` にしてから 1 回だけ `notify.send(())` する」という
   不変条件を守る (自動操作スレッド側の `recv()` は「1 回の `wait_load`
   につき通知はちょうど 1 回」という前提でブロックしているため、この
   不変条件が崩れると次の `wait_load` が古い通知を誤って受け取る):
   - `resolve_automation_wait_if_matching` — `handle_user_event` の
     `LoadFinished` 節の先頭で呼び、`pending` が同じ `window_id`/`tab_id`
     を指していれば解消する。
   - `poll_automation_wait_timeout` — `run()` のイベントループ末尾、
     既存のタブ休止スイープ (`sweep_tabs`) の直後に毎パス呼ぶ。
     `deadline` を過ぎていれば「何を待っていたか」(window/tab/URL、
     取得できれば) を stderr に出して解消する。過ぎていなければ
     `sweep_tabs` の戻り値と同じ枠組みで `ControlFlow::WaitUntil` の
     候補に `deadline` を加える — 次のイベントを待つだけでは
     `deadline` ちょうどに起きられないため。
5. ウィンドウが既に無くなっている場合 (`ui_windows.get_mut(automation_
   window)` が `None`) の `WaitLoad` は、`handle_automation_command` に
   到達する前に `UserEvent::Automation(command)` の分岐で即座に通知して
   打ち切る — 待つ対象が無い自動操作スレッドを永遠にブロックさせない
   ためのガード。

この設計はロックを一切増やしていない (`mpsc::channel` のみ) — `AppState`
に持たせず `page_load_timers` と並ぶ独立した `&mut` 引数にしたのも、既存の
「状態はメインスレッドの引数として明示的に流す」流儀を崩さないため。

**`tests/integration.rs` の置き換え**: 「読み込みが終わるのを待つ」意図の
`wait <ms>` を `wait_load` に置き換えた (対象はほぼ全テスト — module doc
comment に一覧の方針を書いた)。意図的に置き換えなかったもの:
- `downloads_with_several_tabs_open_are_handled_exactly_once` の
  `navigate <download_page>` 直後の 2 箇所。ダウンロードとして横取り
  される遷移で通常の `LoadFinished` が届くかどうかを検証しておらず、
  届かない場合 `wait_load` は「ハングしないが必ずタイムアウトする」動作に
  なる — 実害は無いが無駄にタイムアウトを踏むだけなので、確認が取れる
  までは元の `wait <ms>` のままにした。
- `startup_completes_and_records_a_startup_event` の唯一の `wait`。
  **これは実装中に `wait_load` へ置き換えて検証した結果、判明した本物の
  発見**: このテストが待つべきなのは「ページの読み込み完了」だけでなく
  「`startup` perf レコードの生成」であり、`app::mark_startup`/
  `metrics::StartupTimestamps::report` はコンテンツタブの `LoadFinished`
  **に加えて**トゥールバー Webview 独自の `ready` ハンドシェイク
  (`ToolbarCommand::Ready`) も揃わないと `startup` レコードを書かない。
  `wait_load` はコンテンツタブの `LoadFinished` しか見ないため、負荷の
  かかった環境でトゥールバーの JS 初期化がページ読み込みより遅く終わる
  瞬間があると、`wait_load` が早すぎるタイミングで `quit` を通してしまい
  `startup` レコードが出力される前にプロセスが終了する — 実際に本
  セッションのコンテナ上で `cargo test --test integration` を連続実行して
  2/5 回この形で red になることを確認した (詳細は「動作確認」)。これは
  `wait_load` 自体のバグではなく、`mark_startup` 側の**既存の**競合状態
  (古い固定 `wait 1500` がたまたま覆い隠していただけ) であり、Issue #169
  の primitive の対象外 (issue はページロード完了を待つ命令のみを要求)
  なので、このテストの `wait` は元の `wait 1500` のまま残し、コメントで
  理由を明記した。この既存の競合状態自体は D84 の対象外として残す
  (下記 Revisit condition)。

**動作確認**: `cargo fmt --check` / `cargo clippy --all-targets -- -D
warnings` / `cargo test --lib` (963 件) / `cargo check --target
x86_64-pc-windows-msvc --all-targets` (D61、`cfg` 分岐は増やしていないが
念のため実行) はすべて green。統合テスト
(`VELOX_INTEGRATION_REQUIRE_GUI=1 xvfb-run ... dbus-run-session --
cargo test`) は 11 件全 green を確認し、置き換え後の完全な統合テスト
一式を**連続 10 回以上**実行してすべて green だったこと (上記の
`startup_completes_and_records_a_startup_event` の発見・修正を挟んだ
前後それぞれで確認)、`AutomationCommand::WaitLoad` のタイムアウト経路は
実際に到達不能な (accept はするが応答を返さない) ローカル TCP リスナーへ
`navigate` させて手動で発火させ、ハングせずプロセスが自力で終了 (`quit`
まで到達) すること、stderr に
`wait_load はタイムアウトしました (window=..., tab=..., url=Some("..."))`
という「何を待っていたか」が分かるメッセージが出ることを確認した。
`velox-bench run --scenario tab_create` を実際に 1 試行走らせ、
`page_load_ms` 中央値 8.55ms など従来と同オーダーの数値が出ることを確認
した (生成ロジック自体は無変更なので、この確認は「配線が壊れていないか」
の smoke test)。

所要時間の実測 (before は元の `tests/integration.rs` を一時的に復元し
同じビルドで計測、after は置き換え後):

| テスト | before | after |
| --- | --- | --- |
| `visiting_pages_persists_history_json` | 2.76s | 0.41s |
| `restoring_the_previous_session_reopens_its_tabs_across_a_real_relaunch` | 4.63s | 0.91s |
| `repeated_tab_open_close_cycles_exit_cleanly_and_record_every_page_load` | 11.04s | 2.46s |
| 統合テスト一式 (11 件、`--test-threads=1`) | 41.11s | 約 13〜15s |

### Revisit condition

(1) `startup_completes_and_records_a_startup_event` が踏んだ
`mark_startup`/`StartupTimestamps::report` の競合状態 (トゥールバー
`ready` とコンテンツタブ `LoadFinished` の到着順序に依存する) は、
`wait_load` の副作用として見つかっただけで本 Issue のスコープ外 — 別
Issue で `StartupTimestamps` 側の設計 (例えば `report()` が揃うまで
`quit` 自体を遅延させる、あるいは `wait_load` とは別の「起動完了を待つ」
primitive を用意する) を検討すること。(2)
`downloads_with_several_tabs_open_are_handled_exactly_once` の
ダウンロード遷移が `LoadFinished` を発火させるかどうかは未確認のまま
— 確認できれば残り 2 箇所の `wait <ms>` も `wait_load` に置き換えられる
可能性がある。(3) `wait_load` は「1 スクリプトにつき同時に 1 つの
待ちしか無い」という前提 (`AutomationWaitState::pending` が `Option` 1 枠)
に依存している — 将来 `velox-bench`/統合テストが複数ウィンドウを並行して
待つような使い方を必要とした場合は、この前提から見直すこと。

## D85: 統合テストに残った固定 wait を無くす (#173) — `wait_startup` を追加して D84 の Revisit condition (1) を解消し、(2) は実測の上で「wait_load に置き換えない」結論を確定させた

**対象**: Issue #173。D84 が残した 3 箇所の固定 `wait <ms>` のうち、
`startup_completes_and_records_a_startup_event` の 1 箇所 (主目的) と
`downloads_with_several_tabs_open_are_handled_exactly_once` の 2 箇所
(副次) を扱った。

**主目的 — `wait_startup [timeout_ms]` を追加 (案 A を選択)**: Issue 本文が
示した 2 案 (A: `wait_startup` 専用コマンド / B: `wait_perf <event>` 汎用形)
のうち **A を選んだ**。理由:

- この Issue が実際に必要としている待ち条件は「`startup` perf レコードが
  書かれること」ただ 1 つで、他の perf イベント (`page_load`/`tab_create`/
  `tab_switch`/`rss`/...) を自動操作スクリプトから待つ具体的な需要は
  Issue 本文にも既存のテスト群にも無い。
- B はイベント名の妥当性検証 (存在しないイベント名を指定された場合の扱い、
  `measure_start`/`ipc` のような「1 スクリプトにつき 1 回では終わらない」
  イベントをどう扱うかなど) を余分に設計する必要があり、Issue が明記した
  見積り (cost: low、`automation.rs`/`app.rs`/`tests/integration.rs` の
  3 ファイルで完結) と釣り合わない。
- A はコマンド名自体が「何を待つか」を表しており (`wait_perf startup` より
  自己文書的)、`wait_load` と対になる語彙として自然。

**設計**: D84 の `AutomationWaitState`/`AutomationWait`/
`poll_automation_wait_timeout`/`resolve_automation_wait_if_matching`/
`spawn_automation`/`handle_automation_command` という「main スレッドの
状態機械 + 通知チャネル」をそのまま再利用し、待つ条件を 1 つ増やしただけ
— D44 の枠内 (`VELOX_AUTOMATION_SCRIPT` 以外の制御チャネルを増やさない)
を維持している。具体的な変更:

1. `AutomationWait` に `window_id`/`tab_id` を直接持たせる代わりに、
   `AutomationWaitKind { Load { window_id, tab_id }, Startup }` を導入し、
   `AutomationWait { kind, deadline }` に一般化した。`wait_load` 側の
   呼び出し・照合ロジック (`resolve_automation_wait_if_matching`) は
   `Load` にだけマッチするよう変えただけで、意味は変えていない。
2. `AutomationWaitState` に `startup_reported: bool` を追加した。
   `wait_startup` が「`startup` レコードが書かれた**後**に発行された」
   場合 (`wait_load` の「既に読み込み完了していれば即座に次へ」に相当する
   ケース) に、`handle_automation_command` がこのフラグを見て即座に
   `notify.send(())` するために要る — フラグが無いと、レコードが既に
   書かれた後の `wait_startup` は「待つ相手がもういない `pending`」を
   登録してしまい、次の解消経路が来るまで (実質タイムアウトするまで)
   進めなくなる。
3. `startup_reported` を立てる場所は `mark_startup` (`record_perf_event`
   内) ではなく **`run()` のイベントループ側**にした。`run()` の
   `Event::UserEvent` 節で `record_perf_event` を呼ぶ前後の
   `startup: Option<StartupTimestamps>` を比較し (`Some -> None` の遷移
   = ちょうど今 `mark_startup` がレコードを書いた瞬間、`mark_startup`
   自身が「書いたら `None` にする」ことで保証している一意性と同じ signal
   を再利用)、遷移を検知したら `resolve_automation_wait_for_startup` を
   呼ぶ。`record_perf_event`/`mark_startup` の側には一切手を入れていない
   — この 2 つは他の perf イベント (`page_load`/`tab_create`/...) と
   同じく「自動操作の都合を知らない」ままにしておきたかった
   (`AutomationWaitState` を引数に増やすと、perf 記録ロジックが
   自動操作の待ち機構に依存するという逆向きの結合が生まれる)。
   `record_perf_event` の単体テスト (3 箇所、`app.rs` の `mod tests`) も
   シグネチャ変更なしで無傷のまま通る。
4. `handle_automation_command` の `WaitStartup` 節は `WaitLoad` と対称:
   `automation_wait.startup_reported` が `true` なら即座に解消、`false`
   なら `pending` に `Startup` を登録して `deadline` を設定するだけで
   ここではブロックしない (`WaitLoad` が `Tab::is_loading()` を見るのと
   同じ形)。
5. `UserEvent::Automation(command)` 節の「ウィンドウが既に無くなっている
   場合は即座に通知して打ち切る」ガード (D84 の設計 5.) は `WaitLoad` と
   `WaitStartup` の両方を対象にした — `wait_startup` は実際にはどの
   ウィンドウにも依存しないが、他の自動操作コマンドと同じ
   `ui_windows.get_mut(automation_window)` ゲートを経由して配送される
   ため、同じガードが要る。
6. `parse_wait_load` は `parse_optional_wait(line, keyword, rest)` に
   一般化し、`wait_load`/`wait_startup` の両方から呼ぶ (エラーメッセージの
   コマンド名だけ引数で差し替える)。デフォルトタイムアウト定数
   `automation::DEFAULT_WAIT_LOAD_TIMEOUT_MS` (10000ms) はそのまま
   両コマンドで共有した — 別名の定数を増やすと D84 の本文
   (`docs/decisions.md` の既存エントリ、書き換え禁止) が参照している
   名前と食い違うため、新しい定数は増やさずドキュメントコメントで
   「`wait_startup` とも共有している」と明記するに留めた。

**`tests/integration.rs` の置き換え**: `startup_completes_and_records_a_
startup_event` の `wait 1500` を `wait_startup` (引数なし、デフォルト
10000ms タイムアウト) に置き換えた。

**副次 — ダウンロードへの `navigate` (実測結果)**: `VELOX_DEBUG=1` で
`downloads_with_several_tabs_open_are_handled_exactly_once` と同じ形の
スクリプトを実行し、`velox[debug]: <UserEvent>` のトレースを直接確認した。
分かったこと:

- `navigate <download_page>` そのもの (`download.html` を読み込む遷移) は
  **`LoadFinished` を発火する** — `download.html` 自体は普通の HTML ページ
  で、`with_download_started_handler` (D28) に横取りされるのは、その
  ページの `load` イベント後に JS が `click()` する `download` 属性付き
  リンクの**先** (`data:text/plain;...` への 2 段目のナビゲーション) だけ
  だった。トレース上は `NavigationStarted(..., "file://.../download.html")`
  → `LoadFinished(..., "file://.../download.html")` → (JS の `load`
  ハンドラ発火) → `NavigationStarted(..., "data:text/plain;...")` →
  (`LoadFinished` は無い、代わりに `DownloadStarted`/`DownloadCompleted`)
  という順で観測された。D84 が「届くかどうか未検証」としていた懸念
  (`wait_load` が来ない `LoadFinished` を待ち続けてタイムアウトするだけ、
  実害は無い) はこの意味では外れていた — `wait_load` はハングしない
  どころか、`download.html` 自身の読み込み完了で正常に、しかも今までの
  固定 `wait 2500` よりずっと早く解消する。
- **しかしこれは「置き換えても安全」を意味しなかった**。`wait_load` が
  解消するのは「ページの読み込みが終わった」時点であり、それは
  `DownloadStarted`/`DownloadCompleted` (非同期に、`LoadFinished` より
  後で発火する — 上記トレースでも `DownloadCompleted` は次の
  `navigate`/`LoadStarted` が始まった後に届いていた) より確実に前に来る。
  2 回ダウンロードして 2 回目の直後が `quit` という実際のテストの形を
  そのまま再現し (両方の `navigate <download_page>` の直後を `wait_load`
  に置き換え、末尾の settle 用 `wait` は入れない)、同一スクリプトを
  **10 回連続実行**したところ、**2/10 回で 2 個目のダウンロードファイルが
  存在しなかった** (`velox-test (1).txt` が生成される前にプロセスが
  `quit` してしまうケース)。1 個目のダウンロードは後続の `navigate
  {homepage}`/`wait_load` がその間の実時間を稼ぐため毎回間に合っていたが、
  2 個目には後続コマンドが無く `quit` までの猶予が本質的に無かった。
- 結論: `wait_load`/`wait_startup` はいずれも「ページの読み込み完了」
  「`startup` レコードの記録完了」という条件しか約束しておらず、
  「ダウンロードの完了」は両者と独立した 3 つ目の条件になる。この Issue
  は `wait_perf`/`wait_download` のような追加の待ちプリミティブを要求
  しておらず (受け入れ条件は `wait_startup`/実測記録のみ)、スコープを
  無断で広げてまで作るべきものでもないと判断し、この 2 箇所は**元の
  `wait <ms>` のまま変更していない**。`tests/integration.rs` の当該テスト
  のコメントと module doc comment に、上記の実測結果 (具体的な観測イベント
  列と 10 回中 2 回という数値) を残した — 推測ではなく実測の記録として
  今後の判断材料にできるようにするため。

**動作確認**: `cargo fmt --check` / `cargo clippy --all-targets -- -D
warnings` / `cargo test --lib` (969 件、D84 時点の 963 件 + `wait_startup`
のパーサ単体テスト 6 件) / `cargo check --target x86_64-pc-windows-msvc
--all-targets` (D61。`cfg` 分岐は今回も増やしていないが、D84 に倣い念のため
実行) はすべて green。統合テスト (`VELOX_INTEGRATION_REQUIRE_GUI=1
xvfb-run ... dbus-run-session -- cargo test`) は 11 件全 green を複数回
(3 回) 確認した。

`startup_completes_and_records_a_startup_event` (`wait_startup` への置き換え
本体) は **単体で 25 回連続実行して全て pass** を確認した — 内訳は無負荷
15 回 (各 0.40〜0.46s、旧 `wait 1500` より大幅に短い) と、4 コア全部を
ビジーループで専有した状態での 15 回中10回 (各 0.62〜0.86s、負荷がかかって
実際に遅くなっていることを実行時間で確認した上での 10 回) — 合計 25/25
pass。D84 の記述にあった「このコンテナ上で 5 回に 2 回 red になる」という
条件下 (`wait_load` へ単純置換した場合) との対比としては、`wait_load` への
単純置換を試した時点の再現は本セッションでは行っていない (D84 の記述と
今回の負荷再現実験の両方から、`wait_startup` が実イベントを直接待つ以上
理論的にも同種の flake が起きないことは設計上保証されている、という位置
付け)。

タイムアウト経路は `wait_load`/`wait_startup` の両方を個別に確認した:
`wait_startup` は `VELOX_PERF_METRICS` を設定せず (`startup` レコードが
一生書かれない状況を人工的に作り) `wait_startup 500` を実行し、
500ms 後に stderr に
`wait_startup はタイムアウトしました (startup perf レコードがまだ書き込まれていません — VELOX_PERF_METRICS が有効か確認してください)`
が出た上でプロセスが `quit` まで到達し自力終了する (ハングしない) ことを
確認した。`wait_load` の既存タイムアウト経路も、応答を返さないローカル
TCP リスナー (127.0.0.1:8899、`accept` はするが何も送らない) へ
`navigate` させて回帰していないことを確認した (D84 と同じ手法)。

`velox-bench run --scenario tab_create --trials 1` を実際に 1 試行走らせ、
`tab_create_ms` 中央値 3.30ms・`page_load_ms` 中央値 10.60ms など
D84 で確認した際と同オーダーの数値が出ることを確認した (`generate_bench_
script` は本 Issue で一切変更していないので、この確認も「配線が壊れて
いないか」の smoke test)。

### Revisit condition

D84 の Revisit condition (1) (`startup_completes_and_records_a_startup_
event` の競合状態) は本 Issue で解消済み。(2) (ダウンロード遷移の
`LoadFinished`) も実測により「届くが `wait_load` へは置き換えない」で
確定した — 再訪の必要があるとすれば、ダウンロード完了という 3 つ目の
条件を待つ専用プリミティブ (`wait_download` 相当) を作る価値が生じたとき
のみ。(3) (`AutomationWaitState::pending` が `Option` 1 枠という前提) は
本 Issue でも変えていない — `wait_load`/`wait_startup` を同時に 2 つ
発行するスクリプトは今のところ存在せず、`spawn_automation` が 1 コマンド
ずつ順に処理して次のコマンドへ進む前に必ず解消を待つ設計なので、依然
1 枠で足りている。

## D86: Browser State / Event Dispatch 最適化 (#67) — `persist_session` の無条件ディスク書き込みを唯一の削減対象として特定し、直前スナップショットとの比較でスキップする形にした。tab/window lookup と lock contention は実測前の設計調査だけで「対象外」と判断した

Issue #67 (Epic #57 Phase 3、依存元 #66 の後続)。「Browser/Tab state
更新と event dispatch をプロファイリングし、不要な state 変更・clone・
再描画を削減する」という課題に対して、#66 (D81) と同じ手順 — まず計測を
足し、その実測データだけで削減判断をする — を踏んだ。数値・再現手順は
`docs/performance-targets.md` §19 を参照。**この節の数値はすべて Linux
(WebKitGTK 2.52.6 / Xvfb、GPU なし) での計測であり、Windows (WebView2)
の実力値ではない** (Epic #57 ルール 5)。

### 対象範囲の絞り込み — 「lock contention」は設計上該当しない

Issue 本文が挙げる 6 項目 (state mutation / tab lookup / event routing /
lock contention / redundant updates / UI synchronization) のうち、
**lock contention は計測するまでもなく対象外と判断した**:
`docs/architecture.md` の "Event flow" 節が明記するとおり、全状態
(`AppState`/`Tabs`/`Windows`) はメインスレッドの `UserEvent` ディスパッチ
一本に集約されており、ロックを一切持たない設計 (`app::handle_user_event`
の呼び出し木の外で状態を触るコードは存在しない)。`grep -rn "Mutex\|
RwLock" src/` で見つかった 2 箇所はどちらもこの状態機械の外側にある:
`ui::window`内の 1 つは `with_permission_handler`(`wry` の trait 境界が
`Send + Sync` を要求するだけで、実際には常にメインスレッドから読み書き
される 1 origin 文字列のロック — コード自身のコメントが既にこの理由を
説明している)、もう 1 つは `browser::perf_log::PerfLog`(計測ログの
書き込みを直列化する `Mutex<Sink>` — `VELOX_PERF_METRICS=1` のときだけ
存在し、`spawn_rss_sampler` のバックグラウンドスレッドとメインスレッドが
競合しうる唯一の箇所だが、対象は診断用ログ出力であって browser state
そのものではない)。**どちらも本 Issue が探すべき「state mutation の
ホットパスを塞ぐロック」ではない** — Issue 本文の項目立ては pthread/
Mutex ベースのブラウザ実装を念頭に置いた一般的なチェックリストであり、
本アーキテクチャ (シングルスレッド state machine) には当てはまらない
ことをここに記録する。

### tab lookup / window lookup — 実測未満、構造的に無視できる規模と判断した

`browser::tabs::Tabs::get`/`get_mut`/`position`(`src/browser/tabs.rs`)と
`browser::windows::Windows`の同種メソッド (`src/browser/windows.rs`) は
いずれも `Vec` の線形走査 (`iter().find(|t| t.id() == id)`)。ベンチマーク
までは組まなかった — 判断できる理由が実測を待たずに揃っていたため:

- 走査対象がそもそも小さい。`Tabs`(1 ウィンドウのタブ数) は
  `docs/performance-targets.md` の重量級シナリオでも上限 20〜50、
  `Windows`(開いているウィンドウ数) は実運用でまず 1〜3。`u64`
  (`TabId`/`WindowId`) の等値比較を数十回行うコストは、#66 (D81) が
  実測した「20 タブでの `set_tabs` 構築 (この線形走査を伴う)」が
  sub-millisecond (中央値 0.000ms) だったことに既に織り込まれている —
  `sync_tab_strip` は `tabs.iter()` で全タブを舐めて `TabSummary` を
  組み立てており、この Issue が疑う lookup コストと同じ処理を #66 が
  既に計測済みだった。
  `HashMap<TabId, Tab>` へ切り替えても、この規模では定数倍の違いが
  測定ノイズに埋もれる可能性が高く、しかも表示順序 (`Tabs::iter`が
  タブストリップの並び順そのもの) を別に保持する必要が生じてコードは
  複雑になる — 得られる見返りが実測抜きでも小さいと判断できた。
- `Windows`も同型の設計で、`WindowEntry`を`Vec`で持つ理由は
  `docs/decisions.md` D68 (「なぜ `Tabs` 自体に複数ウィンドウを教えない
  か」) に既に記録されている。
- 万一この判断が誤りだったとしても実害は限定的: `tabs_of`/`Windows::
  tabs`が返す`&mut`参照は呼び出し側で 1 回解決されるだけで (ループの
  内側で毎回再解決される設計ではない — `tabs_of`のドキュメントコメント
  参照)、1 イベントあたりの lookup 回数はおおむね定数。

**Revisit condition**: 将来 1 ウィンドウが数百タブを持つユースケースが
本気で検討されるなら (現状のロードマップにはない)、この判断は再検証が
要る。

### state mutation / redundant updates — 実測して見つかった唯一の対象:
### `persist_session` の無条件ディスク書き込み

`sync_tab_strip`(`app.rs`) は `NavigationStarted`/`LoadFinished`/
`FaviconResolved`/タブの open・close・switch・activate など、ほぼ
すべてのタブ変化イベントから呼ばれ、その末尾で無条件に `persist_session`
を呼んでいた (Issue #25/D65)。`persist_session` は
`persistence::save_session`経由で**実際の同期ディスク I/O**
(`fs::create_dir_all` + `serde_json::to_string_pretty` +
`fs::write`) を行う — #66 (D81) が計測した `set_tabs` などの IPC
(プロセス内の `evaluate_script` 呼び出し) とは性質が違い、コストの
桁が 1 つ上がりうる箇所だと仮説を立てた (Epic #57 ルール 1 の
Hypothesis)。

**計測基盤**: `metrics::PerfRecord::StateWrite`(`name`/`duration`)を
新設し、`app::record_state_write`(`record_tab_latency`と同型、
`state.perf`が`None`なら`Instant::now()`すら呼ばない) から
`persist_session`/`persist_history`/`persist_bookmarks`/
`persist_input_history`の 4 箇所すべてに配線した — #66 の
`PerfRecord::Ipc`と同じ「単一 choke point に 1 行ずつ」方針。
`event=state_write`として同じ perf ログ (`VELOX_PERF_OUTPUT`) に
混在するので、既存の `velox-bench ipc-summary`が読む同じログファイルから
`jq`等で `event=="state_write"`を抜き出すだけで集計でき、専用の
CLI サブコマンドは追加しなかった (`summarize_ipc`と違い、name の
種類が 4 つ固定で組み合わせ爆発しないため、都度のアドホック集計で
十分と判断した)。

**Baseline (計測結果、20 タブ自動操作セッション、修正前)**: 20 タブを
順に開き、mark 後に 10 回切替 + 2 回ナビゲーション + 2 回クローズを行う
セッション (§19.2 に再現手順) で `state_write name=session`が **150 回**
発生し、その `duration_ms`合計は **50.7ms**(中央値 0.100ms、p95
0.300ms、最悪値 10.8ms)。3 タブの軽量セッションでも **39 回 / 8.6ms**。
`SessionSnapshot`が保持するのは url/title/favicon のみ (`loading`フラグ
は含まない) にもかかわらず、`NavigationStarted`(URL 変化なし・
loading フラグのみ変化) のような呼び出しでも `sync_tab_strip`経由で
無条件に書き込みが走っていた — これが唯一実測で見つかった「本当に
不要な state 変更の反映」だった。

**削減の実装**: `AppState`に`last_persisted_session: Option<
SessionSnapshot>`を追加し (直近に書き込んだ内容のキャッシュ)、
`persist_session`が新しく組み立てたスナップショットをこれと比較して
一致すれば `save_session`呼び出し自体を丸ごとスキップするようにした
— #66 の `refresh_history_panel_if_open`(パネルが閉じていれば
`set_history`送信そのものをスキップした) と同じ形の「送るか送らないか」
判断で、batching やタイマー・デバウンスは一切持ち込んでいない (Epic
#57 ルール 1 に照らし、計測上必要と分かった分だけの変更にとどめた)。
書き込みが成功したときだけキャッシュを更新するので (`save_session`が
失敗した回はスキップ判定に使われない)、ディスクへの反映漏れは生じない。
private window / データディレクトリ未解決時の既存の早期 return は
そのまま維持している。

**After (同一セッションでの再測定、2 試行)**: 20 タブセッションで
`state_write name=session`の回数が **150 → 82 (-45.3%)**、これは 2 回の
再測定でどちらも 82 回とまったく同じ値になった (自動操作スクリプトが
決定的で、削減対象がタイマー由来のジッタではなく制御フローそのものの
ため)。`duration_ms`合計は 1 回目 16.7ms、2 回目 51.7ms — 後者は 1 件の
外れ値 (38.2ms、共有 VM 上のスケジューリング揺らぎとみられる) に
支配されており、**中央値 (0.1ms)・p95 (0.2〜0.3ms) は前後でほぼ不変**
(削減されたのは「呼ばれる回数」であって「1 回あたりの速さ」ではない、
という点は #66 の `set_tabs`の結論と同じ形)。3 タブセッションでも
**39 → 22 (-43.6%)**、同傾向。並行して計測した `state_write
name=history`(`persist_history`) は本 Issue で手を入れていないので
前後とも 69 回 / 18 回のまま変化なし — 削減が意図した箇所だけに効いて
いることの裏付けとして記録しておく。

**`persist_history`/`persist_bookmarks`/`persist_input_history`は
削減しなかった。** これらは実際の内容変更 (訪問記録・タイトル確定・
favicon 確定・削除・クリア) の都度呼ばれており、同じ内容を 2 回書く
という意味での「冗長」は起きていない (1 回のページ読み込みで最大 3 回
`persist_history`が呼ばれるのは、訪問記録・タイトル確定・favicon 確定
という 3 つの異なる時点の異なる内容を反映しているためで、`persist_
session`の場合とは事情が違う)。実測でも 20 タブセッションの合計
8.3ms (69 回) と、削減を要する規模ではなかった。3 イベントを 1 回の
書き込みにまとめるバッチ化は着手時点で検討したが、計測上の必要性が
無い状態で複雑さ (ステイル状態のリスクを持つタイマー/デバウンス) を
持ち込むことになるため見送った — Epic #57 ルール 1 通りの判断。

**`write_json`の`fs::create_dir_all`が呼び出しごとに stat 相当の
syscall を発生させている**点も調査中に気づいたが、これも実測 (中央値
0.1ms) の範囲では埋没しており、単独では最適化を正当化する規模ではない
— ディレクトリキャッシュを持ち込むと「起動後にデータディレクトリが
削除された場合に復旧できなくなる」という新しい失敗モードを増やす
リスクもあるため、見送った。

### event routing / UI synchronization — 追加の削減対象は見つからなかった

`handle_user_event`/`handle_toolbar_command`/`handle_content_shortcut`/
`handle_automation_command`(`app.rs`)のディスパッチは `match`文 1 段
(コンパイラがジャンプテーブルに落とす)であり、これ自体を疑う実測上の
根拠は無かった。UI synchronization (`BrowserWindow`への `set_*`呼び出し
群) は #66 (D81) が `eval_toolbar`という単一 choke point に既に集約・
計測済みで、本 Issue で新たに見つかった冗長呼び出しは無い —
`persist_session`(本節で削減した箇所) はディスク書き込みであって
`eval_toolbar`経由の IPC ではないため、#66 の計測範囲の外にあった、
という位置づけになる。

### なぜ `unwrap`/`expect` を増やさずに実装できたか

`record_state_write`は`record_tab_latency`と同じ形 (`state.perf`が
`None`なら即 return、失敗しうる処理は無い) なので、呼び出し側に
`unwrap`/`expect`は増えていない。`persist_session`の`&AppState`→
`&mut AppState`シグネチャ変更は、唯一の呼び出し元 `sync_tab_strip`が
既に`&mut AppState`を受け取っていたため、呼び出し側の変更は不要だった
(`ui::toolbar`からの`PageTitleResolved`直接呼び出し 1 箇所も同様)。

### 後続 Issue (#68) が使えるもの

- `metrics::PerfRecord::StateWrite`/`StateWriteKind`
  (`session`/`history`/`bookmarks`/`input_history`) — `persistence::
  save_*`のディスク書き込みコストを継続的に計測できる。#68
  (Serialization/Allocation Optimization) が`serde_json::to_string_
  pretty`のアロケーションコストを見るときも、同じイベントの
  `duration_ms`が土台になる (現状は create_dir_all を含めた総コストで
  分離していない点は #68 側で必要なら切り分けを追加できる)。
- `AppState::last_persisted_session`という「直近に書いた内容と比較して
  スキップする」パターン — 他の `persist_*`が将来ホットパス化した場合に
  同じ形をそのまま適用できる。

### Revisit condition

(1) 1 ウィンドウが数百タブになるユースケースが検討され始めたら、
tab/window lookup の`Vec`線形走査を再検証すること。(2) `state_write`の
`duration`は`write_json`全体 (`create_dir_all`+ シリアライズ + 書き込み)
の合算であり、内訳を分けていない — #68 がシリアライズコストだけを
見たくなったら、`persistence.rs`側に計測を移すか`write_json`の返り値を
広げる必要がある。(3) 本節の数値はすべて Linux/WebKitGTK — Windows
(WebView2) でのディスク I/O コストは NTFS のメタデータ操作コストが
異なるため未計測 (Epic #57 ルール 5)。
## D87: ページロードの段階計測 (#69) — `NavigationStarted → LoadStarted → LoadFinished` に分解。DNS/接続/TLS timing は wry に無い、VeloX 側の追加最適化も見送り

Issue #69 (Epic #57 Phase 3)。「ページロード経路を分解し、VeloX 側で制御
可能なボトルネックを改善する」という課題に対して、**#59/#66 と同じやり方
で臨んだ**: まず既存の計測点を1段階分解して実測し、その数値だけで
「VeloX 側に短縮余地があるか」を判断した。数値・再現手順は
`docs/performance-targets.md` §20、使い方は `docs/benchmarking.md` を
参照。ここには設計判断とその理由、および調査結果だけを残す。

### 何を分解したか — 既存の `page_load` イベントに1つチェックポイントを足した

Issue #13 からこの方、`page_load` イベントは `NavigationStarted` →
`LoadFinished` の1本の `duration_ms` しか持っていなかった。wry の
`WebViewBuilder` は `with_on_page_load_handler` で `PageLoadEvent::
Started`/`Finished` の2値を渡してくる — `app.rs` は既に両方を
`UserEvent::LoadStarted`/`LoadFinished` として受け取っていたが、
`LoadStarted` は UI 同期 (`sync_tab_strip` 等、`NavigationStarted` と
同じ match アーム) にしか使われておらず、計測には使われていなかった。
`metrics::PageLoadTimer` に `mark_load_started` という新しい任意
チェックポイントを足し、`finish` の戻り値を `Duration` から
`PageLoadOutcome { total, engine: Option<Duration> }` に変えた
(`engine` は `dispatch()` で `total - engine` も導出できる)。`page_load`
イベントの JSON に `engine_duration_ms`/`dispatch_duration_ms` を
**追加**した (既存の `duration_ms`/`url` は変更なし、text 形式も
`format_page_load` の出力に追記するだけ — #59/D42/D43 と同じ「既存の
scraper を壊さない」流儀)。`browser::benchmark::MetricKey` にも
`PageLoadEngineMs`/`PageLoadDispatchMs` を追加し、`velox-bench` の
`aggregate`/`gate` にそのまま乗るようにした。`LoadStarted` が来なかった
ロード (中断されたロード等) は `engine`/`dispatch` とも `None` — 0 を
捏造しない、`total_pss_bytes` (D42) と同じ規約。

### **重要な但し書き: `dispatch` は「VeloX のコスト」ではない**

`PageLoadEvent::Started` は wry の3バックエンドすべてで「ロードが
commit された」タイミングにマップされている (**ソースで確認**):

- WebKitGTK: `webview.connect_load_changed` の `LoadEvent::Committed`
  (`wry-0.56.1/src/webkitgtk/mod.rs:481`)
- WebView2: `ContentLoadingEventHandler` (`.../src/webview2/mod.rs:713-718`)
- WKWebView (macOS/iOS): `didCommitNavigation`
  (`.../src/wkwebview/navigation.rs:17-26`)

つまり `LoadStarted` は「ロードが始まった瞬間」ではなく「エンジンが
接続・リクエスト送信・レスポンス受信開始まで済ませた後」に発火する。
`NavigationStarted` (`with_navigation_handler`、ロード許可の意思決定
ポイント) は逆にネットワーク作業が始まる**前**に発火する。したがって
`dispatch = NavigationStarted → LoadStarted` の区間には VeloX 自身の
イベント処理 (`app::record_perf_event`、`sync_tab_strip` 等) だけでなく
**エンジン側の接続・リクエスト送受信の待ち時間も含まれる**。実測でも
これは裏付けられた — loopback HTTP サーバ (DNS 無し、TLS 無し) の
`minimal.html` に対してすら `dispatch` の中央値 (4.65〜4.7ms) が
`engine` の中央値 (1.4〜1.65ms) を上回った (§20.2)。これを「VeloX の
オーバーヘッドが半分以上」と読むのは誤りで、Epic #57 の最重要注意点
「レンダリングエンジンそのものの性能と VeloX 側のオーバーヘッドを
混同しない」にまさに抵触する読み方になる。#66 (D81) が実測した
IPC の Rust 側コスト (`out` イベント、`sync_tab_strip` を含む
`evaluate_script` 呼び出し) が worst case でも 3.5ms、中央値 0.000ms
だったことを踏まえると、`dispatch` の大半はエンジン側のネットワーク
待ち (今回の環境ではループバック接続の確立・往復) であり、VeloX 自身の
Rust コードは `dispatch` の一部でしかない。ソースコードのコメント
(`metrics::PageLoadTimer`/`MetricKey::PageLoadDispatchMs`) にこの但し
書きを明記した。

重い固定ページ (`dom_heavy.html`) で計測すると、この解釈が裏付けられる:
`dispatch` の中央値は 12.65ms (`minimal.html` の 4.65ms よりやや大きいが
オーダーは同じ) で、ページの重さに依らずほぼ一定に見える一方、`engine`
の中央値は 72.0ms までページの重さに比例して伸びる (§20.2)。「ページが
重いほど支配的になるのは engine 側」という、Epic #57 が前提とする構造
と整合する結果になった。

### DNS/connection/TLS timing は取得可能か — wry native API には無い。JS 標準 API は動くが、この環境では意味のある数値が取れない

wry 0.56.1 のソースを3バックエンドとも確認したが (`grep -rniE
"dns|tls|resource_load|timing" src/*.rs src/{webkitgtk,webview2,wkwebview}/
*.rs`)、`WebViewBuilder` にはナビゲーションの許可可否
(`with_navigation_handler`) とロード完了2値
(`with_on_page_load_handler`) しか無く、**DNS/接続/TLS のタイムスタンプ
を返す API は存在しない**。resource-load 単位のフックも無い。

一方、標準の `PerformanceNavigationTiming` (`performance.
getEntriesByType('navigation')[0]`) は WebKitGTK 2.52.6 で実際に動作する
ことを確認した — 本 Issue の作業ディレクトリ外、`/tmp` スクラッチ上に
`wry`/`tao` (VeloX と同じ 0.56.1/0.37.0) だけに依存する最小 PoC バイナリ
を作り、`scripts/bench/pages/minimal.html` を `http://127.0.0.1:8731/`
経由で読み込んで `window.ipc.postMessage` 経由で結果を回収した:

```
NAV_TIMING_RESULT: {"entryType":"navigation","domainLookupStart":1,
"domainLookupEnd":1,"connectStart":1,"connectEnd":1,
"secureConnectionStart":0,"requestStart":1,"responseStart":2,
"responseEnd":14,"fetchStart":1,"startTime":0,"protocol":"http/1.0"}
```

`domainLookupStart`/`domainLookupEnd`/`connectStart`/`connectEnd` の
フィールド自体は存在し、値も返ってくる (ここではすべて 1ms 付近 — ループ
バック接続に実質的な DNS/TCP コストが無いため)。`secureConnectionStart`
は 0 (HTTP なので TLS 無し)。**同じ PoC を `file://` で開くと、`load`
イベント後の `window.ipc.postMessage` 自体が届かなかった** (タイムアウト
2件、原因未特定 — WebKitGTK が `file://` オリジンで IPC ブリッジを
制限している可能性があるが、深追いはしていない)。

結論:

1. **wry のネイティブ API (Rust 側) に DNS/接続/TLS の hook は無い** —
   ソースで確認済み、3バックエンドとも同様。
2. **エンジンの JS 標準 API (`PerformanceNavigationTiming`) 経由でなら
   理論上は取得できる** — WebKitGTK での動作を実機で確認した。ただし
   これは `evaluate_script` を挟んだ IPC 往復が必要な追加コストであり、
   かつ取得できる値はエンジン (JS エンジン + ネットワークスタック) が
   計測したものであって VeloX 側の処理ではない — Epic #57 ルール3の
   ブラックボックス原則に照らせば「VeloX が見える窓」ではあっても
   「VeloX が制御できる区間」ではない。
3. **この検証環境では意味のある DNS/TLS 数値は取れない。** 本 Issue の
   制約 (外向きネットワークはプロキシ経由に制限、計測は `file://` か
   ローカル HTTP サーバに限定) の下では、実際の DNS 解決や TLS ハンド
   シェイクを伴うページを読み込めない。ループバック接続では
   `domainLookupStart`/`End` や `connectStart`/`End` の差がほぼ 0 に
   潰れ、`secureConnectionStart` も常に 0 になる。実際の DNS/TLS コスト
   を見るには外部 HTTPS サイトへの到達性が要る (本 Issue のスコープ外)。
4. **したがって本 Issue では `PerformanceNavigationTiming` を計測に
   組み込むコードは追加していない。** 追加しても (a) この環境では
   検証できない、(b) 得られる数値がエンジン管轄でありEpic #57 の枠内で
   VeloX 側から縮められる区間ではない、の2点から、ベンチマークなしの
   計装追加は Epic #57 ルール1に反すると判断した。PoC は再現用に
   `docs/performance-targets.md` §20.4 にスクリプトの要旨を残す
   (PoC 自体は作業ディレクトリの外、`/tmp` スクラッチに置いたため
   リポジトリには含まれていない)。

### 「unnecessary UI/IPC work during navigation」の再確認 — #66 の結論を `navigation` シナリオで裏付け、新しい削減は見つからなかった

`NavigationStarted`/`LoadStarted` が同じ match アーム
(`sync_tab_strip` 等を呼ぶ) を共有していることに気付き、「1回のナビ
ゲーションで `set_tabs` が実は #66 が数えた以上に多く送られているの
では」という仮説を立てたが、`navigation` シナリオ (3回ナビゲーション +
起動時ロード = 4ロード) で `velox-bench ipc-summary` を実測したところ
`set_tabs` は 14 件 (4ロードあたり 3.5 件、`duration_ms` 中央値
0.000ms・p95 1.175ms) — #66 が既に報告していた比率 (「120 件 / 約 34
回のタブ影響操作」≈3.5) とオーダーが一致し、コストも sub-millisecond
のまま。**新しい無駄は見つからなかった** — #66 の「タブストリップの
複数回送信は意図的でコストは無視できる」という結論を、ページロード
経路に特化した本 Issue でも裏付けただけに終わった。`tab.
on_navigation_started(&url)` が `NavigationStarted`/`LoadStarted` の
両方で (同じ内容を) 2回呼ばれる点は気付いたが (`app.rs` の共有アーム)、
`String` の再代入程度のコストで、IPC 計測が sub-millisecond と示して
いる以上ベンチマーク上の実害が無く、Epic #57 ルール1に従い変更しな
かった。

### `preload`/`preconnect` と cache 戦略 — VeloX 側から動かせるレバーが無い

wry 0.56.1 のソースを確認した限り、`WebViewBuilder` に preconnect/
prefetch/DNS prefetch 相当の API は存在しない
(`webkitgtk/mod.rs`: `settings.set_enable_page_cache(true)` を無条件に
呼ぶのみで、キャッシュサイズ/戦略を変更する builder メソッドも無い)。
ページ自身が `<link rel="preconnect">` 等を書けばエンジンがそれを解釈
するが、それはページ側の関与であって VeloX (ブラウザ chrome 側) の
コードが増減させる話ではない。オムニボックス入力から先読み的に
`preconnect` する、といった「VeloX 独自の速度対策」も検討したが、
(a) wry にはそもそも「特定オリジンへ接続だけ張っておく」API が無く、
(b) 実装するなら別プロセス/別ソケットで疑似的にウォームアップ接続を
張るような迂回策になり検証コストが高い、(c) この環境では実 DNS/TLS
すら検証できないため効果を測る手段が無い、の3点から見送った。
**「VeloX 側で制御可能な cache/preconnect レバーは (wry 0.56.1 の API
範囲では) 存在しない」ということ自体が本 Issue の調査結果である。**

### OS/WebView 差分の記録

`PageLoadEvent::Started`/`Finished` の意味づけ (`Started` = commit 後、
`Finished` = ロード完了) は wry のソース上 **3 バックエンドで同一**
であることを確認した (上記)。したがって `dispatch`/`engine` という
分解の**構造**は Windows (WebView2) / macOS (WKWebView) でも同じ意味を
持つ。ただし **実際のミリ秒の数値はすべて Linux/WebKitGTK 2.52.6 +
Xvfb (GPU なし) でのみ計測した** (`docs/performance-targets.md` §1)。
Windows が最優先対応 OS である (CLAUDE.md) にもかかわらず本 Issue では
Windows 実機での計測を行っていない — この環境に Windows 実機/WebView2
が無いため。Windows での再計測は Revisit condition に残す。

### なぜ「最適化」と呼べる変更を一切加えなかったか

#59 (D43)・#66 (D81) と同じ形の結論になった: **計測を1段階細かくした
結果、支配的なコストが VeloX 自身のコードではなくエンジン/ネットワーク
側だと分かった。** `dispatch` バケットの大半はエンジンの接続・リクエ
スト待ちであり (上記)、その中で唯一 VeloX 自身が支配できる部分 (IPC
ディスパッチ) は #66 の時点で既に sub-millisecond と実測済みで、今回
`navigation` シナリオで再確認してもコストは変わっていない。`engine`
バケットは定義上ブラックボックス。preconnect/cache チューニングは
wry に API が無い。**したがって「VeloX 側で安全かつ実測に裏付けられた
形で縮められるページロードのコストは、本 Issue の調査時点では見つから
なかった」というのが、本 Issue の正直な結論である。**

### Revisit condition

(1) Windows (WebView2) 実機での `page_load_engine_ms`/
`page_load_dispatch_ms` 計測 — この節の数値はすべて Linux。(2) 実際の
DNS/TLS を伴う外部サイトへの到達性がある環境が用意できれば、
`PerformanceNavigationTiming` を使った DNS/TLS 実測に再挑戦する価値が
ある (ただし Epic #57 ルール3により「エンジン管轄」という結論は変わら
ない可能性が高い)。(3) `file://` ページで `PerformanceNavigationTiming`
の PoC が `window.ipc.postMessage` を返さなかった件は原因未特定のまま
残した — VeloX 本体のコードパスではない (PoC 独自の問題の可能性がある)
ため優先度は低いが、`file://` の IPC ブリッジに何か制約があるなら
別 Issue の調査対象になりうる。(4) `tab.on_navigation_started` が
`NavigationStarted`/`LoadStarted` の両方で呼ばれる件 (重複呼び出し) は
実害なしと判断して変更していないが、将来 `Tab` の状態更新が重くなる
場合はここも見直し対象になる。
## D88: Windows で性能を実測できるようにする (#136) — `sample_process_tree_rss` に Toolhelp32/PSAPI 実装を追加し、PSS 相当は「実装しない」と結論。`perf-windows.yml` (`workflow_dispatch` 限定) を追加

**Scope**: Issue #136。#57 (Phase 3 Epic) の絶対ルール5「OS ごとに結果を分ける」を
守るには Windows 側の実測手段が要るが、`browser::metrics::sample_process_tree_rss`
は Windows で `RssError::Unsupported` を返すだけで RSS すら取得できていなかった
(D42 が PSS を追加した時点でも Windows 側は「Windows has neither」のまま)。この
Issue は (1) Windows で RSS/CPU を取得できるようにする実装、(2) PSS 相当の取得
可否を調査して結論を出すこと、(3) `windows-latest` 上で `velox-bench` を手動実行
できる workflow、の 3 つを扱う。

**この環境の決定的な制約**: 作業は Linux コンテナ上で行っており、Windows 実機は
無い。検証手段は `cargo check --target x86_64-pc-windows-msvc --all-targets`
(D61 が明記するとおりリンクを伴わない型チェックのみ) と、OS 非依存な純粋ロジック
の `cargo test` だけ。**Windows 上で実際に RSS/CPU が正しい値を返すことは、この
セッションでは一切確認できていない。** `perf-windows.yml` を実際に CI (windows
-latest ランナー) 上で走らせて検証するのは、この PR がマージ経路に乗ってから
(親セッション以降) になる。

### RSS/CPU の実装方針

**API**: `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` + `Process32First/NextW`
でプロセスツリー (PID/PPID) を走査する — Linux 版が `/proc` を読むのと同じ役割。
各プロセスの RSS は `GetProcessMemoryInfo` の `WorkingSetSize`、CPU 時間は
`GetProcessTimes` の kernel+user `FILETIME` から取る。いずれも `OpenProcess` で
得たハンドルが要る。

**候補の比較**: Issue 本文が挙げた 3 候補のうち `CreateToolhelp32Snapshot` は
プロセスツリー走査に必須 (これ以外にプロセス一覧+親子関係を取る標準的な手段が無
い)。`GetProcessMemoryInfo` の `WorkingSetSize` は RSS 相当として最も素直で公式
に文書化された値であり、`QueryWorkingSetEx` を自前でページ単位に集計するより
遥かに単純・低リスクなので RSS はこちらを採用した。

**依存クレート**: 新規追加なし。本リポジトリは D59/D76 で既に
`[target.'cfg(windows)'.dependencies] windows = "0.61"` に依存しているため (COM
の `ICoreWebView2`/Shell ダイアログ用)、`windows-sys` を新たに足すのではなく、
同じ `windows` クレートのフィーチャーフラグを 4 つ有効化するだけで済ませた
(`Win32_Foundation` / `Win32_System_Diagnostics_ToolHelp` /
`Win32_System_ProcessStatus` / `Win32_System_Threading`)。いずれも純粋な Win32
API で COM/WinRT の生成を伴わないため、既存フィーチャーとの相互作用のリスクも
無い。CLAUDE.md「依存クレートは必要最小限に保つ」に沿い、クレート数を増やさず
既存依存の適用範囲を広げる形を選んだ。

**`unsafe` の使用**: `src/browser/metrics.rs` の `#[cfg(target_os = "windows")]
mod imp` に 6 箇所 (`CreateToolhelp32Snapshot`/`Process32FirstW`/
`Process32NextW`/`OpenProcess`/`GetProcessMemoryInfo`/`GetProcessTimes` の各
FFI 呼び出し、および `Drop` 内の `CloseHandle`)。すべて `windows` クレートが
`unsafe fn` として公開している薄い FFI ラッパーの呼び出しで、各箇所に「何を保証
しているか」を `// SAFETY:` コメントで明記した — 具体的には (a) 呼び出し先に渡す
バッファはすべてスタック上に正しいサイズ・`dwSize`/`cb` で確保されていること、
(b) ハンドルは呼び出し時点でまだ有効 (`OwnedHandle` という RAII ガードを導入し、
`CreateToolhelp32Snapshot`/`OpenProcess` が返すハンドルを即座にラップして
`Drop` で必ず 1 回だけ `CloseHandle` する — 早期 `return`/`?` を含むどの経路でも
リークしない)。`OpenProcess` は `PROCESS_QUERY_LIMITED_INFORMATION |
PROCESS_VM_READ` のみを要求し、`PROCESS_ALL_ACCESS` は使わない (最小権限)。
VeloX は通常権限で動く前提であり、より高い権限のプロセス (システムプロセス等)
を `OpenProcess` できない場合はエラーではなく「そのプロセスの RSS/CPU を単に
含めない」扱いとした — Linux 側の `/proc/<pid>/status` が読めないプロセスを
スキップする既存方針とそろえている。

**Windows 版の失敗時挙動**: プロセスの `OpenProcess`/`GetProcessMemoryInfo`/
`GetProcessTimes` いずれかが失敗しても、そのプロセスは PID/PPID のみでツリーに
残り (RSS 0、CPU `None`)、サンプル全体は失敗しない。`CreateToolhelp32Snapshot`
自体が失敗した場合のみ `RssError::Io` を返す (Linux 版が `/proc` 自体を開けない
場合に倣った)。

### PSS 相当の取得可否 — 調査した上で「実装しない」と結論

Issue が挙げた候補は「`QueryWorkingSetEx` でページごとの `Shared`/`ShareCount` を
取得し、共有ページを共有プロセス数で割って合算する」という自前計算。**これを
検討した上で、今回は実装しないことにした。** 理由:

1. **正確な PSS には「対象プロセスだけでなく、その共有ページを持つ全プロセスの
   ワーキングセット」を横断的に見る必要がある。** `QueryWorkingSetEx` の
   `VM_COUNTERS_EX`/`PSAPI_WORKING_SET_EX_INFORMATION` は「このプロセスの
   ワーキングセット中のこのページが何個のプロセスで共有されているか
   (`ShareCount`)」までは返すが、Linux の `smaps_rollup`/`Pss:` のようにカーネル
   側で計算済みの値ではない — `ShareCount` を使って `1/ShareCount` を足し上げる
   近似は Issue 本文も「要検証」と明記しているとおり、Windows のページ共有モデル
   (プロトタイプ PTE、AWE、メモリマップドファイル等) に対してどこまで正確かが
   自明ではない。
2. **実機で検証する手段がこのセッションには無い。** 型チェック
   (`cargo check --target x86_64-pc-windows-msvc`) は API 呼び出しのシグネチャが
   合っていることしか保証せず、`QueryWorkingSetEx` が実際に返す値の妥当性は
   Windows 実機でしか確認できない。検証できない計算式をそのまま実装として残す
   ことは、CLAUDE.md が求める「検証できていないことを検証できていないと明記
   する」誠実さの要件と相容れない — 「動くはず」の実装を残すより、「実装しない」
   という判断とその理由を明記する方が、後で実際に Windows 上で必要になったとき
   に再検討しやすい。
3. **RSS が既に取れている。** PSS が無くても `total_rss_bytes` は
   `GetProcessMemoryInfo` から確実に取得できるため (Issue も「PSS 相当は無理に
   実装しなくて構わない」と明記)、Windows でも「何も測れない」状態からは脱却
   できる。

結果として、Windows 版の `RssSample::total_pss_bytes` は非 Linux Unix (macOS/
*BSD の `ps` フォールバック) と同じく常に `None`。将来 Windows 上でメモリ最適化
の効果を細かく見る必要が生じ、RSS だけでは Chromium/Edge との比較 (D41 が示した
「RSS 合計はプロセス数の多いブラウザを不当に不利にする」問題) が避けられなく
なった時点で、`QueryWorkingSetEx` アプローチを実機で検証しながら再挑戦するのが
妥当。

> ⚠️ **将来 Windows で PSS 相当を実装したとしても、Linux の PSS
> (`smaps_rollup` の `Pss:`) と直接比較してはならない。** 算出方法が全く異なる
> ため、OS をまたいだ数値比較は成立しない。Windows 上で Chromium/Edge と横並び
> に測る用途に限られる。`docs/performance-targets.md` にも同じ注意を記載した。

### `perf-windows.yml` (`workflow_dispatch` 限定)

`.github/workflows/perf-windows.yml` を新規作成。`release-windows.yml`
(`workflow_dispatch` + Windows ビルドの先例) に倣い、`on:` は
`workflow_dispatch` (シナリオ/試行回数/URL を入力パラメータ化) に加えて、
**この workflow 自身を変更する PR に限り** `pull_request: paths:
.github/workflows/perf-windows.yml` を付けた — `workflow_dispatch` は main に
マージされるまで Actions タブに現れないため、workflow 自身の変更を検証する唯一
の手段としている (`release-windows.yml` が採用済みの同じパターン)。PR ごとの
自動実行や性能回帰ゲートとしては使わない (Linux の `perf-gate.yml` が既に担保
しており、Issue のスコープ外)。

`cargo build --release` の後、既定では `scripts/bench/pages/` の固定ページを
loopback (`python -m http.server`) で配信し `--url` に渡す (Linux の
`perf-gate.yml`/`docs/benchmarking.md` と同じ「ネットワーク非依存の固定ページ
で測る」方針) — `workflow_dispatch` の `url` 入力を明示的に指定すればそちらを
使う。結果 JSON は Actions Artifact として保存し (`velox-bench aggregate`/
`compare`/`gate` に後からかけられる形式そのまま)、実行環境の情報 (OS ビルド
番号・CPU・メモリ・WebView2 Runtime バージョン) を `Get-CimInstance`/レジストリ
照会でログと Job Summary に残す (`docs/performance-targets.md` §1 の「測定環境を
固定して記録する」要件)。

**最大のリスク (`windows-latest` で GUI/WebView2 ウィンドウが起動できるか) は
未解決のまま**。Linux は Xvfb で仮想ディスプレイを用意しているが、Windows
ランナーには同種の仕組みが無く、GitHub ホスト型 Windows ランナーが GUI プロセス
を起動できる対話セッションを持っているかはこのセッションでは検証できない。
workflow には `velox.exe` を直接起動してプロセス一覧・perf ログの有無を確認する
診断ステップ (`continue-on-error: true`、Linux 側 `perf-gate.yml` の
"Diagnose VeloX under Xvfb" ステップと同じ形) を含めたが、**これが実際に機能する
かどうかはこの PR が CI 上で走って初めて分かる。** 起動できなかった場合は
「何を試して、どう失敗したか」を記録し、セルフホストランナー等の方式再検討が
必要という結論を残すのが正しい進め方であり、この時点で無理に通そうとしていない
(#59 が同じ形で結論づけたのと同様)。

> **追記 (2026-09-07、Issue #180)**: `perf-windows.yml` の初回実行
> (run [`34127310212`](https://github.com/noan98/VeloX/actions/runs/34127310212)、
> job `101758900568`) で上記の診断ステップが実際に走り、**起動できた**ことが
> 確定した — `msedgewebview2.exe` 7 プロセス + `velox.exe` 1 プロセスが起動し、
> `velox.exe` の `MainWindowTitle` が `VeloX` になっていることを確認、perf ログ
> も実際に 34 件書けた。続く `velox-bench run --scenario cold_startup --trials
> 10` も 10/10 試行が完走している。したがって「最大のリスクは未解決のまま」
> という上記の記述は解消済み。詳細と実測値は
> `docs/performance-targets.md` §21 を参照。
>
> この初回実行は `workflow_dispatch` ではなく、`perf-windows.yml` を追加した
> PR #179 に対する `pull_request` トリガー (本 D88 が「`workflow_dispatch` は
> main にマージされるまで Actions タブに現れない」ために付けた `paths` フィルタ
> 経由の経路) で走った。**この検証経路を付けておいた判断が、まさに想定どおり
> 機能した**ことになる — これが無ければ workflow の初回検証は「main にマージ
> してから手動実行してみる」しかなく、起動不可だった場合の手戻りが大きかった。

**このセッションで確認できたこと / できていないこと**:
- 確認できた: `cargo fmt --check` / `cargo clippy --all-targets -D warnings` /
  `xvfb-run ... dbus-run-session -- cargo test` (972 件、Windows 実装追加前の
  969 件 + `filetime_ticks_to_seconds` の単体テスト 3 件) / `cargo build` /
  `cargo check --target x86_64-pc-windows-msvc --all-targets` はすべて green。
  Linux 側の `sample_process_tree_rss`/既存テストの挙動は変更していない。
- 確認できていない: Windows 実機/CI 上での実際の RSS/CPU 値の妥当性、
  `windows-latest` ランナーでの VeloX (WebView2) ウィンドウ起動可否、
  `velox-bench run` の完走、結果 JSON の実際の中身。`docs/performance-targets.md`
  §21 には Windows の数値をまだ書けないため、CI 実行後に埋めるプレースホルダの
  みを記載した。
- **追記 (2026-09-07、Issue #180)**: 上記のうち「`windows-latest` ランナーでの
  VeloX (WebView2) ウィンドウ起動可否」「`velox-bench run` の完走」「結果 JSON
  の実際の中身」は、Issue #136 マージ後の初回ワークフロー実行 (run
  `34127310212`) で確認でき、`docs/performance-targets.md` §21 に記録した。
  ただし「Windows 実機上での RSS/CPU 値の妥当性」は依然として未確認のまま —
  今回確認できたのは GitHub-hosted (共有・仮想化、CPU 2 コア) ランナー上の
  数値であり、Windows 実機の実力値ではない。

## D89: Serialization / Allocation 最適化 (#68) — `escape_js_line_terminators` の常時フルコピーを削除。`write_json` の内訳は実測の結果 `fs::write` が支配的でシリアライズは対象外と判明

Issue #68 (Epic #57 Phase 3、依存元 #66/#67 の後続)。「IPC やブラウザ状態処理の
serialization / allocation / clone を分析し、不要なコストを削減する」という
課題に対して、#66 (D81) / #67 (D86) と同じ手順 (Profile → Baseline →
Optimize → Benchmark → Regression Check) を踏んだ。数値・再現手順は
`docs/performance-targets.md` §22 を参照。**この節の数値はすべて Linux
(WebKitGTK 2.52.6 / Xvfb、GPU なし) での計測であり、Windows (WebView2) の
実力値ではない** (Epic #57 ルール 5)。

### 最初に検討した事項 — D86 の Revisit condition (2): `write_json` の内訳

D86 は「`state_write` の `duration` は `write_json` 全体 (`create_dir_all` +
シリアライズ + 書き込み) の合算で内訳が分離されていない」ことを本 Issue が
最初に検討すべき事項として残していた。まずここから着手した。

`persistence::write_json` が呼ぶ 3 ステップそれぞれを、20 タブ相当の
`SessionSnapshot` (JSON 4,169 bytes) を対象に、既に存在するディレクトリへの
書き込み (`persist_session`が実運用で辿る定常状態) という条件でマイクロ
ベンチマークした (1 プロセス内で 20,000 回ずつ、`std::hint::black_box`で
最適化による消失を防止、3 回実行して再現性を確認 — 具体的な計測コードと
手順は §22.1):

| ステップ | 1 回あたり (3 回の実行、中央値) |
| --- | ---: |
| `fs::create_dir_all`(既存ディレクトリ、stat 相当) | 約 1.2〜1.4µs |
| `serde_json::to_string_pretty` | 約 3.3〜3.5µs |
| `fs::write`(実ディスク書き込み syscall) | 約 90〜97µs |

**`fs::write`の実ディスク書き込みが全体の 90%以上を占め、シリアライズの
25〜30 倍のコストがある。** 3 つの合計 (約 95〜102µs ≈ 0.1ms) は
`docs/performance-targets.md` §19 が報告した `state_write` の実測中央値
(0.1ms) とほぼ一致しており、このマイクロベンチマークが実際の呼び出しコストを
正しく再現できていることの裏付けにもなっている。

**結論: `write_json`のシリアライズ部分を最適化しても、`state_write`全体の
コストにはほとんど効かない。** `serde_json::to_writer`でバッファ経由の
`String`確保を避ける案も検討したが、節約できるのはこの 3.3〜3.5µs の一部
(実際には`to_writer`は複数回の小さい`write()`呼び出しに分割されうるため、
`BufWriter`でラップしない限り`fs::write`1 回より遅くなるリスクすらある)
であり、全体の 90µs 超を占める syscall 本体には触れない。Epic #57 ルール 1
に照らし、**`persistence.rs`側のシリアライズ経路には手を入れなかった** —
D86 が残した「内訳を分離する必要があるか」という問いへの答えは「分離しても
シリアライズは支配的要因ではないので、専用の計測を追加する価値も薄い」
だった。専用の`serialize_duration`フィールドを`PerfRecord::StateWrite`に
追加する案も検討したが、上記の理由でシリアライズが最適化対象にならない
以上、恒久的な計測用フィールドを増やすメリットは実測上ない — 本 Issue の
その場限りのマイクロベンチマークで十分と判断した (§22.1 に再現手順を残す
ことで、将来この判断を再検証したくなった場合の出発点にはなる)。

### 実測して見つかった唯一の削減対象: `escape_js_line_terminators`の常時フルコピー

`ui::toolbar::escape_js_line_terminators`(Issue #43/D62、U+2028/U+2029 を
JS 文字列リテラル内で無害化する関数) は、`set_tabs_script`/`set_url_script`/
`set_candidates_script`/`entries_to_json`(`set_history`/`set_downloads`の
JSON 化)/`value_to_json`(`set_bookmarks`)/`find_query_literal`/
`context_menu_script`など、**Rust → JS へ渡るほぼ全ての `eval_toolbar`呼び出し
経路が最後に通る共通関数**だった。旧実装は `&str`を受け取り、対象文字が
1 つも無い (実運用の大半を占める) ケースでも`json.to_owned()`で**呼び出し元
が既に所有している`String`をまるごとコピーしていた** — `serde_json::
to_string`が確保した`String`を、中身を一切変えないまま複製し直すだけの
アロケーション + memcpy。呼び出し元 6 箇所 (`toolbar.rs`) + 2 箇所
(`window.rs`の`find_query_literal`/context menu) はいずれも呼び出し直前に
`String`を新規構築しており、以降その値を使っていない — つまり所有権を
そのまま渡せば済む場所だった。

**この関数の性質上、`PerfRecord::Ipc`では変化を測れない**: `ui::window::
BrowserWindow::eval_toolbar`が`Instant::now()`を読むのは、呼び出し元が
`toolbar::set_tabs_script(tabs)`(このコピーを含む)を**呼び終えたあと**の
`&str`を受け取ってから — #66 (D81) の`Ipc`の`duration`が測るのは
`evaluate_script`という FFI 呼び出し 1 本だけで、スクリプト文字列の組み立て
コストはそこに一切含まれない。したがって本 Issue で見つけたこのコストは
既存の IPC 計測の外側にあり、確認するには別のマイクロベンチマークが要る
(この事実自体、既存計測の限界として記録しておく価値がある — 将来
`eval_toolbar`呼び出し元でのシリアライズコストを見たくなったら、
`Instant::now()`をスクリプト組み立ての前に移す必要がある)。

**修正**: `escape_js_line_terminators(json: &str) -> String`を
`escape_js_line_terminators(json: String) -> String`に変更し、対象文字が
無い高速経路では所有権をそのまま返すだけ (コピー無し) にした。全呼び出し元
は変更前から既にコピーを作る直前で`String`を所有していたため、`&json`を
渡していた箇所を`json`に変えるだけで済み、シグネチャ以外の呼び出し側の
ロジックは変わっていない。

**before/after (マイクロベンチマーク、`set_tabs_script`、20,000 回のうち
先頭 1,000 回をウォームアップとして除外、`black_box`で最適化消失を防止、
それぞれ 3 回実行、詳細な手順は §22.2)**:

| tabs | before (1 回あたり、3 回の範囲) | after (1 回あたり、3 回の範囲) |
| --- | --- | --- |
| 3   | 943〜1,012ns | 856〜913ns |
| 20  | 4.721〜4.791µs | 4.286〜4.599µs |
| 50  | 10.806〜11.747µs | 10.344〜10.416µs |

3 サイズすべてで **after の 3 回の実行値は before の 3 回の実行値をすべて
下回った** (範囲が重ならない) — 単発のノイズではなく再現する差であること
の確認。20 タブでの改善幅はおおむね 5〜10%。絶対値としては 1 回あたり
数百 ns 相当と小さいが、これは実際に`serde_json::to_string`が確保した
バッファをまるごと複製していたコストがそのまま消えた分であり、`Vec`/
`String`の不要な clone を削るという Issue #68 の対象そのものである。

### 検討した他の対象 — 実測の結果、対象外と判断したもの

- **`ui::toolbar::TabSummary`の所有フィールド (`url`/`title`/`favicon`)
  を`&'a str`の借用に変える案**: `sync_tab_strip`(`app.rs`) は
  `Tab::current_url()`/`title()`/`favicon()`(いずれも借用を返す) から
  `TabSummary`を組み立てる際に`.to_owned()`/`.clone()`しており、タブ数分の
  `String`確保が発生する。`BookmarkFolderView<'a>`(`ui::toolbar.rs`) が
  既に同種の借用パターンを採用しており、技術的には可能。しかし
  `set_tabs_script`全体 (JSON 化含む) が 20 タブで 1 回あたり 4.3〜4.8µs
  (本 Issue の計測) であり、`TabSummary`の構築自体はこのうちさらに小さい
  部分でしかない。#66 (D81) が実測した`set_tabs`の`evaluate_script`呼び出し
  コスト (中央値 0.000ms、最悪 3.5ms) と比べても 3 桁小さい。`TabSummary`
  にライフタイムパラメータを持ち込むと`set_tabs_script`のシグネチャ・
  呼び出し側の型注釈が連鎖的に変わり可読性が下がる一方、削減できる時間は
  20 タブのセッション全体 (120 回呼び出し) を通算しても 1ms に満たないと
  見積もられる — Epic #57 ルール 1 (実測に基づく判断) とルール
  「可読性を大きく損なわない」の両方に照らし、見送った。
- **`toolbar::command_name`(Issue #66) による IPC メッセージの二重パース**:
  JS → Rust の`UserEvent::ToolbarMessage`は`command_name`(タグだけを見る
  簡易パース) と`parse_command`(完全な型付きパース) を両方呼んでおり、一見
  同じ JSON を 2 回パースしているように見える。しかし呼び出し箇所
  (`app::record_perf_event`) は`config.perf_metrics`が有効なとき
  (`VELOX_PERF_METRICS=1`) にしか実行されない診断専用コードパスであり、
  かつ`command_name`は`parse_command`が失敗するメッセージにもラベルを
  残すための意図的な設計 (コード自身のドキュメントコメントに明記済み) —
  実運用のブラウジングでは一切実行されない。本 Issue の対象 (通常運用の
  hot path) には当たらないため変更しなかった。
- **`app::persist_session`の`state.data_dir.clone()`のタイミング**:
  現在の実装はプライバシー判定の直後、スナップショット比較 (直前と同じ
  内容ならディスク書き込み自体をスキップする #67/D86 の分岐) より前に
  `PathBuf`をクローンしている。スキップされる呼び出しでもこのクローンは
  必ず発生する。並べ替えれば無駄なクローンを避けられるが、`PathBuf`1 個
  の clone は数十バイトのヒープ確保 1 回 (見積もりで概ね数十〜100ns 未満)
  であり、本 Issue で計測した他のどの数値 (µs〜ms オーダー) と比べても
  2〜3 桁小さい。並べ替えは`let Some(dir) = ... else { return }`という
  早期リターンの並びを崩し、なぜこの順序なのかを追加のコメントで説明する
  必要が生じる分だけ可読性コストが生じる一方、得られる時間は測定誤差にすら
  埋もれる規模と判断し、変更しなかった。
- **lock scope**: D86 が既に「本アーキテクチャ (シングルスレッド state
  machine) にはロック競合が構造的に発生しない」と結論しており (§19.3)、
  本 Issue で新たに見つかった対象は無い。

### Regression check

`velox-bench gate --scenario tab_create_20`(baseline=本 Issue 着手前の
コミット `e3d1986`、candidate=本 Issue の変更後、各 8 試行 × 2 回、
`--warn-pct 20 --fail-pct 60`) は総合判定 **OK**
(`page_load_ms`/`page_load_engine_ms`/`page_load_dispatch_ms`/
`tab_create_ms`のいずれも baseline 比 -6.2%〜+2.3%、warn 閾値 20%を大きく
下回る)。`cargo test`は変更後も全件成功 (982 ユニットテスト + 11 統合
テスト、`xvfb-run` + `dbus-run-session`)。`cargo fmt --check`/`cargo clippy
--all-targets -- -D warnings`もクリーン。

### 後続 Issue が使えるもの

- `escape_js_line_terminators(json: String) -> String`という「所有権を
  そのまま返せる高速経路ではコピーしない」パターン — 将来 Rust → JS の
  新しいペイロードを追加する際、同じ関数を再利用するだけで恩恵を受けられる
  (コピーを避けるために呼び出し元を書き換える必要はない)。
- `persistence::write_json`の内訳 (mkdir/serialize/write) のマイクロ
  ベンチマーク手順 (§22.1) — 将来ディスク書き込み方式そのもの (非同期化・
  バッチ化など) を検討する際の基礎データとして再利用できる。

### Revisit condition

(1) `TabSummary`を借用ベースに変える判断は、1 ウィンドウのタブ数が
現在の上限 (20〜50) から大きく増える設計変更が検討され始めたら再検証する
こと (D86 の tab/window lookup の Revisit condition (1) と同じ条件)。
(2) `persistence::write_json`が非同期化・バッチ化された場合、本 Issue が
測った「`fs::write`が支配的」という前提ごと崩れるため、内訳の再計測が
必要になる。(3) 本節の数値はすべて Linux/WebKitGTK — Windows (WebView2) の
NTFS 上でのディスク書き込みコスト・アロケータの挙動は異なりうるため未計測
(Epic #57 ルール 5)。

## D90: 自動タブ休止をデフォルト ON にする (Issue #184) — メモリ予算シグナルのみ、700 MiB。D9 の opt-in 方針と D56 Revisit condition (3) の決着

**対象**: Issue #184。D56 (Adaptive Tab Suspension のポリシー本体) の Revisit
condition (3) 「既定を有効にするかどうか」への決着で、D9 が定めた「ユーザが
頼んでいない休止で状態を失わせない」という opt-in デフォルト方針を覆す。**D9
/ D56 はどちらも書き換えず、本節から参照する。**

### 決定

`browser::suspension::SuspensionPolicy::default()` を次のとおり変更した:

| シグナル | 旧既定 (D9/D56) | 新既定 (D90) |
| --- | --- | ---: |
| `memory_budget_bytes` | 無効 (`None`) | **有効 — 700 MiB** (`DEFAULT_MEMORY_BUDGET_BYTES`) |
| `max_live_tabs` | 無効 (`None`) | 無効 (`None`) のまま |
| `idle_after` | 無効 (`None`) | 無効 (`None`) のまま |

700 MiB は D56 が同一セッションで実測済みの `VELOX_MEMORY_BUDGET_MB=700` の
値そのもの (`docs/performance-targets.md` §12: 1/5/10/20 タブで 407.3 /
659.6 / 476.2 / 615.1 MiB、いずれも予算内に収まる挙動を確認済み)。新しい値を
測り直したわけではなく、既に実測済みの値を「既定」に昇格させただけ — Epic
#57 ルール 1 (ベンチマーク無しに最適化しない) に沿っている。

`max_live_tabs` を既定にしない理由は D56 の「なぜ `max_live_tabs=4` を既定に
しないか」の議論そのもの (Issue #184 本文にも転記) — 休止の単位がプロセス
グループであるため上限を 1 超えただけで最大 4 タブ分の状態を失う一方、推奨値
は D56 Revisit condition (2) が「実サイトで決めるべき」としており、
`minimal.html` の計測値をそのまま既定に昇格させるのは Epic #57 ルール 4
(トレードオフの評価) に反する。`idle_after` は D9 の元々の理由 (メモリ圧の
悪い代理指標) が一度も再検証されておらず、本 Issue のスコープでもないため
据え置いた。

### なぜこれは D9 の懸念 (「頼んでいない休止で状態を失う」) と両立するか

D9 の懸念は「メモリに関係なく一律に休止が発生する」ことへの懸念だった
(旧: `idle_after` のみが既定候補で、3 タブを 1 日開いているだけの利用者も
巻き込みうる)。メモリ予算シグナルは性質が異なる: **タブが少ないうちは
プロセスツリーが 700 MiB を超えないため、`suspension::plan` は何も返さない**
(`browser::suspension::tests::default_policy_stays_inert_with_few_tabs_and_
no_over_budget_sample`)。本 Issue の実測 (下記) でも 1/5 タブでは休止が発生
していない。つまり新しい既定は「タブを多く開いてメモリを圧迫している利用者
だけ」に効き、通常利用の体験は旧既定と区別がつかない。

### 実装

1. **`SuspensionPolicy::default()`** (`src/browser/suspension.rs`) —
   `memory_budget_bytes: Some(DEFAULT_MEMORY_BUDGET_BYTES)` (700 MiB の新規
   定数)。既存の保護 (アクティブタブ・読み込み中・音声再生中) はこの変更で
   一切触っていない — `plan`/`Candidate::eligible` は無変更。
2. **より重要な副作用に先に気づく必要があった: 旧 `resolve_suspension` は
   `SuspensionPolicy::default()` を一切参照していなかった。** 実際に起動する
   バイナリが呼ぶのは常に `Config::from_env_and_args` であり、そこで
   `suspension` フィールドは `resolve_suspension(...)` の戻り値でこれまでも
   無条件に上書きされていた (`..defaults` の対象外)。旧 `resolve_suspension`
   は 4 つの env 値だけから毎回ポリシーを**ゼロから組み立てて**おり
   (`positive(raw).map(...)`)、`SuspensionPolicy::default()` の値は使って
   いなかった — 旧既定が「全シグナル無効」だったから、未設定時にたまたま
   無効相当の結果と一致していただけである。**つまり `SuspensionPolicy::
   default()` だけを 700 MiB に変えて `resolve_suspension` に手を入れなければ、
   実際に起動する `velox` バイナリはこの新既定を一切拾わず、旧来どおり
   全シグナル無効のまま動き続けていた** (`Config::default()` を直接呼ぶ
   一部のテストだけが新既定を見る、という食い違った状態になる)。これは
   単なる「無効化する手段が無い」以上の問題 — 新既定そのものが実際には
   効かないバグだった。
   `resolve_suspension` を「env 未設定なら `SuspensionPolicy::default()` に
   フォールバック、明示的な `0` は既定が有効でも常に無効化する」
   `overridable()` ヘルパー経由に書き換え、この両方を解決した:
   - `VELOX_MEMORY_BUDGET_MB` 未設定 / 空 / 数値でない → **既定
     (`SuspensionPolicy::default()`、700 MiB・有効) を継承する** (旧実装は
     ここで無条件に無効へフォールバックしていた)
   - `VELOX_MEMORY_BUDGET_MB=0` → **明示的に無効化** (既定が有効でも) —
     これが既定 ON に対する無効化の手段
   - それ以外の正の値 → その値で上書き
   - `VELOX_AUTO_SUSPEND_AFTER_MS` / `VELOX_MAX_LIVE_TABS` も同じ
     `overridable()` を通すが、既定が `None` のままなので挙動は変わらない
     (対称性のために揃えた)
3. **設定画面 (#30)** (`src/browser/settings.rs`) — `PerformanceSettings::
   default().memory_budget_mb` を `Some(700)` に変更し、
   `#[serde(default = "default_memory_budget_mb")]` を追加した。これにより:
   - 初回起動 (settings.json 無し) の設定画面は「メモリ予算: 700」を表示
     した状態で開く (`Config::to_settings()` が `Config::default()` から
     導出するため)。ユーザは既存の UI (`src/ui/toolbar.html` の「メモリ予算
     (MiB、空欄で無効)」欄) で数値を変更するか、空欄にして保存すれば無効化
     できる — **この経路は Issue #30 で既に実装済みで、今回 UI の変更は
     不要だった** (`optionalNumber()` が空欄を `null` として送る)。
   - `performance` オブジェクトごと欠けた古い settings.json (Issue #30 より
     前の形式) を読み込んだ場合も 700 MiB の新既定を継承する
     (`#[serde(default)]` が `PerformanceSettings::default()` 全体を使う)。
     一方、**`performance` オブジェクト自体は存在するが `memory_budget_mb`
     キーだけが欠けている場合**は `#[serde(default = "default_memory_budget_mb")]`
     によりやはり 700 MiB を補う。**`保存` を一度でも押したことがある
     settings.json は `memory_budget_mb` キーが常に明示的に書き出されて
     いる**ため (serde の `Serialize` は既定で全フィールドを書く)、その
     場合は保存時点の値 (旧バージョンなら `null` = 無効) がそのまま維持
     される — 既存ユーザの明示的な設定を新既定で上書きすることはない。
   - `Config::apply_settings`/`to_settings` の往復契約
     (`apply_settings_with_default_settings_leaves_the_default_on_memory_
     signal_enabled`) は「`Settings::default()` を適用しても既定を変えない」
     という不変条件を保つよう `PerformanceSettings::default()` を
     `SuspensionPolicy::default()` と揃えた。
4. **テスト**: `browser::suspension::tests::default_policy_is_fully_
   disabled` は前提が崩れるため**削除して緑にするのではなく**、
   `default_policy_enables_only_the_memory_budget_signal` (新しい既定の値
   そのものを検証)・`default_policy_stays_inert_with_few_tabs_and_no_over_
   budget_sample` (少タブでは無害)・`default_policy_actually_suspends_
   once_over_budget` (予算超過では実際に休止する) の 3 本に置き換えた。
   既存の per-signal テスト (`..policy()`) は `SuspensionPolicy::default()`
   に依存すると新既定 (メモリ有効) を意図せず引き込むため、明示的に
   全無効な `disabled_policy()` ヘルパーへ切り替えた
   (`..disabled_policy()`)。`config::resolve_suspension` 側のテストも
   同様に「未設定 = 既定」「明示的な 0 = 常に無効」の 2 系統に整理し直した。
   `browser::settings`/`config` 双方の `Settings::default()`/
   `Config::default()` 関連テストのアサーションも新しい既定値に合わせて
   更新した。加えて `tests/integration.rs` の `launch_and_wait_with` に
   `VELOX_MEMORY_BUDGET_MB=0` を既定の起動環境として追加した — 既定 ON に
   なったことで、これを入れないとこのファイルの**全ての**既存統合テスト
   (元々どれもメモリ休止を想定していない) が非決定的になる。この環境
   (Xvfb 上の WebKitGTK) では 1 タブだけでも PSS が約 400 MiB あり
   (`docs/memory-analysis.md` §11)、複数タブを開くテストは 700 MiB を
   実際に超えうるため、これは仮説ではなく実際に確認した (下記実測)。
   その上で、既定のメモリ予算が実際にプロセスツリー越しに動くことを
   確認する新規統合テスト
   (`memory_budget_signal_suspends_a_background_tab_end_to_end`、
   `VELOX_MEMORY_BUDGET_MB=1` で決定的に発火させる) を追加した — #63 の
   時点でもメモリシグナルを実バイナリ経由で検証する統合テストは無かった
   ギャップの解消でもある。

### 実測結果 (このセッション内、Linux / WebKitGTK / Xvfb コンテナ)

`docs/performance-targets.md` §23 に詳細を記録した。要点:

- **1/5/10/20 タブの PSS** (`tab_scaling.py`、3 試行の中央値、`minimal.html`、
  同一セッション内 before/after):

  | タブ数 | Chromium | before (旧既定=無効) | after (新既定=700 MiB、env 上書き無し) |
  | ---: | ---: | ---: | ---: |
  | 1  |  281.7 MiB |  400.2 MiB |  399.8 MiB (±0.0%) |
  | 5  |  321.9 MiB |  646.6 MiB |  646.5 MiB (±0.0%、予算内のため休止なし) |
  | 10 |  369.5 MiB |  979.7 MiB |  464.4 MiB (**-52.6%**) |
  | 20 |  468.2 MiB | 1655.7 MiB |  617.7 MiB (**-62.7%**) |

  絶対値は D56 の元セッション (409/653/990/1612 MiB) と数十 MiB 程度ずれて
  いるが (コンテナの実行時刻・負荷によるドリフト、D46 が言う「異なる
  セッションを比較してはならない」の対象)、**相対的な形は一致**しており
  D56 の知見を新しい既定でも再現した: 5 タブでは予算内のため休止が起きず
  before と同一、10/20 タブでは大きく下がる。Chromium 比は 10 タブ
  **+25.7%**、20 タブ **+31.9%** (T2 目標 +10% 以内には未達、D56 と同じ
  結論のまま — 本 Issue はこの目標に新たに近づけることを目的にしていない)。
- **軽量ケース (1〜3 タブ) のポーリングコスト** (`scripts/profile/
  cpu_usage.py`、idle 30 秒窓、`/proc/<pid>/stat` の utime+stime 差分を
  外部から計測。既定の `memory_check_interval` は当時 2 秒):

  | ケース | メモリ監視 既定 ON | `VELOX_MEMORY_BUDGET_MB=0` (OFF) |
  | --- | ---: | ---: |
  | 1 タブ | CPU 1.2% (0.35〜0.36 秒/30 秒、3 試行) | CPU 0.1% (0.04 秒/30 秒) |
  | 3 タブ | CPU 1.5% (0.44 秒/30 秒) | CPU 0.2% (0.05 秒/30 秒) |

  サンプラ自体 (`/proc` 全体を 2 秒ごとに 1 回歩く) に起因する差分はおよそ
  **1〜1.3 ポイント (1 コア換算)**で、厳密にはゼロではない。**PR #185 の
  時点ではこれを「実用上無視できる」と結論したが、この判断は Issue #187
  で誤りと判定し、以下のとおり覆した** — 1 タブのアイドル状態で CPU が
  OFF 比 12 倍というのは実際には無視すべきでない差であり、間隔を変える
  理由が無いという結論は時期尚早だった。詳細は本節末尾の
  「#187: `memory_check_interval` の既定見直し」を参照。
- **`velox-bench gate`** (`--warn-pct 20 --fail-pct 60`、baseline=旧既定
  バイナリ、candidate=新既定バイナリ ×2、各 8 試行): `cold_startup` /
  `tab_create` / `tab_switch` / `tab_create_20` はいずれも**総合判定 OK**
  (`tab_create_ms`/`tab_switch_ms`/`page_load_*` の変化率はいずれも数%〜
  ±10%程度で warn 閾値 20%を大きく下回る) — 少タブのシナリオは 700 MiB を
  超えないため、既定を変える前と実質的に同じものを測っている。
- **`tab_switch_20` は比較不能になった (重要な発見)**: 20 タブまで開く
  この手動シナリオでは、新既定のメモリ予算がベンチマーク実行中にバック
  グラウンドタブを実際に休止させてしまうため、`switch` コマンドの大半が
  `tab_switch` ではなく `tab_resume`(+`page_load`) として記録される。
  baseline の出力は `tab_switch_ms` のみ、candidate の出力は
  `tab_resume_ms`/`page_load_*` のみとなり、`velox-bench gate` は
  「比較可能なメトリクスがありません」として機械的に**総合判定 OK**を返す
  — これは「回帰が無い」ことの確認では **ない**。`VELOX_MEMORY_BUDGET_MB=0`
  を明示すれば `tab_switch_ms` は再び記録され、baseline とほぼ同じ値
  (0.80ms 中央値、両者一致) になることを確認した — 休止の無効化は完全に
  機能している。**影響範囲は限定的**: 自動化されている唯一の回帰ゲート
  (`.github/workflows/perf-gate.yml`) は `cold_startup` のみを対象にして
  おり、20 タブ級のシナリオは走らせていないため、CI の自動回帰検知が
  サイレントに機能を失っているわけではない。ただし今後 `tab_switch_20`/
  `tab_create_20` のような多タブシナリオを手動で再計測する際は、
  `VELOX_MEMORY_BUDGET_MB=0` を明示しない限り「switch のレイテンシ」を
  測っているつもりが実際には「resume のレイテンシ」を測っていることに
  なる点を、以後の Issue のために書き残す。

### Windows での意味の違い (最重要の検討事項)

D88 が確定させたとおり、`sample_process_tree_rss` は **Linux では PSS**
(`smaps_rollup` の `Pss:`)、**Windows では RSS のみ** (`GetProcessMemoryInfo`
の `WorkingSetSize`、PSS 相当は「実装しない」と結論済み)。
`app::spawn_memory_pressure_sampler` は `total_pss_bytes.unwrap_or(total_
rss_bytes)` で両者を同じ「700 MiB」という数値と比較する。

- **RSS は共有ページを保有プロセスの数だけ二重・多重に計上する** —
  D56 のメモリ信号のドキュメント自身が既に明記しているとおり (「PSS が
  使えない環境では RSS を代わりに使う。これは過大評価であり、PSS 用に
  調整した予算はそちらでは少し早く発動する」)。VeloX は toolbar 用と
  content 用に別々の `WebProcess` を持つ設計 (D3) であり、共有ライブラリ
  ページ (WebKit2/JavaScriptCore 相当の DLL 等) の重複計上は Linux での
  RSS 実測 (本 Issue の tab_scaling 結果: 同一構成で RSS は PSS の
  1.6〜1.8 倍、例えば 20 タブで RSS 940 MiB 対 PSS 618 MiB) からも推測
  できる規模感である。**同じ「700 MiB」という設定値でも、Windows では
  実際に解放されるべき固有メモリのより早い段階で休止が発動する可能性が
  高い。**
- **これは新しい問題ではなく、既存のドキュメント済みの制約が「既定 ON」
  になったことで初めて全 Windows ユーザに影響する、という話である。**
  #63 の時点でも `VELOX_MEMORY_BUDGET_MB` を明示的に設定した Windows
  ユーザは同じ影響を受けていたはずだが、既定が無効だったため実際に
  この経路を踏む Windows ユーザはほぼいなかった。本 Issue はこれを
  「全 Windows ユーザに既定で影響する」設定へ格上げする。
- **結論: 本 Issue では OS 別の既定値を導入しない。** 理由:
  1. **この環境には Windows 実機が無く**、「Windows で 700 MiB がどれだけ
     早く発動するか」を実測する手段が無い (D88 と同じ制約)。実測せずに
     Windows 用の数値をでっち上げることは Epic #57 ルール 1 (ベンチマーク
     無しに最適化しない) に反する。
  2. RSS がメモリ予算を超えやすい方向の誤差は、**過小評価 (休止しすぎない)
     より安全側の誤差**である — 早めに休止してメモリを守る方向のズレで
     あり、OOM やスワップより実害が小さい。D56 も同じ理由でこの誤差を
     「許容範囲」として受け入れている。
  3. 逆方向 (Windows の 700 MiB を Linux より緩めるべきか) を判断する
     材料も無い。
- **未検証であることを明記する**: 本 Issue のセッションでは Windows 上での
  実際の RSS 値・休止の発動タイミング・体感頻度は一切測定していない
  (`cargo check --target x86_64-pc-windows-msvc --all-targets` による
  型チェックのみ)。`docs/performance-targets.md` §21 の Windows 実測環境
  (`perf-windows.yml`、`workflow_dispatch`) はメモリ予算シグナルを対象に
  していないため、このまま Windows 実機/CI で `tab_scaling.py` 相当の
  計測を行い、700 MiB が Windows で「早すぎる」と分かった場合は Windows
  専用の既定値 (例: RSS ベースで同等の実効休止タイミングになるよう
  850〜900 MiB 程度に引き上げる) を再検討することを Revisit condition
  に残す。


### #187/#189: `memory_check_interval` の既定見直し、および `process_map` の二段階化

**対象**: Issue #187 (アイドル CPU の見直し)。上記「軽量ケースのポーリング
コスト」で PR #185 が「実用上無視できる」と結論した判断を、**誤りと判定
して覆す**。判断根拠は定性的な感覚ではなく、以下の実測に基づく。作業は
1 本の PR (#189) の中で 2 段階に分けて進んだ: まず間隔を延ばす対処
(第一段)、続いてレビューで指摘された「スキャン自体を安くできる」という
より良い打ち手 (第二段、下記) を実装した。両方を合わせた最終形をここに
まとめる。

#### 第一段: `smaps_rollup` が支配的コストであることの確認、間隔の暫定見直し

`metrics::sample_process_tree_rss` (Linux) が呼ぶ `process_map()` は
**VeloX 自身のプロセスツリーだけでなく `/proc` に見えるマシン上の全
プロセス**を毎回列挙し、各プロセスの `status`/`smaps_rollup`/`stat` を
読む (`build_sample` が事後にツリーへ絞り込む)。この「フルスキャン 1 回」
のコストを、Rust 実装と同じ手順を踏む外部プローブで直接計測した (VeloX
を 1 タブで起動した状態、20 試行の中央値、このコンテナ、プロセス総数
88・うち `smaps_rollup` が読めたもの 21):

| 内訳 | 時間 (中央値) | 全体比 |
| --- | ---: | ---: |
| フルスキャン合計 | 17.008ms | 100% |
| `status` 読み取り (全 87 プロセス) | 1.208ms | 7.1% |
| **`smaps_rollup` 読み取り (21 プロセスのみ)** | **14.568ms** | **85.7%** |
| `stat` 読み取り (全 87 プロセス) | 1.138ms | 6.7% |

`smaps_rollup` はアクセスできた 21/87 プロセス分だけで全体の 86% を占め、
1 プロセスあたりのコストが `status`/`stat` (定数個のフィールドを読むだけ)
とは桁違いに高いことを確認した — カーネルのページテーブル走査が支配的、
という見立てを裏付ける。**このうち実際に VeloX 自身のツリーに属するのは
約 9 プロセスだけで、残り約 12 プロセス分の `smaps_rollup` 読み取りは
`build_sample` が最終的に捨てる、完全な無駄だった** (この事実は次の
「第二段」につながる)。

間隔別のアイドル CPU (`scripts/profile/cpu_usage.py`、idle 60 秒窓、
`--settle-secs` 6〜8 秒、この時点ではまだ一段階読み [21 プロセス分全て
smaps_rollup を読む] のバイナリで計測):

| 間隔 | 1 タブ CPU% | 3 タブ CPU% |
| ---: | ---: | ---: |
| 2000ms (旧既定) | 1.2%（0.70〜0.72 秒/60 秒、2 試行） | 1.4%（0.86 秒/60 秒） |
| 5000ms | 0.5%（0.31 秒/60 秒） | 0.6%（0.38 秒/60 秒） |
| 10000ms | 0.3%（0.16〜0.19 秒/60 秒、2 試行） | 0.4%（0.25 秒/60 秒） |
| 30000ms | 0.2%（0.11 秒/60 秒） | 0.2%（0.11 秒/60 秒） |
| (参考) OFF (`VELOX_MEMORY_BUDGET_MB=0`) | 0.1% | 0.2% |

この時点では「10 秒への変更」を暫定的に採用しかけたが、Codex のレビュー
指摘 (下記「第二段」) を受けて実装を見直し、最終的な既定値も変わった。

#### 第二段 (#189): `process_map` の二段階化 — スキャン自体を安くする

レビューで、`process_map()` が `smaps_rollup`/`stat` を**無条件に全
プロセス分**読んでおり、`build_sample` 側で事後にツリーへ絞り込んでいる
という構造そのものが無駄の本体であるという指摘を受けた。「間隔を伸ばして
頻度を下げる」のは対症療法で、「1 回あたりのコストを減らす」方がより
良い打ち手であり、Issue #187 自身も「実測で PSS 取得が支配的だと示せた
場合に限り、間隔延長以外の選択肢も検討してよい」としていた — 上記の
実測 (86%) がまさにその条件を満たしていたため、この打ち手を採用した。

**実装**: `browser::metrics::imp::process_map` (Linux・Windows いずれも)
を 2 パスに分割した。

- **パス 1**: 全 PID を安価な情報だけで列挙する。Linux は `status`
  (`PPid:`/`VmRSS:`) のみ、Windows は `CreateToolhelp32Snapshot` の
  スナップショット列挙 (`PROCESSENTRY32W` が `th32ProcessID`/
  `th32ParentProcessID` を無料で返す、`OpenProcess` 系 API は一切呼ばない)
  だけを使い、ここから `root_pid` の子孫集合を `collect_descendants`
  (既存の純粋関数、変更なし) で確定する。
- **パス 2**: その子孫集合**だけ**に対して、高価な API を呼ぶ — Linux は
  `smaps_rollup` (PSS) と `stat` (CPU)、Windows は `OpenProcess` +
  `GetProcessMemoryInfo`/`GetProcessTimes`。`build_sample` は元々ツリー分
  しか集計していないため、**これは意味論を一切変えない純粋な最適化**
  であり、`sample_process_tree_rss`/`build_sample` 自体は無変更 (呼び出し
  シグネチャに `root_pid` を追加しただけ)。ツリー外のプロセスは
  `pss_bytes`/`cpu_seconds` が `None` のまま (「読めなかった」場合と
  区別できない、既存の表現をそのまま使う) で、`build_sample` は元々
  ツリー以外を見ないためこれで何も壊れない。

**効果の実測** (同じ手法・同じ 1 タブ/3 タブケース、間隔ごとに二段階化
前後を比較):

| 間隔 | 1 タブ (一段階読み) | 1 タブ (二段階読み) | 3 タブ (一段階読み) | 3 タブ (二段階読み) |
| ---: | ---: | ---: | ---: | ---: |
| 2000ms | 1.2〜1.3% | **0.8%** | 1.4% | **1.1%** |
| 5000ms | 0.5〜0.6% | **0.4%** | 0.6% | **0.5%** |
| 10000ms | 0.3% | **0.2%** | 0.4% | **0.3%** |

2000ms で約 35%、5000ms で約 25〜30% の追加削減。ツリー外プロセス比率が
このコンテナより高い実運用のデスクトップでは削減幅がさらに大きくなる
見込み (パス 2 の対象は VeloX 自身のプロセス数、約 9 個で一定のまま、
分母だけが増えるため)。

**結論として採用した間隔: 5 秒** (10 秒ではない)。二段階化により 5000ms
のコスト (1 タブ 0.4%・3 タブ 0.5%) が二段階化前の 10000ms とほぼ同等
かそれ以上に下がったため、10 秒まで間隔を延ばして検知遅延を犠牲にする
理由が無くなった。`SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL` (と
`browser::settings::DEFAULT_MEMORY_CHECK_INTERVAL_MS`) を最終的に**2 秒
→ 5 秒**に変更した。旧既定 (2 秒、一段階読み) 比では 1 タブで **1.2% →
0.4%、約 67% 削減**。

**10/20 タブでの収束確認** (`tab_scaling.py`、二段階化後・5 秒間隔の
バイナリ、`--stabilize-secs` を明示的に振って比較):

| stabilize 秒数 | 10 タブ PSS | 20 タブ PSS | 状態 |
| ---: | ---: | ---: | --- |
| 3 秒 (二段階化前・10 秒間隔での基準と同じ待ち時間) | 466.5 MiB | 625.6 MiB (試行によりばらつき) | ほぼ収束 (procs が 4〜5 の間でばらつく試行あり) |
| 8 秒 | 464.5 MiB | 621.0 MiB | 収束済み |
| (script の新しい既定、後述) 15 秒 | 463.9〜619.6 MiB | 同左 | 収束済み |

10 秒間隔だった第一段の時点で必要だった 15 秒という収束待ちに比べ、5 秒
間隔では**8 秒で確実に収束**しており (2 秒間隔・旧既定の「3 秒で収束」に
近い水準まで回復)、二段階化 + 5 秒化の組み合わせが「CPU も検知遅延も
両方改善する」という結果になった。最終到達点自体は変わらない
(463〜465 MiB / 615〜621 MiB、§23.1 の 2 秒間隔の値と誤差範囲で一致)。

**回帰ゲート** (`cold_startup`、baseline=旧既定 [2 秒間隔・一段階読み]、
candidate=新既定 [5 秒間隔・二段階読み] ×2、各 8 試行): **総合判定 OK**
(`startup_toolbar_ready_ms` は -0.7%〜-2.2%、`rss_total_bytes` は
-23%〜-24% と、いずれも改善方向。悪化した指標は無い)。

**Windows 実装も同じ構造で二段階化した (未検証)**: `imp` (Windows) の
`process_map` も、全プロセスに対して `OpenProcess` +
`GetProcessMemoryInfo`/`GetProcessTimes` を無条件に呼んでいた同じ構造の
無駄を持っていたため (D88/#136 の実装)、同じ二段階化を適用した — パス 1
はスナップショット列挙だけ (`OpenProcess` 系 API を一切呼ばない)、パス 2
で `root_pid` の子孫集合だけに `OpenProcess`+`GetProcessMemoryInfo`/
`GetProcessTimes` を呼ぶ。**このコンテナには Windows 実機/CI が無く実行
できないため、実際の削減効果は測定できていない** — `cargo check --target
x86_64-pc-windows-msvc --all-targets` による型チェックのみ確認済み。
Windows では `OpenProcess`+2 API 呼び出しがプロセスごとのカーネル
往復であり、Linux のファイル読み取りより高コストな可能性が高いため
(D88)、削減効果は Linux 以上に大きい可能性があるが、これは推測であり
実測ではない — Revisit condition に残す。

#### `scripts/bench/tab_scaling.py` の既定待ち時間を間隔に追従させる (Codex 指摘)

PR #189 のレビューで、`tab_scaling.py` の `--stabilize-secs` 既定値
(3.0 秒固定) が `memory_check_interval` の実際の値と無関係にハード
コードされており、**間隔を変えるたびにこの定数を人手で追随させないと
静かに「休止前の PSS」を報告する**という指摘を受けた。実際、上記の
「第一段」時点の実測 (10 秒間隔・3 秒 stabilize で 980.4/1668.9 MiB =
無効時とほぼ同じ、未収束) がこれを裏付けている。

対処として、`tab_scaling.py` に以下を実装した:

- `--memory-check-interval-ms` を新設 (既定値は
  `SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL` と同期させた Python
  定数 `DEFAULT_MEMORY_CHECK_INTERVAL_MS = 5000`、コメントで同期の必要性
  を明記)。この値を VeloX の子プロセスに `VELOX_MEMORY_CHECK_INTERVAL_MS`
  として明示的に渡す (`env.setdefault(...)` — 呼び出し元のシェルが既に
  設定していればそちらを優先し上書きしない) — 「この待ち時間はこの間隔を
  前提にしている」という関係をスクリプト内で自己完結させ、Rust 側の
  既定が将来変わっても呼び出し元は `--memory-check-interval-ms` を渡す
  だけで済むようにした。
- `--stabilize-secs` の既定値を `None` に変え、未指定なら実際に使う
  interval (env で既に設定済みならそちらを、なければ
  `--memory-check-interval-ms`) から `max(3.0, 3.0 * interval_secs)` で
  算出する (`default_stabilize_secs`)。3 倍という係数は、上記の収束実測
  (5 秒間隔で 8 秒 [= 1.6 倍] 待てば確実に収束、10 秒間隔では 15 秒
  [= 1.5 倍] 必要) に安全マージンを加えたもの。下限 3.0 秒は元の定数
  そのもの (短い間隔でも極端に短い待ち時間にはしない)。

実際に動かして確認した: 既定 (5000ms) で 20 タブを計測すると
「memory_check_interval=5000ms から 15.0 秒を既定値として使います」と
表示され、619.6 MiB (収束済み、§23.1 の目標値と一致) を報告する。
`--memory-check-interval-ms 2000` を明示すると「... 2000ms から 6.0 秒
...」と表示され、10 タブで 465.4 MiB (収束済み) を報告する — env 経由の
上書きと `--stabilize-secs` 算出の両方が実際に連動して動くことを確認
した。

`scripts/bench/` と `scripts/profile/` の他のスクリプトについても同種の
依存が無いか確認した (`memory_check_interval`/`memory_budget` を参照する
スクリプトを検索) — `tab_scaling.py` 以外に該当するものは無かった。
`scripts/profile/network_activity.py` はアイドル時間シグナル
(`VELOX_AUTO_SUSPEND_AFTER_MS`) を参照するが、値は `--suspend-after-ms`
で都度明示的に指定する設計であり、コンパイル時既定値への暗黙の依存は
無い。`docs/benchmarking.md` は `--stabilize-secs` に触れていないため
更新不要。`docs/memory-analysis.md` の D56 当時の記述 (「既定は
`--stabilize-secs 3`」) は当時の事実の記録であり、書き換えていない。

### 700 MiB という絶対値がすべてのマシンで妥当か (限界)

700 MiB は D56 の計測環境 (このコンテナ、詳細は §1) 1 台で測った値であり、
**搭載 RAM に対する相対値ではない**。RAM 4GB のマシンにとっての 700 MiB
と RAM 32GB のマシンにとっての 700 MiB は体感インパクトが大きく異なる —
前者では早期の積極的な休止が望ましく、後者ではそもそも休止しなくても
困らない可能性が高い。搭載 RAM 相対の予算は Issue #176 (Memory Budget
Manager) のスコープと明記されており、本 Issue はそれを前提に固定値のまま
出荷する。**この判断の限界は次の Revisit condition に残す。**

### Revisit condition

(1) **Windows 実機/CI での 700 MiB の実効性を計測すること** — 上記の
「未検証」を解消し、必要なら Windows 専用の既定値を導入する。(2) **搭載
RAM に対する相対化** — Issue #176 が実装されたら、700 MiB という絶対値は
その基盤の上の「フォールバック値」に格下げされるべきかを再検討する。
(3) `max_live_tabs`/`idle_after` を既定にするかどうかは、D56 Revisit
condition (2) 「実サイトでの推奨値」が解消されるまで見送ったままにする —
本 Issue はこれを一切変更していない。(4) 多タブ規模 (20 タブ級) の
`velox-bench` シナリオを再計測する際は `VELOX_MEMORY_BUDGET_MB=0` を明示し
ない限り `tab_switch_ms`/`tab_create_ms` が `tab_resume_ms`/`page_load_*`
に置き換わりうる点を、次にこれらのシナリオを触る Issue のために記録して
おく (上記実測の「`tab_switch_20` は比較不能になった」参照)。(5)
**(#187/#189)** 二段階化後の 5 秒間隔でも CPU 面ではまだ OFF 時の床
(0.1〜0.2%) より高い (0.4〜0.5%) — さらに間隔を延ばす、あるいは
`process_map` 自体をもう一段最適化する (例: 前回サンプルとの差分検知で
`smaps_rollup` の再読み取りを間引く) 余地は残っている。本 Issue では
「二段階化 + 5 秒」で CPU と検知遅延のバランスが十分改善したと判断し、
これ以上は追わなかった — 実機のバッテリー計測 (powertop 等) でなお不十分
と分かった場合に再検討する。(6) **(#187/#189)** 間隔別・二段階化前後の
アイドル CPU 計測はすべてこのコンテナ 1 台のセッション内で行った。(7)
**(#189)** Windows 版 `process_map` の二段階化は構造上の無駄 (全プロセス
への `OpenProcess`+`GetProcessMemoryInfo`/`GetProcessTimes`) を読解のみで
確認して実装したが、**実機/CI での削減効果は未計測**。Windows では
1 プロセスあたりのコストが Linux のファイル読み取りより重い可能性が高く
(`OpenProcess` は完全なカーネル往復)、削減効果は Linux 以上の可能性が
あるが検証できていない — 上記 (1) の Windows 実機計測と合わせて行うのが
効率的。

## D91: auto-merge がレビューを見ずにマージしていた問題 (#188) — 未解決スレッド / CHANGES_REQUESTED / Codex の head SHA レビューを必須要件化、`automerge-without-codex` で Codex 要件のみ免除

**対象**: Issue #188。D55 で導入した `auto-merge.yml` は head commit の
check-runs / commit status しか見ておらず、**Pull Request のレビューを
一切考慮せずにマージしていた**。Codex (`chatgpt-codex-connector[bot]`) は
check-run を登録しないため、auto-merge からは「レビューが存在しない」のと
区別が付かず、指摘が出ていてもそのままマージされていた。

実際に PR #185 では 15:31:14 の作成から 15:35:32 (2 分以上前) に Codex が
`src/browser/suspension.rs` へ P2 の指摘 (実在のバグ、Issue #186) を投稿
していたにもかかわらず、15:37:55 に auto-merge がマージした。続く PR #189
でも Codex は別の実在の問題を指摘しており、**この Issue の調査時点で Codex
は 3 PR 中 3 PR (#185/#189/#190) すべてで有効な指摘を出している**
(#190 は 35 行のドキュメント追加のみの小さな PR で、指摘が出にくいことを
狙って作った検証用 PR だったが、それでも P2 の指摘が出た — 詳細は後述)。
**「マージ前に一定時間待つ」だけでは防げない**: レビューはマージの数分前
には既に存在していた。「未解決の指摘があればマージしない」という判定が
必要だった。

本 Issue は当初「Codex を必須条件にしない」方針で起票されたが、実装途中で
ユーザの判断により**「Codex のレビューをマージの必須要件にする」方針へ
変更**された (Issue 本文 2026-09-07 更新)。CodeRabbit は Free プランの
制限 (1 時間 1 レビュー、行単位の指摘を出さず要約のみ) により実質機能して
おらず、**リポジトリへのアクセスも除外済み**のため判定に含めない。

> ⚠️⚠️ **この仕組みを将来触る人が最初に知るべき、Codex についての2つの
> 前提** (2026-09-07、PR #192 自身の運用で判明。決定9で対応):
>
> 1. **Codex は push では再レビューしない。** Codex 自身の説明文
>    (“Reviews are triggered when you open a pull request for review /
>    mark a draft as ready / comment `@codex review`.”) のとおり、
>    **push はレビューのトリガーに含まれていない。** 実際に PR #190
>    (Codex がレビューした commit `9bb0dd7551` の後に push `f97b8d9`)
>    、PR #192 自身 (Codex がレビューした commit `f34617bbdc` の後に
>    push `12fb45e`→`3dabf31`、5分以上経過) のいずれでも、push 後の
>    再レビューは一度も観測されなかった。**「指摘に対応して push しただけ」
>    では Codex の head SHA レビュー要件は永久に満たされない**
>    (実際にこの PR 自身が、自分が実装した判定によって一時的にマージ
>    不能になった)。
> 2. **Codex には利用上限がある。** `@codex review` とコメントすれば
>    再レビューを手動で起動できるが、上限に達すると Codex は
>    “You have reached your Codex usage limits for code reviews.”
>    (実測: PR #192 で `@codex review` をコメントした際に返された) を
>    返し、それ以上レビューしない。CodeRabbit を判定から除外した理由
>    (Free プランの制限で実質機能しない) と同種の制約が Codex にもある。
>
> この2つを踏まえ、決定9で「未レビューを検知したら `@codex review` を
> 自動投稿する (同じ head SHA には1回だけ)」「利用上限到達を検知したら
> 要件を緩和する (ただし一度もレビューされていない PR は緩和しない)」
> を実装した。

### 決定

1. **判定ロジックは `.github/scripts/check_review_gate.py` に切り出し、
   `test_check_review_gate.py` (41 件の `unittest`) でテストする。**
   `extract_closing_issues.py` + D83 と同じ流儀 — workflow の YAML には
   ロジックをベタ書きしない。GitHub API から取得した GraphQL レスポンス
   の形をそのまま模したデータでテストできる。判定は `evaluate_review_gate()`
   1 関数に集約し、以下 4 条件のいずれか 1 つでも該当すれば `blocked=True`
   (マージしない) を返す。
   1. **未解決のレビュースレッド** (`reviewThreads[].isResolved == false`
      が 1 件以上)。人間・ボットを問わず適用。ログには「未解決のレビュー
      スレッドが N 件: `path (author)`, ...」の形でファイル・投稿者を
      出す (#168 の教訓 — 原因がログから読めないと同じ事故を繰り返す)。
   2. **`CHANGES_REQUESTED`** がレビュアーごとの最新レビュー状態
      (GraphQL `latestReviews`) に残っている。レビュアー名をログに出す。
   3. **Codex が現在の head SHA をレビュー済みでない** (下記参照。
      `codex_bypass=True` で免除可能)。
   4. **head SHA の push 観測時刻 (`head_push_observed_at`、下記の決定7
      参照) から猶予期間 (既定15分、workflow の `env.GRACE_PERIOD_MINUTES`)
      が経過していない。** レビューボットがまだ投稿していない場合の保険。
   いずれの GraphQL 呼び出し (`review_threads`/`latest_reviews` が `None`)
   もページング不足 (`pageInfo.hasNextPage == true` で全件を確認できない)
   も、**常に安全側 (blocked) に倒す**。「取得できなかったので指摘ゼロと
   みなす」は絶対にしない — Issue の絶対要件。

2. **Codex の head SHA レビュー判定は3つのシグナルの OR。** 「Codex の
   レビューが存在する」だけでは、**古いコミットへのレビューが残っている
   だけ**のケースを見逃す (push 後に再レビューされていない状態でマージ
   してしまう)。そこで以下のいずれか1つでも現在の head SHA を指せば
   「レビュー済み」とみなす (`check_review_gate.py` の
   `_codex_reviewed_head_sha`/`_codex_reacted_after`):
   - **a. `latestReviews[].commit.oid` (GraphQL) が head SHA と完全一致。**
     最も確実なシグナル。GraphQL の `PullRequestReview.commit` は
     レビューが実際に付けられたコミットを指す (レビュー後に新しい commit
     が push されても更新されない)。
   - **b. レビュー本文の `Reviewed commit:` 行から取り出した短縮 SHA が
     head SHA の接頭辞と一致する。** これは a への追加の裏付け (フォール
     バック) として実装した。実データで確認済み: PR #185/#189/#190 の
     3 件すべてで `**Reviewed commit:** \`<10桁の16進>\`` という形式
     (太字マーカー + バッククォート付き) で、値は head SHA の**先頭10桁**
     と一致していた (`9bb0dd755129c1bc...` → `9bb0dd7551` など)。**完全
     一致ではなく「head SHA が短縮 SHA で始まるか」で判定する**
     (`str.startswith`)。正規表現 `_REVIEWED_COMMIT_RE` は太字マーカー
     (`**`) とバッククォートの有無ゆらぎの両方を許容する。
   - **c. Codex による 👍 リアクション (PR 本体への `THUMBS_UP`、GraphQL
     `pullRequest.reactions`) の `createdAt` が **head SHA の push 観測
     時刻 (`head_push_observed_at`、決定7参照)** 以降。** Codex の説明文
     (“If Codex has suggestions, it will comment; otherwise it will react
     with 👍.”) に基づく代替シグナル。**リアクションは commit に紐付か
     ない (1 ユーザ 1 個)** ため、古い push に対する 👍 が新しい push 後も
     残り続ける — そのため単独では「どの push に対する反応か」を厳密に
     特定できず、`createdAt` が push 観測時刻以降であることを条件に加えて
     弱めのシグナルとして扱う。
   > ✅ **シグナル c (👍 リアクション) は PR #191 (0 バイトのファイル
   > 1 本だけを追加する、意図的に「批評対象が何もない」PR) で実測に
   > 成功し、Codex の公式説明どおりの挙動を確認した。** #190
   > (`docs/README.md` を35行追加) までは 3/3 件で Codex が指摘を出して
   > いたため「指摘ゼロ」のケースが観測できずにいたが、差分の内容が
   > 空 (0 バイトのファイル) の PR #191 では次の結果になった:
   >
   > | 項目 | PR #191 の結果 |
   > | --- | --- |
   > | レビュー (`get_reviews`) | 0 件 |
   > | コメント (`get_comments`) | 0 件 |
   > | PR 本体へのリアクション | `+1: 1` |
   >
   > **指摘ゼロのとき Codex はレビュー・コメントを一切残さず、PR 本体
   > (issue) への 👍 リアクションのみを付ける**ことを確認した (レビュー
   > やレビューコメントに付くのではない — シグナル c が
   > `pullRequest.reactions` [issue 本体] を見る設計になっているのは
   > この観測と一致している)。参考値: PR #191 head SHA
   > `e4c8811bcb7298a12247f5c0712bd510cab658b5`、PR 作成
   > `2026-09-07T16:21:12Z`。
   >
   > **残る限界**: この観測に使ったツールはリアクションの**集計値**
   > (`+1: 1` 等) しか返さず、**投稿者が実際に `chatgpt-codex-connector
   > [bot]` であることを API 上で直接確認できていない。** CodeRabbit を
   > 除外した後、本リポジトリで稼働しているレビューボットは Codex のみ
   > であり、かつレビュー/コメントを一切出さなかった PR にだけ +1 が
   > 付いている状況証拠から Codex によるものとみて差し支えないと判断した
   > が、断定はできていない。**そのため `check_review_gate.py` の実装は
   > 集計値ではなく `user.login` の完全一致 (決定7の P2 参照) だけを
   > 対象にする。** 万一 👍 の投稿者が別ユーザだった場合に誤ってマージ可と
   > 判定しないための必須の防御であり、実装済みであることを確認済み。
   > この防御がないと第三者の 👍 でマージが通ってしまう。

3. **`automerge-without-codex` ラベルで Codex レビュー必須要件のみを免除
   する (workflow の `env.CODEX_BYPASS_LABEL`)。** 「指摘ゼロのとき Codex
   が何を残すか」は PR #191 の観測 (上記シグナル c 参照) で確認できたが、
   それでもこのラベルは残す。理由: (a) リアクションの投稿者が Codex か
   どうかを集計値からは断定できていない残余の不確実性がある、(b) Codex
   側の障害・クォータ切れ・設定変更 (レビューが呼ばれなくなる) は今後も
   起こりうる。**Codex を必須要件にする以上、Codex が応答しなければ全 PR
   が永久にマージされなくなるリスクがある** — このラベルはその安全弁。
   - **未解決スレッド判定・`CHANGES_REQUESTED` 判定は免除しない。** それ
     らは人間の明示的な差し戻し (あるいは Codex 以外のレビュアーの指摘)
     を尊重するためのものであり、Codex の可用性とは無関係。
   - `no-automerge` (完全に自動マージ対象外にする) とは役割が異なる:
     `automerge-without-codex` は「Codex 抜きでよいので他の条件が揃えば
     マージしてよい」、`no-automerge` は「一切自動マージしない」。

4. **未解決スレッドは `isOutdated` を無視し `isResolved` のみで判定する
   — 運用上の重要な注意点。** PR #190 で実測: レビュー対象の行を修正して
   push すると GitHub 上でスレッドは `isOutdated: true` になるが、
   **`isResolved` は自動では `true` にならない**。明示的に「Resolve
   conversation」を押す (または API で resolve する) までスレッドは
   未解決のまま残り、**auto-merge は永久にブロックし続ける。**
   `check_review_gate.py` はそもそも `isOutdated` を取得・参照していない
   ため、この挙動と一致している (`isResolved` だけを見る設計で正しい)。
   **運用上の注意**: Codex や人間の指摘に対応してコードを push しただけ
   ではマージされるようにならない。**指摘に対応した後は、PR 上で該当
   スレッドを明示的に resolve する必要がある。** (Codex 自身がスレッドを
   自動 resolve する挙動があるかどうかは、この Issue の調査でも未確認 —
   仮に Codex が resolve しない場合、対応後の resolve は毎回人間の作業に
   なる。)

5. **`schedule` を `*/30` から `*/10` (10分間隔) に詰めた。** 猶予期間
   (既定15分) を追加した分、`workflow_run` で拾えなかった PR が定期実行
   だけに頼るケースの最悪マージ遅延が増える (30分間隔のままだと最悪
   45分程度)。Issue 本文が提示した2案 (a. 間隔を詰める、b. レビュー
   イベントで再実行する仕組みを作る) のうち **a を採用**した。理由:
   - b (`pull_request_review`/`pull_request_review_thread` イベントを
     追加のトリガにする) は、レビュー解消・CHANGES_REQUESTED 解除の反映
     を速くできる利点はあるが、`auto-merge` job は `contents: write` /
     `pull-requests: write` / `issues: write` という強い権限を持つ job
     であり、新しいイベント種別をトリガに追加するたびに「このイベントは
     PR 側の workflow 定義で実行されないか」を再検証するコストが生まれる
     (D55 が `pull_request` トリガそのものを避けた理由と同種のリスク)。
   - a (`schedule` を詰める) は既存の設計 (D55: 「PR ごとの走査で、
     イベントの payload に依存しない」) をそのまま使い回せ、追加のリスク
     が無い。定期実行の負荷は GitHub Actions の無料枠に対して軽微
     (1 job、数秒〜数十秒程度)。
   - **残る制約**: レビュー解消 (スレッドの resolve、`CHANGES_REQUESTED`
     の解除) や Codex の新しいレビュー到着は、`workflow_run` (他の
     CI/perf-gate/release-windows/dependency-audit の完了) にたまたま
     便乗しない限り、**最大 10 分程度の遅延**で auto-merge に反映される。
     これは Issue の完了条件 (「指摘ゼロの PR が停滞しないこと」) を
     10 分程度の遅延はあるが満たしている、と判断した。将来この遅延が
     問題になった場合は b (レビューイベントをトリガに追加) を再検討する。

6. **workflow 自体の検証に dry-run 専用 job (`dry-run-review-gate`) を
   追加した — D88 の「`pull_request` + `paths` フィルタで自分自身を変更
   する PR でのみ検証実行する」パターンを踏襲。** `on.pull_request.paths`
   に `auto-merge.yml` 自身と `check_review_gate.py` /
   `review_gate_decision.sh` を指定し、これらを変更する PR に限って
   起動する。`workflow_dispatch` は `main` にマージされるまで Actions
   タブに現れないため、workflow 自身の変更を検証する唯一の手段になる
   (D88 と同じ理由)。
   - **本番の `auto-merge` job とは完全に別の job に分離し、job 単位の
     `permissions` を `contents: read` / `pull-requests: read` のみに
     絞った。** `pull_request` トリガは (`pull_request_target` と異なり)
     **PR ブランチ側の workflow 定義で実行される** — これは D55 が
     `auto-merge.yml` 全体で `pull_request` トリガそのものを避けた理由
     と同種のリスクであり、dry-run job にも本質的に残る。`auto-merge`
     job が持つ `contents: write` / `pull-requests: write` /
     `issues: write` を万一にも dry-run job に渡さないよう、明示的な
     job 単位 `permissions` で読み取り専用に絞り込んだ (job 単位の
     `permissions` は workflow 単位の設定を完全に置き換える)。これにより
     PR 側でこの job の中身が改変されても、書き込み系の操作
     (`gh pr merge`/`gh issue close` など) は一切できない。
   - `secrets.AUTO_MERGE_TOKEN` は dry-run job では意図的に使わず、
     `secrets.GITHUB_TOKEN` (読み取りスコープに制限済み) のみを渡す。
   - dry-run job は `review_gate_decision.sh` を呼ぶだけで、`gh pr merge`
     は一切呼ばない。判定結果 (ブロック理由の有無) をログに出すのみ。
   - **残る残余リスク**: 同一リポジトリ内のブランチ (フォークでない) から
     の PR では、GitHub は宣言された `permissions` をそのまま尊重する
     ため、理論上は PR 側でこの job の `permissions` 自体を書き換えて
     権限を要求し直すことができる (フォーク PR には GitHub 側の読み取り
     専用強制があるが、同一リポジトリ内のブランチには効かない)。本
     リポジトリは単一オーナー (`noan98`) のプライベートリポジトリで、
     ブランチを作成できるのは信頼できるコラボレータのみという前提の下
     では実害は小さいと判断したが、将来コラボレータが増える場合はこの
     残余リスクを再評価すること。

7. **PR #192 (この実装 PR 自身) に Codex が指摘した P1/P2 の2件を、実装に
   反映した。** 皮肉にも、この Issue が「Codex の指摘を見落とさない
   ようにする」ための実装自体に、Codex が実在の脆弱性を2件見つけた。
   いずれも「Codex を必須要件にする」判定ロジックの正しさを直接損なう
   欠陥だったため、両方とも修正した。

   - **P1 (最重要): 猶予期間とシグナル c の基準時刻に committer date を
     使ってはいけない。** 初版の実装は `head_committed_date` (git commit
     の committer date) を、猶予期間の起点とシグナル c (👍 リアクション)
     の基準時刻の**両方**に使っていた。しかし committer date は「commit
     をローカルで作った時刻」であり「GitHub に push された時刻」ではない
     — ローカルで数時間前に作った commit を今 push する、cherry-pick/
     rebase で古い commit を持ち込む、といった特殊な操作ではない普通の
     git 操作で committer date は容易に過去の日時になる。この状態では
     (a) 以前の head に付いた Codex の 👍 の `createdAt` が新しい (実は
     古い日時の) committer date より後になり、誤って「現在の SHA を
     レビュー済み」と判定されてしまい、(b) 猶予期間も同時に即座に満たさ
     れてしまう — **2つの防御が同一の操作可能なタイムスタンプに依存し、
     同時に破られる**という、判定ロジックの中核に関わる欠陥だった。
     **対処**: `head_committed_date` を `head_push_observed_at` にリネーム
     し、値の取得元を **head SHA に対する check-suite の作成時刻の最小値**
     (`repos/{owner}/{repo}/commits/{sha}/check-suites` の `created_at`)
     に変更した。push を受けて GitHub 自身がサーバ側で作成するものなので
     attacker が直接操作できない (本リポジトリは PR で必ず CI
     [`ci.yml`] が走るため、check-suite は実用上必ず作成される)。取得
     できなかった場合 (check-suite が1件も無い等) は **committer date へ
     のフォールバックはせず**、安全側 (マージしない) に倒す — フォール
     バック自体がこの脆弱性を再現するため。`dry-run-review-gate` job の
     `permissions` に `checks: read` を追加した (`auto-merge` job は既に
     決定1で `checks: read` を持っていたため変更不要)。
     > ⚠️ **この方式は「head SHA に対して check-suite が作成されている」
     > ことに依存する、という前提を明記しておく。** 本リポジトリは PR
     > で必ず CI (`ci.yml`) が走るため実用上は問題にならないが、CI を
     > 持たないリポジトリや、push 直後で GitHub 側の check-suite 作成が
     > まだ反映されていない極めて早いタイミングでは取得できない場合が
     > ある。**その場合は意図的に安全側 (マージしない) に倒す設計であり、
     > 副作用として「(その PR に対する) CI が始まるまでマージされない」
     > という挙動になる。** これは望ましい挙動と判断した (「push 観測を
     > 確認できない = レビューボットがまだ push を認識していない可能性が
     > 排除できない」ため) 上での意図した設計であり、取りこぼしではない。
   - **P2: Codex ログインの判定は前方一致ではなく完全一致にする。**
     初版の実装 (`_is_codex_login`) はログイン名が `chatgpt-codex-
     connector` で**始まるか** (`str.startswith`) で判定していた。この
     ため `chatgpt-codex-connector-review` のような別名アカウントが
     レビューを出せば (本 PR のコメント権限を持つ第三者が実際に作成
     できる)、Codex レビュー必須要件をすり抜けられてしまう。**対処**:
     既知の Codex ログイン名の完全一致許可リスト
     (`_CODEX_LOGINS = frozenset({"chatgpt-codex-connector[bot]"})`、
     大小文字は無視) で判定する `_is_codex_author` に置き換えた。GraphQL
     クエリで取得できる場合は追加で `__typename == "Bot"` であることも
     要求する (`author { login __typename }` — 通常の `User` アカウントが
     たまたま同名を名乗ることはできない [GitHub がログイン名の重複を
     禁止する] が、多層防御として追加した)。
   - **テスト**: `test_check_review_gate.py` に
     `PushObservedAtSecurityRegressionTest` (P1: 呼び出し側が正しい
     push 観測時刻を渡した場合に、古い head に付いた 👍 が正しく拒否され
     ることを確認 — 関数自体は committer date と push 観測時刻を区別する
     情報を持たないため、修正の本体である `review_gate_decision.sh` 側の
     データソース変更と対で見る必要がある) と `CodexLoginExactMatchTest`
     (P2: `chatgpt-codex-connector-review` 等の別名ログインが**前方一致の
     ままだと PASS してしまい [脆弱性を再現]、完全一致に直したことで
     BLOCKED [FAIL→修正後 PASS] になる**ことを確認する回帰テスト) を
     追加した。
   - PR #192 上の Codex のレビュースレッド2件 (P1: discussion_r3951397804
     / P2: discussion_r3951397806) は、修正を返信・push した上で明示的に
     resolve した (決定4の運用注意のとおり、push しただけでは
     `isResolved` は自動で `true` にならない)。

8. **GraphQL に必要な権限は `pull-requests: read` で足り、既存の
   `permissions.pull-requests: write` (D55/D83 で既に付与済み) に包含
   される。** 追加の `permissions` 変更は不要だった (ただし決定7の P1
   対応で `checks: read` を dry-run job に追加している)。GraphQL 呼び出し
   自体が失敗した場合 (権限不足の 403 等) は `gh` の生のエラー出力を
   `::warning::` に含めて安全側ブロックする — #168 で `issues` 権限が
   抜けて 404 になった際にエラーメッセージだけでは原因が分からなかった
   教訓を踏まえた (`review_gate_decision.sh` 内のコメント参照)。

9. **PR #192 自身の運用で判明した「Codex は push で再レビューしない」
   「Codex には利用上限がある」の2点に対応した (上の ⚠️⚠️ 参照)。**
   ユーザの決定に基づき、「通常は厳格に必須。ただし Codex が『利用上限』
   を返した場合に限り、自動で緩める」という方針で実装した。

   - **`@codex review` の自動リクエスト (1 head SHA につき1回のみ)。**
     条件3の a/b/c いずれのシグナルにも一致しない場合、
     `pullRequest.comments` (直近100件、GraphQL) を走査し、現在の
     head SHA を埋め込んだマーカー
     (`<!-- auto-merge:codex-review-request:<head SHA> -->`) を含む
     コメントが既に存在するかを調べる。**無ければ**
     `codex_review_request_needed=True` を返し、`auto-merge` job
     (`pull-requests: write` を持つ本番 job のみ) が実際に
     `@codex review` + マーカーをコメント投稿する
     (`codex_review_request_comment_body()` に本文組み立てを一元化し、
     bash 側で文字列を再構築しない)。**マーカーは head SHA 固有**なので、
     新しい push で head SHA が変われば別のマーカーとして扱われ、
     自動的に「1 push につき1回」の再リクエストが起こる (恒久的な
     バイパスにはならない)。`dry-run-review-gate` job は
     `pull-requests: write` を持たない (決定6) ため、投稿する判定に
     なったことをログに出すだけで実際には投稿しない
     (`allow_codex_request_post` 引数で制御。本番 job のみ `true`)。
   - **利用上限到達時の緩和は「1. 依頼コメントが存在し、2. その依頼
     コメントより後に Codex 本人 (完全一致で照合) が利用上限メッセージ
     (`"reached your codex usage limits"` を含む、大小無視の部分一致)
     を投稿している」の両方を満たす場合に限る。** 「依頼コメントより
     後」を要求するのは、過去の別のリクエストに対する古い上限メッセージ
     を使い回させないため。緩和が発動すると、条件3を「この PR の
     `latestReviews` にレビュアーが Codex であるエントリが1件でもあれば
     良い (head SHA と一致しなくてよい)」に変える。**この PR が一度も
     Codex にレビューされていない場合は緩和しない** (未レビューのまま
     通してしまうため) — `evaluate_review_gate()` は
     `any_codex_review` が空なら緩和せず通常どおりブロックする。
     > 📝 **この「一度も Codex にレビューされていない場合」の扱いは、
     > 決定10 (Claude フォールバック) によって拡張された。** 本決定
     > (決定9) 時点では単純にブロックしていたが、その後 PR #193 で
     > 実際にこの状態が発生したのを受け、ユーザの決定により「Claude に
     > レビューを依頼する」という第2のフォールバック経路が追加された。
     > 詳細は決定10を参照。この決定9のテキスト自体は当時の記述のまま
     > 残し、変更点は決定10に切り出す形にした。
   - **未解決スレッド判定 (条件1) と `CHANGES_REQUESTED` 判定 (条件2)
     は、条件3の緩和と完全に独立している。** 実装上、緩和は条件3の
     ブロック理由を追加しないだけであり、条件1/2は常にそれぞれ独立して
     評価される。そのため「利用上限到達 + 未解決スレッドあり」では
     全体としては引き続きブロックされる (`test_usage_limit_but_
     unresolved_thread_still_blocks`)。
   - **緩和が発動したら `::warning::` で必ず目立たせる。** 黙って
     緩めない、というユーザの明示的な指示どおり、`codex_relaxed`/
     `codex_relaxed_detail` を `evaluate_review_gate()` の戻り値に含め、
     `review_gate_decision.sh` が `blocked` の値に関わらず (relaxed した
     結果ブロックされていなくても) `::warning::PR #<番号>: <detail>` を
     必ず出力する。
   - **上限メッセージの検出は文字列マッチであり、Codex 側の文言が変われば
     検出できなくなる。** その場合は緩和が発動せず「厳格なまま待ち続ける」
     = 安全側に倒れる (誤ってマージされる方向には壊れない)。この限界は
     意図的に許容した — 検出精度を上げるための公式 API 等は Codex 側に
     存在しないため。
   - **`pr_comments` (GraphQL 呼び出し) が取得できなかった場合は、
     リクエストも緩和も行わず安全側でブロックする。** 「取得できな
     かったのでリクエスト不要とみなす」は、重複リクエストを防ぐための
     判定が信頼できないまま投稿しない/しないでおく、という意味で安全側
     の選択。
   - **テスト**: `test_check_review_gate.py` の
     `CodexUsageLimitAndAutoRequestTest` (11件) で、リクエスト要否判定・
     重複防止・緩和条件 (別 commit でのレビューあり/未解決スレッドあり/
     一度もレビューなし/上限メッセージの投稿者が Codex 以外/上限
     メッセージが依頼コメントより前) を網羅した。この機能はこの PR で
     新規追加したものであり、旧実装 (`codex_review_request_needed`/
     `codex_relaxed` キーを持たない `evaluate_review_gate`) に対しては
     必ず `KeyError` で失敗するため、「修正前に失敗し修正後に通る」
     テストという位置付けになる。

10. **Codex の利用上限到達時、この PR が一度も Codex にレビューされて
    いない場合 (決定9の `any_codex_review` が空) のフォールバックとして
    `@claude` メンションでレビューを依頼する仕様に変更した (2026-09-07、
    ユーザの決定)。** きっかけは PR #193 — 作成直後に Codex が利用上限
    メッセージを返し、一度もレビューされないままの状態が実際に発生した。
    決定9のままではこの状態は永久にブロックされ続けるため、決定9を
    置き換えるのではなく**第2のフォールバック経路として追加**した。
    実装は「A. 決定9の Codex 緩和 (この PR の過去のレビューで代替) を
    まず試し、それが無理な場合のみ B. Claude フォールバックを試す」
    という順序 (`check_review_gate.py` の `evaluate_review_gate()` 内、
    利用上限検知ブランチ)。

    - **最大の設計課題は「Claude のレビューを何で識別するか」だった。**
      Codex (`chatgpt-codex-connector[bot]`) と異なり、**このリポジトリ
      には Claude 関連の workflow が無く**、`@claude` メンションに応答
      する仕組み (Claude GitHub App / `claude-code-action` 等) が導入
      されているかは**未確認**。そのため応答時のログイン名もレビュー
      形式 (review/comment/何も残さない) も分かっていない。**ログイン名
      をハードコードしない**という Codex 判定 (`_CODEX_LOGINS`) と対照的
      な方針を採った: `claude_logins` 引数 (workflow の
      `env.CLAUDE_REVIEWER_LOGINS`、カンマ区切り) で明示的に設定された
      ログインのみを許可する。**既定値は空文字列 = 空集合であり、Claude
      経路は常に不成立になる** (`_claude_reviewed_or_commented()` は
      `claude_logins` が空なら即 `False` を返す)。「未設定なのに何となく
      通る」実装を避けるため、`test_claude_allowlist_unset_does_not_
      satisfy_even_with_matching_comment` で「head SHA に一致する
      `claude[bot]` からのレビューが実際に存在していても、許可リストが
      空なら考慮されない」ことを明示的に確認した。
    - **判定シグナルは2つの OR** (`_claude_reviewed_or_commented()`):
      (i) `latestReviews[].commit.oid` が head SHA と完全一致する Claude
      のレビュー (Codex のシグナル a と同じ発想)、(ii) `@claude` 依頼
      コメント (マーカー `<!-- auto-merge:claude-review-request:<head
      SHA> -->`) より**後**に Claude ログインが投稿したコメント。(ii) を
      入れた理由: Claude が正式なレビュー (`PullRequestReview`) ではなく
      単なるコメントで応答する可能性を排除できないため。Codex と異なり
      `__typename == "Bot"` は要求しない (`_is_login_in()` — Claude 側の
      実装が Bot か User か不明なため、ログイン名の完全一致だけで判定)。
    - **`@claude` への依頼コメントも、Codex と同じマーカー方式で
      head SHA ごとに1回だけ投稿する** (`_CLAUDE_REQUEST_MARKER_RE`/
      `claude_review_request_comment_body()`)。コメント本文には
      **依頼理由** (Codex が利用上限に達しており、この head が未レビュー
      であること) と **PR タイトル**を含める (ユーザの明示的な指示 —
      「レビュアーが文脈を掴めない依頼にしないこと」)。PR タイトルは
      `review_gate_decision.sh` の GraphQL クエリに `pullRequest.title`
      を追加して取得し (`prTitle` として payload に渡す)、新たな API
      呼び出しは増やしていない。
    - **投稿は本番の `auto-merge` job のみが行う。** `dry-run-review-gate`
      job は `pull-requests: write` を持たない (決定6) ため、
      `claude_review_request_needed` が `True` でも実際には投稿せず
      「投稿する判定になった」ことをログに出すだけ — Codex の
      `allow_codex_request_post` 引数をそのまま流用した (実質的には
      「このジョブはコメントを投稿してよいか」を表すフラグなので、
      Codex/Claude 両方の投稿可否を1つの引数で共用している)。
    - **未解決スレッド判定 (条件1) と `CHANGES_REQUESTED` 判定 (条件2) は、
      Claude フォールバックでも一切緩めない。** 決定9と同じく、これらは
      条件3とは独立に評価される。`test_usage_limit_with_unresolved_
      thread_still_blocks_even_with_claude_review` で、Claude のレビュー
      があっても未解決スレッドがあれば全体としてはブロックされ続ける
      ことを確認した。
    - **緩和が発動したら `claude_relaxed`/`claude_relaxed_detail` を
      `::warning::` として出力する** (決定9の `codex_relaxed` と同じ
      流儀。`review_gate_decision.sh` で両方を個別にチェックする — 両者
      は互いに排他 [A が成立すれば B は試さない] だが、コードの単純さの
      ため個別の `if` にしてある)。
    > ⚠️⚠️ **決定10 執筆時点では「`@claude` メンションが実際に応答を得ら
    > れるかは一切検証できていない」としていたが、その後 PR #193 で
    > 実地検証が行われ、状況が更新された。詳細は決定11を参照。** 要点だけ
    > 先に書くと: **Claude App 自体は導入済みで動作している**が、
    > **`GITHUB_TOKEN` で投稿したメンションには反応しなかった** (Web UI
    > から人間が投稿したメンションには反応した)。そのため決定11で
    > `@claude` の投稿を `AUTO_MERGE_TOKEN` (PAT) 専用にし、未設定なら
    > 投稿しない構成にした。`AUTO_MERGE_TOKEN` が未設定のままなら、
    > 「Codex が利用上限に達し、かつこの PR が一度もレビューされていない」
    > 状態は `automerge-without-codex` ラベルを付けるまで止まる。

11. **PR #192 の dry-run job が実際に failure になった (`jq: invalid JSON
    text passed to --argjson`)。原因は決定10で追加した CSV→JSON 変換の
    バグで、しかも既定設定 (`CLAUDE_REVIEWER_LOGINS: ""`) で必ず再現する
    ものだった。あわせて、`@claude` メンションの投稿トークンについても
    実測に基づき修正した。**

    - **バグの内容**: `review_gate_decision.sh` が
      `printf '%s' "${CLAUDE_REVIEWER_LOGINS:-}" | jq -R '...'` という形で
      カンマ区切り文字列を JSON 配列に変換していた。`printf '%s' ""` は
      **改行を含まない 0 バイト出力**になり、`jq -R` は入力を行単位で
      読むため、**入力に完全な行が1つも無いと何も出力しない** (jq の
      raw-input モードの仕様)。結果 `claude_logins_json` が空文字列に
      なり、後段の `jq -n --argjson claudeLogins "$claude_logins_json"`
      が「invalid JSON」で失敗していた。`CLAUDE_REVIEWER_LOGINS` の既定値
      そのものが空文字列 (`auto-merge.yml` の `env`) なので、**設定を
      一切いじらない既定状態で必ずクラッシュする**バグだった — 決定10の
      「未設定なら Claude 経路は不成立 (安全にブロック)」という意図とは
      まったく違う、動作不能という結果になっていた。
    - **修正**: `jq -n --arg s "${CLAUDE_REVIEWER_LOGINS:-}" '$s |
      split(",") | map(gsub("^\\s+|\\s+$"; "")) | map(select(length >
      0))'` に変更した。`jq -n` は標準入力を一切読まないため、`$s` が
      空文字列でも確実に有効な JSON (`[]`) を返す。前後の空白除去
      (`gsub`) も追加し、`"a, b"` のような区切りでも壊れないようにした。
    - **点検**: リポジトリ内の他のスクリプト・workflow に同種の
      `printf | jq -R` パターンが無いかを確認し、無いことを確認した
      (`.github/workflows/auto-merge.yml` の他の `--argjson` 呼び出しは
      いずれも `jq -s` [slurp モード。空入力でも確実に `[]` を返す] か、
      REST レスポンスから `--jq` で直接抽出した値であり、この脆弱性の
      パターンには該当しない)。
    - **テスト**: `.github/scripts/test_review_gate_decision.sh` を新規
      追加した (`review_gate_decision.sh` 自体のシェルレベルの統合テスト
      — 判定ロジック本体は `test_check_review_gate.py` で別途網羅済み
      のため、ここでは shell ラッパー固有の挙動だけを見る)。`gh` を
      スタブに差し替え、`CLAUDE_REVIEWER_LOGINS` が未設定/空文字列/
      空白のみ/カンマのみ (`",,"`) のときにクラッシュせず空配列として
      扱われることを確認する。**実際に旧実装 (`printf | jq -R`) に戻して
      実行し、未設定/空文字列の2ケースで `invalid JSON` の再現と fail を
      確認**したうえで、修正版に戻して全件 pass することを確認した
      (修正前に失敗し修正後に通る回帰テスト)。

    **発見: `GITHUB_TOKEN` で投稿した `@claude` メンションは Claude App
    に無視される可能性が高い (PR #193 での実測)。**

    | コメント | 投稿経路 | Claude App の反応 |
    | --- | --- | --- |
    | API トークンで投稿した `@claude` レビュー依頼 | API (`GITHUB_TOKEN`
    相当) | 反応なし (リアクション0件) |
    | Web UI から人間が投稿した `@claude` メンション | Web UI | 👀
    リアクションが付いた |

    > 📝 **これは「反応しなかった」という否定的観測でしかなく、Claude
    > App 側の仕様を断定するものではない。** GITHUB_TOKEN 経由の投稿が
    > 反応されない理由 (Actions からの投稿を意図的に無視している、
    > たまたまこの1回だけ反応が遅れた、等) は特定できていない。ただし
    > `auto-merge.yml` が既に警告している「`GITHUB_TOKEN` でのマージ・
    > push は GitHub 側のそれ以上の自動処理を誘発しない」という既知の
    > 制約 (D55) と構造が似ており、慎重を期して安全側の対応を取った。

    - **対処**: `@claude` の投稿だけは `AUTO_MERGE_TOKEN` (PAT) を明示的
      に使うようにした (`review_gate_decision.sh` 内で、その1回の
      `gh pr comment` 呼び出しにだけ `GH_TOKEN="$AUTO_MERGE_TOKEN"` を
      前置する)。**`AUTO_MERGE_TOKEN` が未設定なら投稿しない**
      (「反応されないまま空打ちする」より「投稿しない」方が安全 — レビュー
      依頼が実際に届いたと誤認させないため)。`@codex review` の投稿は
      これまでどおり `GH_TOKEN` (`AUTO_MERGE_TOKEN||GITHUB_TOKEN` の
      フォールバック) のままにした — こちらは GITHUB_TOKEN 経由でも
      Codex が実際に反応することを確認済みであり (この PR 自身で複数回
      観測)、アプリごとに挙動が異なるため Claude 側だけ個別に対処した。
      `auto-merge.yml` の `Merge eligible pull requests` ステップに
      `AUTO_MERGE_TOKEN: ${{ secrets.AUTO_MERGE_TOKEN }}` を追加した
      (未設定なら空文字列になる — GitHub Actions の仕様)。

### 実装

- `.github/scripts/check_review_gate.py` — 判定ロジック本体
  (`evaluate_review_gate()`)。GitHub API のレスポンス形をそのまま引数に
  取るため、GraphQL 呼び出しをモックせずに単体テストできる。
  `codex_review_request_comment_body()`/`claude_review_request_comment_
  body()` にそれぞれの自動投稿コメント本文組み立てを一元化 (bash 側で
  文字列を再構築しない)。
- `.github/scripts/test_check_review_gate.py` — 94 件の `unittest`。
  PR #185 の実タイムライン (未解決スレッド + 猶予期間でブロック、Codex
  自体は head SHA をレビュー済みなのでブロック理由には含まれない) を
  再現する回帰テストも含む。
- `.github/scripts/review_gate_decision.sh` — GraphQL/REST 呼び出し
  (`gh api graphql`/`gh api repos/.../commits/...`/`gh pr comment`) →
  JSON 整形 → `check_review_gate.py` 呼び出し、という薄い shell
  ラッパー。GraphQL クエリに `pullRequest.title` を追加し、Claude への
  依頼コメントに PR タイトルを含められるようにした。`auto-merge` job
  (本番、`allow_codex_request_post=true`) と `dry-run-review-gate` job
  (検証、同 `false`) の両方から同じスクリプトを呼ぶ (ロジックの二重管理
  を避ける)。終了コード 0=マージ可 / 1=ブロック理由あり / 2=API 呼び
  出し自体が失敗、を返す。
- `.github/scripts/test_review_gate_decision.sh` — 決定11で新規追加。
  `review_gate_decision.sh` 自体の shell レベル統合テスト
  (`gh` スタブ使用)。
- `.github/workflows/auto-merge.yml` — 上記を呼び出す形に変更。
  `env.CLAUDE_REVIEWER_LOGINS` (既定は空文字列) と、`Merge eligible pull
  requests` ステップの `env.AUTO_MERGE_TOKEN` (決定11) を追加。

### 動作確認

`check_review_gate.py`/`test_check_review_gate.py` は 94 件の `unittest`
全件 pass を確認した (`python3 -m unittest test_check_review_gate -v`)。
内訳: 条件1〜4の基本ケース (28件)、Codex 必須判定の基本シグナル a/b/c
(17件)、決定7 P1/P2 の回帰テスト (8件)、決定9 の `@codex review` 自動
リクエスト・利用上限緩和 (11件)、**決定10 の Claude フォールバック
(`ClaudeFallbackTest`、9件)**、複数理由・PR #185 回帰など (21件)。

`review_gate_decision.sh` は `gh` コマンドをスタブに差し替えたローカル
統合テストで、(a) Codex が head SHA を正しくレビュー済みのケース (exit 0)、
(b) Codex のレビューが無いケース (exit 1、bypass ラベル無し) と
`automerge-without-codex` 相当の第3引数で免除されるケース (exit 0)、
(c) GraphQL 呼び出し自体が失敗するケース (exit 2、`::warning::` に生の
エラーを含む)、(d) `@codex review` 未投稿時に実際に `gh pr comment` を
呼ぶケース (`allow_codex_request_post=true`) とログのみに留めるケース
(同 `false`、dry-run 相当)、(e) 利用上限到達を検知して緩和が発動し
`::warning::` を出しつつ `exit 0` になるケース、**(f) 利用上限到達 + 未
レビューで `CLAUDE_REVIEWER_LOGINS` 設定時に実際に `@claude` への依頼を
投稿するケース、(g) 同条件で dry-run (`allow_codex_request_post=false`)
のときは `gh pr comment` を一切呼ばないことを確認するケース**、の7系統
を確認した。

**`dry-run-review-gate` job は、PR #192 (この Issue の実装 PR 自身、
`.github/workflows/auto-merge.yml` を変更している) 上で GitHub Actions
上で実際に起動している。** commit `f34617bbdc` (P1/P2 修正前) の時点では
success で完走し、GraphQL 呼び出しの成功・ブロック理由を最初の1件で
打ち切らず該当する全件出力できることを確認した:

```
wait: Codex のレビュー待ち (head SHA `f34617b` に対するレビュー/👍リアクションが見つかりません)
wait: head commit からまだ 0.9分 しか経過していません (猶予期間 15分、あと 14.1分)
判定結果: マージ見送り (上記の理由による)
```

これにより「workflow 自身の変更を、マージ前に検証する」という決定6の
目的 (D88 のパターン踏襲) は実際に機能することを確認できた — そして
**決定10 (Claude フォールバック) を追加した commit `05c7918` では、まさに
この dry-run job が実際に failure になり、決定11のバグ (`jq: invalid
JSON text passed to --argjson`) をマージ前に検出した。** 「workflow 自身
の変更をマージ前に検証する」という決定6の狙いが、ここでも実地で機能した
ことになる。決定11の修正後、再度 dry-run job が success で完走すること
を commit ハッシュとともに確認する (下記 Revisit condition (5) 参照)。
**本番の `auto-merge` job (実際にマージを行う側) は、`main` にマージされ
`workflow_run`/`schedule` で起動するまで検証できていない** (D83 と
同じ制約 — この job は `pull_request` イベントでは起動しない設計のため、
PR の段階では検証できない)。

### Revisit condition

(1) PR #191 の観測はリアクションの**集計値**しか確認できておらず投稿者を
断定できていない (上記シグナル c 参照)。実運用で auto-merge が実際に
`chatgpt-codex-connector[bot]` からの 👍 でシグナル c を満たして
マージした最初のケースが出た際、ログ (`review_gate_decision.sh` の出力)
で本当に想定どおり判定できていたかを一度確認する。(2) この PR (workflow
自身の変更) がマージされた時点で、`dry-run-review-gate` job が実際に
GitHub Actions 上で正しく起動・完走したかを確認する — 最初の実地検証に
なる。**2026-09-07 時点の経緯**: commit `f34617bbdc` (P1/P2 修正前) では
success 完走を確認したが、その後 commit `05c7918` (決定10、Claude
フォールバック追加) で実際に failure になり、決定11のバグ (`jq:
invalid JSON`) をマージ前に検出した。決定11の修正 push 後、再度
success で完走することをこの PR のマージ前に確認すること (「workflow
自身の変更をマージ前に検証する」という決定6の目的が実際に機能して
いるかの最終確認)。(3) レビュー解消の反映が最大10分
遅延する制約 (決定5) が実運用で問題になった場合は、
`pull_request_review`/`pull_request_review_thread` トリガの追加を
再検討する。(4) 将来コラボレータが増えてブランチ内 PR の信頼性前提が
崩れる場合、dry-run job の残余リスク (決定6) を再評価する。(5) 決定9
(`@codex review` 自動リクエスト・利用上限緩和) は本番の `auto-merge` job
がまだ実地で走っていないため、**実際に GitHub Actions 上でコメントが
投稿されるか・重複投稿を防げているか・緩和が想定どおり発動するかは
未検証。** この PR がマージされた後、最初に「Codex 未レビューで自動
リクエストが必要になった PR」が現れた際に、コメントが正しく1回だけ
投稿されるか (同じ head SHA への push が続いても再投稿されないか) を
確認すること。(6) 上限メッセージの文言検出は文字列マッチであり脆い
(決定9)。Codex 側の文言が変わった場合は緩和が発動しなくなる (安全側)
ため実害は無いが、気づかないまま `automerge-without-codex` に頼る運用が
続くと不便なので、上限到達が疑われる PR が長期間ブロックされたままに
なっていないかは折に触れて確認するとよい。(7) **決定10 (Claude
フォールバック) を実際に機能させるには、このリポジトリに `@claude` に
応答する GitHub App/Actions workflow (`claude-code-action` 等) を導入
する必要があるかもしれない。** これは本 PR のスコープ外。導入する場合は
実際の応答者のログイン名を確認したうえで `env.CLAUDE_REVIEWER_LOGINS`
に設定すること (ハードコードされた既定値は無い — 決定10参照)。導入
しない場合、「Codex が利用上限に達し、かつ PR が一度もレビューされて
いない」状態は `automerge-without-codex` ラベルを付けるまで止まり続ける
仕様であることを、運用開始後に一度確認しておくとよい (PR #193 で実際に
発生した状態)。(8) **決定11の `AUTO_MERGE_TOKEN` 専用化が実際に Claude
App の反応を引き出せるかは、この PR の範囲では検証できていない**
(`AUTO_MERGE_TOKEN` シークレット自体がこのセッションから登録・確認
できないため)。`AUTO_MERGE_TOKEN` が実際に登録されている環境で、
Claude フォールバックが発動した最初のケースにおいて、投稿された
`@claude` メンションに実際に反応があったかを確認すること。反応が無い
場合、PAT を使っても反応しない別の要因 (トークンの権限スコープ、
Claude App 側のインストール範囲など) がある可能性があり、追加調査が
必要になる。
