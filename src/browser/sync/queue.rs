//! オフライン送信キュー。docs/sync.md §7。
//!
//! - **同じレコードへの変更は 1 件に畳む** ([`Record::merge`] = そのまま
//!   デルタの結合)。オフライン中に 1000 回タイトルを直しても送るのは 1 件。
//! - **上限つき**。溢れたらキューを捨てて `needs_full_resync` を立てる。
//!   状態ベース CRDT なので、キューは「送る候補の索引」にすぎず、失っても
//!   ローカル状態の全量を送り直せば収束する。黙って変更を落とすことはない。
//! - **at-least-once**。バッチを送ったら `ack` で消し、`nack` (失敗・
//!   タイムアウト) なら指数バックオフで再送。結合は冪等なので重複配送は
//!   無害。ack が失われても (サーバは受け取った) 再送で済む。
//! - 再起動を跨ぐ。`Serialize`/`Deserialize` でき、読み戻すと飛行中だった
//!   バッチは未送信に戻る。
//! - 時刻・乱数は呼び出し側が注入する (ジッタは呼び出し側で足す)。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::record::{Record, RecordKey};

pub const DEFAULT_MAX_PENDING: usize = 1024;
pub const BACKOFF_BASE_MS: u64 = 1_000;
pub const BACKOFF_CAP_MS: u64 = 60_000;

/// 再送までの待ち時間 (`attempts` 回目の失敗後)。指数、上限つき。
pub fn backoff_ms(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(16);
    (BACKOFF_BASE_MS << shift).min(BACKOFF_CAP_MS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchId(pub u64);

#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub id: BatchId,
    pub records: Vec<Record>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued,
    /// 既存の未送信エントリに結合した。
    Coalesced,
    /// 上限超過。キューを空にして全量再送を要求した。
    Overflowed,
}

#[derive(Debug, Clone, PartialEq)]
struct Pending {
    record: Record,
    /// 飛行中のバッチに載せた時点のスナップショット。
    sent: Option<Record>,
    attempts: u32,
    retry_at: u64,
}

/// ディスク上の形 (`RecordKey` は JSON のキーにできないので配列にする)。
#[derive(Serialize, Deserialize)]
struct QueueDisk {
    max_pending: usize,
    entries: Vec<PendingDisk>,
    next_batch_id: u64,
    needs_full_resync: bool,
}

#[derive(Serialize, Deserialize)]
struct PendingDisk {
    record: Record,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    retry_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "QueueDisk", into = "QueueDisk")]
pub struct OutboundQueue {
    max_pending: usize,
    entries: BTreeMap<RecordKey, Pending>,
    in_flight: Option<BatchId>,
    next_batch_id: u64,
    needs_full_resync: bool,
}

impl From<QueueDisk> for OutboundQueue {
    fn from(d: QueueDisk) -> Self {
        let mut q = OutboundQueue::with_capacity(d.max_pending);
        q.next_batch_id = d.next_batch_id;
        q.needs_full_resync = d.needs_full_resync;
        for e in d.entries.into_iter().take(q.max_pending) {
            // 飛行中だったバッチは未送信に戻る (`sent: None`)。
            q.entries.insert(
                e.record.key.clone(),
                Pending {
                    record: e.record,
                    sent: None,
                    attempts: e.attempts,
                    retry_at: e.retry_at,
                },
            );
        }
        q
    }
}

impl From<OutboundQueue> for QueueDisk {
    fn from(q: OutboundQueue) -> Self {
        QueueDisk {
            max_pending: q.max_pending,
            entries: q
                .entries
                .into_values()
                .map(|p| PendingDisk {
                    record: p.record,
                    attempts: p.attempts,
                    retry_at: p.retry_at,
                })
                .collect(),
            next_batch_id: q.next_batch_id,
            needs_full_resync: q.needs_full_resync,
        }
    }
}

impl Default for OutboundQueue {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MAX_PENDING)
    }
}

impl OutboundQueue {
    pub fn with_capacity(max_pending: usize) -> Self {
        Self {
            max_pending: max_pending.max(1),
            entries: BTreeMap::new(),
            in_flight: None,
            next_batch_id: 1,
            needs_full_resync: false,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn needs_full_resync(&self) -> bool {
        self.needs_full_resync
    }

    /// 全量再送を送り終えたら呼ぶ。
    pub fn clear_full_resync(&mut self) {
        self.needs_full_resync = false;
    }

    /// 全量再送が必要になったことを外から立てる (ローカル状態の破損復旧など)。
    pub fn request_full_resync(&mut self) {
        self.needs_full_resync = true;
    }

    /// 次に再送できる時刻 (待つ必要がなければ `None`)。飛行中は `None`。
    pub fn next_retry_at(&self) -> Option<u64> {
        if self.in_flight.is_some() {
            return None;
        }
        self.entries.values().map(|p| p.retry_at).min()
    }

    pub fn enqueue(&mut self, record: Record) -> EnqueueOutcome {
        if let Some(p) = self.entries.get_mut(&record.key) {
            p.record.merge(&record);
            return EnqueueOutcome::Coalesced;
        }
        if self.entries.len() >= self.max_pending {
            self.entries.clear();
            self.in_flight = None;
            self.needs_full_resync = true;
            return EnqueueOutcome::Overflowed;
        }
        self.entries.insert(
            record.key.clone(),
            Pending {
                record,
                sent: None,
                attempts: 0,
                retry_at: 0,
            },
        );
        EnqueueOutcome::Queued
    }

    /// 送れるもの (待ち時間が明けた未送信) を最大 `max` 件バッチにする。
    /// 飛行中のバッチがあれば `None` (直列に送る)。
    pub fn next_batch(&mut self, now_ms: u64, max: usize) -> Option<Batch> {
        if self.in_flight.is_some() || max == 0 {
            return None;
        }
        let mut records = Vec::new();
        for p in self.entries.values_mut() {
            if records.len() >= max {
                break;
            }
            if p.retry_at <= now_ms {
                p.sent = Some(p.record.clone());
                records.push(p.record.clone());
            }
        }
        if records.is_empty() {
            return None;
        }
        let id = BatchId(self.next_batch_id);
        self.next_batch_id += 1;
        self.in_flight = Some(id);
        Some(Batch { id, records })
    }

    /// 送達確認。送信後に同じレコードへ追加の変更が結合されていたら、その
    /// 差分を残す。未知の (古い/オーバーフローで消えた) バッチ ID は無視。
    pub fn ack(&mut self, id: BatchId) -> bool {
        if self.in_flight != Some(id) {
            return false;
        }
        self.in_flight = None;
        self.entries.retain(|_, p| match p.sent.take() {
            Some(sent) if sent == p.record => false,
            Some(_) => {
                p.attempts = 0;
                p.retry_at = 0;
                true
            }
            None => true,
        });
        true
    }

    /// 送信失敗。バッチを未送信に戻し、指数バックオフを掛ける。
    pub fn nack(&mut self, id: BatchId, now_ms: u64) -> bool {
        if self.in_flight != Some(id) {
            return false;
        }
        self.in_flight = None;
        for p in self.entries.values_mut() {
            if p.sent.take().is_some() {
                p.attempts = p.attempts.saturating_add(1);
                p.retry_at = now_ms.saturating_add(backoff_ms(p.attempts));
            }
        }
        true
    }

    /// 飛行中のバッチを (待ち時間なしで) 未送信に戻す。接続が切れた時など。
    pub fn abort_in_flight(&mut self) {
        self.in_flight = None;
        for p in self.entries.values_mut() {
            p.sent = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::sync::record::tests::hlc;
    use crate::browser::sync::record::Kind;
    use serde_json::json;

    fn put(id: &str, wall: u64, field: &str, v: i64) -> Record {
        Record::put(
            RecordKey::new(Kind::Bookmark, id),
            &hlc(wall, 0, "a"),
            [(field.to_string(), json!(v))],
        )
    }

    #[test]
    fn updates_to_the_same_record_coalesce() {
        let mut q = OutboundQueue::default();
        assert_eq!(q.enqueue(put("a", 1, "t", 1)), EnqueueOutcome::Queued);
        assert_eq!(q.enqueue(put("a", 2, "t", 2)), EnqueueOutcome::Coalesced);
        assert_eq!(q.enqueue(put("a", 3, "u", 9)), EnqueueOutcome::Coalesced);
        assert_eq!(q.len(), 1);
        let b = q.next_batch(0, 10).unwrap();
        assert_eq!(b.records.len(), 1);
        assert_eq!(b.records[0].get("t"), Some(&json!(2)));
        assert_eq!(b.records[0].get("u"), Some(&json!(9)));
    }

    #[test]
    fn delete_after_puts_collapses_into_tombstone_only() {
        let mut q = OutboundQueue::default();
        q.enqueue(put("a", 1, "t", 1));
        q.enqueue(Record::delete(
            RecordKey::new(Kind::Bookmark, "a"),
            &hlc(5, 0, "a"),
        ));
        let b = q.next_batch(0, 10).unwrap();
        assert!(!b.records[0].is_live());
        assert!(b.records[0].tombstone.is_some());
    }

    #[test]
    fn ack_removes_and_nack_backs_off_then_retries() {
        let mut q = OutboundQueue::default();
        q.enqueue(put("a", 1, "t", 1));
        let b = q.next_batch(100, 10).unwrap();
        assert!(
            q.next_batch(100, 10).is_none(),
            "serial: one batch in flight"
        );
        assert!(q.nack(b.id, 100));
        assert_eq!(q.next_retry_at(), Some(100 + BACKOFF_BASE_MS));
        assert!(q.next_batch(100 + BACKOFF_BASE_MS - 1, 10).is_none());
        let b2 = q.next_batch(100 + BACKOFF_BASE_MS, 10).unwrap();
        assert_ne!(b.id, b2.id);
        assert!(q.nack(b2.id, 2_000));
        assert_eq!(q.next_retry_at(), Some(2_000 + 2 * BACKOFF_BASE_MS));
        let b3 = q.next_batch(10_000, 10).unwrap();
        assert!(q.ack(b3.id));
        assert!(q.is_empty());
        assert!(!q.ack(b3.id), "double ack ignored");
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_ms(1), 1_000);
        assert_eq!(backoff_ms(2), 2_000);
        assert_eq!(backoff_ms(3), 4_000);
        assert_eq!(backoff_ms(50), BACKOFF_CAP_MS);
        assert_eq!(backoff_ms(u32::MAX), BACKOFF_CAP_MS);
    }

    #[test]
    fn edit_during_flight_survives_ack() {
        let mut q = OutboundQueue::default();
        q.enqueue(put("a", 1, "t", 1));
        let b = q.next_batch(0, 10).unwrap();
        q.enqueue(put("a", 2, "t", 2)); // 送信中に追加の編集
        assert!(q.ack(b.id));
        assert_eq!(q.len(), 1, "the newer edit is still owed");
        let b2 = q.next_batch(0, 10).unwrap();
        assert_eq!(b2.records[0].get("t"), Some(&json!(2)));
        assert!(q.ack(b2.id));
        assert!(q.is_empty());
    }

    #[test]
    fn batch_size_is_respected() {
        let mut q = OutboundQueue::default();
        for i in 0..5 {
            q.enqueue(put(&format!("r{i}"), 1, "t", 1));
        }
        assert_eq!(q.next_batch(0, 2).unwrap().records.len(), 2);
    }

    #[test]
    fn overflow_clears_and_requests_full_resync() {
        let mut q = OutboundQueue::with_capacity(2);
        q.enqueue(put("a", 1, "t", 1));
        q.enqueue(put("b", 1, "t", 1));
        assert_eq!(q.enqueue(put("c", 1, "t", 1)), EnqueueOutcome::Overflowed);
        assert!(q.is_empty());
        assert!(q.needs_full_resync());
        // 既存キーへの結合は上限に掛からない。
        q.clear_full_resync();
        q.enqueue(put("a", 1, "t", 1));
        assert_eq!(q.enqueue(put("a", 2, "t", 2)), EnqueueOutcome::Coalesced);
        assert!(!q.needs_full_resync());
    }

    #[test]
    fn overflow_while_in_flight_makes_stale_ack_harmless() {
        let mut q = OutboundQueue::with_capacity(1);
        q.enqueue(put("a", 1, "t", 1));
        let b = q.next_batch(0, 10).unwrap();
        assert_eq!(q.enqueue(put("b", 1, "t", 1)), EnqueueOutcome::Overflowed);
        assert!(!q.ack(b.id));
        assert!(q.needs_full_resync());
    }

    #[test]
    fn survives_restart_and_returns_in_flight_to_pending() {
        let mut q = OutboundQueue::default();
        q.enqueue(put("a", 1, "t", 1));
        q.enqueue(put("b", 1, "t", 1));
        let b = q.next_batch(0, 1).unwrap();
        q.nack(b.id, 50);
        let _ = q.next_batch(10_000, 10).unwrap(); // 飛行中のまま保存
        let json = serde_json::to_string(&q).unwrap();
        let mut back: OutboundQueue = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        let again = back.next_batch(10_000, 10).unwrap();
        assert_eq!(again.records.len(), 2);
    }

    #[test]
    fn abort_in_flight_requeues_without_penalty() {
        let mut q = OutboundQueue::default();
        q.enqueue(put("a", 1, "t", 1));
        let _ = q.next_batch(0, 10).unwrap();
        q.abort_in_flight();
        assert!(q.next_batch(0, 10).is_some());
    }
}
