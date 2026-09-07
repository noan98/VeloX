# VeloX

A fast, lightweight web browser built with Rust.

VeloX is an OSS desktop browser. It started as a minimal but genuinely
working foundation — a browser that starts, renders real web pages and
navigates — and now covers the everyday browsing surface, structured so that
speed can be built on top of it rather than bolted on.

## Features

### Browsing

- [x] Browser window (toolbar + content area), multiple windows (`Ctrl`/`Cmd`+`N`)
- [x] URL navigation (typed input, `example.com` is auto-completed to `https://example.com`)
- [x] Back / Forward / Reload
- [x] Tabs (create, switch, close, suspend & resume)
- [x] Omnibox with suggestions ranked from history and bookmarks
- [x] History UI (search, delete)
- [x] Bookmarks (bookmark bar, folders, inline editing)
- [x] Downloads (list, open the download folder)
- [x] Session restore — reopens the previous window's tabs on launch
      (crash *detection* is not possible: wry 0.56 exposes no such hook,
      see docs/decisions.md D65)
- [x] Find in page, view source, save page, print / save as PDF
- [x] Context menu (back/forward/reload, copy/paste, search selection,
      open link in new tab or window)
- [x] Settings screen, dark mode, keyboard shortcut listing
- [x] DevTools (F12)

### Privacy & security

- [x] Private windows — opened as separate windows, each with its own
      isolated storage (`--private` / `VELOX_PRIVATE` still open the whole
      app in private mode)
- [x] Ad & tracker blocking for top-level navigation (all platforms). The
      parser understands EasyList / EasyPrivacy rule syntax, but no list data
      is bundled or downloaded — VeloX ships only a small built-in list
      (docs/decisions.md D64)
- [x] **Subresource** ad/tracker blocking — **Windows (WebView2) only**.
      wry 0.56 exposes no request-interception hook for WebKitGTK or
      WKWebView, so macOS/Linux stay at top-level blocking
      (docs/decisions.md D17 / D59)
- [x] Site permissions (origin-scoped store with safe-by-default answers)
- [x] Cookie & site data clearing (whole-profile; per-origin clearing is
      asymmetric across the three engines and was deliberately skipped —
      docs/decisions.md D66)

### Performance

- [x] Tab suspension driven by idle time, tab count and a memory budget
      (off by default)
- [x] Performance instrumentation and a benchmark suite (`velox-bench`),
      including IPC volume/latency accounting
- [x] Performance regression gate in CI, and a dashboard that tracks results
      over time (`scripts/dashboard/`)

Per-platform gaps are recorded in [docs/decisions.md](docs/decisions.md)
rather than left implicit — Windows is the priority platform, and macOS /
Linux are kept at "builds, doesn't break existing features" for now.

## Development

VeloX is plain `cargo` — no extra build system.

| Command | Purpose |
|---|---|
| `cargo build` | build |
| `cargo run` | build & launch the browser |
| `cargo test` | run unit tests (URL handling, tab state, IPC protocol) plus the integration tests, which launch the real binary |
| `cargo clippy --all-targets -- -D warnings` | lint |
| `cargo fmt --check` | formatting |

The integration tests spawn the real `velox` binary, so they need a display
(and, on Linux, a D-Bus session bus — WebKitGTK's web process will not start
without one). Without those they print why and self-skip, which keeps
`cargo test` green on a headless developer machine. To actually run them
the way CI does:

```sh
xvfb-run -a --server-args="-screen 0 1280x900x24" \
  dbus-run-session -- cargo test
```

See docs/decisions.md D46 / D47 for why the skip is a runtime check rather
than `#[ignore]`, and how CI is made to fail loudly if that setup ever breaks
instead of silently skipping.

## Build

Rust stable (1.77+) is required.

### Linux

VeloX renders web content with the system WebKitGTK:

```sh
sudo apt install libwebkit2gtk-4.1-dev   # Debian/Ubuntu
cargo build
```

#### Release build via GitHub Actions

`.github/workflows/release-linux.yml` builds `velox` (and `velox-bench`) in
release mode on an `ubuntu-latest` runner and packages them into a tar.gz —
the same two triggers as the Windows release below (manual `workflow_dispatch`
or a `v*` tag push). This is a minimal, unpackaged build (no AppImage/deb);
see docs/decisions.md D70 for why native packaging formats are out of scope
for now.

### macOS

No extra dependencies — the system WKWebView is used.

```sh
cargo build
```

There is currently no macOS release-build workflow in CI (no `.dmg`/`.app`
packaging, no GitHub Release publishing) — see docs/decisions.md D70. Per
CLAUDE.md's OS priority policy, macOS support stays at "builds, doesn't
break existing features" until Windows quality is established; a dedicated
release workflow can follow later using the same pattern as
`release-windows.yml` / `release-linux.yml`.

### Windows

Uses WebView2, which is preinstalled on Windows 11 (and modern Windows 10).

```powershell
cargo build
```

#### Release build via GitHub Actions

`.github/workflows/release-windows.yml` builds `velox.exe` (and
`velox-bench.exe`) in release mode on a `windows-latest` runner:

- **Manual**: Actions → "Release (Windows)" → "Run workflow". The zip is
  attached to the run as the `velox-windows-x86_64` artifact.
- **Tag push**: `git tag v0.1.0 && git push origin v0.1.0` additionally
  creates a GitHub Release with the zip and its SHA-256 attached. The same
  tag also triggers `release-linux.yml`, which attaches a Linux tar.gz to
  the same Release. Both workflows verify the tag (`vX.Y.Z`) matches the
  `version` in `Cargo.toml` and fail the build if it doesn't, so a Release
  can't be published under a version that doesn't match its own artifacts.

The zip contains the two executables plus README/LICENSE. WebView2 Runtime
must be present on the target machine (it is on Windows 11).

#### Code signing

`velox.exe` / `velox-bench.exe` are **currently unsigned** — this project
does not (yet) hold a code-signing certificate, so Windows SmartScreen may
warn on first run. `release-windows.yml` has an opt-in Authenticode signing
step (via Azure Trusted Signing) that activates automatically once the
required repository secrets/variables are configured; see
[docs/windows-code-signing.md](docs/windows-code-signing.md) for what's
needed, what was investigated, and why macOS signing/notarization is out of
scope for now (Issue #42 / docs/decisions.md D73).

#### Verifying a release download

Every release asset (Windows zip, Linux tar.gz) ships with a `.sha256` file
next to it. Until signing is enabled (see above), this is the only way to
confirm a downloaded archive matches what CI built. Compare the computed
hash against the value in the `.sha256` file:

```powershell
# Windows (PowerShell)
Get-FileHash .\velox-<version>-windows-x86_64.zip -Algorithm SHA256
```

```sh
# Linux / macOS
sha256sum -c velox-<version>-linux-x86_64.tar.gz.sha256
```

## Run

```sh
cargo run
```

A window opens with back / forward / reload buttons and an address bar.
Type a URL (with or without `https://`) and press Enter.

## Architecture

```
UI (toolbar webview + native window)
        ↓ IPC / events
Browser application (event loop, per-tab state)
        ↓
Navigation (URL normalization, history commands)
        ↓
Web engine (wry → WebKitGTK / WKWebView / WebView2)
```

- `src/ui/` — window, layout, toolbar (the toolbar is a small HTML page in a
  dedicated webview, isolated from page content)
- `src/browser/` — engine-independent logic and the main target of the unit
  tests: URL normalization, tab and window state, history, bookmarks,
  downloads, block lists, site permissions and site data, session
  persistence, settings, shortcuts, context menus, find, print, save page,
  view source, suspension policy, metrics and benchmarking
- `src/app.rs` — event loop wiring
- `src/config/` — startup configuration
- `assets/logo/` — the VeloX logo; `assets/icon/` — the app icon derived
  from it (`velox.ico` is embedded into `velox.exe` by `build.rs`, the PNG
  is the runtime window icon; see docs/decisions.md D52)

See [docs/architecture.md](docs/architecture.md) for the full design and
[docs/decisions.md](docs/decisions.md) for why wry was chosen over embedding
Servo directly — and for every subsequent design decision, including the
per-platform gaps deliberately left open.

Performance work has its own documentation:

| Document | Contents |
|---|---|
| [docs/performance-targets.md](docs/performance-targets.md) | measurement environment, targets T1–T4, and every result measured so far |
| [docs/benchmarking.md](docs/benchmarking.md) | how to run `velox-bench`, and the pitfalls that invalidate a measurement |
| [docs/profiling.md](docs/profiling.md) | perf / heaptrack workflows |
| [docs/memory-analysis.md](docs/memory-analysis.md) | where the memory actually goes |
| [docs/performance-dashboard.md](docs/performance-dashboard.md) | tracking results over time |

## Roadmap

The first two roadmaps are largely done (they are the Features list above).

### Performance (Phase 3)

Phase 3 is about *proving* what VeloX is fast at, not just asserting it, so
several of its items ended in a measured conclusion rather than a code
change. All numbers below were measured on **Linux (WebKitGTK)** — Windows
(WebView2) figures do not exist yet.

Measured and settled:

- **Tab switching** — 0.40ms with 20 tabs, two orders of magnitude under the
  100ms target. The cost of tab work is dominated by spawning a web process,
  not by tab count (docs/decisions.md D57)
- **Startup** — the bottleneck is tao/GTK init and WebKitGTK webview
  creation; VeloX's own work is ~0.1ms, so there is nothing here for VeloX
  to shorten (D43)
- **Background tab CPU** — already suppressed ~99.4% by the engine; no
  VeloX-side throttling was added (D58)
- **Background tab network** — measured: ordinary polling intervals are *not*
  throttled when a tab is backgrounded (only sub-second timers are). Full
  suppression only comes from tab suspension (D80)
- **IPC** — measurement added, and one real find fixed: the history panel was
  re-sent on every page event even while closed (-98.5%). The tab strip's
  full re-send was left alone at a measured worst case of 3.5ms (D81)
- **Memory** — the leak audit found and fixed an unbounded map, and long-run
  growth under repeated tab churn is bounded (D79). Footprint at 20 tabs is
  **+245% versus Chromium with the default settings**, dropping to **+20%
  once tab suspension is enabled** — still short of the +10% target either
  way. Suspension being off by default is exactly why that gap is still open
  (D48 / D56)

Still open:

- Browser state / event dispatch, serialization / allocation, and page load
  optimization
- Whether tab suspension should be **on** by default (it cuts 20-tab memory
  by 65%, but is off today)
- Windows performance measurement — no runner is set up for it yet

### Remaining browser features (Phase 2)

- Download progress and mid-transfer cancellation
- Session restore across *multiple* windows
- Reassigning keyboard shortcuts from the settings screen
- Packaging, code signing and notarization for macOS / Windows

Progress is tracked in the Phase 2 (#53) and Phase 3 (#57) epics.

## License

[MIT](LICENSE)
