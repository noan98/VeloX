//! Build script: on Windows, embed `assets/icon/velox.ico` into the
//! executable as its resource icon so Explorer, the taskbar and the Start
//! menu show the VeloX logo (docs/decisions.md D52). The *window* icon
//! shown in the title bar is set at runtime from `assets/icon/velox-128.png`
//! instead (`ui::window::load_window_icon`), which works on every platform.
//!
//! Everything else is a no-op: only the `cfg(windows)` build-dependency on
//! `winresource` is pulled in, so Linux/macOS builds are unaffected.
//!
//! また、WebKitGTK バックエンドを使うターゲット (Linux / BSD) で
//! `cfg(gtk_backend)` を立てる。ソース中で同じ 5 つの `target_os` を
//! 列挙した `cfg(any(...))` を繰り返さないためのエイリアス。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon/velox.ico");
    emit_gtk_backend_cfg();
    embed_windows_icon();
}

/// wry が WebKitGTK バックエンドを使う `target_os` の一覧。Cargo.toml の
/// `gtk` 依存の `[target.'cfg(any(...))'.dependencies]` と一致させること
/// (Cargo.toml 側ではカスタム cfg を使えないため列挙が残る)。
const GTK_TARGET_OSES: [&str; 5] = ["linux", "dragonfly", "freebsd", "openbsd", "netbsd"];

/// ビルド対象 (ホストではなく `--target`) が WebKitGTK 系なら
/// `cfg(gtk_backend)` を有効にする。build script 自身の `cfg!` はホストを
/// 指すので、Cargo が渡す `CARGO_CFG_TARGET_OS` を見る。
fn emit_gtk_backend_cfg() {
    println!("cargo:rustc-check-cfg=cfg(gtk_backend)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if GTK_TARGET_OSES.contains(&target_os.as_str()) {
        println!("cargo:rustc-cfg=gtk_backend");
    }
}

#[cfg(windows)]
fn embed_windows_icon() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon/velox.ico");
    // A missing/broken icon must not break the build: warn and ship an
    // icon-less exe rather than fail the release workflow over cosmetics.
    if let Err(err) = res.compile() {
        println!("cargo:warning=failed to embed the Windows icon resource: {err}");
    }
}

#[cfg(not(windows))]
fn embed_windows_icon() {}
