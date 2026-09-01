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
