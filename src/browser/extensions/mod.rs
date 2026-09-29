//! 拡張機能ホストのコア (Issue #83 Extension API / #84 Lifecycle・Storage)。
//!
//! **アプリの実行経路には配線していない** (拡張機能の JS はどの WebView でも動かない。
//! 挙動は変わらない)。`docs/extensions.md` の設計 (D159) に沿った、テスト可能な
//! ホスト側の核だけを純粋ロジック + 薄い IO 層として実装している。配線しない理由と
//! 残りの作業は docs/decisions の D163 を参照。
//!
//! - [`api`] — バージョン付きの要求/応答スキーマ・メソッド表・パラメータ検証
//! - [`host`] — 信頼しない要求の検証 → 権限検査 → [`host::BrowserHost`] 呼び出し
//! - [`permissions`] — 承認済み権限 (必須 + 実行時付与の任意) とホスト一致
//! - [`lifecycle`] — 状態機械 (enable/disable/update/再承認) とイベント
//! - [`registry`] — インストール/更新/アンインストール、索引の永続化と復旧
//! - [`package`] — 未信頼パッケージの検査 (zip slip・サイズ・件数)
//! - [`storage`] — 拡張機能ごとのクォータ付き KV ストレージ

pub mod api;
mod fsutil;
pub mod host;
pub mod lifecycle;
pub mod package;
pub mod permissions;
pub mod registry;
pub mod storage;
