//! プロバイダ trait、要求/応答モデル、エラー分類、レジストリ。
//! プロバイダ固有の型はここに出さない (UI が依存してよいのはこの層まで)。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Message {
            role,
            content: content.into(),
        }
    }
}

/// モデル設定。`model` はプロバイダ固有の名前を不透明な文字列として運ぶ。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelConfig {
    pub model: String,
    pub max_tokens: u32,
    /// `None` はプロバイダ既定。
    pub temperature: Option<f32>,
}

impl ModelConfig {
    pub fn new(model: impl Into<String>) -> Self {
        ModelConfig {
            model: model.into(),
            max_tokens: 1024,
            temperature: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AiRequest {
    pub messages: Vec<Message>,
    pub config: ModelConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    MaxTokens,
}

/// エラー分類。`Display` の文言に資格情報・要求本文を含めないこと
/// (プロバイダ実装の責務。`Secret` を使えば `{}` でも漏れない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    /// ユーザ操作または呼び出し側による中止。
    Cancelled,
    /// dispatcher が期限を超えたと判断した。
    Timeout,
    /// 資格情報が無い・拒否された。
    Auth,
    RateLimited,
    /// 接続できない・途中で切れた。
    Network(String),
    /// 要求自体が不正 (モデル名・トークン数など)。
    InvalidRequest(String),
    /// プロバイダ未選択・未設定・停止中。
    Unavailable(String),
    /// 上記に当てはまらないプロバイダ側の失敗 (panic を含む)。
    Provider(String),
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::Cancelled => f.write_str("cancelled"),
            AiError::Timeout => f.write_str("timed out"),
            AiError::Auth => f.write_str("authentication failed"),
            AiError::RateLimited => f.write_str("rate limited"),
            AiError::Network(m) => write!(f, "network error: {m}"),
            AiError::InvalidRequest(m) => write!(f, "invalid request: {m}"),
            AiError::Unavailable(m) => write!(f, "unavailable: {m}"),
            AiError::Provider(m) => write!(f, "provider error: {m}"),
        }
    }
}

impl std::error::Error for AiError {}

/// 協調的キャンセル。`clone` は同じフラグを共有する。
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// 端末内で完結する (データは外へ出ない)。
    Local,
    /// ネットワーク越しに送る。UI は送信先が外部であることを示すこと。
    Remote,
}

/// AI プロバイダ。同期・ブロッキングで、ストリームは `on_chunk` への
/// 呼び出しで返す。実行は dispatcher のワーカースレッドで行われるので
/// メインスレッドをブロックしない。長い処理の合間に `cancel` を確認する
/// こと (dispatcher は無視するプロバイダを待たずに期限/キャンセルを通知
/// するが、スレッド自体は止められない)。
pub trait AiProvider: Send + Sync {
    /// レジストリ上の安定 ID (例: `"echo"`)。
    fn id(&self) -> &str;
    fn kind(&self) -> ProviderKind;
    fn complete(
        &self,
        request: &AiRequest,
        on_chunk: &mut dyn FnMut(&str),
        cancel: &CancelToken,
    ) -> Result<FinishReason, AiError>;
}

/// 登録済みプロバイダと選択中の 1 つ。空で始まり、何も既定で有効にしない。
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn AiProvider>>,
    active: Option<String>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 同じ ID は差し替える。
    pub fn register(&mut self, provider: Arc<dyn AiProvider>) {
        self.providers.insert(provider.id().to_string(), provider);
    }

    pub fn ids(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AiProvider>> {
        self.providers.get(id).cloned()
    }

    pub fn select(&mut self, id: &str) -> Result<(), AiError> {
        if self.providers.contains_key(id) {
            self.active = Some(id.to_string());
            Ok(())
        } else {
            Err(AiError::Unavailable(format!("unknown provider `{id}`")))
        }
    }

    pub fn active(&self) -> Result<Arc<dyn AiProvider>, AiError> {
        self.active
            .as_deref()
            .and_then(|id| self.get(id))
            .ok_or_else(|| AiError::Unavailable("no provider selected".into()))
    }
}
