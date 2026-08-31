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
| Page rendering, network, cookies | web engine (wry) |
| Session history (back/forward) | web engine (wry) |

VeloX deliberately does **not** duplicate the engine's session history. The
engine already tracks redirects, `pushState`, anchors etc.; a parallel Rust
history would drift from reality. `Tab` mirrors only what the UI needs
(current URL, loading flag).

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

Implemented in `browser::metrics` (see D8 in `docs/decisions.md`), gated by
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
  `velox[perf] page_load …` line per load.
- **Memory**: `metrics::sample_process_tree_rss(pid)` walks the whole
  process tree (WebKit's network/render helpers included) and sums RSS. It
  is a standalone public function with no dependency on `Config` or the
  running app — callable on demand from anywhere (e.g. Issue #5's tab
  suspension work, to compare RSS before/after suspending a tab). When
  `perf_rss_interval` is set, `app::run` also spawns a background thread
  that samples it periodically and logs `velox[perf] rss …` lines.
  Implementation reads `/proc` directly on Linux (no extra dependency);
  other Unix falls back to parsing `ps` output; Windows is not implemented
  yet (`RssError::Unsupported`).
- **Tab switch time**: once tabs exist, activation is a single code path in
  `BrowserWindow`, so it can be timed trivially — not implemented yet.
- The `Config` struct is the home for these toggles; `Config::from_env`
  layers the environment-variable overrides onto `Config::default`.

When metrics are off, `app::run` never spawns the RSS thread and every
checkpoint is a single `Option`-is-`None` check with no `Instant::now()`
call — the disabled path stays effectively free.

The layering matters more than any single hook: measurements attach to the
application layer, so swapping or tuning the engine below does not invalidate
them.
