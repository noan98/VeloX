//! 拡張機能ごとに分離されたキーバリューストレージ (Issue #84、docs/extensions.md §7)。
//!
//! - **分離**: 1 拡張機能 = 1 ディレクトリ (`storage/<id>/storage.json`)。他の
//!   拡張機能・ページ・VeloX 本体の永続ファイルとは混ざらない。
//! - **クォータ**: キー長・値サイズ・件数・総バイト数に上限を持つ。超過は書き込み
//!   失敗として返し、状態は変えない。
//! - **原子的な書き込み**: 一時ファイル → rename。書き込みに失敗したらメモリ上の
//!   状態も元に戻す (ディスクとメモリがずれない)。
//! - **破損からの復旧**: 壊れた/上限超過のファイルは `storage.json.corrupt` へ
//!   退避し、その拡張機能だけ空のストレージで続行する。
//! - 平文で保存する。秘密情報を置く場所ではない (§7-6)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::fsutil;

pub const STORAGE_FILE: &str = "storage.json";
const STORAGE_SCHEMA_VERSION: u32 = 1;

/// ストレージの上限。既定値は本番用。テストでは小さい値を渡せる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageLimits {
    /// キーの最大バイト数。
    pub max_key_bytes: usize,
    /// 1 つの値 (JSON シリアライズ後) の最大バイト数。
    pub max_value_bytes: usize,
    /// 最大件数。
    pub max_items: usize,
    /// キー + 値の総バイト数の上限。
    pub max_total_bytes: usize,
}

impl Default for StorageLimits {
    fn default() -> Self {
        StorageLimits {
            max_key_bytes: 128,
            max_value_bytes: 64 * 1024,
            max_items: 512,
            max_total_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    InvalidKey(String),
    ValueTooLarge {
        key: String,
        bytes: usize,
    },
    TooManyItems {
        limit: usize,
    },
    QuotaExceeded {
        limit: usize,
    },
    /// ディスクへ書けなかった (状態は変更されていない)。
    Io(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::InvalidKey(k) => write!(f, "キー `{k}` が不正 (空・長すぎ・制御文字)"),
            StorageError::ValueTooLarge { key, bytes } => {
                write!(f, "`{key}` の値が大きすぎる ({bytes} バイト)")
            }
            StorageError::TooManyItems { limit } => write!(f, "件数の上限 {limit} を超える"),
            StorageError::QuotaExceeded { limit } => {
                write!(f, "総容量の上限 {limit} バイトを超える")
            }
            // パス等の内部情報は拡張機能へ返さない。
            StorageError::Io(_) => write!(f, "ストレージへ書き込めなかった"),
        }
    }
}

impl std::error::Error for StorageError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageFile {
    schema_version: u32,
    items: BTreeMap<String, Value>,
}

#[derive(Debug)]
pub struct ExtensionStorage {
    dir: PathBuf,
    data: BTreeMap<String, Value>,
    limits: StorageLimits,
    recovered: bool,
}

fn value_bytes(value: &Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

fn item_bytes(key: &str, value: &Value) -> usize {
    key.len() + value_bytes(value)
}

impl ExtensionStorage {
    /// `dir` のストレージを開く。無ければ空。壊れていれば退避して空で続行する
    /// (この関数は失敗しない)。
    pub fn open(dir: &Path, limits: StorageLimits) -> ExtensionStorage {
        let file = dir.join(STORAGE_FILE);
        // 書き込み途中で落ちた一時ファイルは読まない。
        let _ = std::fs::remove_file(fsutil::tmp_path(&file));
        let mut recovered = false;
        let data = match std::fs::read(&file) {
            Ok(bytes) => match Self::decode(&bytes, &limits) {
                Some(d) => d,
                None => {
                    fsutil::quarantine_file(&file);
                    recovered = true;
                    BTreeMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(_) => {
                fsutil::quarantine_file(&file);
                recovered = true;
                BTreeMap::new()
            }
        };
        ExtensionStorage {
            dir: dir.to_path_buf(),
            data,
            limits,
            recovered,
        }
    }

    fn decode(bytes: &[u8], limits: &StorageLimits) -> Option<BTreeMap<String, Value>> {
        // 解析する前にも上限を掛ける (巨大ファイルの解析コストを払わない)。
        if bytes.len() > limits.max_total_bytes.saturating_mul(2) + 1024 {
            return None;
        }
        let file: StorageFile = serde_json::from_slice(bytes).ok()?;
        if file.schema_version != STORAGE_SCHEMA_VERSION {
            return None;
        }
        Self::check_all(&file.items, limits).ok()?;
        Some(file.items)
    }

    fn check_key(key: &str, limits: &StorageLimits) -> Result<(), StorageError> {
        if key.is_empty() || key.len() > limits.max_key_bytes || key.chars().any(char::is_control) {
            return Err(StorageError::InvalidKey(key.chars().take(32).collect()));
        }
        Ok(())
    }

    /// 全体が上限内か検査する。
    fn check_all(
        data: &BTreeMap<String, Value>,
        limits: &StorageLimits,
    ) -> Result<(), StorageError> {
        if data.len() > limits.max_items {
            return Err(StorageError::TooManyItems {
                limit: limits.max_items,
            });
        }
        let mut total = 0usize;
        for (k, v) in data {
            Self::check_key(k, limits)?;
            let bytes = value_bytes(v);
            if bytes > limits.max_value_bytes {
                return Err(StorageError::ValueTooLarge {
                    key: k.chars().take(32).collect(),
                    bytes,
                });
            }
            total += k.len() + bytes;
        }
        if total > limits.max_total_bytes {
            return Err(StorageError::QuotaExceeded {
                limit: limits.max_total_bytes,
            });
        }
        Ok(())
    }

    /// 開いたときに破損を検出して復旧したか。
    pub fn was_recovered(&self) -> bool {
        self.recovered
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// `keys` が `None` なら全件、`Some` ならそのキーのうち存在するものだけ。
    pub fn get(&self, keys: Option<&[String]>) -> serde_json::Map<String, Value> {
        match keys {
            None => self
                .data
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            Some(keys) => keys
                .iter()
                .filter_map(|k| self.data.get(k).map(|v| (k.clone(), v.clone())))
                .collect(),
        }
    }

    /// 使用中のバイト数 (キー + 値)。`keys` が `None` なら全件。
    pub fn bytes_in_use(&self, keys: Option<&[String]>) -> usize {
        match keys {
            None => self.data.iter().map(|(k, v)| item_bytes(k, v)).sum(),
            Some(keys) => keys
                .iter()
                .filter_map(|k| self.data.get(k).map(|v| item_bytes(k, v)))
                .sum(),
        }
    }

    /// 複数キーをまとめて書く。全部成功するか、何も変わらないかのどちらか。
    pub fn set(&mut self, items: serde_json::Map<String, Value>) -> Result<(), StorageError> {
        let mut next = self.data.clone();
        for (k, v) in items {
            next.insert(k, v);
        }
        Self::check_all(&next, &self.limits)?;
        self.commit(next)
    }

    pub fn remove(&mut self, keys: &[String]) -> Result<(), StorageError> {
        let mut next = self.data.clone();
        for k in keys {
            next.remove(k);
        }
        self.commit(next)
    }

    pub fn clear(&mut self) -> Result<(), StorageError> {
        self.commit(BTreeMap::new())
    }

    fn commit(&mut self, next: BTreeMap<String, Value>) -> Result<(), StorageError> {
        let file = StorageFile {
            schema_version: STORAGE_SCHEMA_VERSION,
            items: next,
        };
        let bytes = serde_json::to_vec(&file).map_err(|e| StorageError::Io(e.to_string()))?;
        fsutil::atomic_write(&self.dir.join(STORAGE_FILE), &bytes)
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.data = file.items;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::util::unique_temp_path;
    use serde_json::json;

    fn map(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    fn small() -> StorageLimits {
        StorageLimits {
            max_key_bytes: 8,
            max_value_bytes: 32,
            max_items: 3,
            max_total_bytes: 64,
        }
    }

    #[test]
    fn set_get_remove_clear_round_trip_through_disk() {
        let dir = unique_temp_path("velox-ext-storage-rt");
        let mut s = ExtensionStorage::open(&dir, StorageLimits::default());
        s.set(map(json!({"a": 1, "b": {"x": [1, 2]}}))).unwrap();
        assert_eq!(s.get(Some(&["a".to_owned()])), map(json!({"a": 1})));

        let reopened = ExtensionStorage::open(&dir, StorageLimits::default());
        assert_eq!(reopened.get(None), map(json!({"a": 1, "b": {"x": [1, 2]}})));
        assert!(!reopened.was_recovered());

        s.remove(&["a".to_owned()]).unwrap();
        assert_eq!(s.len(), 1);
        s.clear().unwrap();
        assert!(ExtensionStorage::open(&dir, StorageLimits::default()).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quota_and_key_limits_reject_without_changing_state() {
        let dir = unique_temp_path("velox-ext-storage-quota");
        let mut s = ExtensionStorage::open(&dir, small());
        s.set(map(json!({"a": 1}))).unwrap();

        assert!(matches!(
            s.set(map(json!({"": 1}))),
            Err(StorageError::InvalidKey(_))
        ));
        assert!(matches!(
            s.set(map(json!({"waytoolongkey": 1}))),
            Err(StorageError::InvalidKey(_))
        ));
        assert!(matches!(
            s.set(map(json!({"k": "x".repeat(40)}))),
            Err(StorageError::ValueTooLarge { .. })
        ));
        assert!(matches!(
            s.set(map(json!({"b": 1, "c": 2, "d": 3}))),
            Err(StorageError::TooManyItems { limit: 3 })
        ));
        // 総容量: 1 件ずつは収まるが合計が 64 バイトを超える。
        s.set(map(json!({"b": "y".repeat(30)}))).unwrap();
        assert!(matches!(
            s.set(map(json!({"c": "z".repeat(30)}))),
            Err(StorageError::QuotaExceeded { limit: 64 })
        ));
        // 失敗した書き込みは何も変えていない。
        assert_eq!(s.len(), 2);
        assert_eq!(ExtensionStorage::open(&dir, small()).len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_is_quarantined_and_storage_starts_empty() {
        let dir = unique_temp_path("velox-ext-storage-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STORAGE_FILE), "{not json").unwrap();
        let mut s = ExtensionStorage::open(&dir, StorageLimits::default());
        assert!(s.was_recovered());
        assert!(s.is_empty());
        assert!(dir.join("storage.json.corrupt").exists());
        // 復旧後は通常どおり書ける。
        s.set(map(json!({"a": 1}))).unwrap();
        assert!(!ExtensionStorage::open(&dir, StorageLimits::default()).was_recovered());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_or_unknown_schema_files_count_as_corrupt() {
        let dir = unique_temp_path("velox-ext-storage-tamper");
        std::fs::create_dir_all(&dir).unwrap();
        let too_many = json!({"schema_version":1,"items":{"a":1,"b":2,"c":3,"d":4}});
        std::fs::write(dir.join(STORAGE_FILE), too_many.to_string()).unwrap();
        assert!(ExtensionStorage::open(&dir, small()).was_recovered());

        let future = json!({"schema_version":99,"items":{}});
        std::fs::write(dir.join(STORAGE_FILE), future.to_string()).unwrap();
        assert!(ExtensionStorage::open(&dir, small()).was_recovered());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leftover_tmp_file_from_a_crash_is_ignored() {
        let dir = unique_temp_path("velox-ext-storage-tmp");
        let mut s = ExtensionStorage::open(&dir, StorageLimits::default());
        s.set(map(json!({"a": 1}))).unwrap();
        std::fs::write(dir.join("storage.json.tmp"), "half writ").unwrap();
        let s2 = ExtensionStorage::open(&dir, StorageLimits::default());
        assert!(!s2.was_recovered());
        assert_eq!(s2.get(None), map(json!({"a": 1})));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_write_leaves_memory_state_unchanged() {
        let dir = unique_temp_path("velox-ext-storage-iofail");
        let mut s = ExtensionStorage::open(&dir, StorageLimits::default());
        s.set(map(json!({"a": 1}))).unwrap();
        // ストレージのディレクトリをファイルで塞いで書き込みを失敗させる。
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, "in the way").unwrap();
        assert!(matches!(
            s.set(map(json!({"b": 2}))),
            Err(StorageError::Io(_))
        ));
        assert_eq!(s.get(None), map(json!({"a": 1})));
        std::fs::remove_file(&dir).ok();
    }
}
