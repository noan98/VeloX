//! AI プロバイダ抽象 (Issue #76, D162)。設計の全体像は `docs/ai-providers.md`。
//!
//! UI / app 層は [`AiRequest`] / [`AiEvent`] / [`AiError`] だけを知り、特定
//! プロバイダの API・型には触れない。**このモジュールはまだ app に
//! 配線していない** (挙動変更なし)。標準で有効なプロバイダは無く、
//! [`ProviderRegistry`] は空で始まる。`mock` は決定的なローカル実装で、
//! テストとデモ用であり、外部へは何も送らない。
//!
//! - [`secret`] — ログ・`Debug`・`Display` に出ない資格情報型
//! - [`provider`] — trait・要求/応答型・エラー分類・レジストリ
//! - [`dispatcher`] — ワーカースレッド実行、期限とキャンセルの強制
//! - [`mock`] — `EchoProvider` など

pub mod dispatcher;
pub mod mock;
pub mod provider;
pub mod secret;

pub use dispatcher::{dispatch, AiEvent, AiEventKind, AiHandle};
pub use provider::{
    AiError, AiProvider, AiRequest, CancelToken, FinishReason, Message, ModelConfig, ProviderKind,
    ProviderRegistry, Role,
};
pub use secret::Secret;
