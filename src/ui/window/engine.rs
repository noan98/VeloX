//! エンジン (WebKitGTK / WKWebView / WebView2) ごとに実装が分かれる
//! webview 操作。呼び出し側が `#[cfg]` を書かずに済むよう、各関数は
//! 対応していないプラットフォームでは何もしない版を持つ。

use tao::event_loop::EventLoopProxy;
use tao::window::Window;
use wry::{WebContext, WebView, WebViewBuilder};

use crate::app::UserEvent;
use crate::browser::suspension::{BackgroundMemoryTarget, SuspendMechanism};
use crate::browser::{TabId, WindowId};

/// Start a [`WebViewBuilder`], sharing `context` when one is given.
///
/// See docs/decisions.md D49: in non-private mode `BrowserWindow` holds one
/// `WebContext` shared by the toolbar and every tab's content webview, so
/// `wry` stops creating a fresh `WebKitWebProcess`/`WebKitNetworkProcess`
/// pair per webview. `context` is `None` in private mode (see
/// `BrowserWindow::context`'s doc comment for why sharing is skipped there
/// rather than attempted and ignored) — `WebViewBuilder::new()` reproduces
/// today's behavior in that case.
pub(super) fn new_webview_builder(context: Option<&mut WebContext>) -> WebViewBuilder<'_> {
    match context {
        Some(context) => WebViewBuilder::new_with_web_context(context),
        None => WebViewBuilder::new(),
    }
}

/// Ask WebKitGTK to put the webview `builder` is about to create into the
/// same `WebKitWebProcess` as `related` (docs/decisions.md D54).
///
/// D49's shared `WebContext` merged the per-webview `WebKitNetworkProcess`
/// but left one `WebKitWebProcess` per webview (`docs/memory-analysis.md`
/// §9.3): WebKitGTK only shares a web process between views that are
/// explicitly *related*, which wry exposes as
/// `WebViewBuilderExtUnix::with_related_view`. A content webview built
/// after the first (a newly opened tab, a suspended tab rebuilt on resume)
/// is therefore related to one that is already alive — which one, and
/// whether at all, is decided by [`pick_process_group`](super::pick_process_group) — so a window's
/// tabs end up in a handful of shared `WebKitWebProcess`es instead of one
/// each.
///
/// The toolbar is deliberately *never* passed here: it is VeloX's trusted
/// UI (docs/decisions.md D18/D23 draw the IPC trust boundary around it),
/// and sharing a renderer process with page content would put untrusted
/// pages on the same side of that boundary as the toolbar's own DOM.
///
/// `webkit2gtk::WebView` (the type `with_related_view` wants) is obtained
/// from wry's own `WebViewExtUnix::webview` accessor on the existing
/// `wry::WebView`, so no new dependency crate is needed and nothing outside
/// this function ever names a WebKitGTK type — the layering rule in D20
/// (`browser::` never sees `wry`/`gtk`) is untouched, and `src/ui/` already
/// depends on the platform backend.
///
/// On every other platform this is the identity: wry has no equivalent
/// there (WKWebView shares its content process pool per
/// `WKProcessPool`/configuration automatically, WebView2 per environment),
/// and D48/D49 measured this problem on WebKitGTK only.
#[cfg(gtk_backend)]
pub(super) fn with_related_content_view<'a>(
    builder: WebViewBuilder<'a>,
    related: Option<&WebView>,
) -> WebViewBuilder<'a> {
    use wry::{WebViewBuilderExtUnix, WebViewExtUnix};
    match related {
        Some(related) => builder.with_related_view(related.webview()),
        None => builder,
    }
}

#[cfg(not(gtk_backend))]
pub(super) fn with_related_content_view<'a>(
    builder: WebViewBuilder<'a>,
    _related: Option<&WebView>,
) -> WebViewBuilder<'a> {
    builder
}

/// WebKitGTK's `is-playing-audio` for `webview` — see
/// [`BrowserWindow::is_playing_audio`](super::BrowserWindow::is_playing_audio). Guarded by `has_property` so a
/// WebKitGTK build without the property (it has existed since 2.8, so this
/// is purely defensive) reads as "not playing" instead of a GLib panic.
#[cfg(gtk_backend)]
pub(super) fn webview_is_playing_audio(webview: &WebView) -> bool {
    use gtk::glib::prelude::*;
    use wry::WebViewExtUnix;
    let inner = webview.webview();
    inner.has_property("is-playing-audio", Some(bool::static_type()))
        && inner.property::<bool>("is-playing-audio")
}

/// Windows reads the same thing from WebView2 (Issue #247). Until then this
/// fell through to the `false` below, so the "never auto-suspend a tab that
/// is playing" protection (D56) had never applied on the OS VeloX treats as
/// its priority target.
#[cfg(windows)]
pub(super) fn webview_is_playing_audio(webview: &WebView) -> bool {
    crate::ui::webview2_suspend::is_playing_audio(webview)
}

#[cfg(not(any(windows, gtk_backend)))]
pub(super) fn webview_is_playing_audio(_webview: &WebView) -> bool {
    false
}

/// Tell the engine what `webview` may use, now that it has moved on or off
/// screen (Issue #242).
///
/// Never fails the activation: a runtime too old for `ICoreWebView2_19`
/// simply means the hint is not available, and the tab behaves exactly as it
/// did before #242. Logged rather than propagated, the `log_failure` posture
/// of `app.rs`.
#[cfg(windows)]
pub(super) fn apply_memory_target(webview: &WebView, target: BackgroundMemoryTarget) {
    if let Err(err) = crate::ui::webview2_suspend::set_memory_usage_target(webview, target) {
        eprintln!(
            "velox: メモリ目標 ({}) を設定できませんでした (#242): {err}",
            target.as_str()
        );
    }
}

/// No engine-level memory hint exists off Windows, so this is inert
/// (Issue #242) — the same position Linux is in for freezing
/// (docs/decisions.md D120 決定5, Issue #240).
#[cfg(not(windows))]
pub(super) fn apply_memory_target(_webview: &WebView, _target: BackgroundMemoryTarget) {}

/// The mechanism this window will *actually* use, given the configured one
/// (Issue #176 Stage 3, docs/decisions.md D138 決定3).
///
/// **`Freeze` is a request, not a guarantee.** [`freeze_webview`] falls
/// back to discarding whenever the engine has no freeze path, and off
/// Windows there is none at all. That fallback is fine for suspending —
/// the tab still goes — but it is *not* fine for the memory budget, which
/// has to know whether a suspension will return memory before it decides
/// how many to order (`browser::suspension::plan`). Asking for `Freeze` and
/// silently getting `Discard` would switch the memory signal off on a build
/// where suspension does reclaim, which is the opposite mistake to the one
/// D138 決定1 fixes.
///
/// So the answer is resolved **once, here, at window creation**, from the
/// same side-effect-free probe the Stage 2 diagnostic logs
/// (`webview2_suspend::support`) — the runtime cannot gain or lose
/// `ICoreWebView2_3` while the process runs. Storing the resolved value
/// also keeps `suspend_tab` from retrying, and log-failing, a dispatch that
/// is known to be impossible.
#[cfg(windows)]
pub(super) fn effective_suspend_mechanism(
    configured: SuspendMechanism,
    webview: &WebView,
) -> SuspendMechanism {
    if configured == SuspendMechanism::Freeze
        && !crate::ui::webview2_suspend::support(webview).try_suspend
    {
        eprintln!(
            "velox: VELOX_SUSPEND_MECHANISM=freeze を指定されたが、この \
WebView2 Runtime は TrySuspend を持っていない — discard で続行する (#243)"
        );
        return SuspendMechanism::Discard;
    }
    configured
}

/// Off Windows there is no engine-level freeze at all (see
/// [`freeze_webview`]), so a `Freeze` request always ends up discarding.
/// Resolving that here rather than per suspension is what lets the memory
/// budget keep working on Linux/macOS even when the knob says `freeze`
/// (docs/decisions.md D138 決定3).
#[cfg(not(windows))]
pub(super) fn effective_suspend_mechanism(
    _configured: SuspendMechanism,
    _webview: &WebView,
) -> SuspendMechanism {
    SuspendMechanism::Discard
}

/// Ask the engine to freeze `webview` in place (Issue #243), returning
/// whether the request was *dispatched*. `false` means the caller must fall
/// back to discarding the webview; `true` means the answer will arrive as
/// [`UserEvent::TabFreezeFinished`], which discards it if the engine
/// declined.
///
/// Windows only — `ICoreWebView2_3::TrySuspend`, confirmed present on the
/// target runtime by docs/decisions.md D120 決定1.
#[cfg(windows)]
pub(super) fn freeze_webview(
    webview: &WebView,
    proxy: &EventLoopProxy<UserEvent>,
    window_id: WindowId,
    id: TabId,
) -> bool {
    match crate::ui::webview2_suspend::try_suspend(webview, proxy.clone(), window_id, id) {
        Ok(()) => true,
        Err(err) => {
            // Overwhelmingly likely to be `E_NOINTERFACE` on a WebView2
            // Runtime older than `ICoreWebView2_3`. Logged once per attempt
            // rather than once per process: this is the path a measurement
            // needs to see, and if it fires at all the `freeze` arm is
            // silently measuring `discard`.
            eprintln!("velox: tab {id:?} の freeze を発行できませんでした (#243): {err}");
            false
        }
    }
}

/// No engine-level freeze exists off Windows, so a `Freeze` request always
/// falls back to discarding (Issue #243). Linux's `WebKitMemoryPressure
/// Settings` is unreachable through wry 0.56 — docs/decisions.md D120 決定5,
/// Issue #240.
#[cfg(not(windows))]
pub(super) fn freeze_webview(
    _webview: &WebView,
    _proxy: &EventLoopProxy<UserEvent>,
    _window_id: WindowId,
    _id: TabId,
) -> bool {
    false
}

/// Undo [`freeze_webview`] for a tab whose webview survived suspension
/// (Issue #243). Never fails the resume: a webview that was not actually
/// frozen resumes to a no-op, and an error here would only mean the tab
/// wakes the way it always did, so it is logged rather than propagated
/// (the `log_failure` posture of `app.rs`).
#[cfg(windows)]
pub(super) fn thaw_webview(webview: &WebView, id: TabId) {
    if let Err(err) = crate::ui::webview2_suspend::resume(webview) {
        eprintln!("velox: tab {id:?} の resume に失敗しました (#243): {err}");
    }
}

/// A tab cannot have been frozen off Windows ([`freeze_webview`] always
/// declines there), so there is nothing to undo (Issue #243).
#[cfg(not(windows))]
pub(super) fn thaw_webview(_webview: &WebView, _id: TabId) {}

/// Windows-only: `wry::WebViewBuilderExtWindows::with_default_context_menus`
/// disables WebView2's native context menu at the engine level. See the
/// call site in [`content_webview_builder`] for why this exists alongside
/// (not instead of) `context_menu_script`'s `event.preventDefault()`. A
/// plain identity function on every other platform, so the call site never
/// needs its own `#[cfg]`.
#[cfg(windows)]
pub(super) fn disable_default_context_menus(builder: WebViewBuilder<'_>) -> WebViewBuilder<'_> {
    use wry::WebViewBuilderExtWindows;
    builder.with_default_context_menus(false)
}

#[cfg(not(windows))]
pub(super) fn disable_default_context_menus(builder: WebViewBuilder<'_>) -> WebViewBuilder<'_> {
    builder
}

/// Create the container every webview of `window` is placed in on
/// Linux/BSD: tao windows are gtk windows and wry webviews are gtk widgets,
/// so every webview goes in this one `gtk::Fixed` (positioned via
/// `with_bounds`/`set_bounds`), created once per window and reused for
/// every tab opened afterwards. Everywhere else wry supports true child
/// webviews directly, so no such container exists.
#[cfg(gtk_backend)]
pub(super) fn create_webview_host(
    window: &Window,
) -> Result<gtk::Fixed, Box<dyn std::error::Error>> {
    use gtk::prelude::{BoxExt, WidgetExt};
    use tao::platform::unix::WindowExtUnix;

    let vbox = window
        .default_vbox()
        .ok_or("tao window was created without its default gtk vbox")?;
    let fixed = gtk::Fixed::new();
    vbox.pack_start(&fixed, true, true, 0);
    fixed.show_all();
    Ok(fixed)
}

/// Attach a webview built by `builder` to the window: as a gtk widget in
/// the window's shared `gtk::Fixed` on Linux/BSD. Used both by
/// `BrowserWindow::new` (toolbar + first tab) and by
/// `BrowserWindow::open_tab` (every tab opened afterwards).
///
/// A free function taking `host` explicitly rather than a `&self` method —
/// see the call site in `BrowserWindow::open_tab` — because `builder` may
/// already hold a `&mut` borrow of `self.context` (docs/decisions.md D49);
/// a `&self` method here would borrow the whole struct and conflict with
/// that, whereas a disjoint `&self.host` argument does not.
#[cfg(gtk_backend)]
pub(super) fn attach_webview(
    host: &gtk::Fixed,
    builder: WebViewBuilder<'_>,
) -> wry::Result<WebView> {
    use wry::WebViewBuilderExtUnix;
    builder.build_gtk(host)
}

#[cfg(not(gtk_backend))]
/// Everywhere else wry supports true child webviews directly. See the
/// Linux/BSD `attach_webview` above for why this takes `window` explicitly.
pub(super) fn attach_webview(window: &Window, builder: WebViewBuilder<'_>) -> wry::Result<WebView> {
    builder.build_as_child(window)
}
