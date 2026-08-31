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

## D8: DevTools — feature gating, shortcut delivery, and the IPC trust boundary

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
(rather than inlining `self.content.open_devtools()` at each call site) so
that #2's planned move to `Vec<WebView>` only has to change what "active"
resolves to here, not at every caller.
