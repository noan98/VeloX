//! WebView2 の休止 API (`TrySuspend` / `MemoryUsageTargetLevel`) が
//! **この実行環境で使えるか**を調べるだけのモジュール (Issue #176 Stage 2)。
//!
//! **ここでは休止しない。** Stage 2 のチェック項目は「調査・実装検討」で
//! あり、本モジュールはその「調査」の側を、**推測ではなく実行時の事実**に
//! するためのものである。
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

use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2_19, ICoreWebView2_3};
use windows::core::Interface;
use wry::{WebView, WebViewExtWindows};

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
