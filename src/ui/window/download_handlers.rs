//! ダウンロードと「名前を付けて保存」の保存先決定・イベント送信
//! (Issue #16/#46、docs/decisions.md D28/D53/D76)。

use std::path::{Path, PathBuf};

use tao::event_loop::EventLoopProxy;
use wry::WebViewBuilder;

use crate::app::UserEvent;
use crate::browser::downloads;
use crate::browser::WindowId;

/// Whether wry's download-completed callback can report a **successful**
/// download as failed because some *earlier* download failed (Issue #128,
/// docs/decisions.md D140).
///
/// `true` only on WebKitGTK, and that is checked against all three of wry
/// 0.56.1's backends rather than assumed from
/// [`DOWNLOAD_HANDLERS_PER_CONTEXT`]:
///
/// - **WebKitGTK** (`webkitgtk/web_context.rs:315`): one
///   `failed: Rc<RefCell<bool>>` per `register_download_handler` call,
///   created *outside* `connect_download_started`, set by every download's
///   `connect_failed` and never cleared. Every later `connect_finished`
///   reports `(!failed)` for both the flag and the path. **Poisoned.**
/// - **WebView2** (`webview2/mod.rs:896`): `success` is
///   `state == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED`, read from *that*
///   download operation. Nothing is shared between downloads.
/// - **WKWebView** (`wkwebview/download.rs`): `download_did_finish` passes
///   `true`, `download_did_fail` passes `false`, per download. Nothing is
///   shared. (`path` is always `None` there — D28 — which is a separate
///   matter.)
///
/// This is deliberately its own constant rather than a reuse of
/// [`DOWNLOAD_HANDLERS_PER_CONTEXT`]. The two ask different questions
/// (*where* handlers are registered vs. *whether a flag is shared*) and
/// happen to have the same answer today; tying them together would mean a
/// future change to one silently moving the other.
pub const DOWNLOAD_SUCCESS_FLAG_IS_SHARED: bool = cfg!(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
));

/// Whether wry registers a webview's download handlers on the
/// [`WebContext`](wry::WebContext) the webview is built against rather than on the webview
/// itself. `true` on WebKitGTK, where `with_download_started_handler` /
/// `with_download_completed_handler` end up in
/// `WebContext::register_download_handler` →
/// `WebKitWebContext::connect_download_started` (wry 0.56.1,
/// `src/webkitgtk/mod.rs` → `webkitgtk/web_context.rs`); `false` on
/// WKWebView (a per-webview download delegate) and WebView2 (a
/// per-controller `add_DownloadStarting`). See docs/decisions.md D53.
pub(super) const DOWNLOAD_HANDLERS_PER_CONTEXT: bool = cfg!(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
));

/// Which webview(s) VeloX's download handlers are registered on — the
/// output of [`download_handler_host`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadHandlerHost {
    /// Register once, on the first webview built against the shared
    /// `WebContext` (the toolbar, built before any tab in
    /// `BrowserWindow::new`), and on no content webview.
    SharedContext,
    /// Register on every content webview, never on the toolbar.
    EachContentWebview,
}

/// Decide where VeloX's download handlers go (docs/decisions.md D53).
///
/// `per_context` is [`DOWNLOAD_HANDLERS_PER_CONTEXT`] in production (a
/// parameter so the decision table below is unit-testable on every
/// platform); `private` is this window's own private-browsing flag (D14,
/// D74).
///
/// - `per_context && !private` (WebKitGTK, normal mode): the toolbar and
///   every tab share one `WebContext` (D49), and wry appends *each*
///   webview's handlers to that context's `download-started` signal. Two
///   things go wrong if the handlers are attached per content webview as
///   they are elsewhere:
///   1. wry's `WebViewAttributes::default()` already carries a
///      `download_started_handler: Some(|_, _| true)`, so even the toolbar
///      webview (which registers no handler of its own) adds a
///      `download-started` listener. `WebKitDownload::decide-destination`
///      uses `g_signal_accumulator_true_handled`, so the *first* connected
///      listener that returns `true` stops the rest — and since the
///      toolbar is built first, its do-nothing default wins every time:
///      VeloX's real handler never runs, `UserEvent::DownloadStarted` is
///      never sent, the downloads panel stays empty, and the file lands
///      in wry's own default directory (bypassing `VELOX_DOWNLOAD_DIR`
///      and `downloads::prepare_destination`'s sanitizing).
///   2. `WebKitDownload::finished` has no such accumulator, so every
///      content webview ever built on the context — closed tabs included,
///      nothing ever disconnects — fires `UserEvent::DownloadCompleted`
///      once per download: N tabs, N events.
///
///   Registering exactly once, on the toolbar (the first webview on the
///   shared context), makes VeloX's handler the first `decide-destination`
///   listener and the only `finished` listener. Content webviews still
///   add wry's default started handler each (unavoidable with wry 0.56.1's
///   builder API), but it is never reached.
/// - Otherwise (private mode: every webview gets its own ephemeral
///   context per D15; or WKWebView/WebView2: per-webview delegates): the
///   handlers must live on each content webview, exactly as before D53.
pub(super) fn download_handler_host(private: bool, per_context: bool) -> DownloadHandlerHost {
    if per_context && !private {
        DownloadHandlerHost::SharedContext
    } else {
        DownloadHandlerHost::EachContentWebview
    }
}

/// Attach VeloX's download handlers (Issue #16, docs/decisions.md D28) to
/// `builder`. Called from exactly one place per webview kind, chosen by
/// [`download_handler_host`].
///
/// Both handlers fire on every backend VeloX ships on (confirmed from wry
/// 0.56.1's source — see D28). `with_download_started_handler` must decide
/// synchronously (return `bool`) and may rewrite the destination `PathBuf`
/// in place, so the destination resolution itself
/// (`browser::downloads::prepare_destination`, which sanitizes the
/// suggested file name and avoids same-name collisions) has to run right
/// here, not after a round trip through the event loop — the same
/// synchronous-decision-then-async-notify shape the navigation handler in
/// [`content_webview_builder`](super::content_webview::content_webview_builder) uses for content blocking. VeloX always
/// accepts every download (`true`, matching wry's own default), so this
/// only ever *redirects* a download, never blocks one.
///
/// `download_dir_override` is `Config::download_dir_override` as of window
/// creation (Issue #30's settings screen, Downloads tab — see
/// docs/decisions.md D67); `None` keeps the pre-#30 behavior
/// (`VELOX_DOWNLOAD_DIR`/the platform default, via
/// `browser::downloads::resolve_download_dir_with_override`).
pub(super) fn with_download_handlers<'a>(
    builder: WebViewBuilder<'a>,
    own_id: WindowId,
    proxy: &EventLoopProxy<UserEvent>,
    download_dir_override: Option<String>,
) -> WebViewBuilder<'a> {
    let download_started_proxy = proxy.clone();
    let download_completed_proxy = proxy.clone();
    builder
        .with_download_started_handler(move |url, destination| {
            let suggested_name = file_name_or(destination, String::new());
            // Prefer VeloX's own directory resolution (the settings
            // screen's override, then `VELOX_DOWNLOAD_DIR`, see
            // docs/decisions.md D28/D67) over whatever default wry already
            // picked; fall back to wry's own
            // suggested directory only if neither the override nor an
            // environment variable could be resolved at all, rather than
            // failing the download outright.
            let dir =
                downloads::resolve_download_dir_with_override(download_dir_override.as_deref())
                    .or_else(|| destination.parent().map(Path::to_path_buf))
                    .unwrap_or_else(|| PathBuf::from("."));
            let final_path = match downloads::prepare_destination(&dir, &suggested_name) {
                Ok(path) => path,
                Err(err) => {
                    eprintln!(
                        "velox: failed to prepare download destination in {dir:?}: {err}; \
                         refusing this download"
                    );
                    return false;
                }
            };
            let file_name = file_name_or(&final_path, suggested_name);
            *destination = final_path.clone();
            let _ = download_started_proxy.send_event(UserEvent::DownloadStarted {
                window_id: own_id,
                url,
                file_name,
                destination: final_path,
                started_at: unix_now(),
            });
            true
        })
        .with_download_completed_handler(move |url, path, success| {
            let _ = download_completed_proxy.send_event(UserEvent::DownloadCompleted {
                window_id: own_id,
                url,
                path,
                success,
            });
        })
}

/// Current time as a unix timestamp (seconds); `0` on a clock set before
/// 1970, which should never happen in practice. A separate copy of
/// `app::now_unix` (private there) — `ui::window` needs a timestamp at the
/// moment a download is accepted, inside a wry callback that has no access
/// to `app.rs`'s state, so duplicating this trivial conversion is simpler
/// than threading a clock dependency through.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 保存対象のタブに生きた webview がないときに「名前を付けて保存」
/// (Issue #46) が報告する理由。Windows / 非 Windows の両実装で共有する。
pub(super) const SAVE_PAGE_NO_WEBVIEW_MESSAGE: &str = "ページが表示されていないため保存できません";

/// `path` のファイル名部分 (表示用の lossy 変換)。ファイル名が取れない
/// ときは `fallback`。
pub(super) fn file_name_or(path: &Path, fallback: String) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(fallback)
}

/// [`UserEvent::SavePageStarted`] を送る (`started_at` は現在時刻)。
pub(super) fn send_save_page_started(
    proxy: &EventLoopProxy<UserEvent>,
    window_id: WindowId,
    url: String,
    file_name: String,
    destination: PathBuf,
) {
    let _ = proxy.send_event(UserEvent::SavePageStarted {
        window_id,
        url,
        file_name,
        destination,
        started_at: unix_now(),
    });
}

/// Report a "名前を付けて保存" (Issue #46) failure that happened *before* a
/// real destination could ever be chosen (no live webview for the tab, the
/// native Save-As dialog itself failing, or the fallback download directory
/// being unwritable) — every such case in
/// [`BrowserWindow::request_save_page`](super::BrowserWindow::request_save_page)'s
/// two `#[cfg]`'d bodies.
///
/// Sends both [`UserEvent::SavePageStarted`] and
/// [`UserEvent::SavePageFinished`] back to back (with a shared, matching
/// `PathBuf::new()` placeholder destination — `DownloadStore::resolve_completion`'s
/// exact-destination-match branch is what pairs them back up) rather than
/// only `SavePageFinished`: this is the difference between the failure
/// showing up as a `DownloadState::Failed` row in the Downloads panel (with
/// `reason` visible, satisfying the issue's "エラー時に原因を表示できる"
/// acceptance criterion) and it silently having no `DownloadEntry` at all,
/// since `SavePageFinished` alone has nothing to resolve against.
pub(super) fn report_save_page_failure(
    proxy: &EventLoopProxy<UserEvent>,
    window_id: WindowId,
    url: String,
    file_name: String,
    reason: String,
) {
    send_save_page_started(proxy, window_id, url.clone(), file_name, PathBuf::new());
    let _ = proxy.send_event(UserEvent::SavePageFinished {
        window_id,
        url,
        destination: PathBuf::new(),
        error: Some(reason),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_handlers_go_to_shared_context_only_on_webkitgtk_normal_mode() {
        // WebKitGTK, normal mode: one registration on the shared context.
        assert_eq!(
            download_handler_host(false, true),
            DownloadHandlerHost::SharedContext
        );
        // WebKitGTK, private mode: every webview has its own ephemeral
        // context (D15), so per-webview registration is both correct and
        // the only option.
        assert_eq!(
            download_handler_host(true, true),
            DownloadHandlerHost::EachContentWebview
        );
        // WKWebView / WebView2: per-webview delegates, regardless of mode.
        assert_eq!(
            download_handler_host(false, false),
            DownloadHandlerHost::EachContentWebview
        );
        assert_eq!(
            download_handler_host(true, false),
            DownloadHandlerHost::EachContentWebview
        );
    }
}
