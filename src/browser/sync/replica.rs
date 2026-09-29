//! 1 端末ぶんの同期エンジン (トランスポート抽象つき)。docs/sync.md §3, §7, §8。
//!
//! `Transport` は「暗号化済みの不透明なバイト列を追記ログに積む/カーソルで
//! 読む」だけを要求する。サーバの実装・認証・E2E 暗号化 (#86) はこの trait の
//! 向こう側で、ここは知らない。実アプリには**まだ接続していない**。

use std::fmt;

use serde_json::Value;

use super::clock::{DeviceId, HlcClock};
use super::merge::SyncState;
use super::protocol::{DecodeError, Envelope, VersionRange};
use super::queue::{EnqueueOutcome, OutboundQueue};
use super::record::{Kind, Record, RecordKey};
use super::scope::SyncScope;

/// 1 エンベロープに載せるレコード数。
pub const PUSH_BATCH: usize = 100;
/// 1 回の pull で受け取るペイロード数の上限。
pub const PULL_LIMIT: usize = 100;

/// サーバが採番する不透明な位置 (増分同期のカーソル)。`0` = 最初から。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Cursor(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Offline,
    Server(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Offline => f.write_str("offline"),
            TransportError::Server(m) => write!(f, "server error: {m}"),
        }
    }
}

impl std::error::Error for TransportError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullBatch {
    pub payloads: Vec<Vec<u8>>,
    pub next: Cursor,
    pub more: bool,
}

pub trait Transport {
    /// ペイロードをサーバのログに追記する。`Ok` は「永続化した」の意味。
    fn push(&mut self, payload: &[u8]) -> Result<(), TransportError>;
    /// `after` より後のペイロードを最大 `limit` 件。
    fn pull(&mut self, after: Cursor, limit: usize) -> Result<PullBatch, TransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    Transport(TransportError),
    /// 未対応の新しいプロトコル版に出会った。適用も、カーソルを進めることも
    /// しない (アップデート後にやり直せば取りこぼさない)。
    UpgradeRequired(u32),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Transport(e) => write!(f, "{e}"),
            SyncError::UpgradeRequired(v) => write!(f, "sync protocol v{v} requires an update"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<TransportError> for SyncError {
    fn from(e: TransportError) -> Self {
        SyncError::Transport(e)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub pushed_records: usize,
    pub applied_records: usize,
    /// 検証・時計・スコープではじいたレコードと、壊れたペイロードの数。
    pub rejected: usize,
}

#[derive(Debug)]
pub struct Replica {
    clock: HlcClock,
    pub state: SyncState,
    pub queue: OutboundQueue,
    cursor: Cursor,
    scope: SyncScope,
    accept: VersionRange,
}

impl Replica {
    pub fn new(device: DeviceId, scope: SyncScope) -> Self {
        Self {
            clock: HlcClock::new(device),
            state: SyncState::new(),
            queue: OutboundQueue::default(),
            cursor: Cursor::default(),
            scope,
            accept: VersionRange::CURRENT,
        }
    }

    pub fn device(&self) -> &DeviceId {
        self.clock.device()
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn scope(&self) -> SyncScope {
        self.scope
    }

    pub fn set_scope(&mut self, scope: SyncScope) {
        self.scope = scope;
    }

    /// ローカルでフィールドを書く。同期対象外 (種別オフ・プライベート
    /// モード) なら何もせず `false`。
    pub fn local_put(
        &mut self,
        now_ms: u64,
        kind: Kind,
        id: &str,
        fields: impl IntoIterator<Item = (String, Value)>,
        private_mode: bool,
    ) -> bool {
        if !self.scope.accepts(kind, private_mode) {
            return false;
        }
        let hlc = self.clock.tick(now_ms);
        let rec = Record::put(RecordKey::new(kind, id), &hlc, fields);
        self.commit_local(rec);
        true
    }

    pub fn local_delete(&mut self, now_ms: u64, kind: Kind, id: &str, private_mode: bool) -> bool {
        if !self.scope.accepts(kind, private_mode) {
            return false;
        }
        let hlc = self.clock.tick(now_ms);
        self.commit_local(Record::delete(RecordKey::new(kind, id), &hlc));
        true
    }

    fn commit_local(&mut self, rec: Record) {
        self.state.apply(&rec);
        // オーバーフロー時は全量再送に切り替わる。ここでは何もしなくてよい。
        let _: EnqueueOutcome = self.queue.enqueue(rec);
    }

    /// 1 往復: 溜まった変更を送り、増分を受け取る。オフラインなら
    /// `Err(Transport(Offline))` で、キューは残る (次回のバックオフ後に再送)。
    pub fn sync(
        &mut self,
        transport: &mut dyn Transport,
        now_ms: u64,
    ) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::default();
        self.push_all(transport, now_ms, &mut report)?;
        self.pull_all(transport, now_ms, &mut report)?;
        Ok(report)
    }

    fn push_all(
        &mut self,
        transport: &mut dyn Transport,
        now_ms: u64,
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        if self.queue.needs_full_resync() {
            // キューを介さず、状態の全量 (墓石込み) を直接送る。
            let all: Vec<Record> = self
                .state
                .all()
                .filter(|r| self.scope.kind_enabled(r.key.kind))
                .cloned()
                .collect();
            for chunk in all.chunks(PUSH_BATCH) {
                let bytes = Envelope::new(self.device().clone(), chunk.to_vec()).encode();
                transport.push(&bytes)?;
                report.pushed_records += chunk.len();
            }
            self.queue.clear_full_resync();
        }
        while let Some(batch) = self.queue.next_batch(now_ms, PUSH_BATCH) {
            let bytes = Envelope::new(self.device().clone(), batch.records.clone()).encode();
            match transport.push(&bytes) {
                Ok(()) => {
                    report.pushed_records += batch.records.len();
                    self.queue.ack(batch.id);
                }
                Err(e) => {
                    self.queue.nack(batch.id, now_ms);
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    fn pull_all(
        &mut self,
        transport: &mut dyn Transport,
        now_ms: u64,
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        loop {
            let resp = transport.pull(self.cursor, PULL_LIMIT)?;
            for payload in &resp.payloads {
                match Envelope::decode(payload, self.accept) {
                    Ok(env) => {
                        if env.device != *self.device() {
                            self.apply_remote(env.records, now_ms, report);
                        }
                    }
                    Err(DecodeError::UnsupportedVersion(v)) => {
                        return Err(SyncError::UpgradeRequired(v));
                    }
                    Err(_) => report.rejected += 1,
                }
            }
            // 全ペイロードを処理し終えてからカーソルを進める (途中で
            // UpgradeRequired なら進めない)。
            self.cursor = resp.next;
            if !resp.more {
                return Ok(());
            }
        }
    }

    fn apply_remote(&mut self, records: Vec<Record>, now_ms: u64, report: &mut SyncReport) {
        for rec in records {
            if !self.scope.kind_enabled(rec.key.kind) {
                report.rejected += 1;
                continue;
            }
            if let Some(h) = rec.max_hlc() {
                if self.clock.observe(h, now_ms).is_err() {
                    report.rejected += 1;
                    continue;
                }
            }
            if self.state.apply(&rec) {
                report.applied_records += 1;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// サーバのログ (追記のみ)。カーソル = 位置。
    #[derive(Default)]
    pub struct ServerLog {
        pub entries: Vec<Vec<u8>>,
    }

    pub struct FakeTransport {
        pub log: Rc<RefCell<ServerLog>>,
        pub online: bool,
        /// `true` の間、push はログに積むが応答を失う (ack ロスト)。
        pub lose_push_ack: bool,
        pub pushes: usize,
    }

    impl FakeTransport {
        pub fn new(log: &Rc<RefCell<ServerLog>>) -> Self {
            Self {
                log: Rc::clone(log),
                online: true,
                lose_push_ack: false,
                pushes: 0,
            }
        }
    }

    impl Transport for FakeTransport {
        fn push(&mut self, payload: &[u8]) -> Result<(), TransportError> {
            if !self.online {
                return Err(TransportError::Offline);
            }
            self.log.borrow_mut().entries.push(payload.to_vec());
            self.pushes += 1;
            if self.lose_push_ack {
                return Err(TransportError::Server("timeout".into()));
            }
            Ok(())
        }

        fn pull(&mut self, after: Cursor, limit: usize) -> Result<PullBatch, TransportError> {
            if !self.online {
                return Err(TransportError::Offline);
            }
            let log = self.log.borrow();
            let start = (after.0 as usize).min(log.entries.len());
            let end = (start + limit).min(log.entries.len());
            Ok(PullBatch {
                payloads: log.entries[start..end].to_vec(),
                next: Cursor(end as u64),
                more: end < log.entries.len(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use crate::browser::sync::merge::{bookmark_view, history_view, settings_view};
    use crate::browser::sync::queue::BACKOFF_BASE_MS;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn replica(name: &str) -> Replica {
        Replica::new(DeviceId::new(name).unwrap(), SyncScope::default())
    }

    fn f(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn server() -> Rc<RefCell<ServerLog>> {
        Rc::new(RefCell::new(ServerLog::default()))
    }

    fn add_bookmark(r: &mut Replica, now: u64, id: &str, url: &str) {
        r.local_put(
            now,
            Kind::Bookmark,
            id,
            f(&[("url", json!(url)), ("created_at", json!(now))]),
            false,
        );
    }

    #[test]
    fn two_devices_sync_through_the_log() {
        let log = server();
        let (mut a, mut b) = (replica("a"), replica("b"));
        let (mut ta, mut tb) = (FakeTransport::new(&log), FakeTransport::new(&log));
        add_bookmark(&mut a, 1000, "b1", "https://x/");
        a.sync(&mut ta, 1000).unwrap();
        let rep = b.sync(&mut tb, 1000).unwrap();
        assert_eq!(rep.applied_records, 1);
        assert_eq!(bookmark_view(&a.state), bookmark_view(&b.state));
        assert_eq!(b.cursor(), Cursor(1));
        // 増分: 何も無ければ何も適用されない。
        assert_eq!(b.sync(&mut tb, 1001).unwrap().applied_records, 0);
    }

    #[test]
    fn offline_edits_on_both_sides_converge_after_reconnect() {
        let log = server();
        let (mut a, mut b) = (replica("a"), replica("b"));
        let (mut ta, mut tb) = (FakeTransport::new(&log), FakeTransport::new(&log));
        add_bookmark(&mut a, 1000, "b1", "https://x/");
        a.sync(&mut ta, 1000).unwrap();
        b.sync(&mut tb, 1000).unwrap();

        ta.online = false;
        tb.online = false;
        // a: タイトル編集、b: 同じ項目の別フィールド編集 + 同じタイトルも編集。
        a.local_put(
            2000,
            Kind::Bookmark,
            "b1",
            f(&[("title", json!("from-a"))]),
            false,
        );
        b.local_put(
            2500,
            Kind::Bookmark,
            "b1",
            f(&[("folder", json!("nope"))]),
            false,
        );
        b.local_put(
            2600,
            Kind::Bookmark,
            "b1",
            f(&[("title", json!("from-b"))]),
            false,
        );
        assert_eq!(
            a.sync(&mut ta, 3000),
            Err(SyncError::Transport(TransportError::Offline))
        );
        assert!(b.sync(&mut tb, 3000).is_err());
        assert_eq!(a.queue.len(), 1);
        assert_eq!(
            b.queue.len(),
            1,
            "3 edits coalesced into one pending record"
        );

        ta.online = true;
        tb.online = true;
        // バックオフが明けてから再送される。
        let later = 3000 + BACKOFF_BASE_MS;
        a.sync(&mut ta, later).unwrap();
        b.sync(&mut tb, later).unwrap();
        a.sync(&mut ta, later).unwrap();
        assert_eq!(a.state, b.state);
        let v = bookmark_view(&a.state);
        assert_eq!(v[0].title.as_deref(), Some("from-b")); // 後の HLC が勝つ
        assert_eq!(v[0].folder, None); // 実在しないフォルダはルートへ
        assert!(a.queue.is_empty() && b.queue.is_empty());
    }

    #[test]
    fn backoff_holds_back_retries_until_due() {
        let log = server();
        let mut a = replica("a");
        let mut t = FakeTransport::new(&log);
        add_bookmark(&mut a, 1000, "b1", "https://x/");
        t.online = false;
        assert!(a.sync(&mut t, 1000).is_err());
        t.online = true;
        // まだ待ち時間内: 送られない (pull だけ成功)。
        a.sync(&mut t, 1000 + BACKOFF_BASE_MS - 1).unwrap();
        assert_eq!(t.pushes, 0);
        a.sync(&mut t, 1000 + BACKOFF_BASE_MS).unwrap();
        assert_eq!(t.pushes, 1);
    }

    #[test]
    fn lost_ack_causes_duplicate_delivery_but_same_state() {
        let log = server();
        let (mut a, mut b) = (replica("a"), replica("b"));
        let (mut ta, mut tb) = (FakeTransport::new(&log), FakeTransport::new(&log));
        add_bookmark(&mut a, 1000, "b1", "https://x/");
        ta.lose_push_ack = true;
        assert!(a.sync(&mut ta, 1000).is_err());
        ta.lose_push_ack = false;
        a.sync(&mut ta, 1000 + BACKOFF_BASE_MS).unwrap();
        assert_eq!(log.borrow().entries.len(), 2, "delivered twice");
        b.sync(&mut tb, 5000).unwrap();
        assert_eq!(a.state, b.state);
    }

    #[test]
    fn delete_on_one_device_vs_edit_on_another() {
        let log = server();
        let (mut a, mut b) = (replica("a"), replica("b"));
        let (mut ta, mut tb) = (FakeTransport::new(&log), FakeTransport::new(&log));
        add_bookmark(&mut a, 1000, "b1", "https://x/");
        a.sync(&mut ta, 1000).unwrap();
        b.sync(&mut tb, 1000).unwrap();
        // 同時に: a は削除、b は削除より後にタイトル編集。
        a.local_delete(2000, Kind::Bookmark, "b1", false);
        b.local_put(
            3000,
            Kind::Bookmark,
            "b1",
            f(&[("title", json!("edited"))]),
            false,
        );
        for _ in 0..2 {
            a.sync(&mut ta, 9000).unwrap();
            b.sync(&mut tb, 9000).unwrap();
        }
        assert_eq!(a.state, b.state);
        // 後の編集が勝って復活するが、url は削除前の値なので欠け、
        // 未完成として隠れる (壊れた項目を見せない)。
        assert!(bookmark_view(&a.state).is_empty());
    }

    #[test]
    fn delete_wins_over_earlier_concurrent_edit() {
        let log = server();
        let (mut a, mut b) = (replica("a"), replica("b"));
        let (mut ta, mut tb) = (FakeTransport::new(&log), FakeTransport::new(&log));
        add_bookmark(&mut a, 1000, "b1", "https://x/");
        a.sync(&mut ta, 1000).unwrap();
        b.sync(&mut tb, 1000).unwrap();
        b.local_put(
            2000,
            Kind::Bookmark,
            "b1",
            f(&[("title", json!("edited"))]),
            false,
        );
        a.local_delete(3000, Kind::Bookmark, "b1", false);
        for _ in 0..2 {
            a.sync(&mut ta, 9000).unwrap();
            b.sync(&mut tb, 9000).unwrap();
        }
        assert_eq!(a.state, b.state);
        assert!(bookmark_view(&a.state).is_empty());
    }

    #[test]
    fn three_devices_converge_for_every_delivery_order() {
        // 3 台がオフラインで好き勝手に編集 → 出てきた全レコードを、あらゆる
        // 順序 (3! ではなく全 n! の一部 + 重複) で新しい端末に流し込んでも
        // 同じ状態になる。
        let mut devs = [replica("a"), replica("b"), replica("c")];
        add_bookmark(&mut devs[0], 100, "x", "https://x/");
        add_bookmark(&mut devs[1], 110, "y", "https://y/");
        add_bookmark(&mut devs[2], 120, "z", "https://x/"); // URL 重複
        devs[0].local_put(200, Kind::Bookmark, "x", f(&[("title", json!("A"))]), false);
        devs[1].local_put(210, Kind::Bookmark, "x", f(&[("title", json!("B"))]), false);
        devs[2].local_put(220, Kind::Bookmark, "x", f(&[("order", json!(3))]), false);
        devs[1].local_delete(230, Kind::Bookmark, "y", false);
        devs[2].local_put(
            240,
            Kind::Bookmark,
            "y",
            f(&[("title", json!("Y2"))]),
            false,
        );
        devs[0].local_put(
            250,
            Kind::Setting,
            "appearance.theme",
            f(&[("value", json!("dark"))]),
            false,
        );
        devs[2].local_put(
            250,
            Kind::Setting,
            "appearance.theme",
            f(&[("value", json!("light"))]),
            false,
        );

        let mut all: Vec<Record> = Vec::new();
        for d in &mut devs {
            while let Some(b) = d.queue.next_batch(0, 1) {
                all.extend(b.records);
                d.queue.ack(b.id);
            }
        }
        assert!(all.len() >= 6);

        let mut reference = SyncState::new();
        for r in &all {
            reference.apply(r);
        }
        let mut idx: Vec<usize> = (0..all.len()).collect();
        let mut count = 0;
        permute(&mut idx, 0, &mut |order| {
            let mut s = SyncState::new();
            for &i in order {
                s.apply(&all[i]);
            }
            // 重複配送も無害。
            for &i in order.iter().rev() {
                s.apply(&all[i]);
            }
            assert_eq!(s, reference, "order {order:?}");
            count += 1;
        });
        assert!(count >= 720);
        assert_eq!(
            settings_view(&reference)["appearance.theme"],
            json!("light")
        );
    }

    fn permute(v: &mut Vec<usize>, k: usize, f: &mut dyn FnMut(&[usize])) {
        if k == v.len() {
            f(v);
            return;
        }
        for i in k..v.len() {
            v.swap(k, i);
            permute(v, k + 1, f);
            v.swap(k, i);
        }
    }

    #[test]
    fn three_devices_full_sync_via_server_in_different_schedules() {
        let run = |schedule: &[usize]| {
            let log = server();
            let mut devs = [replica("a"), replica("b"), replica("c")];
            let mut ts: Vec<_> = (0..3).map(|_| FakeTransport::new(&log)).collect();
            add_bookmark(&mut devs[0], 100, "x", "https://x/");
            add_bookmark(&mut devs[1], 101, "y", "https://y/");
            devs[2].local_put(
                102,
                Kind::Setting,
                "search.engine",
                f(&[("value", json!("ddg"))]),
                false,
            );
            devs[1].local_put(300, Kind::Bookmark, "x", f(&[("title", json!("t"))]), false);
            for (round, &i) in schedule.iter().enumerate() {
                let _ = devs[i].sync(&mut ts[i], 10_000 * (round as u64 + 1));
            }
            for i in 0..3 {
                for round in 0..3 {
                    devs[i].sync(&mut ts[i], 1_000_000 + round).unwrap();
                }
            }
            (
                bookmark_view(&devs[0].state),
                settings_view(&devs[0].state),
                devs[0].state == devs[1].state && devs[1].state == devs[2].state,
            )
        };
        let base = run(&[0, 1, 2]);
        assert!(base.2);
        assert_eq!(base.0.len(), 2);
        for sched in [&[2, 1, 0][..], &[1, 1, 0, 2], &[2, 0, 2, 1, 0]] {
            let r = run(sched);
            assert!(r.2);
            assert_eq!((r.0, r.1), (base.0.clone(), base.1.clone()));
        }
    }

    #[test]
    fn private_mode_and_scope_keep_data_local() {
        let mut a = replica("a");
        assert!(!a.local_put(
            1,
            Kind::Bookmark,
            "p",
            f(&[("url", json!("https://p/"))]),
            true
        ));
        assert!(!a.local_put(
            1,
            Kind::History,
            "h",
            f(&[("url", json!("https://h/"))]),
            false
        ));
        assert!(a.state.is_empty());
        assert!(a.queue.is_empty());
        assert!(!a.local_delete(2, Kind::Bookmark, "p", true));
    }

    #[test]
    fn remote_records_outside_scope_are_not_applied() {
        let log = server();
        let mut sender = Replica::new(
            DeviceId::new("s").unwrap(),
            SyncScope {
                history: true,
                ..SyncScope::default()
            },
        );
        let mut me = replica("m"); // 履歴オフ
        let (mut ts, mut tm) = (FakeTransport::new(&log), FakeTransport::new(&log));
        sender.local_put(
            1000,
            Kind::History,
            "h1",
            f(&[("url", json!("https://h/")), ("visited_at", json!(5))]),
            false,
        );
        sender.sync(&mut ts, 1000).unwrap();
        let rep = me.sync(&mut tm, 1000).unwrap();
        assert_eq!(rep.rejected, 1);
        assert!(history_view(&me.state).is_empty());
    }

    #[test]
    fn far_future_remote_clock_is_rejected() {
        let log = server();
        let evil = Envelope::new(
            DeviceId::new("evil").unwrap(),
            vec![Record::put(
                RecordKey::new(Kind::Bookmark, "e"),
                &crate::browser::sync::clock::Hlc {
                    wall: u64::MAX / 2,
                    counter: 0,
                    device: DeviceId::new("evil").unwrap(),
                },
                [("url".to_string(), json!("https://e/"))],
            )],
        );
        log.borrow_mut().entries.push(evil.encode());
        let mut a = replica("a");
        let mut t = FakeTransport::new(&log);
        let rep = a.sync(&mut t, 1000).unwrap();
        assert_eq!(rep.rejected, 1);
        assert!(a.state.is_empty());
    }

    #[test]
    fn future_protocol_stops_sync_without_advancing_cursor() {
        let log = server();
        log.borrow_mut()
            .entries
            .push(br#"{"v":2,"device":"z","records":[]}"#.to_vec());
        let mut a = replica("a");
        let mut t = FakeTransport::new(&log);
        assert_eq!(a.sync(&mut t, 1), Err(SyncError::UpgradeRequired(2)));
        assert_eq!(a.cursor(), Cursor(0));
    }

    #[test]
    fn garbage_payloads_are_counted_and_skipped() {
        let log = server();
        log.borrow_mut().entries.push(b"\xff\xfe".to_vec());
        let mut a = replica("a");
        let mut t = FakeTransport::new(&log);
        let rep = a.sync(&mut t, 1).unwrap();
        assert_eq!(rep.rejected, 1);
        assert_eq!(a.cursor(), Cursor(1));
    }

    #[test]
    fn queue_overflow_recovers_via_full_resync() {
        let log = server();
        let (mut a, mut b) = (replica("a"), replica("b"));
        a.queue = OutboundQueue::with_capacity(2);
        let (mut ta, mut tb) = (FakeTransport::new(&log), FakeTransport::new(&log));
        for i in 0..5 {
            add_bookmark(&mut a, 1000 + i, &format!("b{i}"), &format!("https://{i}/"));
        }
        assert!(a.queue.needs_full_resync());
        a.sync(&mut ta, 2000).unwrap();
        assert!(!a.queue.needs_full_resync());
        b.sync(&mut tb, 2000).unwrap();
        assert_eq!(bookmark_view(&b.state).len(), 5);
        assert_eq!(a.state, b.state);
    }

    #[test]
    fn late_joiner_bootstraps_from_cursor_zero_including_tombstones() {
        let log = server();
        let mut a = replica("a");
        let mut ta = FakeTransport::new(&log);
        add_bookmark(&mut a, 1000, "b1", "https://1/");
        add_bookmark(&mut a, 1001, "b2", "https://2/");
        a.local_delete(1002, Kind::Bookmark, "b1", false);
        a.sync(&mut ta, 1002).unwrap();
        let mut c = replica("c");
        let mut tc = FakeTransport::new(&log);
        c.sync(&mut tc, 2000).unwrap();
        assert_eq!(bookmark_view(&c.state).len(), 1);
        assert_eq!(a.state, c.state);
    }
}
