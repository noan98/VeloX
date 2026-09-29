//! 決定的でローカルなプロバイダ。テストとデモ用で、外部には何も送らない。

use std::time::Duration;

use super::provider::{
    AiError, AiProvider, AiRequest, CancelToken, FinishReason, ProviderKind, Role,
};

/// 最後の user メッセージを空白区切りで 1 語ずつ `echo:` 付きで返す。
pub struct EchoProvider;

impl AiProvider for EchoProvider {
    fn id(&self) -> &str {
        "echo"
    }
    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }
    fn complete(
        &self,
        request: &AiRequest,
        on_chunk: &mut dyn FnMut(&str),
        cancel: &CancelToken,
    ) -> Result<FinishReason, AiError> {
        let text = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .ok_or_else(|| AiError::InvalidRequest("no user message".into()))?;
        on_chunk("echo:");
        for word in text.split_whitespace() {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }
            on_chunk(&format!(" {word}"));
        }
        Ok(FinishReason::Stop)
    }
}

/// 常に指定のエラーを返す。
pub struct FailingProvider(pub AiError);

impl AiProvider for FailingProvider {
    fn id(&self) -> &str {
        "failing"
    }
    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }
    fn complete(
        &self,
        _: &AiRequest,
        _: &mut dyn FnMut(&str),
        _: &CancelToken,
    ) -> Result<FinishReason, AiError> {
        Err(self.0.clone())
    }
}

/// 何も返さず、キャンセルされるまで (安全弁として 30 秒) 待つ。
/// dispatcher の期限強制を試すためのもの。
pub struct StallProvider;

impl AiProvider for StallProvider {
    fn id(&self) -> &str {
        "stall"
    }
    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }
    fn complete(
        &self,
        _: &AiRequest,
        _: &mut dyn FnMut(&str),
        cancel: &CancelToken,
    ) -> Result<FinishReason, AiError> {
        for _ in 0..3000 {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(FinishReason::Stop)
    }
}
