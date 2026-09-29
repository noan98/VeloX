//! Extension API のバージョン付きスキーマ (Issue #83、docs/extensions.md §8)。
//!
//! 拡張機能 (background / content script) が Rust ホストへ送る**信頼しない**
//! リクエストの形と、その応答の形をここで定義する。
//!
//! ```text
//! 要求: {"api_version": 1, "id": 7, "method": "storage.get", "params": {"keys": ["a"]}}
//! 成功: {"api_version": 1, "id": 7, "ok": true,  "result": {...}}
//! 失敗: {"api_version": 1, "id": 7, "ok": false, "error": {"code": "permission_denied", "message": "..."}}
//! ```
//!
//! 設計上の規律 (D159 / D163):
//! - **要求に拡張機能 ID のフィールドは無い。** 呼び出し元は Rust が経路から決める
//!   (`ExtensionHost::handle_request` の引数)。
//! - 未知のフィールド・未知のメソッド・未知の `api_version` は拒否する。
//! - 要求全体・キー数・オリジン数に上限を持ち、**パース前に**サイズを見る。
//! - どのメソッドがどの権限を要るかは [`METHODS`] の 1 か所に集約する
//!   (`schema_json()` が機械可読な仕様として出力する)。

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::browser::extension_manifest::{HostPattern, Permission};

/// このビルドが話す API の版。互換を壊す変更は版を上げ、`MIN_SUPPORTED_API_VERSION`
/// との間で交渉する。
pub const API_VERSION: u32 = 1;
/// 受け付ける最小の版。
pub const MIN_SUPPORTED_API_VERSION: u32 = 1;
/// 要求 1 件の最大バイト数 (パース前に検査する)。
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// 1 回の呼び出しで指定できるキー数の上限。
pub const MAX_KEYS_PER_CALL: usize = 512;
/// `permissions.*` で指定できる項目数の上限。
pub const MAX_PERMISSION_ITEMS: usize = 60;

/// 1 メソッドの仕様。
#[derive(Debug, Clone, Copy)]
pub struct MethodSpec {
    pub name: &'static str,
    /// 呼ぶために承認済みでなければならない権限。`None` は権限不要 (ただし
    /// メソッド内で個別の検査をするものがある。`note` を参照)。
    pub permission: Option<Permission>,
    pub note: &'static str,
}

/// API の全メソッド (これに無いものは存在しない)。
pub const METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "runtime.getInfo",
        permission: None,
        note: "API 版・自分の ID・版を返す",
    },
    MethodSpec {
        name: "runtime.getManifest",
        permission: None,
        note: "自分の (承認済み) マニフェスト情報を返す",
    },
    MethodSpec {
        name: "storage.get",
        permission: Some(Permission::Storage),
        note: "params.keys 省略で全件",
    },
    MethodSpec {
        name: "storage.set",
        permission: Some(Permission::Storage),
        note: "クォータ超過は quota_exceeded",
    },
    MethodSpec {
        name: "storage.remove",
        permission: Some(Permission::Storage),
        note: "",
    },
    MethodSpec {
        name: "storage.clear",
        permission: Some(Permission::Storage),
        note: "",
    },
    MethodSpec {
        name: "storage.getBytesInUse",
        permission: Some(Permission::Storage),
        note: "",
    },
    MethodSpec {
        name: "tabs.query",
        permission: Some(Permission::Tabs),
        note: "http/https のタブだけ。active で絞り込める",
    },
    MethodSpec {
        name: "tabs.get",
        permission: None,
        note: "tabs 権限、または対象タブの active_tab 付与が必要",
    },
    MethodSpec {
        name: "scripting.executeScript",
        permission: Some(Permission::Scripting),
        note: "加えて、対象タブの URL がホスト権限に一致するか active_tab 付与が必要",
    },
    MethodSpec {
        name: "permissions.getAll",
        permission: None,
        note: "",
    },
    MethodSpec {
        name: "permissions.contains",
        permission: None,
        note: "",
    },
    MethodSpec {
        name: "permissions.request",
        permission: None,
        note: "optional_* に宣言したものだけ。ユーザ操作 (user activation) の直後に限る",
    },
    MethodSpec {
        name: "permissions.remove",
        permission: None,
        note: "付与済みの optional_* だけ取り消せる",
    },
];

pub fn method_spec(name: &str) -> Option<&'static MethodSpec> {
    METHODS.iter().find(|m| m.name == name)
}

/// 機械可読な API 仕様 (バージョン付きスキーマ)。
pub fn schema_json() -> Value {
    json!({
        "api_version": API_VERSION,
        "min_supported_api_version": MIN_SUPPORTED_API_VERSION,
        "max_request_bytes": MAX_REQUEST_BYTES,
        "methods": METHODS.iter().map(|m| json!({
            "name": m.name,
            "permission": m.permission.map(|p| p.as_str()),
            "note": m.note,
        })).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorCode {
    /// 形が不正・大きすぎる・JSON でない。
    InvalidRequest,
    /// `api_version` が未指定/未対応。
    UnsupportedVersion,
    UnknownMethod,
    /// `params` が不正。
    InvalidParams,
    /// 権限が無い (未承認・宣言外・ユーザ操作なし・ホスト不一致)。
    PermissionDenied,
    /// 拡張機能が無効/未インストール。
    ExtensionDisabled,
    NotFound,
    QuotaExceeded,
    Internal,
}

impl ApiErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ApiErrorCode::InvalidRequest => "invalid_request",
            ApiErrorCode::UnsupportedVersion => "unsupported_version",
            ApiErrorCode::UnknownMethod => "unknown_method",
            ApiErrorCode::InvalidParams => "invalid_params",
            ApiErrorCode::PermissionDenied => "permission_denied",
            ApiErrorCode::ExtensionDisabled => "extension_disabled",
            ApiErrorCode::NotFound => "not_found",
            ApiErrorCode::QuotaExceeded => "quota_exceeded",
            ApiErrorCode::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn new(code: ApiErrorCode, message: impl Into<String>) -> ApiError {
        ApiError {
            code,
            message: message.into(),
        }
    }
}

/// 型付きの API 呼び出し (パース・形の検証済み。権限検査はまだ)。
#[derive(Debug, Clone, PartialEq)]
pub enum ApiCall {
    RuntimeGetInfo,
    RuntimeGetManifest,
    StorageGet {
        keys: Option<Vec<String>>,
    },
    StorageSet {
        items: Map<String, Value>,
    },
    StorageRemove {
        keys: Vec<String>,
    },
    StorageClear,
    StorageGetBytesInUse {
        keys: Option<Vec<String>>,
    },
    TabsQuery {
        active: Option<bool>,
    },
    TabsGet {
        tab_id: u64,
    },
    ScriptingExecuteScript {
        tab_id: u64,
        file: String,
    },
    PermissionsGetAll,
    PermissionsContains {
        permissions: Vec<Permission>,
        origins: Vec<HostPattern>,
    },
    PermissionsRequest {
        permissions: Vec<Permission>,
        origins: Vec<HostPattern>,
    },
    PermissionsRemove {
        permissions: Vec<Permission>,
        origins: Vec<HostPattern>,
    },
}

/// 検証済みのリクエスト封筒。
#[derive(Debug)]
pub struct Envelope {
    pub id: Option<u64>,
    pub api_version: u32,
    pub method: String,
    pub params: Option<Value>,
}

/// 封筒のパース失敗。`id` は取り出せたときだけ入る (応答の相関用)。
#[derive(Debug)]
pub struct EnvelopeError {
    pub id: Option<u64>,
    pub error: ApiError,
}

/// 生のバイト列から封筒を取り出す。サイズはパース**前**に検査する。
pub fn parse_envelope(raw: &[u8]) -> Result<Envelope, EnvelopeError> {
    let fail = |id, code, msg: &str| EnvelopeError {
        id,
        error: ApiError::new(code, msg),
    };
    if raw.len() > MAX_REQUEST_BYTES {
        return Err(fail(
            None,
            ApiErrorCode::InvalidRequest,
            "リクエストが大きすぎる",
        ));
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| {
        fail(
            None,
            ApiErrorCode::InvalidRequest,
            "JSON として解釈できない",
        )
    })?;
    let Value::Object(mut obj) = value else {
        return Err(fail(
            None,
            ApiErrorCode::InvalidRequest,
            "リクエストはオブジェクトでなければならない",
        ));
    };
    let id = obj.get("id").and_then(Value::as_u64);
    let mut take = |k: &str| obj.remove(k);
    let (v, i, m, p) = (
        take("api_version"),
        take("id"),
        take("method"),
        take("params"),
    );
    if let Some(unknown) = obj.keys().next() {
        return Err(fail(
            id,
            ApiErrorCode::InvalidRequest,
            &format!(
                "未知のフィールド `{}`",
                unknown.chars().take(32).collect::<String>()
            ),
        ));
    }
    if i.is_some() && id.is_none() {
        return Err(fail(None, ApiErrorCode::InvalidRequest, "id は整数"));
    }
    let api_version = v
        .as_ref()
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            fail(
                id,
                ApiErrorCode::UnsupportedVersion,
                "api_version (整数) が必要",
            )
        })?;
    let method = match m {
        Some(Value::String(s)) if s.len() <= 64 => s,
        _ => {
            return Err(fail(
                id,
                ApiErrorCode::InvalidRequest,
                "method (64 バイト以内の文字列) が必要",
            ))
        }
    };
    Ok(Envelope {
        id,
        api_version,
        method,
        params: p,
    })
}

/// 版の交渉。受け付けられれば応答に使う版を返す。
pub fn negotiate_version(requested: u32) -> Result<u32, ApiError> {
    if (MIN_SUPPORTED_API_VERSION..=API_VERSION).contains(&requested) {
        Ok(requested)
    } else {
        Err(ApiError::new(
            ApiErrorCode::UnsupportedVersion,
            format!(
                "api_version {requested} は未対応 (対応: {MIN_SUPPORTED_API_VERSION}..={API_VERSION})"
            ),
        ))
    }
}

// --- params の型 (deny_unknown_fields) ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeysOpt {
    keys: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeysReq {
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetParams {
    items: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryParams {
    active: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TabParams {
    tab_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecParams {
    tab_id: u64,
    file: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermSetParams {
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    origins: Vec<String>,
}

fn params_as<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, ApiError> {
    // 省略と null は空オブジェクトとして扱う。
    let v = match params {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(v) => v,
    };
    serde_json::from_value(v)
        .map_err(|e| ApiError::new(ApiErrorCode::InvalidParams, format!("params が不正: {e}")))
}

fn check_keys(keys: &[String]) -> Result<(), ApiError> {
    if keys.len() > MAX_KEYS_PER_CALL {
        return Err(ApiError::new(
            ApiErrorCode::InvalidParams,
            "キーの数が上限を超えている",
        ));
    }
    Ok(())
}

fn permission_set(p: PermSetParams) -> Result<(Vec<Permission>, Vec<HostPattern>), ApiError> {
    let bad = |m: &str| ApiError::new(ApiErrorCode::InvalidParams, m);
    if p.permissions.len() > MAX_PERMISSION_ITEMS || p.origins.len() > MAX_PERMISSION_ITEMS {
        return Err(bad("項目数が上限を超えている"));
    }
    let permissions = p
        .permissions
        .iter()
        .map(|n| Permission::from_name(n).ok_or_else(|| bad("未知の権限名")))
        .collect::<Result<Vec<_>, _>>()?;
    let origins = p
        .origins
        .iter()
        .map(|o| HostPattern::parse(o).map_err(|_| bad("ホストパターンが不正")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((permissions, origins))
}

/// メソッド名と `params` を型付きの呼び出しへ変換する。未知のメソッドは
/// `UnknownMethod`、形の不正は `InvalidParams`。
pub fn parse_call(method: &str, params: Option<Value>) -> Result<ApiCall, ApiError> {
    Ok(match method {
        "runtime.getInfo" => {
            params_as::<NoParams>(params)?;
            ApiCall::RuntimeGetInfo
        }
        "runtime.getManifest" => {
            params_as::<NoParams>(params)?;
            ApiCall::RuntimeGetManifest
        }
        "storage.get" => {
            let p: KeysOpt = params_as(params)?;
            if let Some(k) = &p.keys {
                check_keys(k)?;
            }
            ApiCall::StorageGet { keys: p.keys }
        }
        "storage.set" => {
            let p: SetParams = params_as(params)?;
            if p.items.len() > MAX_KEYS_PER_CALL {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidParams,
                    "キーの数が上限を超えている",
                ));
            }
            ApiCall::StorageSet { items: p.items }
        }
        "storage.remove" => {
            let p: KeysReq = params_as(params)?;
            check_keys(&p.keys)?;
            ApiCall::StorageRemove { keys: p.keys }
        }
        "storage.clear" => {
            params_as::<NoParams>(params)?;
            ApiCall::StorageClear
        }
        "storage.getBytesInUse" => {
            let p: KeysOpt = params_as(params)?;
            if let Some(k) = &p.keys {
                check_keys(k)?;
            }
            ApiCall::StorageGetBytesInUse { keys: p.keys }
        }
        "tabs.query" => ApiCall::TabsQuery {
            active: params_as::<QueryParams>(params)?.active,
        },
        "tabs.get" => ApiCall::TabsGet {
            tab_id: params_as::<TabParams>(params)?.tab_id,
        },
        "scripting.executeScript" => {
            let p: ExecParams = params_as(params)?;
            ApiCall::ScriptingExecuteScript {
                tab_id: p.tab_id,
                file: p.file,
            }
        }
        "permissions.getAll" => {
            params_as::<NoParams>(params)?;
            ApiCall::PermissionsGetAll
        }
        "permissions.contains" => {
            let (permissions, origins) = permission_set(params_as(params)?)?;
            ApiCall::PermissionsContains {
                permissions,
                origins,
            }
        }
        "permissions.request" => {
            let (permissions, origins) = permission_set(params_as(params)?)?;
            ApiCall::PermissionsRequest {
                permissions,
                origins,
            }
        }
        "permissions.remove" => {
            let (permissions, origins) = permission_set(params_as(params)?)?;
            ApiCall::PermissionsRemove {
                permissions,
                origins,
            }
        }
        _ => return Err(ApiError::new(ApiErrorCode::UnknownMethod, "未知のメソッド")),
    })
}

pub fn response_ok(api_version: u32, id: Option<u64>, result: Value) -> String {
    json!({"api_version": api_version, "id": id, "ok": true, "result": result}).to_string()
}

pub fn response_err(api_version: u32, id: Option<u64>, error: &ApiError) -> String {
    json!({
        "api_version": api_version,
        "id": id,
        "ok": false,
        "error": {"code": error.code.as_str(), "message": error.message},
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(s: &str) -> Result<Envelope, EnvelopeError> {
        parse_envelope(s.as_bytes())
    }

    #[test]
    fn envelope_happy_path() {
        let e = env(r#"{"api_version":1,"id":7,"method":"storage.get","params":{"keys":["a"]}}"#)
            .unwrap();
        assert_eq!(
            (e.id, e.api_version, e.method.as_str()),
            (Some(7), 1, "storage.get")
        );
    }

    #[test]
    fn envelope_rejections() {
        let code = |s: &str| env(s).unwrap_err().error.code;
        assert_eq!(code("not json"), ApiErrorCode::InvalidRequest);
        assert_eq!(code("[]"), ApiErrorCode::InvalidRequest);
        assert_eq!(
            code(r#"{"id":1,"method":"x"}"#),
            ApiErrorCode::UnsupportedVersion
        );
        assert_eq!(
            code(r#"{"api_version":"1","id":1,"method":"x"}"#),
            ApiErrorCode::UnsupportedVersion
        );
        assert_eq!(
            code(r#"{"api_version":1,"id":1}"#),
            ApiErrorCode::InvalidRequest
        );
        // 自己申告の拡張機能 ID などの未知フィールドは拒否 (id は相関のため保持)。
        let err =
            env(r#"{"api_version":1,"id":3,"method":"x","extension_id":"evil"}"#).unwrap_err();
        assert_eq!(err.error.code, ApiErrorCode::InvalidRequest);
        assert_eq!(err.id, Some(3));
        assert_eq!(
            code(r#"{"api_version":1,"id":"a","method":"x"}"#),
            ApiErrorCode::InvalidRequest
        );
    }

    #[test]
    fn oversized_request_is_rejected_before_parsing() {
        let big = format!(
            r#"{{"api_version":1,"id":1,"method":"storage.set","params":{{"pad":"{}"}}}}"#,
            "x".repeat(MAX_REQUEST_BYTES)
        );
        assert_eq!(
            env(&big).unwrap_err().error.code,
            ApiErrorCode::InvalidRequest
        );
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate_version(1), Ok(1));
        assert_eq!(
            negotiate_version(0).unwrap_err().code,
            ApiErrorCode::UnsupportedVersion
        );
        assert_eq!(
            negotiate_version(API_VERSION + 1).unwrap_err().code,
            ApiErrorCode::UnsupportedVersion
        );
    }

    #[test]
    fn every_method_in_the_table_parses_and_nothing_else_does() {
        // 各メソッドが最小の妥当な params でパースできる (表と parse_call のずれ検出)。
        let samples: &[(&str, Value)] = &[
            ("runtime.getInfo", json!({})),
            ("runtime.getManifest", json!({})),
            ("storage.get", json!({})),
            ("storage.set", json!({"items": {"a": 1}})),
            ("storage.remove", json!({"keys": ["a"]})),
            ("storage.clear", json!({})),
            ("storage.getBytesInUse", json!({})),
            ("tabs.query", json!({"active": true})),
            ("tabs.get", json!({"tab_id": 1})),
            (
                "scripting.executeScript",
                json!({"tab_id": 1, "file": "a.js"}),
            ),
            ("permissions.getAll", json!({})),
            ("permissions.contains", json!({"permissions": ["tabs"]})),
            (
                "permissions.request",
                json!({"origins": ["https://a.example.com/*"]}),
            ),
            ("permissions.remove", json!({"permissions": ["tabs"]})),
        ];
        assert_eq!(samples.len(), METHODS.len());
        for (name, params) in samples {
            assert!(method_spec(name).is_some(), "{name} not in METHODS");
            parse_call(name, Some(params.clone())).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
        assert_eq!(
            parse_call("tabs.remove", None).unwrap_err().code,
            ApiErrorCode::UnknownMethod
        );
    }

    #[test]
    fn params_are_strictly_typed() {
        let code = |m: &str, p: Value| parse_call(m, Some(p)).unwrap_err().code;
        assert_eq!(
            code("storage.get", json!({"keys": "a"})),
            ApiErrorCode::InvalidParams
        );
        assert_eq!(
            code("storage.get", json!({"bogus": 1})),
            ApiErrorCode::InvalidParams
        );
        assert_eq!(
            code("storage.clear", json!({"x": 1})),
            ApiErrorCode::InvalidParams
        );
        assert_eq!(
            code("tabs.get", json!({"tab_id": -1})),
            ApiErrorCode::InvalidParams
        );
        assert_eq!(
            code("permissions.request", json!({"permissions": ["cookies"]})),
            ApiErrorCode::InvalidParams
        );
        assert_eq!(
            code("permissions.request", json!({"origins": ["file:///*"]})),
            ApiErrorCode::InvalidParams
        );
        let many: Vec<String> = (0..=MAX_KEYS_PER_CALL).map(|i| format!("k{i}")).collect();
        assert_eq!(
            code("storage.remove", json!({"keys": many})),
            ApiErrorCode::InvalidParams
        );
        // params 省略・null は空オブジェクト扱い。
        assert!(parse_call("storage.get", None).is_ok());
        assert!(parse_call("storage.clear", Some(Value::Null)).is_ok());
    }

    #[test]
    fn schema_lists_every_method_with_its_permission() {
        let s = schema_json();
        assert_eq!(s["api_version"], API_VERSION);
        let methods = s["methods"].as_array().unwrap();
        assert_eq!(methods.len(), METHODS.len());
        let storage_get = methods.iter().find(|m| m["name"] == "storage.get").unwrap();
        assert_eq!(storage_get["permission"], "storage");
        let info = methods
            .iter()
            .find(|m| m["name"] == "runtime.getInfo")
            .unwrap();
        assert!(info["permission"].is_null());
    }

    #[test]
    fn responses_are_well_formed() {
        let ok: Value = serde_json::from_str(&response_ok(1, Some(2), json!({"a": 1}))).unwrap();
        assert_eq!(
            (ok["ok"].clone(), ok["id"].clone()),
            (json!(true), json!(2))
        );
        let err: Value = serde_json::from_str(&response_err(
            1,
            None,
            &ApiError::new(ApiErrorCode::PermissionDenied, "no"),
        ))
        .unwrap();
        assert_eq!(err["error"]["code"], "permission_denied");
        assert!(err["id"].is_null());
    }
}
