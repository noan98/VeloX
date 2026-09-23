//! ダウンロードパネルからの操作 (Issue #16, docs/decisions.md D28):
//! ファイル/フォルダを開く、キャンセルする。完了通知の記録もここで行う。

use std::path::Path;

use super::*;

/// Open a completed download's file with the OS's default handler
/// (`ToolbarCommand::OpenDownload`). A no-op — logged, not an error — for
/// an unknown id or a download that has not reached
/// `DownloadState::Completed`; opening an in-progress/failed/cancelled
/// download's (possibly partial or nonexistent) file would be misleading.
pub(super) fn open_download(state: &AppState, id: DownloadId) {
    match state.downloads.get(id) {
        Some(entry) if entry.state == crate::browser::DownloadState::Completed => {
            log_failure("open download", downloads::spawn_open(&entry.destination));
        }
        Some(_) => eprintln!("velox: open_download: {id:?} has not completed yet"),
        None => eprintln!("velox: open_download: unknown download {id:?}"),
    }
}

/// Open the downloads directory with the OS's default file manager
/// (`ToolbarCommand::OpenDownloadsFolder`). Creates the directory first
/// (best-effort) so opening it before anything has ever been downloaded
/// does not fail with a confusing "no such directory" error.
pub(super) fn open_downloads_folder(download_dir_override: Option<&str>) {
    let Some(dir) = downloads::resolve_download_dir_with_override(download_dir_override) else {
        eprintln!(
            "velox: open_downloads_folder: could not resolve a downloads directory \
             (no VELOX_DOWNLOAD_DIR/HOME/USERPROFILE)"
        );
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("velox: failed to create downloads directory {dir:?}: {err}");
    }
    log_failure("open downloads folder", downloads::spawn_open(&dir));
}

/// Best-effort cancel of an in-progress download (`ToolbarCommand::CancelDownload`):
/// marks it `DownloadState::Cancelled` and attempts to delete whatever
/// partial file exists at its destination. Does **not** stop the underlying
/// engine transfer — wry 0.56 exposes no API to do that; see
/// docs/decisions.md D28. A no-op for an unknown id or a download that has
/// already reached a terminal state.
pub(super) fn cancel_download(state: &mut AppState, id: DownloadId) {
    let Some(destination) = state
        .downloads
        .get(id)
        .map(|entry| entry.destination.clone())
    else {
        return;
    };
    if state.downloads.cancel(id, now_unix()) {
        // Best-effort only: if the engine is still writing to this path, it
        // may recreate the file (or fail silently) after this runs — see
        // docs/decisions.md D28's "what's unverified"/limitation note.
        match std::fs::remove_file(&destination) {
            Ok(()) => {}
            // Already gone (never actually started writing, or the engine
            // had not created the file yet) — not an error worth logging.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!(
                "velox: failed to remove cancelled download's partial file {destination:?}: {err}"
            ),
        }
    }
}

/// `UserEvent::DownloadCompleted` の結果を `state.downloads` に反映する。
/// どのダウンロードかは `DownloadStore::resolve_completion` で特定し、
/// 特定できなければログに残すだけで何もしない。
pub(super) fn record_download_completion(
    state: &mut AppState,
    url: &str,
    path: Option<&Path>,
    success: bool,
) {
    let now = now_unix();
    // Issue #128 / D140. The `success` flag alone is not reliable on
    // Linux: wry 0.56 shares one set-only `failed` flag across every
    // download of a registration, and D53 made that registration
    // session-wide — so after any single failure every later
    // download is reported failed (and with `path: None`) even
    // though its file lands correctly. Reproduced end to end.
    //
    // The destination this entry recorded when it *started* is the
    // check: on a real failure WebKitGTK removes the partial file,
    // so a file that is there means the download finished. See
    // `downloads::completion_succeeded` for the truth table and why
    // presence — not size — is the signal.
    //
    // **Gated to the backend that actually has the bug**
    // (`DOWNLOAD_SUCCESS_FLAG_IS_SHARED`, D140 決定5). WebView2 and
    // WKWebView answer per download, so their `false` is real and
    // overruling it would misreport a genuine failure as a success
    // — on Windows, the priority OS, on the strength of behavior
    // nobody has measured there.
    //
    // Reading the filesystem is why this lives here and not in
    // `browser::downloads`, which stays pure (D20): that module
    // gets the two booleans and decides.
    let resolved = state.downloads.resolve_completion(url, path);
    let succeeded = resolved.is_some_and(|id| {
        // Only the poisoned backend gets its verdict second-guessed,
        // and only then is the filesystem touched at all.
        let exists = crate::ui::window::DOWNLOAD_SUCCESS_FLAG_IS_SHARED
            && state
                .downloads
                .get(id)
                .is_some_and(|entry| entry.destination.exists());
        downloads::completion_succeeded(
            success,
            exists,
            crate::ui::window::DOWNLOAD_SUCCESS_FLAG_IS_SHARED,
        )
    });
    if debug_logging_enabled() {
        eprintln!(
            "velox: download completion url={url:?} path={path:?} \
reported_success={success} recorded_as_success={succeeded}"
        );
    }
    match resolved {
        Some(id) if succeeded => {
            state.downloads.complete(id, now);
        }
        Some(id) => {
            state
                .downloads
                .fail(id, "ダウンロードに失敗しました".to_owned(), now);
        }
        None => {
            eprintln!(
                "velox: could not correlate download completion for {url:?} \
                 (path={path:?}, success={success})"
            );
        }
    }
}

/// `UserEvent::SavePageFinished` (Issue #46) の結果を `state.downloads` に
/// 反映する。保存先は最初から確定しているので、`resolve_completion` の
/// 保存先完全一致の経路で特定する。
pub(super) fn record_save_page_completion(
    state: &mut AppState,
    url: &str,
    destination: &Path,
    error: Option<String>,
) {
    let now = now_unix();
    match state.downloads.resolve_completion(url, Some(destination)) {
        Some(id) => match error {
            Some(reason) => {
                state.downloads.fail(id, reason, now);
            }
            None => {
                state.downloads.complete(id, now);
            }
        },
        None => {
            eprintln!(
                "velox: could not correlate page-save completion for {url:?} \
                 (destination={destination:?}, error={error:?})"
            );
        }
    }
}
