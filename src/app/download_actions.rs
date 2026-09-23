//! ダウンロードパネルからの操作 (Issue #16, docs/decisions.md D28):
//! ファイル/フォルダを開く、キャンセルする。

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
