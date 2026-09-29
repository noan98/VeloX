//! インストール済み拡張機能のレジストリ (Issue #84、docs/extensions.md §5)。
//!
//! ディスク上の配置 (`root` は呼び出し側が決める。例: `<データディレクトリ>/extensions`):
//!
//! ```text
//! root/
//!   index.json          レジストリ索引 (状態・承認)。原子的に書く
//!   index.json.corrupt  壊れた索引の退避先
//!   packages/<id>/      展開済みパッケージ (manifest.json を含む)
//!   storage/<id>/       拡張機能ごとのストレージ (storage.rs)
//!   staging/            インストール/更新の作業領域 (起動時に必ず片付ける)
//! ```
//!
//! ## 安全性の要点
//!
//! - **インストールは「一時領域に書く → 検証 → rename」**。承認 (`consent`) が無い、
//!   検証に失敗した、索引を書けなかった、のいずれでも何も残さない。
//! - **索引は状態の記録に過ぎず、権限の根拠はマニフェスト + 承認記録**。起動時に
//!   ディスク上のマニフェストを必ず再検証し、承認を超える宣言 (改ざん・部分的な
//!   更新の残骸) は `NeedsReconsent` で無効化、読めなければ `Corrupt` で隔離する。
//!   **壊れた拡張機能が起動を止めることはない。**
//! - 索引が壊れていたら退避して、`packages/` から**承認なし (要再承認)** で再構築する
//!   (承認を推測で復元しない)。
//! - アンインストールは索引から外して**トゥームストーン**を書いてから実ファイルを
//!   消す。消せなかった分は次回起動で再試行し、その間 ID の再利用は拒否する。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::fsutil;
use super::lifecycle::{
    self, DisabledReason, ExtensionState, LifecycleError, LifecycleEvent, LifecycleRecord,
};
use super::package::{check_declared_resources, ExtensionPackage, PackageError, MANIFEST_FILE};
use super::permissions::Approval;
use crate::browser::extension_manifest::{
    parse_manifest, ExtensionVersion, HostPattern, Manifest, Permission,
};

const INDEX_FILE: &str = "index.json";
const INDEX_SCHEMA_VERSION: u32 = 1;

/// 拡張機能 ID として安全か (ディレクトリ名に使うため、索引のような信頼できない
/// 入力に対しても必ず検査する)。`extension_manifest` の `id` 規則と同じ。
pub fn is_valid_extension_id(id: &str) -> bool {
    let b = id.as_bytes();
    (3..=64).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
        && !id.contains("..")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    Package(PackageError),
    /// ユーザの承認が無い (インストール/更新の承認ダイアログを通っていない)。
    ConsentRequired,
    AlreadyInstalled(String),
    NotInstalled(String),
    /// 更新は単調増加のみ (ロールバック攻撃対策)。
    NotNewer {
        current: String,
        offered: String,
    },
    VeloxTooOld {
        required: String,
    },
    /// 前回のアンインストールの削除が終わっておらず、ID を再利用できない。
    RemovalPending(String),
    Lifecycle(LifecycleError),
    /// マニフェストを持たない (隔離中の) 拡張機能には行えない操作。
    Unavailable(String),
    /// 宣言していない任意権限の付与要求など。
    NotDeclared,
    Io(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Package(e) => write!(f, "{e}"),
            RegistryError::ConsentRequired => write!(f, "ユーザの承認が必要"),
            RegistryError::AlreadyInstalled(id) => write!(f, "`{id}` はインストール済み"),
            RegistryError::NotInstalled(id) => write!(f, "`{id}` はインストールされていない"),
            RegistryError::NotNewer { current, offered } => {
                write!(f, "更新は新しい版のみ (現在 {current}、提示 {offered})")
            }
            RegistryError::VeloxTooOld { required } => {
                write!(f, "VeloX {required} 以上が必要")
            }
            RegistryError::RemovalPending(id) => {
                write!(f, "`{id}` の前回の削除が完了していない")
            }
            RegistryError::Lifecycle(e) => write!(f, "{e}"),
            RegistryError::Unavailable(id) => write!(f, "`{id}` は隔離中で操作できない"),
            RegistryError::NotDeclared => write!(f, "マニフェストが宣言していない権限"),
            RegistryError::Io(e) => write!(f, "ファイル操作に失敗: {e}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<PackageError> for RegistryError {
    fn from(e: PackageError) -> Self {
        RegistryError::Package(e)
    }
}

impl From<LifecycleError> for RegistryError {
    fn from(e: LifecycleError) -> Self {
        RegistryError::Lifecycle(e)
    }
}

/// 起動時の復旧で何をしたかの記録。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// 索引が壊れていて退避した (`index.json.corrupt`)。
    pub index_corrupt: bool,
    /// 索引の中の解釈できなかった/不正なエントリの数。
    pub dropped_entries: usize,
    /// 索引が無い/壊れているため `packages/` から再構築した拡張機能 (要再承認)。
    pub rebuilt: Vec<String>,
    /// マニフェスト再検証に失敗して隔離した拡張機能。
    pub quarantined: Vec<String>,
    /// 索引に無いクラッシュの残骸として削除したパッケージ/ストレージ。
    pub removed_orphans: Vec<String>,
    /// 索引が無く、読めもしなかったので**手を付けず**放置したディレクトリ。
    pub unreadable_packages: Vec<String>,
    /// 片付けた作業領域のエントリ数。
    pub staging_cleaned: usize,
    /// 更新の途中で落ちていて、旧版へ戻したもの。
    pub restored_previous: Vec<String>,
    /// 再試行してアンインストールを完了したもの。
    pub completed_removals: Vec<String>,
}

/// インストール済み拡張機能 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledExtension {
    pub id: String,
    pub version: String,
    pub state: ExtensionState,
    pub approval: Approval,
    /// ディスクから再検証したマニフェスト。隔離中で読めない場合は `None`。
    pub manifest: Option<Manifest>,
}

/// インストール前に承認ダイアログへ渡す情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPreview {
    pub id: String,
    pub name: String,
    pub version: String,
    /// 必須の権限 (承認される全部)。
    pub permissions: Vec<Permission>,
    /// 必須のホストアクセス (`content_scripts.matches` を含む)。
    pub hosts: Vec<String>,
    /// 警告を強調すべき権限か、全ホスト権限を含む。
    pub sensitive: bool,
    pub all_hosts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub id: String,
    pub from: String,
    pub to: String,
    /// 承認を超える権限が要求され、無効化して再承認待ちになった。
    pub needs_reconsent: bool,
    pub added_permissions: Vec<String>,
    pub added_hosts: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    id: String,
    version: String,
    state: ExtensionState,
    approval: Approval,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexFile {
    schema_version: u32,
    /// エントリごとに個別に解釈する (1 件の破損で全件を失わない)。
    #[serde(default)]
    extensions: Vec<Value>,
    #[serde(default)]
    pending_removals: Vec<String>,
}

struct LoadedIndex {
    present: bool,
    corrupt: bool,
    dropped: usize,
    entries: Vec<IndexEntry>,
    pending: Vec<String>,
}

pub struct ExtensionRegistry {
    root: PathBuf,
    entries: BTreeMap<String, InstalledExtension>,
    pending_removals: BTreeSet<String>,
    events: Vec<LifecycleRecord>,
    report: RecoveryReport,
    velox_version: ExtensionVersion,
    /// 作業領域の名前を決定的に一意にするための連番。
    seq: u64,
}

fn load_installed_manifest(pkg_dir: &Path, id: &str) -> Result<Manifest, String> {
    let bytes = std::fs::read(pkg_dir.join(MANIFEST_FILE))
        .map_err(|e| format!("manifest.json を読めない ({:?})", e.kind()))?;
    let manifest = parse_manifest(&bytes).map_err(|e| e.to_string())?;
    if manifest.id != id {
        return Err("manifest の id が索引と一致しない".to_owned());
    }
    check_declared_resources(&manifest, |p| pkg_dir.join(p).is_file())
        .map_err(|e| e.to_string())?;
    Ok(manifest)
}

fn list_dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

impl ExtensionRegistry {
    /// `root` のレジストリを開く。**失敗しない**: 破損は復旧して続行し、何をしたかは
    /// [`recovery_report`](Self::recovery_report) に残る。
    pub fn open(root: &Path, velox_version: ExtensionVersion) -> ExtensionRegistry {
        let mut reg = ExtensionRegistry {
            root: root.to_path_buf(),
            entries: BTreeMap::new(),
            pending_removals: BTreeSet::new(),
            events: Vec::new(),
            report: RecoveryReport::default(),
            velox_version,
            seq: 0,
        };
        let _ = std::fs::create_dir_all(&reg.root);
        reg.recover_staging();
        let mut changed = reg.load();
        changed |= reg.retry_pending_removals();
        changed |= reg.reconcile_directories();
        if changed {
            let _ = reg.save_index();
        }
        reg
    }

    // --- 配置 ---

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }
    fn packages_dir(&self) -> PathBuf {
        self.root.join("packages")
    }
    fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }
    /// 展開済みパッケージのディレクトリ。
    pub fn package_dir(&self, id: &str) -> PathBuf {
        self.packages_dir().join(id)
    }
    /// 拡張機能のストレージのディレクトリ (`ExtensionStorage::open` へ渡す)。
    pub fn storage_dir(&self, id: &str) -> PathBuf {
        self.root.join("storage").join(id)
    }

    // --- 参照 ---

    pub fn get(&self, id: &str) -> Option<&InstalledExtension> {
        self.entries.get(id)
    }

    pub fn list(&self) -> impl Iterator<Item = &InstalledExtension> {
        self.entries.values()
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.state.is_enabled())
    }

    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.report
    }

    /// 溜まったライフサイクルイベントを取り出す (発生順)。
    pub fn drain_events(&mut self) -> Vec<LifecycleRecord> {
        std::mem::take(&mut self.events)
    }

    fn push_event(&mut self, id: &str, event: LifecycleEvent) {
        self.events.push(LifecycleRecord {
            extension_id: id.to_owned(),
            event,
        });
    }

    // --- 起動時の復旧 ---

    /// 作業領域を片付ける。更新の途中で落ちて `<id>.old` だけが残り本体が無い場合は
    /// 旧版へ戻す。
    fn recover_staging(&mut self) {
        let staging = self.staging_dir();
        for name in list_dir_names(&staging) {
            let path = staging.join(&name);
            if let Some(id) = name.strip_suffix(".old") {
                if is_valid_extension_id(id)
                    && !self.package_dir(id).exists()
                    && std::fs::create_dir_all(self.packages_dir()).is_ok()
                    && std::fs::rename(&path, self.package_dir(id)).is_ok()
                {
                    self.report.restored_previous.push(id.to_owned());
                    self.report.staging_cleaned += 1;
                    continue;
                }
            }
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            self.report.staging_cleaned += 1;
        }
    }

    fn read_index(&mut self) -> LoadedIndex {
        let path = self.index_path();
        let mut out = LoadedIndex {
            present: false,
            corrupt: false,
            dropped: 0,
            entries: Vec::new(),
            pending: Vec::new(),
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
            Err(_) => {
                out.present = true;
                out.corrupt = true;
                fsutil::quarantine_file(&path);
                return out;
            }
        };
        out.present = true;
        // 索引に妥当な上限を掛ける (拡張機能は最大でも数百件)。
        let file: Option<IndexFile> = if bytes.len() > 4 * 1024 * 1024 {
            None
        } else {
            serde_json::from_slice(&bytes)
                .ok()
                .filter(|f: &IndexFile| f.schema_version == INDEX_SCHEMA_VERSION)
        };
        let Some(file) = file else {
            out.corrupt = true;
            fsutil::quarantine_file(&path);
            return out;
        };
        for v in file.extensions {
            match serde_json::from_value::<IndexEntry>(v) {
                Ok(e) if is_valid_extension_id(&e.id) => out.entries.push(e),
                _ => out.dropped += 1,
            }
        }
        for id in file.pending_removals {
            if is_valid_extension_id(&id) {
                out.pending.push(id);
            } else {
                out.dropped += 1;
            }
        }
        out
    }

    /// 索引を読み、各拡張機能のマニフェストを再検証する。何か直したら `true`。
    fn load(&mut self) -> bool {
        let idx = self.read_index();
        let mut changed = idx.corrupt || idx.dropped > 0;
        self.report.index_corrupt = idx.corrupt;
        self.report.dropped_entries = idx.dropped;
        self.pending_removals = idx.pending.into_iter().collect();
        let lossless = idx.present && !idx.corrupt && idx.dropped == 0;

        for ie in idx.entries {
            if self.entries.contains_key(&ie.id) {
                self.report.dropped_entries += 1;
                changed = true;
                continue;
            }
            let before = (ie.state, ie.approval.clone(), ie.version.clone());
            let mut entry = InstalledExtension {
                id: ie.id,
                version: ie.version,
                state: ie.state,
                approval: ie.approval,
                manifest: None,
            };
            match load_installed_manifest(&self.package_dir(&entry.id), &entry.id) {
                Ok(m) => {
                    entry.approval.sanitize(&m);
                    entry.version = m.version.to_string();
                    let (p, h) = entry.approval.missing_for(&m);
                    if (!p.is_empty() || !h.is_empty())
                        && entry.state != ExtensionState::Disabled(DisabledReason::Corrupt)
                    {
                        // ディスク上の宣言が承認を超えている (改ざん/更新の残骸)。
                        entry.state = lifecycle::after_update(entry.state, true);
                        self.push_event(
                            &entry.id.clone(),
                            LifecycleEvent::ConsentRequired {
                                added_permissions: p,
                                added_hosts: h,
                            },
                        );
                    }
                    entry.manifest = Some(m);
                }
                Err(detail) => {
                    if entry.state != ExtensionState::Disabled(DisabledReason::Corrupt) {
                        self.push_event(&entry.id.clone(), LifecycleEvent::Quarantined { detail });
                    }
                    entry.state = ExtensionState::Disabled(DisabledReason::Corrupt);
                    self.report.quarantined.push(entry.id.clone());
                }
            }
            if before != (entry.state, entry.approval.clone(), entry.version.clone()) {
                changed = true;
            }
            self.entries.insert(entry.id.clone(), entry);
        }

        // 索引が無い/壊れている場合: packages/ から承認なしで再構築する。
        if !lossless {
            for name in list_dir_names(&self.packages_dir()) {
                if self.entries.contains_key(&name) || self.pending_removals.contains(&name) {
                    continue;
                }
                if !is_valid_extension_id(&name) {
                    self.report.unreadable_packages.push(name);
                    continue;
                }
                match load_installed_manifest(&self.package_dir(&name), &name) {
                    Ok(m) => {
                        let approval = Approval::default();
                        let (p, h) = approval.missing_for(&m);
                        self.push_event(
                            &name,
                            LifecycleEvent::ConsentRequired {
                                added_permissions: p,
                                added_hosts: h,
                            },
                        );
                        self.entries.insert(
                            name.clone(),
                            InstalledExtension {
                                id: name.clone(),
                                version: m.version.to_string(),
                                state: ExtensionState::Disabled(DisabledReason::NeedsReconsent),
                                approval,
                                manifest: Some(m),
                            },
                        );
                        self.report.rebuilt.push(name);
                        changed = true;
                    }
                    Err(_) => self.report.unreadable_packages.push(name),
                }
            }
        }
        changed
    }

    fn retry_pending_removals(&mut self) -> bool {
        let mut changed = false;
        for id in self.pending_removals.clone() {
            if self.entries.contains_key(&id) {
                // 索引が「存在する」と言うなら削除対象ではない。
                self.pending_removals.remove(&id);
                changed = true;
            } else if self.remove_files(&id).is_ok() {
                self.pending_removals.remove(&id);
                self.report.completed_removals.push(id);
                changed = true;
            }
        }
        changed
    }

    /// 索引が無傷なら、索引にないパッケージ/ストレージはインストール/アンインストール
    /// 途中のクラッシュの残骸なので消す。
    fn reconcile_directories(&mut self) -> bool {
        if self.report.index_corrupt
            || self.report.dropped_entries > 0
            || !self.index_path().exists()
        {
            return false;
        }
        let mut changed = false;
        for (dir, is_pkg) in [
            (self.packages_dir(), true),
            (self.root.join("storage"), false),
        ] {
            for name in list_dir_names(&dir) {
                if self.entries.contains_key(&name) || self.pending_removals.contains(&name) {
                    continue;
                }
                if std::fs::remove_dir_all(dir.join(&name)).is_ok() {
                    if is_pkg {
                        self.report.removed_orphans.push(name);
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    // --- 索引の永続化 ---

    fn save_index(&self) -> Result<(), RegistryError> {
        let extensions = self
            .entries
            .values()
            .map(|e| {
                serde_json::to_value(IndexEntry {
                    id: e.id.clone(),
                    version: e.version.clone(),
                    state: e.state,
                    approval: e.approval.clone(),
                })
                .map_err(|e| RegistryError::Io(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let file = IndexFile {
            schema_version: INDEX_SCHEMA_VERSION,
            extensions,
            pending_removals: self.pending_removals.iter().cloned().collect(),
        };
        let bytes =
            serde_json::to_vec_pretty(&file).map_err(|e| RegistryError::Io(e.to_string()))?;
        fsutil::atomic_write(&self.index_path(), &bytes)
            .map_err(|e| RegistryError::Io(e.to_string()))
    }

    /// エントリを差し替えて索引を書く。書けなければメモリ上も元に戻す。
    fn commit(&mut self, entry: InstalledExtension) -> Result<(), RegistryError> {
        let id = entry.id.clone();
        let old = self.entries.insert(id.clone(), entry);
        if let Err(e) = self.save_index() {
            match old {
                Some(o) => self.entries.insert(id, o),
                None => self.entries.remove(&id),
            };
            return Err(e);
        }
        Ok(())
    }

    fn remove_files(&self, id: &str) -> std::io::Result<()> {
        let a = fsutil::remove_dir_if_exists(&self.package_dir(id));
        let b = fsutil::remove_dir_if_exists(&self.storage_dir(id));
        a.and(b)
    }

    fn next_staging(&mut self, id: &str) -> PathBuf {
        self.seq += 1;
        self.staging_dir().join(format!("{id}.{}", self.seq))
    }

    fn check_velox_version(&self, m: &Manifest) -> Result<(), RegistryError> {
        match &m.min_velox_version {
            Some(min) if *min > self.velox_version => Err(RegistryError::VeloxTooOld {
                required: min.to_string(),
            }),
            _ => Ok(()),
        }
    }

    /// パッケージを検証して承認ダイアログ用の情報を返す (ディスクには何もしない)。
    pub fn preview(&self, pkg: &ExtensionPackage) -> Result<InstallPreview, RegistryError> {
        let m = pkg.validate()?;
        self.check_velox_version(&m)?;
        let hosts: Vec<String> = m
            .required_host_patterns()
            .iter()
            .map(|p| p.to_string())
            .collect();
        Ok(InstallPreview {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.to_string(),
            sensitive: m.permissions.iter().any(|p| p.is_sensitive())
                || m.allow_all_urls
                || m.required_host_patterns().iter().any(|p| p.is_all_hosts()),
            all_hosts: m.required_host_patterns().iter().any(|p| p.is_all_hosts()),
            permissions: m.permissions.clone(),
            hosts,
        })
    }

    // --- インストール ---

    /// 新規インストール。`consent` は承認ダイアログでユーザが必須の権限・ホストの
    /// **全部**を承認したか (部分承認は無い)。成功すると有効な状態で登録される。
    /// 失敗時は何も残さない。
    pub fn install(
        &mut self,
        pkg: &ExtensionPackage,
        consent: bool,
    ) -> Result<String, RegistryError> {
        let manifest = pkg.validate()?;
        self.check_velox_version(&manifest)?;
        if !consent {
            return Err(RegistryError::ConsentRequired);
        }
        let id = manifest.id.clone();
        if self.entries.contains_key(&id) {
            return Err(RegistryError::AlreadyInstalled(id));
        }
        if self.pending_removals.contains(&id) {
            if self.remove_files(&id).is_err() {
                return Err(RegistryError::RemovalPending(id));
            }
            self.pending_removals.remove(&id);
        }

        let staging = self.next_staging(&id);
        let final_dir = self.package_dir(&id);
        let stage_result = (|| {
            fsutil::remove_dir_if_exists(&staging)?;
            std::fs::create_dir_all(self.staging_dir())?;
            pkg.write_to(&staging)?;
            // 登録されていない同名の残骸があっても、索引が権威。
            fsutil::remove_dir_if_exists(&final_dir)?;
            std::fs::create_dir_all(self.packages_dir())?;
            std::fs::rename(&staging, &final_dir)
        })();
        if let Err(e) = stage_result {
            let _ = fsutil::remove_dir_if_exists(&staging);
            return Err(RegistryError::Io(e.to_string()));
        }
        // 過去の同 ID の残骸ストレージを引き継がない。
        let _ = fsutil::remove_dir_if_exists(&self.storage_dir(&id));

        let entry = InstalledExtension {
            id: id.clone(),
            version: manifest.version.to_string(),
            state: ExtensionState::Enabled,
            approval: Approval::for_manifest(&manifest),
            manifest: Some(manifest),
        };
        if let Err(e) = self.commit(entry) {
            let _ = fsutil::remove_dir_if_exists(&final_dir);
            return Err(e);
        }
        self.push_event(&id, LifecycleEvent::Installed);
        Ok(id)
    }

    // --- 更新 ---

    /// 既存の拡張機能を新しい版へ更新する。ストレージは保つ。
    ///
    /// - 版は**厳密に増える**こと (隔離中の修復だけは同版を許す)。
    /// - 新マニフェストが承認済みを超える権限・ホストを要求したら、**更新は適用した
    ///   まま無効化**して再承認を待つ (`approve`)。減る分は黙って適用する。
    /// - 置き換えは原子的で、失敗したら旧版のまま。
    pub fn update(&mut self, pkg: &ExtensionPackage) -> Result<UpdateOutcome, RegistryError> {
        let manifest = pkg.validate()?;
        self.check_velox_version(&manifest)?;
        let id = manifest.id.clone();
        let prev = self
            .entries
            .get(&id)
            .cloned()
            .ok_or_else(|| RegistryError::NotInstalled(id.clone()))?;
        let current = ExtensionVersion::parse(&prev.version);
        let repairing = prev.state == ExtensionState::Disabled(DisabledReason::Corrupt);
        // 記録された版が読めない (索引の破損・改ざん) ときに単調増加の検査を
        // 飛ばすと、ダウングレードが素通りする。修復中 (破損で無効化された
        // もの) の置き換えだけは許し、それ以外は拒否する。
        let newer = match &current {
            Some(cur) => manifest.version > *cur || (repairing && manifest.version == *cur),
            None => repairing,
        };
        if !newer {
            return Err(RegistryError::NotNewer {
                current: prev.version.clone(),
                offered: manifest.version.to_string(),
            });
        }

        let mut approval = prev.approval.clone();
        approval.sanitize(&manifest);
        let (added_permissions, added_hosts) = approval.missing_for(&manifest);
        let exceeds = !added_permissions.is_empty() || !added_hosts.is_empty();
        let next_state = lifecycle::after_update(prev.state, exceeds);

        // 原子的な置き換え: staging に書く → 旧版を .old へ退避 → 新版を配置。
        let staging = self.next_staging(&id);
        let old_dir = self.staging_dir().join(format!("{id}.old"));
        let final_dir = self.package_dir(&id);
        let prepared = (|| {
            fsutil::remove_dir_if_exists(&staging)?;
            fsutil::remove_dir_if_exists(&old_dir)?;
            std::fs::create_dir_all(self.staging_dir())?;
            pkg.write_to(&staging)
        })();
        if let Err(e) = prepared {
            let _ = fsutil::remove_dir_if_exists(&staging);
            return Err(RegistryError::Io(e.to_string()));
        }
        let had_old = final_dir.exists();
        let swapped = (|| {
            if had_old {
                std::fs::rename(&final_dir, &old_dir)?;
            }
            std::fs::create_dir_all(self.packages_dir())?;
            std::fs::rename(&staging, &final_dir).inspect_err(|_| {
                if had_old {
                    let _ = std::fs::rename(&old_dir, &final_dir);
                }
            })
        })();
        if let Err(e) = swapped {
            let _ = fsutil::remove_dir_if_exists(&staging);
            return Err(RegistryError::Io(e.to_string()));
        }

        let new_entry = InstalledExtension {
            id: id.clone(),
            version: manifest.version.to_string(),
            state: next_state,
            approval,
            manifest: Some(manifest.clone()),
        };
        if let Err(e) = self.commit(new_entry) {
            // 索引を書けなかった: 旧版へ戻す。
            let _ = fsutil::remove_dir_if_exists(&final_dir);
            if had_old {
                let _ = std::fs::rename(&old_dir, &final_dir);
            }
            return Err(e);
        }
        let _ = fsutil::remove_dir_if_exists(&old_dir);

        self.push_event(
            &id,
            LifecycleEvent::Updated {
                from: prev.version.clone(),
                to: manifest.version.to_string(),
            },
        );
        if exceeds {
            self.push_event(
                &id,
                LifecycleEvent::ConsentRequired {
                    added_permissions: added_permissions.clone(),
                    added_hosts: added_hosts.clone(),
                },
            );
        }
        if let Some(ev) = lifecycle::state_change_event(prev.state, next_state) {
            self.push_event(&id, ev);
        }
        Ok(UpdateOutcome {
            id,
            from: prev.version,
            to: manifest.version.to_string(),
            needs_reconsent: exceeds,
            added_permissions,
            added_hosts,
        })
    }

    // --- 有効化・無効化・再承認 ---

    fn entry_clone(&self, id: &str) -> Result<InstalledExtension, RegistryError> {
        self.entries
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::NotInstalled(id.to_owned()))
    }

    fn set_state(&mut self, id: &str, next: ExtensionState) -> Result<(), RegistryError> {
        let mut entry = self.entry_clone(id)?;
        let prev = entry.state;
        if prev == next {
            return Ok(());
        }
        entry.state = next;
        self.commit(entry)?;
        if let Some(ev) = lifecycle::state_change_event(prev, next) {
            self.push_event(id, ev);
        }
        Ok(())
    }

    pub fn enable(&mut self, id: &str) -> Result<(), RegistryError> {
        let next = lifecycle::enable(self.entry_clone(id)?.state)?;
        self.set_state(id, next)
    }

    pub fn disable(&mut self, id: &str) -> Result<(), RegistryError> {
        let next = lifecycle::disable(self.entry_clone(id)?.state);
        self.set_state(id, next)
    }

    /// 再承認 (更新で増えた権限・ホストをユーザが承認した)。承認を新しいマニフェストの
    /// 必須宣言全部へ更新し、有効化する。`NeedsReconsent` でなければ何もしない。
    pub fn approve(&mut self, id: &str) -> Result<(), RegistryError> {
        let mut entry = self.entry_clone(id)?;
        let next = lifecycle::approve(entry.state)?;
        if entry.state != ExtensionState::Disabled(DisabledReason::NeedsReconsent) {
            return Ok(());
        }
        let manifest = entry
            .manifest
            .clone()
            .ok_or_else(|| RegistryError::Unavailable(id.to_owned()))?;
        let mut approval = Approval::for_manifest(&manifest);
        let carried = entry.approval.carry_over_optional(&manifest);
        approval.optional_permissions = carried.optional_permissions;
        approval.optional_hosts = carried.optional_hosts;
        let prev = entry.state;
        entry.approval = approval;
        entry.state = next;
        self.commit(entry)?;
        if let Some(ev) = lifecycle::state_change_event(prev, next) {
            self.push_event(id, ev);
        }
        Ok(())
    }

    // --- 任意権限の実行時付与 ---

    /// マニフェストが `optional_*` に宣言した権限・ホストを付与する。宣言外は
    /// `NotDeclared` で何も変更しない。
    pub fn grant_optional(
        &mut self,
        id: &str,
        permissions: &[Permission],
        hosts: &[HostPattern],
    ) -> Result<(), RegistryError> {
        let mut entry = self.entry_clone(id)?;
        let manifest = entry
            .manifest
            .clone()
            .ok_or_else(|| RegistryError::Unavailable(id.to_owned()))?;
        if !entry.approval.grant_optional(&manifest, permissions, hosts) {
            return Err(RegistryError::NotDeclared);
        }
        self.commit(entry)
    }

    /// 付与済みの任意権限・ホストを取り消す。取り消したものがあれば `true`。
    /// 取り消しは即時に効く (API 呼び出しごとに承認を見るため)。
    pub fn revoke_optional(
        &mut self,
        id: &str,
        permissions: &[Permission],
        hosts: &[HostPattern],
    ) -> Result<bool, RegistryError> {
        let mut entry = self.entry_clone(id)?;
        if !entry.approval.revoke_optional(permissions, hosts) {
            return Ok(false);
        }
        self.commit(entry)?;
        Ok(true)
    }

    // --- アンインストール ---

    /// パッケージ・ストレージ・承認記録をすべて削除する。ファイルを消せなかった場合は
    /// トゥームストーンを残し (`Ok(false)`)、次回起動で再試行する。
    pub fn uninstall(&mut self, id: &str) -> Result<bool, RegistryError> {
        let entry = self.entry_clone(id)?;
        self.entries.remove(id);
        self.pending_removals.insert(id.to_owned());
        if let Err(e) = self.save_index() {
            // まだ何も消していない。元に戻す。
            self.entries.insert(id.to_owned(), entry);
            self.pending_removals.remove(id);
            return Err(e);
        }
        self.push_event(id, LifecycleEvent::Uninstalled);
        if self.remove_files(id).is_ok() {
            self.pending_removals.remove(id);
            // 失敗しても、トゥームストーンが残るだけで次回起動で片付く。
            let _ = self.save_index();
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::util::unique_temp_path;

    fn pkg(version: &str, extra: &str) -> ExtensionPackage {
        let manifest = format!(
            r#"{{"manifest_version":1,"id":"com.example.reg","name":"Reg","version":"{version}"{extra}}}"#
        );
        ExtensionPackage::new()
            .with_file("manifest.json", manifest)
            .unwrap()
            .with_file("bg.js", format!("// {version}"))
            .unwrap()
    }

    fn pkg_bg(version: &str, extra: &str) -> ExtensionPackage {
        pkg(
            version,
            &format!(r#","background":{{"script":"bg.js"}}{extra}"#),
        )
    }

    fn open(root: &Path) -> ExtensionRegistry {
        ExtensionRegistry::open(root, ExtensionVersion::parse("1.0.0").unwrap())
    }

    #[test]
    fn valid_extension_id_rules() {
        assert!(is_valid_extension_id("com.example.a"));
        assert!(!is_valid_extension_id("../x"));
        assert!(!is_valid_extension_id("a/b"));
        assert!(!is_valid_extension_id("a..b"));
        assert!(!is_valid_extension_id("ab"));
        assert!(!is_valid_extension_id("Abc"));
    }

    #[test]
    fn install_requires_consent_and_leaves_nothing_on_failure() {
        let root = unique_temp_path("velox-ext-reg-consent");
        let mut reg = open(&root);
        assert_eq!(
            reg.install(&pkg_bg("1.0.0", ""), false),
            Err(RegistryError::ConsentRequired)
        );
        assert!(reg.get("com.example.reg").is_none());
        assert!(!reg.package_dir("com.example.reg").exists());
        // 宣言したスクリプトが無いパッケージは拒否。
        let broken = ExtensionPackage::new()
            .with_file(
                "manifest.json",
                r#"{"manifest_version":1,"id":"com.example.reg","name":"R","version":"1.0.0","background":{"script":"bg.js"}}"#,
            )
            .unwrap();
        assert!(matches!(
            reg.install(&broken, true),
            Err(RegistryError::Package(PackageError::MissingResource(_)))
        ));
        assert_eq!(list_dir_names(&root.join("staging")).len(), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_survives_restart_and_rejects_duplicates() {
        let root = unique_temp_path("velox-ext-reg-restart");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        assert_eq!(
            reg.install(&pkg_bg("1.0.0", ""), true),
            Err(RegistryError::AlreadyInstalled(id.clone()))
        );
        drop(reg);
        let reg = open(&root);
        assert!(reg.is_enabled(&id));
        assert_eq!(reg.get(&id).unwrap().version, "1.0.0");
        assert_eq!(reg.recovery_report(), &RecoveryReport::default());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn min_velox_version_is_enforced() {
        let root = unique_temp_path("velox-ext-reg-minver");
        let mut reg = open(&root);
        assert!(matches!(
            reg.install(&pkg_bg("1.0.0", r#","min_velox_version":"9.0.0""#), true),
            Err(RegistryError::VeloxTooOld { .. })
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn update_is_refused_when_the_recorded_version_is_unreadable() {
        let root = unique_temp_path("velox-ext-reg-badver");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("2.0.0", ""), true).unwrap();
        // 索引が壊れて (または改ざんされて) 版が読めなくなった状態。
        reg.entries.get_mut(&id).unwrap().version = "not-a-version".into();
        assert!(matches!(
            reg.update(&pkg_bg("0.0.1", "")),
            Err(RegistryError::NotNewer { .. })
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn update_rules_downgrade_and_reconsent() {
        let root = unique_temp_path("velox-ext-reg-update");
        let mut reg = open(&root);
        let id = reg
            .install(&pkg_bg("1.1.0", r#","permissions":["storage"]"#), true)
            .unwrap();
        // 同版・旧版は拒否。
        for v in ["1.1.0", "1.0.9"] {
            assert!(matches!(
                reg.update(&pkg_bg(v, r#","permissions":["storage"]"#)),
                Err(RegistryError::NotNewer { .. })
            ));
        }
        // 権限が減る更新は黙って適用。
        let out = reg.update(&pkg_bg("1.2.0", "")).unwrap();
        assert!(!out.needs_reconsent);
        assert!(reg.is_enabled(&id));
        assert!(!reg
            .get(&id)
            .unwrap()
            .approval
            .permissions
            .contains("storage"));
        // 権限が増える更新は適用して無効化。
        let out = reg
            .update(&pkg_bg("1.3.0", r#","permissions":["tabs"]"#))
            .unwrap();
        assert!(out.needs_reconsent);
        assert_eq!(out.added_permissions, vec!["tabs".to_owned()]);
        assert_eq!(
            reg.get(&id).unwrap().state,
            ExtensionState::Disabled(DisabledReason::NeedsReconsent)
        );
        assert_eq!(reg.get(&id).unwrap().version, "1.3.0");
        assert_eq!(
            reg.enable(&id),
            Err(RegistryError::Lifecycle(LifecycleError::ConsentRequired))
        );
        reg.approve(&id).unwrap();
        assert!(reg.is_enabled(&id));
        assert!(reg.get(&id).unwrap().approval.permissions.contains("tabs"));
        // 作業領域は空。
        assert!(list_dir_names(&root.join("staging")).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn uninstall_removes_everything_and_blocks_reuse_until_cleaned() {
        let root = unique_temp_path("velox-ext-reg-uninstall");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        std::fs::create_dir_all(reg.storage_dir(&id)).unwrap();
        std::fs::write(reg.storage_dir(&id).join("storage.json"), "{}").unwrap();
        assert_eq!(reg.uninstall(&id), Ok(true));
        assert!(!reg.package_dir(&id).exists());
        assert!(!reg.storage_dir(&id).exists());
        assert_eq!(
            reg.uninstall(&id),
            Err(RegistryError::NotInstalled(id.clone()))
        );
        // 再インストールは新規扱い (ストレージは空)。
        reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        assert!(!reg.storage_dir(&id).exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tombstone_completes_removal_on_next_open() {
        let root = unique_temp_path("velox-ext-reg-tomb");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        drop(reg);
        // 「索引から外した後、ファイルを消す前に落ちた」状態を作る。
        let index = serde_json::json!({
            "schema_version": 1, "extensions": [], "pending_removals": [id]
        });
        std::fs::write(root.join("index.json"), index.to_string()).unwrap();
        let reg = open(&root);
        assert!(reg.get(&id).is_none());
        assert!(!reg.package_dir(&id).exists());
        assert_eq!(reg.recovery_report().completed_removals, vec![id]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn corrupt_index_is_backed_up_and_rebuilt_as_needing_consent() {
        let root = unique_temp_path("velox-ext-reg-corrupt-index");
        let mut reg = open(&root);
        let id = reg
            .install(&pkg_bg("1.0.0", r#","permissions":["storage"]"#), true)
            .unwrap();
        drop(reg);
        std::fs::write(root.join("index.json"), "{{{ not json").unwrap();
        let mut reg = open(&root);
        assert!(reg.recovery_report().index_corrupt);
        assert!(root.join("index.json.corrupt").exists());
        assert_eq!(reg.recovery_report().rebuilt, vec![id.clone()]);
        // 承認を推測で復元しない: 無効で、承認は空。
        let e = reg.get(&id).unwrap();
        assert_eq!(
            e.state,
            ExtensionState::Disabled(DisabledReason::NeedsReconsent)
        );
        assert!(e.approval.permissions.is_empty());
        reg.approve(&id).unwrap();
        assert!(reg.is_enabled(&id));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn broken_entry_is_dropped_but_other_entries_survive() {
        let root = unique_temp_path("velox-ext-reg-partial");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        drop(reg);
        let mut index: Value =
            serde_json::from_slice(&std::fs::read(root.join("index.json")).unwrap()).unwrap();
        index["extensions"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": "../../etc", "version": "1.0.0"}));
        std::fs::write(root.join("index.json"), index.to_string()).unwrap();
        let reg = open(&root);
        assert_eq!(reg.recovery_report().dropped_entries, 1);
        assert!(reg.is_enabled(&id));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tampered_or_missing_manifest_quarantines_only_that_extension() {
        let root = unique_temp_path("velox-ext-reg-quarantine");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        drop(reg);
        std::fs::write(
            root.join("packages").join(&id).join("manifest.json"),
            "garbage",
        )
        .unwrap();
        let mut reg = open(&root);
        assert_eq!(
            reg.get(&id).unwrap().state,
            ExtensionState::Disabled(DisabledReason::Corrupt)
        );
        assert_eq!(reg.recovery_report().quarantined, vec![id.clone()]);
        assert_eq!(
            reg.enable(&id),
            Err(RegistryError::Lifecycle(LifecycleError::Quarantined))
        );
        // 同版の再インストール (更新経路) で修復できる。
        reg.update(&pkg_bg("1.0.0", "")).unwrap();
        assert_eq!(
            reg.get(&id).unwrap().state,
            ExtensionState::Disabled(DisabledReason::User)
        );
        reg.enable(&id).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn manifest_widened_on_disk_is_disabled_at_startup() {
        let root = unique_temp_path("velox-ext-reg-widen");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        drop(reg);
        // ディスク上のマニフェストへ権限を書き足す (改ざん)。
        let widened = pkg_bg("1.0.0", r#","permissions":["tabs"]"#);
        std::fs::write(
            root.join("packages").join(&id).join("manifest.json"),
            &widened.files()["manifest.json"],
        )
        .unwrap();
        let reg = open(&root);
        assert_eq!(
            reg.get(&id).unwrap().state,
            ExtensionState::Disabled(DisabledReason::NeedsReconsent)
        );
        assert!(!reg.get(&id).unwrap().approval.permissions.contains("tabs"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn crash_leftovers_are_cleaned_and_interrupted_update_is_rolled_back() {
        let root = unique_temp_path("velox-ext-reg-crash");
        let mut reg = open(&root);
        let id = reg.install(&pkg_bg("1.0.0", ""), true).unwrap();
        drop(reg);
        // 1) 索引に無い packages/ の残骸 (install が rename 後、索引を書く前に落ちた)。
        std::fs::create_dir_all(root.join("packages/com.example.orphan")).unwrap();
        // 2) staging の残骸。
        std::fs::create_dir_all(root.join("staging/half.1")).unwrap();
        // 3) 更新の途中 (旧版を .old へ退避した後、新版を置く前) で落ちた。
        std::fs::rename(
            root.join("packages").join(&id),
            root.join("staging").join(format!("{id}.old")),
        )
        .unwrap();
        let reg = open(&root);
        assert_eq!(reg.recovery_report().restored_previous, vec![id.clone()]);
        assert_eq!(
            reg.recovery_report().removed_orphans,
            vec!["com.example.orphan".to_owned()]
        );
        assert!(reg.is_enabled(&id));
        assert!(!root.join("packages/com.example.orphan").exists());
        assert!(list_dir_names(&root.join("staging")).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
