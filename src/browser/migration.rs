//! 永続データの版管理・段階的移行・バックアップ・退避 (Issue #92 /
//! docs/decisions.md D164、手順は docs/migration.md)。
//!
//! `persistence` が扱う JSON ファイル (設定・履歴・ブックマーク・入力履歴・
//! サイト権限・セッション) はすべてここを通して読み書きする。約束は 4 つ:
//!
//! 1. **版を持つ。** 保存時にトップレベルへ `"schema_version": N` を必ず書く。
//!    この欄が無いファイル (Issue #92 以前の VeloX が書いたもの) は
//!    [`Schema::legacy_version`] の版として読む。既存ファイルとの後方互換は
//!    ここで保つ。
//! 2. **移行する前に元を残す。** 古い版のファイルは、移行を試みる前に元の
//!    バイト列をそのまま `<name>.v<旧版>.bak` へ書き出す。移行が失敗しても
//!    旧データはそこに残る。
//! 3. **知らない新しい版を黙って捨てない。** 自分より新しい版のファイル
//!    (新版からダウングレードした場合) は `<name>.v<その版>.bak` へ写してから
//!    読めるだけ読む。読めなければ、自分の版のバックアップ (更新前に取られた
//!    もの) があればそれを使う。新版に戻したときに復元できる。
//! 4. **壊れたデータで起動不能にしない。** 解釈できないファイルは
//!    `<name>.corrupt-<unixミリ秒>` へ退避し (直近 [`MAX_CORRUPT_KEPT`] 件)、
//!    呼び出し側には「無い」と答える。呼び出し側は既定値で起動を続ける。
//!
//! 書き込みは [`fsutil::atomic_write`] (一時ファイル + `rename`) で、途中で
//! 落ちても半端な JSON が残らない。
//!
//! このモジュールはファイルの形 (`serde_json::Value`) だけを扱い、各ストアの
//! 型は知らない。移行処理 ([`Migration`]) は `Value → Value` の純粋関数で、
//! ストアごとの [`Schema`] に並べて登録する。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::fsutil;

/// 版を書くトップレベルのキー。設定 (`Settings::schema_version`) が以前から
/// 使っている名前に揃えた。
pub const SCHEMA_VERSION_KEY: &str = "schema_version";

/// 退避した壊れたファイル (`<name>.corrupt-*`) をファイルごとに何件残すか。
/// 壊れ続ける環境でディスクを食い潰さないための上限。
pub const MAX_CORRUPT_KEPT: usize = 3;

/// 1 版ぶんの移行。`from` 版の JSON を受け取り `from + 1` 版の JSON を返す。
/// 失敗は `Err(理由)` で返す (パニックしない)。
#[derive(Clone, Copy)]
pub struct Migration {
    pub from: u32,
    pub run: fn(Value) -> Result<Value, String>,
}

impl fmt::Debug for Migration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Migration(v{} -> v{})", self.from, self.from + 1)
    }
}

/// 1 種類の永続ファイルの版情報。
#[derive(Debug, Clone, Copy)]
pub struct Schema {
    /// この VeloX が読み書きする版。
    pub current: u32,
    /// `schema_version` 欄が無いファイルをどの版とみなすか。
    pub legacy_version: u32,
    /// 段階的な移行の列。`current` 未満の各版 `v` について `from == v` の
    /// 要素が 1 つずつあれば、どの古い版からでも `current` まで辿れる。
    pub migrations: &'static [Migration],
}

impl Schema {
    /// 移行が 1 つも無い、版 `current` のスキーマ。欄の無い旧ファイルも
    /// `current` として読む (今ある全ストアがこれ — 形が変わったことはまだ無い)。
    pub const fn initial(current: u32) -> Schema {
        Schema {
            current,
            legacy_version: current,
            migrations: &[],
        }
    }

    /// JSON のトップレベルから版を読む。オブジェクトでない、または欄が無い
    /// ときは [`Schema::legacy_version`]。欄が `u32` の整数でなければ `Err`
    /// (壊れたファイルとして扱う)。
    pub fn detect_version(&self, value: &Value) -> Result<u32, String> {
        let Some(raw) = value.as_object().and_then(|o| o.get(SCHEMA_VERSION_KEY)) else {
            return Ok(self.legacy_version);
        };
        raw.as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| format!("{SCHEMA_VERSION_KEY} が 32bit 符号なし整数ではない: {raw}"))
    }

    /// `from` 版の `value` を 1 版ずつ `current` まで移行する。途中の版の
    /// 移行が登録されていない・移行が失敗した場合は `Err`。`from >= current`
    /// ならそのまま返す。
    pub fn migrate(&self, mut value: Value, from: u32) -> Result<Value, String> {
        let mut version = from;
        while version < self.current {
            let step = self
                .migrations
                .iter()
                .find(|m| m.from == version)
                .ok_or_else(|| format!("v{version} から先の移行が登録されていない"))?;
            value =
                (step.run)(value).map_err(|e| format!("v{version} -> v{}: {e}", version + 1))?;
            version += 1;
            if let Some(obj) = value.as_object_mut() {
                obj.insert(SCHEMA_VERSION_KEY.to_owned(), Value::from(version));
            }
        }
        Ok(value)
    }
}

/// 自分より新しい版のファイルをどう読んだか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewerSource {
    /// 新しい版のファイルをそのまま読めた (追加フィールドだけの変更)。
    BestEffort,
    /// 読めなかったので、自分の版のバックアップ (`.v<current>.bak`) を使った。
    OwnVersionBackup,
    /// どちらも使えなかった。呼び出し側は既定値で続行する。
    Unusable,
}

/// [`load`] の結果の内訳。呼び出し側がログを出すためのもので、分岐は
/// [`Loaded::value`] だけ見れば足りる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadStatus {
    /// ファイルが無い (初回起動など)。
    Missing,
    /// 現在の版のファイルをそのまま読めた。
    Current,
    /// 古い版から移行した。`backup` は移行前の元データの保存先。
    Migrated { from: u32, backup: Option<PathBuf> },
    /// 移行に失敗した。元データは `backup` (取れなかったときは退避先) に残る。
    MigrationFailed {
        from: u32,
        reason: String,
        backup: Option<PathBuf>,
    },
    /// 自分より新しい版のファイル。`backup` はその写しの保存先。
    NewerVersion {
        found: u32,
        backup: Option<PathBuf>,
        source: NewerSource,
    },
    /// 解釈できないファイルを退避した (`moved_to` が `None` なら退避にも
    /// 失敗し、削除した)。
    Quarantined {
        reason: String,
        moved_to: Option<PathBuf>,
    },
    /// 読み出し自体に失敗した (権限・ロックなど)。一時的な可能性があるので
    /// 退避はしない。
    Unreadable { reason: String },
}

impl LoadStatus {
    /// 利用者に知らせる価値のある出来事か (欠落・正常読み込み以外)。
    pub fn is_notable(&self) -> bool {
        !matches!(self, LoadStatus::Missing | LoadStatus::Current)
    }
}

fn show(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map_or_else(|| "(なし)".to_owned(), |p| p.display().to_string())
}

impl fmt::Display for LoadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadStatus::Missing => write!(f, "ファイルなし"),
            LoadStatus::Current => write!(f, "読み込み成功"),
            LoadStatus::Migrated { from, backup } => {
                write!(f, "v{from} から移行 (移行前の控え: {})", show(backup))
            }
            LoadStatus::MigrationFailed {
                from,
                reason,
                backup,
            } => write!(
                f,
                "v{from} からの移行に失敗 ({reason})。既定値で続行 (元データ: {})",
                show(backup)
            ),
            LoadStatus::NewerVersion {
                found,
                backup,
                source,
            } => write!(
                f,
                "この VeloX より新しい v{found} のデータ (写し: {}、読み方: {source:?})",
                show(backup)
            ),
            LoadStatus::Quarantined { reason, moved_to } => write!(
                f,
                "壊れたファイルを退避 ({reason})。既定値で続行 (退避先: {})",
                show(moved_to)
            ),
            LoadStatus::Unreadable { reason } => {
                write!(f, "読み出せない ({reason})。既定値で続行")
            }
        }
    }
}

/// [`load`] の結果。
#[derive(Debug)]
pub struct Loaded<T> {
    /// 使える値。`None` なら呼び出し側は既定値で続行する。
    pub value: Option<T>,
    pub status: LoadStatus,
}

/// `path` の版 `version` のバックアップのパス (`<name>.v<version>.bak`)。
pub fn backup_path(path: &Path, version: u32) -> PathBuf {
    fsutil::sibling_with_suffix(path, &format!(".v{version}.bak"))
}

/// `path` を読み、必要なら移行して `T` にする。この関数は失敗しない
/// (結果の内訳は [`Loaded::status`])。モジュール先頭の約束 1〜4 を参照。
pub fn load<T: DeserializeOwned>(path: &Path, schema: &Schema) -> Loaded<T> {
    // 書き込み途中で落ちた一時ファイルは読まない (次の保存で作り直される)。
    let _ = fs::remove_file(fsutil::tmp_path(path));

    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                value: None,
                status: LoadStatus::Missing,
            }
        }
        Err(e) => {
            return Loaded {
                value: None,
                status: LoadStatus::Unreadable {
                    reason: e.to_string(),
                },
            }
        }
    };

    let json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return quarantined(path, format!("JSON として解釈できない: {e}")),
    };
    let version = match schema.detect_version(&json) {
        Ok(v) => v,
        Err(reason) => return quarantined(path, reason),
    };

    if version == schema.current {
        return match serde_json::from_value(json) {
            Ok(value) => Loaded {
                value: Some(value),
                status: LoadStatus::Current,
            },
            Err(e) => quarantined(path, format!("想定した形ではない: {e}")),
        };
    }

    if version > schema.current {
        return load_newer(path, schema, &bytes, json, version);
    }

    // 古い版: 何かする前に元のバイト列を控える。
    let backup = write_backup(path, version, &bytes);
    let migrated = schema
        .migrate(json, version)
        .and_then(|v| serde_json::from_value(v).map_err(|e| format!("移行後の形が不正: {e}")));
    match migrated {
        Ok(value) => Loaded {
            value: Some(value),
            status: LoadStatus::Migrated {
                from: version,
                backup,
            },
        },
        Err(reason) => {
            // 控えが取れなかったときは、次の保存で上書きされないよう元を退避する。
            let backup = match backup {
                Some(b) => Some(b),
                None => quarantine(path),
            };
            Loaded {
                value: None,
                status: LoadStatus::MigrationFailed {
                    from: version,
                    reason,
                    backup,
                },
            }
        }
    }
}

fn load_newer<T: DeserializeOwned>(
    path: &Path,
    schema: &Schema,
    bytes: &[u8],
    json: Value,
    found: u32,
) -> Loaded<T> {
    let backup = write_backup(path, found, bytes);
    let (value, source) = if let Ok(v) = serde_json::from_value(json) {
        (Some(v), NewerSource::BestEffort)
    } else if let Some(v) = read_own_backup(path, schema) {
        (Some(v), NewerSource::OwnVersionBackup)
    } else {
        (None, NewerSource::Unusable)
    };
    Loaded {
        value,
        status: LoadStatus::NewerVersion {
            found,
            backup,
            source,
        },
    }
}

/// 自分の版のバックアップ (新版へ更新したときに取られたもの) を読む。
fn read_own_backup<T: DeserializeOwned>(path: &Path, schema: &Schema) -> Option<T> {
    let bytes = fs::read(backup_path(path, schema.current)).ok()?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    if schema.detect_version(&json).ok()? != schema.current {
        return None;
    }
    serde_json::from_value(json).ok()
}

fn write_backup(path: &Path, version: u32, bytes: &[u8]) -> Option<PathBuf> {
    let dest = backup_path(path, version);
    fsutil::atomic_write(&dest, bytes).ok().map(|()| dest)
}

fn quarantined<T>(path: &Path, reason: String) -> Loaded<T> {
    Loaded {
        value: None,
        status: LoadStatus::Quarantined {
            reason,
            moved_to: quarantine(path),
        },
    }
}

/// `path` を `<name>.corrupt-<unixミリ秒>` へ移し、古い退避を
/// [`MAX_CORRUPT_KEPT`] 件まで間引く。移せなければ削除して `None`
/// (壊れたファイルが次回起動を妨げ続けないように)。
pub fn quarantine(path: &Path) -> Option<PathBuf> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    // 既存の退避より必ず後ろに並ぶ名前にする (同じミリ秒・時計の逆行でも
    // 新しいものが間引かれないように)。
    let (millis, seq) = match corrupt_entries(path).last() {
        Some(&((m, s), _)) if m >= millis => (m, s + 1),
        _ => (millis, 0),
    };
    let suffix = if seq == 0 {
        format!(".corrupt-{millis}")
    } else {
        format!(".corrupt-{millis}-{seq}")
    };
    let dest = fsutil::sibling_with_suffix(path, &suffix);
    let moved = if fs::rename(path, &dest).is_ok() {
        Some(dest)
    } else {
        let _ = fs::remove_file(path);
        None
    };
    prune_corrupt(path);
    moved
}

/// `path` の退避ファイル一覧 (古い順)。
pub fn corrupt_files(path: &Path) -> Vec<PathBuf> {
    corrupt_entries(path).into_iter().map(|(_, p)| p).collect()
}

/// 退避ファイルを `(ミリ秒, 連番)` の昇順で。
fn corrupt_entries(path: &Path) -> Vec<((u128, u32), PathBuf)> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.corrupt-", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<((u128, u32), PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().into_owned();
            let rest = file_name.strip_prefix(&prefix)?;
            let mut parts = rest.splitn(2, '-');
            let millis = parts.next()?.parse::<u128>().ok()?;
            let seq = match parts.next() {
                Some(s) => s.parse::<u32>().ok()?,
                None => 0,
            };
            Some(((millis, seq), e.path()))
        })
        .collect();
    found.sort();
    found
}

fn prune_corrupt(path: &Path) {
    let files = corrupt_files(path);
    let excess = files.len().saturating_sub(MAX_CORRUPT_KEPT);
    for old in &files[..excess] {
        let _ = fs::remove_file(old);
    }
}

/// `value` を `schema.current` 版として `path` へ原子的に書く。トップレベルが
/// オブジェクトなら `schema_version` を付ける (既にあれば上書き)。
pub fn save<T: Serialize>(path: &Path, schema: &Schema, value: &T) -> std::io::Result<()> {
    let mut json = serde_json::to_value(value).map_err(std::io::Error::other)?;
    if let Some(obj) = json.as_object_mut() {
        obj.insert(SCHEMA_VERSION_KEY.to_owned(), Value::from(schema.current));
    }
    let data = serde_json::to_vec_pretty(&json).map_err(std::io::Error::other)?;
    fsutil::atomic_write(path, &data)
}

/// `path` について存在するバックアップの版 (昇順)。
pub fn list_backups(path: &Path) -> Vec<u32> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.v", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut versions: Vec<u32> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().into_owned();
            file_name
                .strip_prefix(&prefix)?
                .strip_suffix(".bak")?
                .parse()
                .ok()
        })
        .collect();
    versions.sort_unstable();
    versions
}

/// 版 `version` のバックアップを `path` へ書き戻す (ロールバック)。現在の
/// `path` は上書きする前に、その版のバックアップとして控える (同じ版・版が
/// 読めないときは `.corrupt-*` へ退避する) ので、書き戻しもやり直せる。
/// バックアップ自体は残す。
pub fn restore_backup(path: &Path, schema: &Schema, version: u32) -> std::io::Result<()> {
    let restored = fs::read(backup_path(path, version))?;
    if let Ok(current) = fs::read(path) {
        let current_version = serde_json::from_slice::<Value>(&current)
            .ok()
            .and_then(|v| schema.detect_version(&v).ok());
        match current_version {
            Some(v) if v != version => {
                fsutil::atomic_write(&backup_path(path, v), &current)?;
            }
            // 同じ版 (控えると書き戻す元を潰す) や版が読めないものは退避する。
            _ => {
                quarantine(path);
            }
        }
    }
    fsutil::atomic_write(path, &restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::util::unique_temp_path;
    use serde_json::json;

    #[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
    struct V3 {
        name: String,
        count: u64,
        #[serde(default)]
        tags: Vec<String>,
    }

    /// v1: `{"title": ..}` → v2: `title` を `name` に改名 → v3: `count` を追加。
    fn v1_to_v2(mut v: Value) -> Result<Value, String> {
        let obj = v.as_object_mut().ok_or("オブジェクトではない")?;
        let title = obj.remove("title").ok_or("title が無い")?;
        obj.insert("name".to_owned(), title);
        Ok(v)
    }

    fn v2_to_v3(mut v: Value) -> Result<Value, String> {
        let obj = v.as_object_mut().ok_or("オブジェクトではない")?;
        obj.entry("count").or_insert(json!(0));
        Ok(v)
    }

    const MIGRATIONS: &[Migration] = &[
        Migration {
            from: 1,
            run: v1_to_v2,
        },
        Migration {
            from: 2,
            run: v2_to_v3,
        },
    ];

    const SCHEMA: Schema = Schema {
        current: 3,
        legacy_version: 1,
        migrations: MIGRATIONS,
    };

    fn setup(label: &str, contents: &str) -> (PathBuf, PathBuf) {
        let dir = unique_temp_path(label);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn missing_file_is_reported_as_missing() {
        let dir = unique_temp_path("velox-migr-missing");
        let loaded: Loaded<V3> = load(&dir.join("data.json"), &SCHEMA);
        assert_eq!(loaded.value, None);
        assert_eq!(loaded.status, LoadStatus::Missing);
    }

    #[test]
    fn current_version_loads_without_backup() {
        let (dir, path) = setup(
            "velox-migr-current",
            r#"{"schema_version":3,"name":"a","count":2}"#,
        );
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.status, LoadStatus::Current);
        assert_eq!(loaded.value.unwrap().count, 2);
        assert!(list_backups(&path).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_file_without_version_migrates_step_by_step_and_keeps_a_backup() {
        let original = r#"{"title":"old"}"#;
        let (dir, path) = setup("velox-migr-legacy", original);
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(
            loaded.value,
            Some(V3 {
                name: "old".to_owned(),
                count: 0,
                tags: vec![]
            })
        );
        assert_eq!(
            loaded.status,
            LoadStatus::Migrated {
                from: 1,
                backup: Some(backup_path(&path, 1))
            }
        );
        // 控えは元のバイト列そのもの、元ファイルは (次の保存まで) 触らない。
        assert_eq!(fs::read_to_string(backup_path(&path, 1)).unwrap(), original);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migration_starts_from_the_recorded_version() {
        let (dir, path) = setup(
            "velox-migr-v2",
            r#"{"schema_version":2,"name":"n","count":7}"#,
        );
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.value.unwrap().count, 7);
        assert!(matches!(
            loaded.status,
            LoadStatus::Migrated { from: 2, .. }
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_migration_protects_the_old_data() {
        // v1 だが `title` が無いので v1 -> v2 が失敗する。
        let original = r#"{"schema_version":1,"unexpected":true}"#;
        let (dir, path) = setup("velox-migr-fail", original);
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.value, None);
        match loaded.status {
            LoadStatus::MigrationFailed { from, backup, .. } => {
                assert_eq!(from, 1);
                assert_eq!(fs::read_to_string(backup.unwrap()).unwrap(), original);
            }
            other => panic!("unexpected status {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_gap_in_the_migration_chain_is_a_failure_not_a_panic() {
        const GAPPY: Schema = Schema {
            current: 3,
            legacy_version: 1,
            migrations: &[Migration {
                from: 2,
                run: v2_to_v3,
            }],
        };
        let (dir, path) = setup("velox-migr-gap", r#"{"title":"x"}"#);
        let loaded: Loaded<V3> = load(&path, &GAPPY);
        assert_eq!(loaded.value, None);
        assert!(matches!(
            loaded.status,
            LoadStatus::MigrationFailed { from: 1, .. }
        ));
        assert!(backup_path(&path, 1).exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newer_version_is_copied_aside_and_read_best_effort() {
        let original = r#"{"schema_version":9,"name":"future","count":1,"extra":{}}"#;
        let (dir, path) = setup("velox-migr-newer", original);
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.value.unwrap().name, "future");
        assert_eq!(
            loaded.status,
            LoadStatus::NewerVersion {
                found: 9,
                backup: Some(backup_path(&path, 9)),
                source: NewerSource::BestEffort,
            }
        );
        assert_eq!(fs::read_to_string(backup_path(&path, 9)).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unreadable_newer_version_falls_back_to_own_version_backup() {
        let (dir, path) = setup("velox-migr-newer-own", r#"{"schema_version":4,"n":1}"#);
        fs::write(
            backup_path(&path, 3),
            r#"{"schema_version":3,"name":"before-upgrade","count":5}"#,
        )
        .unwrap();
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.value.unwrap().name, "before-upgrade");
        assert!(matches!(
            loaded.status,
            LoadStatus::NewerVersion {
                found: 4,
                source: NewerSource::OwnVersionBackup,
                ..
            }
        ));
        assert!(backup_path(&path, 4).exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unusable_newer_version_without_backup_yields_none() {
        let (dir, path) = setup("velox-migr-newer-none", r#"{"schema_version":4}"#);
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.value, None);
        assert!(matches!(
            loaded.status,
            LoadStatus::NewerVersion {
                source: NewerSource::Unusable,
                ..
            }
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_json_is_quarantined_and_not_left_in_place() {
        let (dir, path) = setup("velox-migr-corrupt", "{not json");
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.value, None);
        let LoadStatus::Quarantined { moved_to, .. } = loaded.status else {
            panic!("expected quarantine");
        };
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(moved_to.unwrap()).unwrap(), "{not json");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_non_integer_schema_version_is_treated_as_corrupt() {
        for bad in [
            r#"{"schema_version":"1"}"#,
            r#"{"schema_version":-1}"#,
            r#"{"schema_version":1.5}"#,
            r#"{"schema_version":99999999999}"#,
        ] {
            let (dir, path) = setup("velox-migr-badver", bad);
            let loaded: Loaded<V3> = load(&path, &SCHEMA);
            assert!(
                matches!(loaded.status, LoadStatus::Quarantined { .. }),
                "input {bad}: {:?}",
                loaded.status
            );
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn only_the_most_recent_corrupt_files_are_kept() {
        let dir = unique_temp_path("velox-migr-rotate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");
        for i in 0..(MAX_CORRUPT_KEPT + 2) {
            fs::write(&path, format!("broken {i}")).unwrap();
            let _: Loaded<V3> = load(&path, &SCHEMA);
        }
        let kept = corrupt_files(&path);
        assert_eq!(kept.len(), MAX_CORRUPT_KEPT);
        // 最新のものが残っている。
        let newest = fs::read_to_string(kept.last().unwrap()).unwrap();
        assert_eq!(newest, format!("broken {}", MAX_CORRUPT_KEPT + 1));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_leftover_temp_file_is_removed_and_ignored() {
        let (dir, path) = setup(
            "velox-migr-tmp",
            r#"{"schema_version":3,"name":"ok","count":1}"#,
        );
        fs::write(fsutil::tmp_path(&path), "half-writ").unwrap();
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.status, LoadStatus::Current);
        assert!(!fsutil::tmp_path(&path).exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_stamps_the_current_version_and_round_trips() {
        let dir = unique_temp_path("velox-migr-save");
        let path = dir.join("nested").join("data.json");
        let value = V3 {
            name: "n".to_owned(),
            count: 3,
            tags: vec!["t".to_owned()],
        };
        save(&path, &SCHEMA, &value).unwrap();
        let raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw[SCHEMA_VERSION_KEY], json!(3));
        assert!(!fsutil::tmp_path(&path).exists());
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        assert_eq!(loaded.status, LoadStatus::Current);
        assert_eq!(loaded.value, Some(value));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_backup_rolls_back_and_keeps_the_replaced_data() {
        let (dir, path) = setup("velox-migr-restore", r#"{"title":"old"}"#);
        // 移行して保存 (= 更新後の VeloX が一度保存した状態)。
        let loaded: Loaded<V3> = load(&path, &SCHEMA);
        save(&path, &SCHEMA, &loaded.value.unwrap()).unwrap();
        assert_eq!(list_backups(&path), vec![1]);

        restore_backup(&path, &SCHEMA, 1).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"title":"old"}"#);
        // 置き換えた v3 のデータも控えられている。
        assert_eq!(list_backups(&path), vec![1, 3]);
        let v3: Value = serde_json::from_slice(&fs::read(backup_path(&path, 3)).unwrap()).unwrap();
        assert_eq!(v3["name"], json!("old"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_of_a_missing_backup_is_an_error_and_changes_nothing() {
        let (dir, path) = setup(
            "velox-migr-restore-missing",
            r#"{"schema_version":3,"name":"n","count":1}"#,
        );
        assert!(restore_backup(&path, &SCHEMA, 1).is_err());
        assert!(fs::read_to_string(&path).unwrap().contains("\"n\""));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_version_of_non_objects_is_the_legacy_version() {
        assert_eq!(SCHEMA.detect_version(&json!([1, 2])), Ok(1));
        assert_eq!(SCHEMA.detect_version(&json!({})), Ok(1));
        assert_eq!(SCHEMA.detect_version(&json!({"schema_version": 2})), Ok(2));
    }
}
