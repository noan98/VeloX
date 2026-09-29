//! プロバイダをワーカースレッドで走らせ、期限とキャンセルを **dispatcher が**
//! 強制する。プロバイダが期限を守る保証には頼らない。
//!
//! 構成: ワーカー (`provider.complete`) → 内部 channel → 監督スレッド →
//! 外部 channel ([`AiHandle::events`])。監督スレッドは `recv_timeout` で
//! 期限まで待つので、キャンセルは内部 channel への合図で即座に起こす
//! (ポーリングなし)。終端イベント (`Finished` / `Failed`) はちょうど 1 回で、
//! その後は何も流れない。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::provider::{AiError, AiProvider, AiRequest, CancelToken, FinishReason};

#[derive(Debug, Clone, PartialEq)]
pub enum AiEventKind {
    Chunk(String),
    Finished(FinishReason),
    Failed(AiError),
}

impl AiEventKind {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, AiEventKind::Chunk(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AiEvent {
    pub request_id: u64,
    pub kind: AiEventKind,
}

enum Inner {
    Chunk(String),
    Done(Result<FinishReason, AiError>),
    Cancel,
}

pub struct AiHandle {
    pub request_id: u64,
    pub events: Receiver<AiEvent>,
    cancel: CancelToken,
    wake: Sender<Inner>,
}

impl AiHandle {
    /// 中止する。以降のチャンクは捨てられ、`Failed(Cancelled)` が 1 回届く
    /// (既に終端していれば何も起きない)。
    pub fn cancel(&self) {
        self.cancel.cancel();
        let _ = self.wake.send(Inner::Cancel);
    }
}

/// `notify` はイベントを外側の channel に入れるたびに呼ばれる
/// (app 側では `EventLoopProxy::send_event(UserEvent::AiEvent)` に繋ぐ想定)。
pub fn dispatch(
    provider: Arc<dyn AiProvider>,
    request: AiRequest,
    timeout: Duration,
    request_id: u64,
    notify: impl Fn() + Send + 'static,
) -> AiHandle {
    let deadline = Instant::now() + timeout;
    let cancel = CancelToken::new();
    let (inner_tx, inner_rx) = channel::<Inner>();
    let (out_tx, out_rx) = channel::<AiEvent>();
    let handle = AiHandle {
        request_id,
        events: out_rx,
        cancel: cancel.clone(),
        wake: inner_tx.clone(),
    };

    let emit = move |kind: AiEventKind| {
        let _ = out_tx.send(AiEvent { request_id, kind });
        notify();
    };

    let worker_tx = inner_tx;
    let worker_cancel = cancel.clone();
    let spawned = thread::Builder::new()
        .name("velox-ai-worker".into())
        .spawn(move || {
            let chunk_tx = worker_tx.clone();
            let result = catch_unwind(AssertUnwindSafe(|| {
                provider.complete(
                    &request,
                    &mut |c| {
                        let _ = chunk_tx.send(Inner::Chunk(c.to_string()));
                    },
                    &worker_cancel,
                )
            }))
            .unwrap_or_else(|_| Err(AiError::Provider("provider panicked".into())));
            let _ = worker_tx.send(Inner::Done(result));
        });
    if spawned.is_err() {
        emit(AiEventKind::Failed(AiError::Unavailable(
            "failed to start worker thread".into(),
        )));
        return handle;
    }

    let supervisor = thread::Builder::new()
        .name("velox-ai-supervisor".into())
        .spawn(move || {
            let terminal = loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match inner_rx.recv_timeout(remaining) {
                    // キャンセル後に届いたチャンクは (合図より先に着いても) 捨てる。
                    Ok(Inner::Chunk(_)) if cancel.is_cancelled() => {
                        break AiEventKind::Failed(AiError::Cancelled)
                    }
                    Ok(Inner::Chunk(c)) => emit(AiEventKind::Chunk(c)),
                    Ok(Inner::Done(Ok(r))) => break AiEventKind::Finished(r),
                    Ok(Inner::Done(Err(e))) => break AiEventKind::Failed(e),
                    Ok(Inner::Cancel) => break AiEventKind::Failed(AiError::Cancelled),
                    Err(RecvTimeoutError::Timeout) => break AiEventKind::Failed(AiError::Timeout),
                    Err(RecvTimeoutError::Disconnected) => {
                        break AiEventKind::Failed(AiError::Provider("worker vanished".into()))
                    }
                }
            };
            // 期限切れ・キャンセルではワーカーへも協調的に伝える。
            if matches!(
                terminal,
                AiEventKind::Failed(AiError::Timeout | AiError::Cancelled)
            ) {
                cancel.cancel();
            }
            emit(terminal);
        });
    if supervisor.is_err() {
        // 監督スレッドを起こせない: 終端を出せないので、ワーカーを止めて諦める。
        handle.cancel.cancel();
    }
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::ai::mock::{EchoProvider, FailingProvider, StallProvider};
    use crate::browser::ai::provider::{
        Message, ModelConfig, ProviderKind, ProviderRegistry, Role,
    };
    use std::sync::mpsc::sync_channel;
    use std::sync::Mutex;

    const WAIT: Duration = Duration::from_secs(20);

    fn req(text: &str) -> AiRequest {
        AiRequest {
            messages: vec![Message::new(Role::User, text)],
            config: ModelConfig::new("m"),
        }
    }

    fn collect(h: &AiHandle) -> Vec<AiEventKind> {
        let mut out = Vec::new();
        loop {
            match h.events.recv_timeout(WAIT) {
                Ok(e) => {
                    assert_eq!(e.request_id, h.request_id);
                    out.push(e.kind);
                }
                Err(RecvTimeoutError::Disconnected) => return out,
                Err(RecvTimeoutError::Timeout) => panic!("no terminal event"),
            }
        }
    }

    #[test]
    fn echo_streams_chunks_then_finishes() {
        let h = dispatch(Arc::new(EchoProvider), req("a b"), WAIT, 7, || {});
        assert_eq!(
            collect(&h),
            vec![
                AiEventKind::Chunk("echo:".into()),
                AiEventKind::Chunk(" a".into()),
                AiEventKind::Chunk(" b".into()),
                AiEventKind::Finished(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn notify_is_called_once_per_event() {
        let (tx, rx) = channel::<()>();
        let tx = Mutex::new(tx);
        let h = dispatch(Arc::new(EchoProvider), req("x"), WAIT, 1, move || {
            let _ = tx.lock().unwrap().send(());
        });
        let n = collect(&h).len();
        assert_eq!(rx.try_iter().count(), n);
    }

    #[test]
    fn errors_propagate() {
        let p = Arc::new(FailingProvider(AiError::Auth));
        let h = dispatch(p, req("x"), WAIT, 1, || {});
        assert_eq!(collect(&h), vec![AiEventKind::Failed(AiError::Auth)]);

        let empty = AiRequest {
            messages: vec![],
            config: ModelConfig::new("m"),
        };
        let h = dispatch(Arc::new(EchoProvider), empty, WAIT, 1, || {});
        assert!(matches!(
            collect(&h).as_slice(),
            [AiEventKind::Failed(AiError::InvalidRequest(_))]
        ));
    }

    #[test]
    fn deadline_is_enforced_by_the_dispatcher() {
        let h = dispatch(
            Arc::new(StallProvider),
            req("x"),
            Duration::from_millis(50),
            1,
            || {},
        );
        assert_eq!(collect(&h), vec![AiEventKind::Failed(AiError::Timeout)]);
    }

    /// キャンセルを無視するプロバイダでも、UI へは期限で通知される。
    #[test]
    fn deadline_holds_even_if_provider_ignores_cancel() {
        struct Deaf(Mutex<Receiver<()>>);
        impl AiProvider for Deaf {
            fn id(&self) -> &str {
                "deaf"
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
                // 送信側が落ちるまで返らない。
                let _ = self.0.lock().unwrap().recv();
                Ok(FinishReason::Stop)
            }
        }
        let (release, rx) = channel::<()>();
        let h = dispatch(
            Arc::new(Deaf(Mutex::new(rx))),
            req("x"),
            Duration::from_millis(50),
            1,
            || {},
        );
        assert_eq!(collect(&h), vec![AiEventKind::Failed(AiError::Timeout)]);
        drop(release);
    }

    #[test]
    fn cancel_mid_stream_stops_events_and_reports_cancelled() {
        struct Pausing {
            started: Mutex<std::sync::mpsc::SyncSender<()>>,
        }
        impl AiProvider for Pausing {
            fn id(&self) -> &str {
                "pausing"
            }
            fn kind(&self) -> ProviderKind {
                ProviderKind::Local
            }
            fn complete(
                &self,
                _: &AiRequest,
                on_chunk: &mut dyn FnMut(&str),
                cancel: &CancelToken,
            ) -> Result<FinishReason, AiError> {
                on_chunk("first");
                self.started.lock().unwrap().send(()).unwrap();
                while !cancel.is_cancelled() {
                    std::thread::yield_now();
                }
                on_chunk("after-cancel");
                Err(AiError::Cancelled)
            }
        }
        let (tx, started) = sync_channel(1);
        let p = Arc::new(Pausing {
            started: Mutex::new(tx),
        });
        let h = dispatch(p, req("x"), WAIT, 3, || {});
        started.recv_timeout(WAIT).unwrap();
        h.cancel();
        let evs = collect(&h);
        assert_eq!(evs.last(), Some(&AiEventKind::Failed(AiError::Cancelled)));
        assert_eq!(evs.iter().filter(|e| e.is_terminal()).count(), 1);
        assert!(!evs.contains(&AiEventKind::Chunk("after-cancel".into())));
        // 終端後の cancel は無害。
        h.cancel();
    }

    #[test]
    fn provider_panic_becomes_an_error() {
        struct Boom;
        impl AiProvider for Boom {
            fn id(&self) -> &str {
                "boom"
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
                panic!("boom");
            }
        }
        let h = dispatch(Arc::new(Boom), req("x"), WAIT, 1, || {});
        assert!(matches!(
            collect(&h).as_slice(),
            [AiEventKind::Failed(AiError::Provider(_))]
        ));
    }

    #[test]
    fn registry_swaps_providers_without_the_caller_changing() {
        let mut reg = ProviderRegistry::new();
        assert!(matches!(reg.active(), Err(AiError::Unavailable(_))));
        reg.register(Arc::new(EchoProvider));
        reg.register(Arc::new(FailingProvider(AiError::RateLimited)));
        assert_eq!(reg.ids(), vec!["echo", "failing"]);
        assert!(reg.select("nope").is_err());

        let run = |reg: &ProviderRegistry| {
            let h = dispatch(reg.active().unwrap(), req("hi"), WAIT, 1, || {});
            collect(&h).pop().unwrap()
        };
        reg.select("echo").unwrap();
        assert_eq!(run(&reg), AiEventKind::Finished(FinishReason::Stop));
        reg.select("failing").unwrap();
        assert_eq!(run(&reg), AiEventKind::Failed(AiError::RateLimited));
    }

    #[test]
    fn secret_interpolated_into_errors_stays_redacted() {
        use crate::browser::ai::secret::Secret;
        let key = Secret::new("sk-leak-me");
        let e = AiError::Network(format!("connect failed (key {key})"));
        assert!(!format!("{e} {e:?}").contains("sk-leak-me"));
    }
}
