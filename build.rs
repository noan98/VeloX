//! Build script: on Windows, embed `assets/icon/velox.ico` into the
//! executable as its resource icon so Explorer, the taskbar and the Start
//! menu show the VeloX logo (docs/decisions.md D52). The *window* icon
//! shown in the title bar is set at runtime from `assets/icon/velox-128.png`
//! instead (`ui::window::load_window_icon`), which works on every platform.
//!
//! Everything else is a no-op: only the `cfg(windows)` build-dependency on
//! `winresource` is pulled in, so Linux/macOS builds are unaffected.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon/velox.ico");
    embed_windows_icon();
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
