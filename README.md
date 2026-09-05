# VeloX

A fast, lightweight web browser built with Rust.

VeloX is an early-stage OSS desktop browser. The current goal is a minimal
but genuinely working foundation — a browser that starts, renders real web
pages and navigates — structured so that speed and features can be built on
top of it, not bolted on.

## Features

- [x] Basic browser window (toolbar + content area)
- [x] URL navigation (typed input, `example.com` is auto-completed to `https://example.com`)
- [x] Back / Forward / Reload
- [x] Tabs (create, switch, close, suspend & resume)
- [x] Omnibox with suggestions ranked from history and bookmarks
- [x] History UI (search, delete)
- [x] Bookmarks (bookmark bar, folders, inline editing)
- [x] Downloads (list, open the download folder)
- [x] Ad & tracker blocking — top-level navigation only; subresource
      blocking is not possible with the current engine API
      (see docs/decisions.md D17)
- [x] Private browsing — whole-app, via `--private` / `VELOX_PRIVATE`;
      a separate private *window* needs multi-window support first
- [x] Tab suspension driven by idle time, tab count and a memory budget
- [x] Performance instrumentation and a benchmark suite (`velox-bench`)
- [x] DevTools (F12)

## Development

VeloX is plain `cargo` — no extra build system.

| Command | Purpose |
|---|---|
| `cargo build` | build |
| `cargo run` | build & launch the browser |
| `cargo test` | run unit tests (URL handling, tab state, IPC protocol) |
| `cargo clippy --all-targets -- -D warnings` | lint |
| `cargo fmt --check` | formatting |

## Build

Rust stable (1.77+) is required.

### Linux

VeloX renders web content with the system WebKitGTK:

```sh
sudo apt install libwebkit2gtk-4.1-dev   # Debian/Ubuntu
cargo build
```

### macOS

No extra dependencies — the system WKWebView is used.

```sh
cargo build
```

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
  creates a GitHub Release with the zip and its SHA-256 attached.

The zip contains the two executables plus README/LICENSE. WebView2 Runtime
must be present on the target machine (it is on Windows 11).

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
- `src/browser/` — engine-independent logic: URL normalization, tab state,
  history, bookmarks, downloads, block list, suspension policy, metrics
- `src/app.rs` — event loop wiring
- `src/config/` — startup configuration
- `assets/logo/` — the VeloX logo; `assets/icon/` — the app icon derived
  from it (`velox.ico` is embedded into `velox.exe` by `build.rs`, the PNG
  is the runtime window icon; see docs/decisions.md D52)

See [docs/architecture.md](docs/architecture.md) for the full design and
[docs/decisions.md](docs/decisions.md) for why wry was chosen over embedding
Servo directly.

## Roadmap

The first roadmap is done (it is the Features list above). What is
being worked on next:

**Performance** (Phase 3)

- Startup, tab creation/switching and page load optimization
- Memory footprint reduction and leak/lifetime auditing
- Background tab CPU and network throttling
- IPC / serialization overhead reduction
- Competitive benchmarks against Chrome / Firefox, and regression detection in CI

**Browser features** (Phase 2, still open)

- Subresource ad/tracker blocking, EasyList / EasyPrivacy list updates
- Site permissions UI, cookie & storage management
- Multiple windows, private windows, session restore and crash recovery
- Settings UI, dark mode, keyboard shortcut management, context menus
- Find in page, view source, save page, print / save as PDF
- Packaging, code signing and notarization for macOS / Windows

Progress is tracked in the Phase 2 (#53) and Phase 3 (#57) epics.

## License

[MIT](LICENSE)
