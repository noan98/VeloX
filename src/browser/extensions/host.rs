//! 拡張機能ホスト: 信頼しない要求を検証し、権限検査を通ったものだけを実行する
//! (Issue #83 / #84、docs/extensions.md §3.2 / §4)。
//!
//! ```text
//!  拡張機能 (信頼しない)        Rust: ExtensionHost                  BrowserHost (trait)
//!  ─ JSON 要求 ─────────▶  1. サイズ → 封筒 → 版の交渉
//!  (呼び出し元 ID は          2. 拡張機能が「有効」か (毎回)
//!   Rust が経路から渡す)       3. 承認済み権限の検査 (毎回)          tabs / execute_script /
//!                            4. params の型検査                     同意ダイアログ / user activation
//!                            5. メソッド固有の検査 (ホスト・タブ)  ─▶ ブラウザの実操作
//!  ◀─ JSON 応答 ────────
//! ```
//!
//! - **呼び出し元の拡張機能 ID は `handle_request` の引数**で、要求本文からは読まない。
//! - 権限は使用時 (呼び出しごと) にレジストリの現在の承認状態で検査する。無効化・
//!   取り消しは次の呼び出しから即時に効く (TOCTOU を作らない)。
//! - ブラウザ本体への操作は `BrowserHost` トレイト越しだけ。ここは wry にも
//!   `app.rs` にも依存しない (未配線。D163)。

use std::collections::HashMap;

use serde_json::{json, Value};

use super::api::{self, ApiCall, ApiError, ApiErrorCode, API_VERSION, METHODS};
use super::lifecycle::LifecycleRecord;
use super::package::ExtensionPackage;
use super::registry::{
    ExtensionRegistry, InstallPreview, InstalledExtension, RegistryError, UpdateOutcome,
};
use super::storage::{ExtensionStorage, StorageError, StorageLimits};
use crate::browser::extension_manifest::{validate_resource_path, HostPattern, Permission};

/// `scripting.executeScript` で注入できるスクリプトの最大バイト数。
pub const MAX_SCRIPT_BYTES: u64 = 256 * 1024;

/// 拡張機能へ見せてよいタブの情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub active: bool,
}

/// 実行時の権限要求 (`permissions.request`) をユーザへ提示するための内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRequest {
    pub permissions: Vec<Permission>,
    pub origins: Vec<String>,
}

/// ホストがブラウザ本体へ求める操作。実装は `app.rs` 側 (未配線)。テストでは偽物を使う。
///
/// **実装側の責務**: `tabs` は拡張機能へ見せてよいタブだけを返す (プライベート
/// ウィンドウのタブは、ユーザが拡張機能ごとに許可していない限り含めない。D15)。
pub trait BrowserHost {
    /// 拡張機能から見えるタブの一覧。
    fn tabs(&self) -> Vec<TabInfo>;

    fn tab(&self, id: u64) -> Option<TabInfo> {
        self.tabs().into_iter().find(|t| t.id == id)
    }

    /// `tab_id` のタブへスクリプトを注入する。ホストが権限・URL 一致を確認済み。
    fn execute_script(
        &mut self,
        extension_id: &str,
        tab_id: u64,
        source: &str,
    ) -> Result<(), String>;

    /// 直前にユーザ操作 (クリック等) があったかを確認し、あれば**消費**する。
    fn consume_user_activation(&mut self, extension_id: &str) -> bool;

    /// 信頼 UI で権限の追加をユーザに尋ねる。承認なら `true`。
    fn request_consent(&mut self, extension_id: &str, request: &ConsentRequest) -> bool;
}

fn is_web_url(url: &str) -> bool {
    HostPattern::all_urls().matches(url)
}

fn deny(msg: &str) -> ApiError {
    ApiError::new(ApiErrorCode::PermissionDenied, msg)
}

fn tab_json(t: &TabInfo) -> Value {
    json!({"id": t.id, "url": t.url, "title": t.title, "active": t.active})
}

pub struct ExtensionHost {
    registry: ExtensionRegistry,
    storage_limits: StorageLimits,
    storages: HashMap<String, ExtensionStorage>,
    /// 拡張機能ごとの `active_tab` 付与: (タブ ID, 付与時の URL)。ナビゲーションで
    /// URL が変わると失効する。
    active_tab_grants: HashMap<String, (u64, String)>,
}

impl ExtensionHost {
    pub fn new(registry: ExtensionRegistry) -> ExtensionHost {
        ExtensionHost::with_storage_limits(registry, StorageLimits::default())
    }

    pub fn with_storage_limits(
        registry: ExtensionRegistry,
        storage_limits: StorageLimits,
    ) -> ExtensionHost {
        ExtensionHost {
            registry,
            storage_limits,
            storages: HashMap::new(),
            active_tab_grants: HashMap::new(),
        }
    }

    /// レジストリの読み取り専用ビュー。変更は必ずこのホストのメソッド経由
    /// (ストレージのキャッシュ等と食い違わせないため)。
    pub fn registry(&self) -> &ExtensionRegistry {
        &self.registry
    }

    pub fn drain_events(&mut self) -> Vec<LifecycleRecord> {
        self.registry.drain_events()
    }

    // --- ライフサイクル操作 (信頼 UI から呼ぶ) ---

    pub fn preview(&self, pkg: &ExtensionPackage) -> Result<InstallPreview, RegistryError> {
        self.registry.preview(pkg)
    }

    pub fn install(
        &mut self,
        pkg: &ExtensionPackage,
        consent: bool,
    ) -> Result<String, RegistryError> {
        let id = self.registry.install(pkg, consent)?;
        self.storages.remove(&id);
        self.active_tab_grants.remove(&id);
        Ok(id)
    }

    pub fn update(&mut self, pkg: &ExtensionPackage) -> Result<UpdateOutcome, RegistryError> {
        let out = self.registry.update(pkg)?;
        self.active_tab_grants.remove(&out.id);
        Ok(out)
    }

    pub fn enable(&mut self, id: &str) -> Result<(), RegistryError> {
        self.registry.enable(id)
    }

    pub fn disable(&mut self, id: &str) -> Result<(), RegistryError> {
        self.registry.disable(id)?;
        self.active_tab_grants.remove(id);
        Ok(())
    }

    pub fn approve(&mut self, id: &str) -> Result<(), RegistryError> {
        self.registry.approve(id)
    }

    pub fn uninstall(&mut self, id: &str) -> Result<bool, RegistryError> {
        let done = self.registry.uninstall(id)?;
        self.storages.remove(id);
        self.active_tab_grants.remove(id);
        Ok(done)
    }

    /// ユーザが拡張機能を明示的に起動した (ツールバーのボタン等) ときに信頼 UI が呼ぶ。
    /// そのタブ 1 枚・その時点の URL に限って `active_tab` を与える。
    pub fn grant_active_tab(
        &mut self,
        extension_id: &str,
        tab_id: u64,
        browser: &dyn BrowserHost,
    ) -> Result<(), ApiError> {
        let entry = self.enabled_entry(extension_id)?;
        if !entry.approval.has_permission(Permission::ActiveTab) {
            return Err(deny("active_tab 権限が無い"));
        }
        let tab = browser
            .tab(tab_id)
            .filter(|t| is_web_url(&t.url))
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "タブが無い"))?;
        self.active_tab_grants
            .insert(extension_id.to_owned(), (tab.id, tab.url));
        Ok(())
    }

    // --- リクエスト処理 ---

    fn enabled_entry(&self, id: &str) -> Result<&InstalledExtension, ApiError> {
        match self.registry.get(id) {
            Some(e) if e.state.is_enabled() => Ok(e),
            Some(_) => Err(ApiError::new(
                ApiErrorCode::ExtensionDisabled,
                "拡張機能は無効",
            )),
            None => Err(ApiError::new(
                ApiErrorCode::ExtensionDisabled,
                "拡張機能はインストールされていない",
            )),
        }
    }

    /// 拡張機能 `extension_id` からの要求 `raw` を処理して JSON 応答を返す。
    ///
    /// `extension_id` は **Rust が経路 (どの拡張機能の webview の IPC ハンドラか) から
    /// 決めた値**でなければならない。`raw` の中身は一切信用しない。
    pub fn handle_request(
        &mut self,
        extension_id: &str,
        raw: &[u8],
        browser: &mut dyn BrowserHost,
    ) -> String {
        let env = match api::parse_envelope(raw) {
            Ok(e) => e,
            Err(e) => return api::response_err(API_VERSION, e.id, &e.error),
        };
        let version = match api::negotiate_version(env.api_version) {
            Ok(v) => v,
            Err(e) => return api::response_err(API_VERSION, env.id, &e),
        };
        match self.dispatch(extension_id, &env.method, env.params, browser) {
            Ok(result) => api::response_ok(version, env.id, result),
            Err(e) => api::response_err(version, env.id, &e),
        }
    }

    fn dispatch(
        &mut self,
        ext: &str,
        method: &str,
        params: Option<Value>,
        browser: &mut dyn BrowserHost,
    ) -> Result<Value, ApiError> {
        let spec = METHODS
            .iter()
            .find(|m| m.name == method)
            .ok_or_else(|| ApiError::new(ApiErrorCode::UnknownMethod, "未知のメソッド"))?;
        // 使用時検査 1: 有効か。2: 承認済み権限。
        let entry = self.enabled_entry(ext)?;
        if let Some(p) = spec.permission {
            if !entry.approval.has_permission(p) {
                return Err(deny(&format!("権限 `{}` が必要", p.as_str())));
            }
        }
        let call = api::parse_call(method, params)?;
        match call {
            ApiCall::RuntimeGetInfo => Ok(json!({
                "api_version": API_VERSION,
                "min_supported_api_version": api::MIN_SUPPORTED_API_VERSION,
                "extension_id": ext,
                "version": entry.version,
            })),
            ApiCall::RuntimeGetManifest => {
                let m = entry
                    .manifest
                    .as_ref()
                    .ok_or_else(|| ApiError::new(ApiErrorCode::Internal, "manifest が無い"))?;
                Ok(json!({
                    "id": m.id,
                    "name": m.name,
                    "version": m.version.to_string(),
                    "description": m.description,
                    "permissions": entry.approval.permission_names(),
                    "host_permissions": entry.approval.host_names(),
                }))
            }
            ApiCall::StorageGet { keys } => {
                let items = self.storage(ext).get(keys.as_deref());
                Ok(json!({"items": items}))
            }
            ApiCall::StorageSet { items } => {
                self.storage(ext).set(items).map_err(storage_error)?;
                Ok(json!({}))
            }
            ApiCall::StorageRemove { keys } => {
                self.storage(ext).remove(&keys).map_err(storage_error)?;
                Ok(json!({}))
            }
            ApiCall::StorageClear => {
                self.storage(ext).clear().map_err(storage_error)?;
                Ok(json!({}))
            }
            ApiCall::StorageGetBytesInUse { keys } => {
                let n = self.storage(ext).bytes_in_use(keys.as_deref());
                Ok(json!({"bytes": n}))
            }
            ApiCall::TabsQuery { active } => {
                let tabs: Vec<Value> = browser
                    .tabs()
                    .iter()
                    .filter(|t| is_web_url(&t.url))
                    .filter(|t| active.is_none_or(|a| t.active == a))
                    .map(tab_json)
                    .collect();
                Ok(Value::Array(tabs))
            }
            ApiCall::TabsGet { tab_id } => {
                let tab = browser
                    .tab(tab_id)
                    .filter(|t| is_web_url(&t.url))
                    .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "タブが無い"))?;
                let allowed = entry.approval.has_permission(Permission::Tabs)
                    || self.active_tab_valid(ext, &tab);
                if !allowed {
                    return Err(deny("tabs 権限、または対象タブの active_tab が必要"));
                }
                Ok(tab_json(&tab))
            }
            ApiCall::ScriptingExecuteScript { tab_id, file } => {
                let tab = browser
                    .tab(tab_id)
                    .filter(|t| is_web_url(&t.url))
                    .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "タブが無い"))?;
                // ホスト権限は「今の」タブ URL に対して見る (ナビゲーション後の URL)。
                let host_ok =
                    entry.approval.allows_url(&tab.url) || self.active_tab_valid(ext, &tab);
                if !host_ok {
                    return Err(deny("このタブの URL へのホスト権限が無い"));
                }
                validate_resource_path(&file)
                    .map_err(|_| ApiError::new(ApiErrorCode::InvalidParams, "file のパスが不正"))?;
                let source = self.read_package_script(ext, &file)?;
                browser.execute_script(ext, tab.id, &source).map_err(|_| {
                    ApiError::new(ApiErrorCode::Internal, "スクリプトを注入できなかった")
                })?;
                Ok(json!({}))
            }
            ApiCall::PermissionsGetAll => Ok(json!({
                "permissions": entry.approval.permission_names(),
                "origins": entry.approval.host_names(),
            })),
            ApiCall::PermissionsContains {
                permissions,
                origins,
            } => Ok(json!({"result": entry.approval.contains_all(&permissions, &origins)})),
            ApiCall::PermissionsRequest {
                permissions,
                origins,
            } => self.permissions_request(ext, permissions, origins, browser),
            ApiCall::PermissionsRemove {
                permissions,
                origins,
            } => {
                let removed = self
                    .registry
                    .revoke_optional(ext, &permissions, &origins)
                    .map_err(|_| {
                        ApiError::new(ApiErrorCode::Internal, "取り消しを保存できなかった")
                    })?;
                Ok(json!({"removed": removed}))
            }
        }
    }

    fn permissions_request(
        &mut self,
        ext: &str,
        permissions: Vec<Permission>,
        origins: Vec<HostPattern>,
        browser: &mut dyn BrowserHost,
    ) -> Result<Value, ApiError> {
        if permissions.is_empty() && origins.is_empty() {
            return Err(ApiError::new(ApiErrorCode::InvalidParams, "要求が空"));
        }
        let entry = self.enabled_entry(ext)?;
        let manifest = entry
            .manifest
            .as_ref()
            .ok_or_else(|| ApiError::new(ApiErrorCode::Internal, "manifest が無い"))?;
        // optional_* に宣言したものだけ要求できる (宣言は「要求」の上限)。
        let declared = permissions.iter().all(|p| {
            manifest.optional_permissions.contains(p) || entry.approval.has_permission(*p)
        }) && origins.iter().all(|o| {
            manifest.optional_host_permissions.contains(o)
                || entry.approval.contains_all(&[], std::slice::from_ref(o))
        });
        if !declared {
            return Err(deny("optional として宣言されていない権限は要求できない"));
        }
        if entry.approval.contains_all(&permissions, &origins) {
            return Ok(json!({"granted": true}));
        }
        // ユーザ操作の直後に限る。消費するので使い回せない。
        if !browser.consume_user_activation(ext) {
            return Err(deny("権限の要求にはユーザ操作が必要"));
        }
        let request = ConsentRequest {
            permissions: permissions.clone(),
            origins: origins.iter().map(|o| o.to_string()).collect(),
        };
        if !browser.request_consent(ext, &request) {
            // 拒否は正常系。
            return Ok(json!({"granted": false}));
        }
        self.registry
            .grant_optional(ext, &permissions, &origins)
            .map_err(|_| ApiError::new(ApiErrorCode::Internal, "付与を保存できなかった"))?;
        Ok(json!({"granted": true}))
    }

    fn active_tab_valid(&self, ext: &str, tab: &TabInfo) -> bool {
        let has = self
            .registry
            .get(ext)
            .is_some_and(|e| e.approval.has_permission(Permission::ActiveTab));
        has && self
            .active_tab_grants
            .get(ext)
            .is_some_and(|(id, url)| *id == tab.id && *url == tab.url)
    }

    fn storage(&mut self, ext: &str) -> &mut ExtensionStorage {
        let dir = self.registry.storage_dir(ext);
        let limits = self.storage_limits;
        self.storages
            .entry(ext.to_owned())
            .or_insert_with(|| ExtensionStorage::open(&dir, limits))
    }

    /// パッケージ内のスクリプトを読む。パスは検証済みで、ディレクトリ外へは出ない。
    fn read_package_script(&self, ext: &str, file: &str) -> Result<String, ApiError> {
        let path = self.registry.package_dir(ext).join(file);
        let not_found = || ApiError::new(ApiErrorCode::NotFound, "スクリプトがパッケージに無い");
        let meta = std::fs::symlink_metadata(&path).map_err(|_| not_found())?;
        if !meta.is_file() {
            return Err(not_found());
        }
        if meta.len() > MAX_SCRIPT_BYTES {
            return Err(ApiError::new(
                ApiErrorCode::InvalidParams,
                "スクリプトが大きすぎる",
            ));
        }
        let bytes = std::fs::read(&path).map_err(|_| not_found())?;
        String::from_utf8(bytes)
            .map_err(|_| ApiError::new(ApiErrorCode::InvalidParams, "スクリプトが UTF-8 でない"))
    }
}

fn storage_error(e: StorageError) -> ApiError {
    let code = match e {
        StorageError::InvalidKey(_) => ApiErrorCode::InvalidParams,
        StorageError::ValueTooLarge { .. }
        | StorageError::TooManyItems { .. }
        | StorageError::QuotaExceeded { .. } => ApiErrorCode::QuotaExceeded,
        StorageError::Io(_) => ApiErrorCode::Internal,
    };
    ApiError::new(code, e.to_string())
}
