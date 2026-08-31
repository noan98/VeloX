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
| Content-blocking rule matching | `browser::blocklist::FilterList` |
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

## Content blocking

`browser::blocklist::FilterList` is a small EasyList-subset parser/matcher
(`||domain^` block rules, `@@||domain^` exceptions) built at startup from an
embedded default list (`browser/default_blocklist.txt`) plus an optional
user file (`Config::extra_blocklist_path`). It is pure logic with no engine
dependency, so it is unit-tested directly without a webview.

`app::run` builds one `FilterList`, wraps it in an `Arc`, and hands it to
`BrowserWindow::new`, which closes over it in the content webview's
`with_navigation_handler` callback:

```
content webview navigates to `url`
        │
        ▼
FilterList::is_blocked(url)?  (host lookup + domain-suffix match)
   │ yes                              │ no
   ▼                                  ▼
return false (navigation refused)     UserEvent::NavigationStarted(url)
UserEvent::NavigationBlocked(url)     (existing flow)
        │
        ▼
Tab::on_navigation_blocked  →  toolbar block-count badge
```

This only covers **main-frame navigation** — wry 0.56 exposes no hook for
subresource requests (images/scripts/XHR), so ad/tracker resources loaded
*within* an allowed page are not filtered today. See docs/decisions.md D8
for the platform-by-platform investigation and why that gap is not closed
in this iteration.

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
