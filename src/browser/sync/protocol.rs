//! 線上プロトコルの形とバージョン交渉。docs/sync.md §9。
//!
//! ペイロードは JSON。暗号化・認証はこの層の外 (#86)。この層は
//! 「復号済みの平文エンベロープ」だけを知る。

use std::fmt;

use serde::{Deserialize, Serialize};

use super::clock::DeviceId;
use super::record::{Record, RecordError};

/// この実装が話すプロトコルの最新版。
pub const PROTOCOL_VERSION: u32 = 1;
/// この実装が読める最古の版。
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// 1 エンベロープのバイト数上限。
pub const MAX_ENVELOPE_BYTES: usize = 4 * 1024 * 1024;
/// 1 エンベロープのレコード数上限。
pub const MAX_RECORDS_PER_ENVELOPE: usize = 1000;

/// 話せる版の範囲 (両端を含む)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    pub min: u32,
    pub max: u32,
}

impl VersionRange {
    pub const CURRENT: VersionRange = VersionRange {
        min: MIN_PROTOCOL_VERSION,
        max: PROTOCOL_VERSION,
    };

    pub fn contains(&self, v: u32) -> bool {
        self.min <= v && v <= self.max
    }
}

/// 交渉の失敗: 共通の版が無い。どちら側が古いかで利用者への案内が変わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationError {
    /// 相手の方が新しい (こちらを更新すべき)。
    LocalTooOld,
    /// 相手の方が古い (相手の更新待ち)。
    RemoteTooOld,
    /// 範囲自体が不正 (`min > max`)。
    Malformed,
}

impl fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NegotiationError::LocalTooOld => "peer requires a newer sync protocol",
            NegotiationError::RemoteTooOld => "peer only speaks an older sync protocol",
            NegotiationError::Malformed => "malformed version range",
        })
    }
}

impl std::error::Error for NegotiationError {}

/// 共通の最大の版を選ぶ。
pub fn negotiate(local: VersionRange, remote: VersionRange) -> Result<u32, NegotiationError> {
    if local.min > local.max || remote.min > remote.max {
        return Err(NegotiationError::Malformed);
    }
    let v = local.max.min(remote.max);
    if v >= local.min && v >= remote.min {
        Ok(v)
    } else if remote.min > local.max {
        Err(NegotiationError::LocalTooOld)
    } else {
        Err(NegotiationError::RemoteTooOld)
    }
}

/// 送受信の単位。レコードはデルタ (どんな順序・重複でも結合できる)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// プロトコル版。**フィールド追加は同じ版のまま** (未知フィールドは
    /// 無視される)、意味の変わる変更だけ上げる。
    pub v: u32,
    pub device: DeviceId,
    #[serde(default)]
    pub records: Vec<Record>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    TooLarge,
    Malformed,
    /// `v` が範囲外。適用せず、更新を促す状態に入る。
    UnsupportedVersion(u32),
    TooManyRecords,
    InvalidRecord(RecordError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::TooLarge => f.write_str("envelope too large"),
            DecodeError::Malformed => f.write_str("malformed envelope"),
            DecodeError::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            DecodeError::TooManyRecords => f.write_str("too many records"),
            DecodeError::InvalidRecord(e) => write!(f, "invalid record: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl Envelope {
    pub fn new(device: DeviceId, records: Vec<Record>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            device,
            records,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        // 構造体のシリアライズは失敗しない (キーは文字列のみ)。
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// 検証つきデコード。版を**先に**見る: 未来の版はスキーマが違いうるので、
    /// 全体のパースに失敗して「壊れた」と誤診する前に `UnsupportedVersion`
    /// を返す。
    pub fn decode(bytes: &[u8], accept: VersionRange) -> Result<Self, DecodeError> {
        if bytes.len() > MAX_ENVELOPE_BYTES {
            return Err(DecodeError::TooLarge);
        }
        #[derive(Deserialize)]
        struct Probe {
            v: u32,
        }
        let probe: Probe = serde_json::from_slice(bytes).map_err(|_| DecodeError::Malformed)?;
        if !accept.contains(probe.v) {
            return Err(DecodeError::UnsupportedVersion(probe.v));
        }
        let env: Envelope = serde_json::from_slice(bytes).map_err(|_| DecodeError::Malformed)?;
        if env.records.len() > MAX_RECORDS_PER_ENVELOPE {
            return Err(DecodeError::TooManyRecords);
        }
        for r in &env.records {
            r.validate().map_err(DecodeError::InvalidRecord)?;
        }
        Ok(env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::sync::record::tests::{hlc, key};
    use serde_json::json;

    fn r(min: u32, max: u32) -> VersionRange {
        VersionRange { min, max }
    }

    #[test]
    fn negotiate_picks_highest_common() {
        assert_eq!(negotiate(r(1, 3), r(2, 5)), Ok(3));
        assert_eq!(negotiate(r(1, 1), r(1, 1)), Ok(1));
        assert_eq!(negotiate(r(1, 5), r(1, 2)), Ok(2));
    }

    #[test]
    fn negotiate_reports_which_side_is_old() {
        assert_eq!(
            negotiate(r(1, 2), r(3, 4)),
            Err(NegotiationError::LocalTooOld)
        );
        assert_eq!(
            negotiate(r(3, 4), r(1, 2)),
            Err(NegotiationError::RemoteTooOld)
        );
        assert_eq!(
            negotiate(r(3, 1), r(1, 2)),
            Err(NegotiationError::Malformed)
        );
    }

    fn sample() -> Envelope {
        let rec = Record::put(
            key(),
            &hlc(1, 0, "a"),
            [("url".to_string(), json!("https://x/"))],
        );
        Envelope::new(DeviceId::new("a").unwrap(), vec![rec])
    }

    #[test]
    fn roundtrip() {
        let e = sample();
        assert_eq!(Envelope::decode(&e.encode(), VersionRange::CURRENT), Ok(e));
    }

    #[test]
    fn future_version_is_reported_before_shape_is_checked() {
        let bytes = br#"{"v":99,"device":"a","records":"not-even-a-list"}"#;
        assert_eq!(
            Envelope::decode(bytes, VersionRange::CURRENT),
            Err(DecodeError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn unknown_fields_are_ignored_within_a_version() {
        let mut v: serde_json::Value = serde_json::from_slice(&sample().encode()).unwrap();
        v["added_later"] = json!({"x": 1});
        let bytes = serde_json::to_vec(&v).unwrap();
        assert_eq!(
            Envelope::decode(&bytes, VersionRange::CURRENT),
            Ok(sample())
        );
    }

    #[test]
    fn rejects_garbage_oversize_and_invalid_records() {
        assert_eq!(
            Envelope::decode(b"nope", VersionRange::CURRENT),
            Err(DecodeError::Malformed)
        );
        let big = vec![b' '; MAX_ENVELOPE_BYTES + 1];
        assert_eq!(
            Envelope::decode(&big, VersionRange::CURRENT),
            Err(DecodeError::TooLarge)
        );
        let mut e = sample();
        e.records[0].key.id = String::new();
        assert_eq!(
            Envelope::decode(&e.encode(), VersionRange::CURRENT),
            Err(DecodeError::InvalidRecord(RecordError::EmptyId))
        );
        let mut many = sample();
        many.records = vec![many.records[0].clone(); MAX_RECORDS_PER_ENVELOPE + 1];
        assert_eq!(
            Envelope::decode(&many.encode(), VersionRange::CURRENT),
            Err(DecodeError::TooManyRecords)
        );
    }

    #[test]
    fn invalid_device_id_on_the_wire_is_malformed() {
        let bytes = br#"{"v":1,"device":"a/b","records":[]}"#;
        assert_eq!(
            Envelope::decode(bytes, VersionRange::CURRENT),
            Err(DecodeError::Malformed)
        );
    }
}
