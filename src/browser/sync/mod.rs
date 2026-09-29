//! 複数端末同期のための純粋ロジック (Issue #85 / Epic #80 / D160)。
//!
//! **アプリ本体にはまだ接続していない** (挙動の変更なし)。サーバも
//! アカウントも E2E 暗号化 (#86) も無い状態で、設計を実行可能な形で固定する
//! ためのモジュールで、`docs/sync.md` が仕様。
//!
//! - `clock` — 端末 ID とハイブリッド論理時計 (時刻は注入)
//! - `record` — フィールド単位 LWW + 墓石のレコード (デルタ = 状態)
//! - `merge` — 状態 ([`SyncState`]) と、ブックマーク/設定/履歴のビュー
//! - `scope` — 何を同期するか (プライベートモードは常に除外)
//! - `queue` — オフライン送信キュー (畳み込み・上限・ack/再送)
//! - `protocol` — エンベロープ・版交渉
//! - `replica` — `Transport` trait と 1 端末ぶんのエンジン

pub mod clock;
pub mod merge;
pub mod protocol;
pub mod queue;
pub mod record;
pub mod replica;
pub mod scope;

pub use clock::{ClockError, DeviceId, Hlc, HlcClock, InvalidDeviceId};
pub use merge::{
    bookmark_view, folder_view, history_view, is_syncable_setting, settings_view, BookmarkView,
    FolderView, HistoryView, SyncState,
};
pub use protocol::{
    negotiate, DecodeError, Envelope, NegotiationError, VersionRange, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
pub use queue::{Batch, BatchId, EnqueueOutcome, OutboundQueue};
pub use record::{Field, Kind, Record, RecordError, RecordKey};
pub use replica::{Cursor, PullBatch, Replica, SyncError, SyncReport, Transport, TransportError};
pub use scope::SyncScope;
