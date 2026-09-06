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
