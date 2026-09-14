//! WebView2 の休止 API (`TrySuspend` / `Resume` / `IsSuspended`) を包む
//! モジュール (Issue #176 Stage 2 → Issue #243)。
//!
//! 二段階で育った。Stage 2 では**使えるかを確かめる probe だけ**を置き
//! (D120 決定1、[`probe`] / [`log_support_once`])、Issue #243 で**実際に
//! 休止させる呼び出し**を足した ([`try_suspend`] / [`resume`] /
//! [`is_suspended`])。後者は既定では使われない — `VELOX_SUSPEND_MECHANISM=
//! freeze` を明示したときだけ `ui::window` がこちらを通る
//! (`browser::suspension::SuspendMechanism`)。
//!
//! **`unsafe` はこのファイルに閉じる** (D120 決定4)。呼び出し側には
//! `wry::WebView` を取る safe な関数だけを見せ、COM の生 vtable 呼び出しは
//! ここから外に出さない。`ui::webview2_blocking` (D59) /
//! `ui::webview2_print` (D75) と同じ流儀である。
//!
//! ## なぜこれが #176 にとって重要か
//!
//! 今日の休止 (D9) は **webview を drop する**。エンジンのプロセスは消えて
//! メモリは戻るが、**スクロール位置・入力中のフォーム・セッション履歴も
//! 一緒に消える** (D105 が「数値化できていない」と残した状態喪失の正体)。
//! 復帰には webview の作り直しが要り、それが約 110〜130 ms かかることは
//! D112 / §32 が実測した。
//!
//! `ICoreWebView2_3::TrySuspend` は**破棄せずに休止する**。成功すれば
//! 状態が残り、`Resume` で戻せる — つまり D112 決定4 が「避けられない」と
//! 結論した対価の、**別の支払い方**になりうる。
//!
//! ⚠️ **「なりうる」以上のことは、まだ何も分かっていない。** 休止でメモリが
//! どれだけ戻るか、`Resume` が webview の作り直しより速いか、`TrySuspend`
//! がどのくらいの割合で成功するかは、**すべて未計測**である。本モジュールは
//! その計測を始められる地点まで来たことを確かめるだけのものである。
//!
//! ## 分かっている制約
//!
//! - `TrySuspend` は **webview が非表示でないと失敗する。** VeloX は既に
//!   タブ切替時に `WebView::set_visible(false)` を呼んでいる
//!   (`ui::window` の `show_tab`) ので、背景タブはこの前提を満たす。
//! - `TrySuspend` は非同期で、完了ハンドラで成否が返る。「試す」API であり、
//!   **成功が保証されない** (名前のとおり)。
//! - 休止中の webview に触れると暗黙に復帰する。
//! - どちらの API も**実行環境の WebView2 Runtime のバージョン**に依存する
//!   (`TrySuspend` は `ICoreWebView2_3`、`MemoryUsageTargetLevel` は
//!   `ICoreWebView2_19`)。**だから静的な調査では答えが出ず、この probe が
//!   要る。**
//!
//! `ui::webview2_blocking` (D59) / `ui::webview2_print` (D75) と同じく、
//! wry の `WebViewExtWindows::webview()` が返す `ICoreWebView2` から
//! COM の `QueryInterface` (`windows-core` の `cast`) で目的の
//! インターフェースへ降りる。

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2_19, ICoreWebView2_3, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
};
use webview2_com::TrySuspendCompletedHandler;
use windows::core::Interface;
use wry::{WebView, WebViewExtWindows};

use crate::app::UserEvent;
use crate::browser::suspension::BackgroundMemoryTarget;
use crate::browser::{TabId, WindowId};

/// この実行環境の WebView2 Runtime が持っている休止関連 API。
///
/// どちらも `false` でも VeloX は今までどおり動く — 今日の休止 (D9) は
/// これらを使っていない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SuspendSupport {
    /// `ICoreWebView2_3` — `TrySuspend` / `Resume` / `IsSuspended`。
    pub try_suspend: bool,
    /// `ICoreWebView2_19` — `MemoryUsageTargetLevel` の get/set。
    pub memory_usage_target_level: bool,
}

impl SuspendSupport {
    /// 何も使えない実行環境か。
    pub fn is_none(self) -> bool {
        !self.try_suspend && !self.memory_usage_target_level
    }
}

impl std::fmt::Display for SuspendSupport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "webview2 suspend support: TrySuspend={} MemoryUsageTargetLevel={}",
            self.try_suspend, self.memory_usage_target_level
        )
    }
}

/// この webview から休止 API に到達できるかを調べる。**副作用は無い** —
/// `QueryInterface` を 2 回試すだけで、休止も設定変更もしない。
pub fn probe(webview: &WebView) -> SuspendSupport {
    let core = webview.webview();
    // `cast` は COM の `QueryInterface` そのもので、対象の実行環境が
    // そのインターフェースを実装していなければ `E_NOINTERFACE` で `Err`
    // を返す。**これが「Runtime のバージョンを直接聞く」ことに当たる。**
    // バージョン番号を読んで比較する形にしないのは、Microsoft が
    // インターフェースの追加でバージョンを表現しているためで、
    // `QueryInterface` の可否がそのまま答えになる。
    //
    // 安全性: `cast` は `windows-core` の safe API (内部で `unsafe` を
    // 閉じ込めている) なので、ここに `unsafe` ブロックは要らない。
    // `ui::webview2_blocking` が `unsafe` を要したのは COM の**メソッド**を
    // 呼んでいたからで、こちらはインターフェース照会だけである。
    SuspendSupport {
        try_suspend: core.cast::<ICoreWebView2_3>().is_ok(),
        memory_usage_target_level: core.cast::<ICoreWebView2_19>().is_ok(),
    }
}

/// [`probe`] をプロセスにつき 1 回だけ実行し、結果を stderr に出す。
///
/// **これが Stage 2 の「利用可否を確認」の実体である。** 静的な調査では
/// 答えが出ない (Runtime のバージョン次第) ので、実機で走らせて読み取る。
/// `perf-windows.yml` の診断ステップや統合テストの出力に出る。
///
/// タブごとではなく 1 回に絞るのは、答えがプロセス内で変わらないためで
/// ある (同じ Runtime を全 webview が共有する)。100 タブ開いて同じ行が
/// 100 回出ても情報は増えない。
///
/// 失敗させない: これは調査であって機能ではなく、読めなくても VeloX は
/// 今までどおり動く (`app.rs` の `log_failure` と同じ方針)。
pub fn log_support_once(webview: &WebView) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let support = probe(webview);
        eprintln!("velox: {support} (Issue #176 Stage 2 probe)");
        if support.is_none() {
            eprintln!(
                "velox: この WebView2 Runtime は休止 API を 1 つも持っていない — \
#176 Stage 2 の Windows 側は、この環境では検討の余地が無い"
            );
        }
    });
}

/// 背景タブ `webview` を**破棄せずに**休止させる (Issue #243)。
///
/// `ICoreWebView2_3::TrySuspend` は名前のとおり「試す」API で、成功が
/// 保証されない。したがってこの関数の戻り値は**呼び出しを発行できたか**
/// だけを表す — 実際に休止できたかは WebView2 の完了ハンドラから
/// [`UserEvent::TabFreezeFinished`] として後から届く。
/// `ui::webview2_print::export_as_pdf` (D75) と同じ二段構えである。
///
/// `Err` は「そもそも始められなかった」場合だけで、いちばんありうるのは
/// Runtime が古くて `ICoreWebView2_3` を実装していないケース
/// (`QueryInterface` が `E_NOINTERFACE`)。呼び出し側 (`ui::window`) は
/// これを受けて**その場で従来どおり webview を捨てる**ので、タブが休止
/// されないまま残ることはない。
///
/// ⚠️ 休止中の webview に触れると暗黙に復帰するため、休止後は
/// [`resume`] を通すまで触らないこと。`set_visible(false)` 済みの背景タブ
/// にしか呼ばない前提である (D120 決定2)。
pub fn try_suspend(
    webview: &WebView,
    proxy: EventLoopProxy<UserEvent>,
    window_id: WindowId,
    tab_id: TabId,
) -> windows::core::Result<()> {
    let core = webview.webview();

    // SAFETY: `core` は wry 自身の `WebViewExtWindows::webview()` が返す
    // 生きた COM 参照で、借用している `webview` が生きている間は有効
    // (このモジュールの doc コメント参照)。`.cast::<T>()` はただの
    // `QueryInterface` で、Runtime が古ければ不正なオブジェクトではなく
    // `Err` を返す (`?` で伝搬する)。完了ハンドラのクロージャが触るのは
    // move で所有権を取った値 (`proxy` / `window_id` / `tab_id`) と、
    // WebView2 がこの 1 回のコールバックで渡してくる引数だけで、
    // この関数が有効性を保証できないポインタには一切触れない。
    unsafe {
        core.cast::<ICoreWebView2_3>()?
            .TrySuspend(&TrySuspendCompletedHandler::create(Box::new(
                move |result, is_successful| {
                    let error = match &result {
                        Ok(()) if is_successful => None,
                        // 「発行はできたが休止しなかった」ケース。ページ側の
                        // 事情 (再生中のメディア、進行中のダウンロードなど) で
                        // WebView2 が断ることがある。
                        Ok(()) => Some("WebView2 が休止を拒否しました".to_owned()),
                        Err(err) => Some(err.to_string()),
                    };
                    let _ = proxy.send_event(UserEvent::TabFreezeFinished {
                        window_id,
                        tab_id,
                        success: result.is_ok() && is_successful,
                        error,
                    });
                    Ok(())
                },
            )))
    }
}

/// [`try_suspend`] で休止させた `webview` を戻す (Issue #243)。
///
/// `TrySuspend` と違い同期で、こちらは「試す」API ではない。休止して
/// いない webview に対しても成功する (何も起きない) ので、呼び出し側は
/// 休止済みかどうかを先に確かめなくてよい。
pub fn resume(webview: &WebView) -> windows::core::Result<()> {
    let core = webview.webview();
    // SAFETY: [`try_suspend`] と同じ COM 参照の議論。`Resume` は引数を
    // 取らない単純なメソッド呼び出しである。
    unsafe { core.cast::<ICoreWebView2_3>()?.Resume() }
}

/// `webview` が今 (エンジンから見て) 休止しているか (Issue #243)。
///
/// VeloX 自身は休止状態を `browser::Tabs` 側で持っているので通常の動作で
/// は使わない。**エンジンの認識と VeloX の認識が食い違っていないか**を
/// 確かめるための窓であり、計測時の検証に使う。
pub fn is_suspended(webview: &WebView) -> windows::core::Result<bool> {
    let core = webview.webview();
    let mut suspended = windows::core::BOOL::from(false);
    // SAFETY: [`try_suspend`] と同じ COM 参照の議論。`IsSuspended` は
    // 出力引数 (`*mut BOOL`) に書き込むだけで、渡しているのはこの関数の
    // スタック上に確保した `suspended` へのポインタ。呼び出しの間ずっと
    // 生きており、書き込みは 1 回で、他から参照されていない。
    unsafe {
        core.cast::<ICoreWebView2_3>()?
            .IsSuspended(&mut suspended)?
    };
    Ok(suspended.as_bool())
}

/// Tell the engine how much memory `webview` may use (Issue #242).
///
/// `ICoreWebView2_19::SetMemoryUsageTargetLevel` is a plain synchronous
/// property setter — unlike [`try_suspend`] there is no completion handler
/// and nothing to wait for, and unlike suspension it does not change what
/// the page *is*: the webview stays live, scriptable and ready to show.
/// It only states an intent the engine may act on.
///
/// `Err` is almost always a runtime too old for `ICoreWebView2_19`
/// (`QueryInterface` → `E_NOINTERFACE`). The caller logs and carries on;
/// there is nothing to fall back to, because "say nothing" *is* the
/// pre-#242 behavior.
pub fn set_memory_usage_target(
    webview: &WebView,
    target: BackgroundMemoryTarget,
) -> windows::core::Result<()> {
    let level = match target {
        BackgroundMemoryTarget::Normal => COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
        BackgroundMemoryTarget::Low => COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    };
    let core = webview.webview();
    // SAFETY: [`try_suspend`] と同じ COM 参照の議論。`SetMemoryUsageTargetLevel`
    // は値渡しの enum を 1 つ取るだけのプロパティ setter で、ポインタを
    // 渡さない。
    unsafe {
        core.cast::<ICoreWebView2_19>()?
            .SetMemoryUsageTargetLevel(level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_none_only_when_neither_interface_is_available() {
        assert!(SuspendSupport::default().is_none());
        assert!(!SuspendSupport {
            try_suspend: true,
            memory_usage_target_level: false,
        }
        .is_none());
        assert!(!SuspendSupport {
            try_suspend: false,
            memory_usage_target_level: true,
        }
        .is_none());
    }

    #[test]
    fn display_names_both_interfaces() {
        // 実機のログから読み取る文字列なので、両方の名前が出ることを固定
        // する — 片方しか出ていなければ「調べていない」のか「使えない」の
        // かが区別できない。
        let text = SuspendSupport {
            try_suspend: true,
            memory_usage_target_level: false,
        }
        .to_string();
        assert!(text.contains("TrySuspend=true"), "{text}");
        assert!(text.contains("MemoryUsageTargetLevel=false"), "{text}");
    }
}
