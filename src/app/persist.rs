//! 履歴・ブックマーク・入力履歴・タブセッションのディスクへの永続化
//! (Issue #25/#67, docs/decisions.md D65/D86)。

use std::path::Path;
use std::time::Instant;

use super::*;

/// `persist_history`/`persist_bookmarks`/`persist_input_history` の共通部:
/// `data_dir` があれば `save` で書き出し、書き込み時間を `kind` として
/// 記録 (metrics 有効時のみ) し、失敗は `action` としてログに出す。
/// `data_dir` が無ければ何もしない。
pub(super) fn persist_store(
    state: &AppState,
    kind: metrics::StateWriteKind,
    action: &str,
    save: impl FnOnce(&Path) -> std::io::Result<()>,
) {
    if let Some(dir) = &state.data_dir {
        let started = Instant::now();
        let result = save(dir);
        record_state_write(state, kind, started);
        log_failure(action, result);
    }
}

pub(super) fn persist_history(state: &AppState) {
    persist_store(
        state,
        metrics::StateWriteKind::History,
        "save history",
        |dir| persistence::save_history(dir, &state.history),
    );
}

pub(super) fn persist_bookmarks(state: &AppState) {
    persist_store(
        state,
        metrics::StateWriteKind::Bookmarks,
        "save bookmarks",
        |dir| persistence::save_bookmarks(dir, &state.bookmarks),
    );
}

pub(super) fn persist_input_history(state: &AppState) {
    persist_store(
        state,
        metrics::StateWriteKind::InputHistory,
        "save input history",
        |dir| persistence::save_input_history(dir, &state.input_history),
    );
}

/// Persist the current tab session (Issue #25 — see docs/decisions.md D65).
///
/// Called from [`sync_tab_strip`] — the one place nearly every
/// tab-affecting change already routes through — rather than only at exit:
/// an exit-only save would never run for exactly the case session restore
/// is meant to help with (a crash, `kill -9`, a power loss), so this saves
/// eagerly, the same "every mutation writes back to disk" pattern
/// `persist_history`/`persist_bookmarks` already follow.
///
/// Gated by [`window_is_private`] — the same per-window choke point
/// `record_visit_if_enabled` uses (docs/decisions.md D13/D14/D74) — so a
/// private window never writes what tabs it had open to disk, regardless of
/// whether `Config::restore_previous_session` is even on; saving is
/// unconditional otherwise, so turning the setting on later always has a
/// recent session to restore from. A missing `data_dir` is a silent no-op,
/// like every other `persist_*` function here.
///
/// **Multi-window (Issue #149, was the D68/D74 follow-up)**: every open
/// window is written, in `Windows`' own order, so the next launch restores
/// all of them. The snapshot is built from all of them no matter which
/// window's change triggered this, because a change in one window (a tab
/// closed) does not make the others' tabs any less current.
///
/// **Private windows are left out entirely**, not written as empty ones: a
/// private window's tabs must not reach disk (D14/D74), and an empty
/// placeholder would restore as a window the user never gets their tabs
/// back in. The filter lives here rather than in `browser::session`, which
/// never learns what privacy is (D20).
///
/// A launch that is private as a whole (`--private`/`VELOX_PRIVATE`, D74)
/// therefore has no non-private window at all and writes nothing, exactly
/// as before.
///
/// **Redundant-write skip (Issue #67, D86)**: `sync_tab_strip` — the only
/// caller — runs this after nearly every tab-affecting event, but
/// `SessionSnapshot` only carries url/title/favicon, so a burst of events
/// from a single navigation (`NavigationStarted`/`LoadFinished`/
/// `PageTitleResolved`/`FaviconResolved`) mostly produces the *same*
/// snapshot content more than once (the loading flag they actually
/// differ on isn't part of it). This compares the freshly-built snapshot
/// against `state.last_persisted_session` and skips the disk write
/// entirely when nothing changed, exactly like `app::
/// refresh_history_panel_if_open` (Issue #66) skipped a redundant
/// `set_history` push — same shape, different layer (disk I/O, not IPC).
pub(super) fn persist_session(state: &mut AppState) {
    let Some(dir) = state.data_dir.as_deref() else {
        return;
    };
    let restorable: Vec<&Tabs> = state
        .windows
        .ids()
        .filter(|id| !window_is_private(state, *id))
        .filter_map(|id| state.windows.tabs(id))
        .collect();
    if restorable.is_empty() {
        return;
    }
    let snapshot = SessionSnapshot::from_windows(restorable);
    if state.last_persisted_session.as_ref() == Some(&snapshot) {
        return;
    }
    let started = Instant::now();
    let result = persistence::save_session(dir, &snapshot);
    record_state_write(state, metrics::StateWriteKind::Session, started);
    let succeeded = result.is_ok();
    log_failure("save session", result);
    if succeeded {
        state.last_persisted_session = Some(snapshot);
    }
}
