//! 同期レコード (状態ベース CRDT のデルタ)。docs/sync.md §5。
//!
//! 1 レコード = 1 論理オブジェクト (ブックマーク 1 件、設定 1 キー、履歴 1
//! 訪問)。中身は**フィールド単位の LWW レジスタ** + **墓石 (tombstone)**。
//! 同じ型が (a) 端末内の状態、(b) 送信キューの要素、(c) 線上のペイロードの
//! すべてを兼ねる。[`Record::merge`] が結合則・交換則・冪等則を満たすので、
//! 「配送順序・重複・欠落 (後で再送)」に対して最終状態が一致する。

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::clock::Hlc;

pub const MAX_ID_LEN: usize = 256;
pub const MAX_FIELDS: usize = 32;
pub const MAX_FIELD_NAME_LEN: usize = 64;
/// 1 フィールドの値のシリアライズ後サイズ上限 (バイト)。
pub const MAX_VALUE_BYTES: usize = 16 * 1024;

/// 同期対象データの種別。ここに無いもの (セッション中のタブ、サイト権限、
/// 入力履歴、ダウンロード、フィルタ等) は**同期しない** (docs/sync.md §2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bookmark,
    BookmarkFolder,
    Setting,
    History,
}

/// レコードの同一性 (種別 + 作成端末が採番した不透明 ID)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordKey {
    pub kind: Kind,
    pub id: String,
}

impl RecordKey {
    pub fn new(kind: Kind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }
}

/// 値 + それを書いた時点の HLC。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub hlc: Hlc,
    pub value: Value,
}

impl Field {
    /// LWW: HLC が大きい方が勝つ。HLC が同一で値が違う (= 不正な入力) 場合
    /// も、値の JSON 表現の辞書順で決めて収束を保つ。
    fn supersedes(&self, other: &Field) -> bool {
        match self.hlc.cmp(&other.hlc) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => {
                let (mine, theirs) = (self.value.to_string(), other.value.to_string());
                mine > theirs
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub key: RecordKey,
    #[serde(default)]
    pub fields: BTreeMap<String, Field>,
    /// 削除時刻。これ以前に書かれたフィールドは無効 (削除後に書かれた
    /// フィールドだけが生き残る = 「後の編集は先の削除に勝つ」)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<Hlc>,
}

/// [`Record::validate`] の失敗。リモート入力は必ず通す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    IdTooLong,
    EmptyId,
    TooManyFields,
    BadFieldName,
    ValueTooLarge,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecordError::IdTooLong => "record id too long",
            RecordError::EmptyId => "record id empty",
            RecordError::TooManyFields => "too many fields",
            RecordError::BadFieldName => "bad field name",
            RecordError::ValueTooLarge => "field value too large",
        })
    }
}

impl std::error::Error for RecordError {}

impl Record {
    /// 指定フィールドを同じ HLC で書く。
    pub fn put(
        key: RecordKey,
        hlc: &Hlc,
        fields: impl IntoIterator<Item = (String, Value)>,
    ) -> Self {
        Self {
            key,
            fields: fields
                .into_iter()
                .map(|(k, value)| {
                    (
                        k,
                        Field {
                            hlc: hlc.clone(),
                            value,
                        },
                    )
                })
                .collect(),
            tombstone: None,
        }
    }

    pub fn delete(key: RecordKey, hlc: &Hlc) -> Self {
        Self {
            key,
            fields: BTreeMap::new(),
            tombstone: Some(hlc.clone()),
        }
    }

    /// 生きている (墓石より新しいフィールドが 1 つ以上ある) か。
    pub fn is_live(&self) -> bool {
        !self.fields.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.get(name).map(|f| &f.value)
    }

    /// このレコードに含まれる最大の HLC (時計の観測用)。
    pub fn max_hlc(&self) -> Option<&Hlc> {
        self.fields
            .values()
            .map(|f| &f.hlc)
            .chain(self.tombstone.as_ref())
            .max()
    }

    /// `other` を結合する。変化があれば `true`。
    ///
    /// 結合 = フィールドごとの LWW + 墓石の max + 墓石以前のフィールドの
    /// 刈り取り。刈り取りは墓石が残る限り冪等なので結合則・交換則を壊さない。
    pub fn merge(&mut self, other: &Record) -> bool {
        debug_assert_eq!(self.key, other.key);
        let before = self.clone();
        for (name, theirs) in &other.fields {
            match self.fields.get(name) {
                Some(mine) if !theirs.supersedes(mine) => {}
                _ => {
                    self.fields.insert(name.clone(), theirs.clone());
                }
            }
        }
        if other.tombstone > self.tombstone {
            self.tombstone = other.tombstone.clone();
        }
        if let Some(t) = &self.tombstone {
            self.fields.retain(|_, f| f.hlc > *t);
        }
        *self != before
    }

    /// リモート入力の形の検査 (サイズ・個数の上限)。
    pub fn validate(&self) -> Result<(), RecordError> {
        if self.key.id.is_empty() {
            return Err(RecordError::EmptyId);
        }
        if self.key.id.len() > MAX_ID_LEN {
            return Err(RecordError::IdTooLong);
        }
        if self.fields.len() > MAX_FIELDS {
            return Err(RecordError::TooManyFields);
        }
        for (name, f) in &self.fields {
            if name.is_empty() || name.len() > MAX_FIELD_NAME_LEN {
                return Err(RecordError::BadFieldName);
            }
            if f.value.to_string().len() > MAX_VALUE_BYTES {
                return Err(RecordError::ValueTooLarge);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::browser::sync::clock::DeviceId;
    use serde_json::json;

    pub(crate) fn hlc(wall: u64, counter: u32, dev: &str) -> Hlc {
        Hlc {
            wall,
            counter,
            device: DeviceId::new(dev).unwrap(),
        }
    }

    pub(crate) fn key() -> RecordKey {
        RecordKey::new(Kind::Bookmark, "b1")
    }

    fn put(h: Hlc, fields: &[(&str, Value)]) -> Record {
        Record::put(
            key(),
            &h,
            fields.iter().map(|(k, v)| (k.to_string(), v.clone())),
        )
    }

    fn joined(a: &Record, b: &Record) -> Record {
        let mut r = a.clone();
        r.merge(b);
        r
    }

    #[test]
    fn per_field_lww_keeps_both_sides_edits() {
        let a = put(hlc(10, 0, "a"), &[("title", json!("A"))]);
        let b = put(hlc(11, 0, "b"), &[("folder", json!("f1"))]);
        let m = joined(&a, &b);
        assert_eq!(m.get("title"), Some(&json!("A")));
        assert_eq!(m.get("folder"), Some(&json!("f1")));
    }

    #[test]
    fn same_field_later_hlc_wins_either_order() {
        let a = put(hlc(10, 0, "a"), &[("title", json!("old"))]);
        let b = put(hlc(10, 0, "b"), &[("title", json!("new"))]);
        assert_eq!(joined(&a, &b), joined(&b, &a));
        assert_eq!(joined(&a, &b).get("title"), Some(&json!("new")));
    }

    #[test]
    fn identical_hlc_different_value_still_converges() {
        let a = put(hlc(10, 0, "a"), &[("t", json!("x"))]);
        let b = put(hlc(10, 0, "a"), &[("t", json!("y"))]);
        assert_eq!(joined(&a, &b), joined(&b, &a));
    }

    #[test]
    fn delete_beats_earlier_edit_but_later_edit_resurrects() {
        let base = put(
            hlc(10, 0, "a"),
            &[("url", json!("u")), ("title", json!("t"))],
        );
        let del = Record::delete(key(), &hlc(20, 0, "b"));
        let dead = joined(&base, &del);
        assert!(!dead.is_live());
        assert!(dead.fields.is_empty());

        // 削除より後の編集は復活させる。ただし古いフィールドは戻らない。
        let edit = put(hlc(30, 0, "a"), &[("title", json!("t2"))]);
        let back = joined(&dead, &edit);
        assert!(back.is_live());
        assert_eq!(back.get("title"), Some(&json!("t2")));
        assert_eq!(back.get("url"), None);
        assert_eq!(back, joined(&edit, &dead));
    }

    #[test]
    fn merge_reports_change_and_is_idempotent() {
        let a = put(hlc(10, 0, "a"), &[("t", json!(1))]);
        let mut s = a.clone();
        assert!(!s.merge(&a));
        assert!(s.merge(&put(hlc(11, 0, "a"), &[("t", json!(2))])));
        assert!(!s.merge(&a));
    }

    #[test]
    fn merge_is_commutative_and_associative_bruteforce() {
        let recs = vec![
            put(hlc(1, 0, "a"), &[("x", json!(1)), ("y", json!(1))]),
            put(hlc(2, 0, "b"), &[("x", json!(2))]),
            Record::delete(key(), &hlc(3, 0, "c")),
            put(hlc(4, 0, "a"), &[("y", json!(4))]),
            Record::delete(key(), &hlc(2, 5, "b")),
        ];
        for a in &recs {
            for b in &recs {
                assert_eq!(joined(a, b), joined(b, a));
                for c in &recs {
                    assert_eq!(joined(&joined(a, b), c), joined(a, &joined(b, c)));
                }
            }
        }
    }

    #[test]
    fn validate_limits() {
        let mut r = put(hlc(1, 0, "a"), &[("x", json!(1))]);
        assert!(r.validate().is_ok());
        r.key.id = String::new();
        assert_eq!(r.validate(), Err(RecordError::EmptyId));
        r.key.id = "x".repeat(MAX_ID_LEN + 1);
        assert_eq!(r.validate(), Err(RecordError::IdTooLong));
        let big = put(
            hlc(1, 0, "a"),
            &[("x", json!("y".repeat(MAX_VALUE_BYTES + 1)))],
        );
        assert_eq!(big.validate(), Err(RecordError::ValueTooLarge));
        let many = Record::put(
            key(),
            &hlc(1, 0, "a"),
            (0..=MAX_FIELDS).map(|i| (format!("f{i}"), json!(i))),
        );
        assert_eq!(many.validate(), Err(RecordError::TooManyFields));
        let bad = put(hlc(1, 0, "a"), &[("", json!(1))]);
        assert_eq!(bad.validate(), Err(RecordError::BadFieldName));
    }

    #[test]
    fn wire_roundtrip_and_unknown_fields_ignored() {
        let r = put(hlc(1, 2, "a"), &[("x", json!({"k": [1, 2]}))]);
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Record>(&s).unwrap(), r);
        let mut v: Value = serde_json::from_str(&s).unwrap();
        v["future_field"] = json!(true);
        assert_eq!(serde_json::from_value::<Record>(v).unwrap(), r);
    }
}
