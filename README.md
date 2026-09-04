# VeloX

A fast, lightweight web browser built with Rust.

VeloX is an early-stage OSS desktop browser. The current goal is a minimal
but genuinely working foundation — a browser that starts, renders real web
pages and navigates — structured so that speed and features can be built on
top of it, not bolted on.

## Features

- [x] Basic browser window (toolbar + content area)
- [x] URL navigation (typed input, `example.com` is auto-completed to `https://example.com`)
- [x] Back
- [x] Forward
- [x] Reload
- [ ] Tabs
- [ ] History UI / bookmarks / downloads
- [ ] Ad & tracker blocking
- [ ] DevTools

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
- `src/browser/` — engine-independent logic: URL normalization, tab state
- `src/app.rs` — event loop wiring
- `src/config/` — startup configuration

See [docs/architecture.md](docs/architecture.md) for the full design and
[docs/decisions.md](docs/decisions.md) for why wry was chosen over embedding
Servo directly.

## Roadmap

- Multiple tabs (the core already talks to a `Tab` abstraction)
- History and bookmarks
- Performance instrumentation: startup time, memory, page load time
- Tab suspension and cache tuning
- Content blocking (ads / trackers)
- Private browsing
- DevTools integration

## License

[MIT](LICENSE)
