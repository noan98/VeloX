//! WebView2 の `ProcessFailed` イベントの購読 (Issue #89, docs/decisions.md
//! D156)。
//!
//! wry 0.57 は WebView のプロセス異常を通知する API を WebView2 に対しては
//! 出していない (`with_on_web_content_process_terminate_handler` は macOS /
//! iOS 専用)。そこで `ui::webview2_blocking` (D59) と同じく、
//! `WebViewExtWindows::webview()` が返す生の `ICoreWebView2` に
//! `add_ProcessFailed` で直接ハンドラを付ける。`unsafe` は COM の
//! メソッド呼び出しに必要な範囲だけで、このファイルに閉じる。
//!
//! ハンドラが送るのは (window, tab, 種類) だけ。イベント引数から URL などは
//! 一切読まない (プライバシー、D156)。

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_PROCESS_FAILED_KIND;
use webview2_com::ProcessFailedEventHandler;
use wry::{WebView, WebViewExtWindows};

use crate::app::UserEvent;
use crate::browser::crash_report::ProcessFailureKind;
use crate::browser::{TabId, WindowId};

/// `webview` の `ProcessFailed` を購読し、`UserEvent::ContentProcessFailed`
/// として送る。登録に失敗しても stderr に出すだけで続行する (クラッシュ復旧
/// が効かないだけで、ブラウザ自体は動く)。
pub fn attach(
    webview: &WebView,
    window_id: WindowId,
    tab_id: TabId,
    proxy: EventLoopProxy<UserEvent>,
) {
    let core = webview.webview();
    let mut token: i64 = 0;
    // SAFETY: `core` は wry が保持する生きた `ICoreWebView2` で、この
    // 関数は `webview` の借用の間だけそれを使う。ハンドラの引数
    // (`args`) は WebView2 がこの 1 回のコールバックの間だけ有効にして
    // 渡すもので、外へ持ち出さない。`ProcessFailedKind` は出力引数に
    // 1 回書き込むだけで、渡すのはこのクロージャのスタック上の変数。
    let result = unsafe {
        core.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let mut code = COREWEBVIEW2_PROCESS_FAILED_KIND(0);
                if args.ProcessFailedKind(&mut code).is_err() {
                    return Ok(());
                }
                let _ = proxy.send_event(UserEvent::ContentProcessFailed {
                    window_id,
                    tab_id,
                    kind: ProcessFailureKind::from_webview2_code(code.0),
                });
                Ok(())
            })),
            &mut token,
        )
    };
    if let Err(err) = result {
        eprintln!("velox: ProcessFailed の登録に失敗しました: {err}");
    }
}
