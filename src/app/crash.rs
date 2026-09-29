//! クラッシュ処理 (Issue #89, docs/decisions.md D156): WebView プロセス異常
//! (Windows / WebView2 のみ検知可能) への対応。
//!
//! 純粋な判断は `browser::crash_report`、ファイル IO は
//! `browser::crash_store` にあり、ここはイベントループ側の配線だけを持つ。

use super::*;
use crate::browser::crash_report::{ProcessFailureKind, Recovery};
use crate::browser::crash_store;

/// `UserEvent::ContentProcessFailed` の処理本体。呼び出し側が `window_id` を
/// 生きている `window` に解決済みであること。URL・タイトルは扱わない。
pub(super) fn handle_content_process_failed(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    tab_id: TabId,
    kind: ProcessFailureKind,
) {
    eprintln!(
        "velox: WebView プロセスの異常を検知しました: {}",
        kind.label()
    );
    if state.crash_reports_enabled {
        if let Some(dir) = state.data_dir.as_deref() {
            log_failure(
                "write crash report",
                crash_store::write_process_failure(dir, kind.label()).map(|_| ()),
            );
        }
    }
    match kind.recovery() {
        Recovery::LogOnly => {}
        Recovery::NoticeOnly => show_print_status(
            window,
            &format!(
                "ページの表示エンジンに問題が発生しました ({})",
                kind.label()
            ),
        ),
        Recovery::RecoverTab => {
            if tabs_of(state, window_id).active_id() == tab_id {
                // 同じタブが短時間に繰り返し落ちるなら再読み込みしない
                // (決定的に描画プロセスを落とすページでのループ防止)。
                if state
                    .reload_guard
                    .allow_reload((window_id, tab_id), crate::browser::util::now_unix())
                {
                    log_failure("reload crashed tab", window.reload());
                    show_print_status(window, "ページが異常終了したため再読み込みしました");
                } else {
                    show_print_status(
                        window,
                        "ページが繰り返し異常終了したため、自動再読み込みを停止しました",
                    );
                }
            } else if !suspend_tab(window, window_id, state, tab_id) {
                // 固定タブなどで休止できないときは通知だけに留める。
                show_print_status(window, "バックグラウンドのタブが異常終了しました");
            }
        }
    }
}
