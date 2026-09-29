//! Extension API / Lifecycle の統合テスト (Issue #83 / #84)。
//!
//! 実バイナリは起動しない。`ExtensionHost` を、偽のブラウザ (`FakeBrowserHost`) と
//! 一時ディレクトリで端から端まで駆動する: インストール → 有効化 → 権限による
//! 許可/拒否 → ストレージのクォータ → 無効化で遮断 → 権限が増える更新 → アンインストール、
//! および破損データからの復旧。時計も乱数も使わないので決定的。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use velox::browser::extension_manifest::ExtensionVersion;
use velox::browser::extensions::host::{BrowserHost, ConsentRequest, ExtensionHost, TabInfo};
use velox::browser::extensions::lifecycle::{
    DisabledReason, ExtensionState, LifecycleEvent, LifecycleRecord,
};
use velox::browser::extensions::package::ExtensionPackage;
use velox::browser::extensions::registry::{ExtensionRegistry, RegistryError};
use velox::browser::extensions::storage::StorageLimits;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// テストごとに一意な一時ディレクトリ (プロセス ID + 連番)。Drop で削除する。
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("velox-ext-it-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakeBrowserHost {
    tabs: Vec<TabInfo>,
    injected: Vec<(String, u64, String)>,
    activations: u32,
    consent_answer: bool,
    consent_requests: Vec<ConsentRequest>,
}

impl FakeBrowserHost {
    fn with_tabs() -> FakeBrowserHost {
        FakeBrowserHost {
            tabs: vec![
                TabInfo {
                    id: 1,
                    url: "https://example.com/page".into(),
                    title: "Example".into(),
                    active: true,
                },
                TabInfo {
                    id: 2,
                    url: "https://other.test/".into(),
                    title: "Other".into(),
                    active: false,
                },
                TabInfo {
                    id: 3,
                    url: "velox://settings".into(),
                    title: "Settings".into(),
                    active: false,
                },
            ],
            ..FakeBrowserHost::default()
        }
    }
    fn navigate(&mut self, id: u64, url: &str) {
        for t in &mut self.tabs {
            if t.id == id {
                t.url = url.to_owned();
            }
        }
    }
}

impl BrowserHost for FakeBrowserHost {
    fn tabs(&self) -> Vec<TabInfo> {
        self.tabs.clone()
    }
    fn execute_script(&mut self, ext: &str, tab_id: u64, source: &str) -> Result<(), String> {
        self.injected
            .push((ext.to_owned(), tab_id, source.to_owned()));
        Ok(())
    }
    fn consume_user_activation(&mut self, _ext: &str) -> bool {
        if self.activations > 0 {
            self.activations -= 1;
            true
        } else {
            false
        }
    }
    fn request_consent(&mut self, _ext: &str, request: &ConsentRequest) -> bool {
        self.consent_requests.push(request.clone());
        self.consent_answer
    }
}

fn velox_version() -> ExtensionVersion {
    ExtensionVersion::parse("1.0.0").unwrap()
}

fn open_host(root: &Path) -> ExtensionHost {
    ExtensionHost::new(ExtensionRegistry::open(root, velox_version()))
}

fn open_host_with(root: &Path, limits: StorageLimits) -> ExtensionHost {
    ExtensionHost::with_storage_limits(ExtensionRegistry::open(root, velox_version()), limits)
}

const ID: &str = "com.example.hello";

/// マニフェストの「id / name / version 以外」を `extra` で足したパッケージ。
fn package_with(id: &str, version: &str, extra: &str) -> ExtensionPackage {
    let manifest = format!(
        r#"{{"manifest_version":1,"id":"{id}","name":"Hello","version":"{version}",
            "background":{{"script":"bg.js"}}{extra}}}"#
    );
    ExtensionPackage::new()
        .with_file("manifest.json", manifest)
        .unwrap()
        .with_file("bg.js", "// background")
        .unwrap()
        .with_file("inject/hello.js", format!("/* hello {version} */"))
        .unwrap()
}

fn package(version: &str, extra: &str) -> ExtensionPackage {
    package_with(ID, version, extra)
}

/// 要求を送り、応答 JSON を返す。
fn call(
    host: &mut ExtensionHost,
    browser: &mut FakeBrowserHost,
    ext: &str,
    method: &str,
    params: Value,
) -> Value {
    let req = json!({"api_version": 1, "id": 1, "method": method, "params": params});
    let out = host.handle_request(ext, req.to_string().as_bytes(), browser);
    serde_json::from_str(&out).expect("host always answers with JSON")
}

fn ok(resp: &Value) -> &Value {
    assert_eq!(resp["ok"], true, "expected success, got {resp}");
    &resp["result"]
}

fn err_code(resp: &Value) -> &str {
    assert_eq!(resp["ok"], false, "expected failure, got {resp}");
    resp["error"]["code"].as_str().unwrap()
}

fn events_of(host: &mut ExtensionHost) -> Vec<LifecycleEvent> {
    host.drain_events()
        .into_iter()
        .map(|r: LifecycleRecord| r.event)
        .collect()
}

#[test]
fn full_lifecycle_end_to_end() {
    let root = TempDir::new("full");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();

    // 承認ダイアログ用のプレビュー → 承認してインストール。
    let pkg = package(
        "1.0.0",
        r#","permissions":["storage","tabs"],"host_permissions":["https://example.com/*"]"#,
    );
    let preview = host.preview(&pkg).unwrap();
    assert_eq!(preview.id, ID);
    assert!(preview.sensitive, "tabs は警告を強調すべき権限");
    assert_eq!(preview.hosts, vec!["https://example.com/*".to_owned()]);
    assert_eq!(host.install(&pkg, true).unwrap(), ID);
    assert_eq!(events_of(&mut host), vec![LifecycleEvent::Installed]);

    // 許可される呼び出し。
    let r = call(&mut host, &mut b, ID, "runtime.getInfo", json!({}));
    assert_eq!(ok(&r)["extension_id"], ID);
    assert_eq!(ok(&r)["api_version"], 1);
    ok(&call(
        &mut host,
        &mut b,
        ID,
        "storage.set",
        json!({"items": {"n": 5}}),
    ));
    let r = call(&mut host, &mut b, ID, "storage.get", json!({"keys": ["n"]}));
    assert_eq!(ok(&r)["items"], json!({"n": 5}));
    // tabs.query は http/https のタブだけ (velox:// は見えない)。
    let r = call(&mut host, &mut b, ID, "tabs.query", json!({}));
    let urls: Vec<&str> = ok(&r)
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        urls,
        vec!["https://example.com/page", "https://other.test/"]
    );
    let r = call(&mut host, &mut b, ID, "tabs.query", json!({"active": true}));
    assert_eq!(ok(&r).as_array().unwrap().len(), 1);

    // 権限のない操作は拒否 (scripting は未承認)。
    let r = call(
        &mut host,
        &mut b,
        ID,
        "scripting.executeScript",
        json!({"tab_id": 1, "file": "inject/hello.js"}),
    );
    assert_eq!(err_code(&r), "permission_denied");
    assert!(b.injected.is_empty());

    // 無効化するとすべての呼び出しが止まる (データは残る)。
    host.disable(ID).unwrap();
    for method in ["runtime.getInfo", "storage.get", "tabs.query"] {
        let r = call(&mut host, &mut b, ID, method, json!({}));
        assert_eq!(err_code(&r), "extension_disabled", "{method}");
    }
    assert_eq!(
        events_of(&mut host),
        vec![LifecycleEvent::Disabled(DisabledReason::User)]
    );
    // 再有効化でストレージが復元される。
    host.enable(ID).unwrap();
    let r = call(&mut host, &mut b, ID, "storage.get", json!({}));
    assert_eq!(ok(&r)["items"], json!({"n": 5}));
    assert_eq!(events_of(&mut host), vec![LifecycleEvent::Enabled]);

    // アンインストールで全データが消え、以後の呼び出しは拒否される。
    let pkg_dir = host.registry().package_dir(ID);
    let storage_dir = host.registry().storage_dir(ID);
    assert!(pkg_dir.exists() && storage_dir.exists());
    assert_eq!(host.uninstall(ID), Ok(true));
    assert!(!pkg_dir.exists() && !storage_dir.exists());
    assert_eq!(events_of(&mut host), vec![LifecycleEvent::Uninstalled]);
    let r = call(&mut host, &mut b, ID, "storage.get", json!({}));
    assert_eq!(err_code(&r), "extension_disabled");
    // 再インストールしても古いデータは戻らない。
    host.install(&package("1.0.0", r#","permissions":["storage"]"#), true)
        .unwrap();
    let r = call(&mut host, &mut b, ID, "storage.get", json!({}));
    assert_eq!(ok(&r)["items"], json!({}));
}

#[test]
fn install_needs_consent_and_bad_packages_leave_nothing() {
    let root = TempDir::new("install");
    let mut host = open_host(root.path());
    let pkg = package("1.0.0", "");
    assert_eq!(
        host.install(&pkg, false),
        Err(RegistryError::ConsentRequired)
    );
    assert!(host.registry().get(ID).is_none());
    assert!(!host.registry().package_dir(ID).exists());

    // zip slip / 隠しファイルは、パッケージを組み立てる段階で丸ごと拒否される。
    for bad in [
        "../evil.js",
        "/etc/passwd",
        "a/../../b.js",
        ".git/config",
        "a\\b.js",
    ] {
        let mut p = ExtensionPackage::new();
        assert!(p.insert(bad, vec![]).is_err(), "{bad}");
    }
    // 検証に失敗する manifest (未知の権限) は何も残さない。
    let bad = package("1.0.0", r#","permissions":["cookies"]"#);
    assert!(matches!(
        host.install(&bad, true),
        Err(RegistryError::Package(_))
    ));
    assert!(host.registry().list().next().is_none());
    let staging = root.path().join("staging");
    assert!(!staging.exists() || std::fs::read_dir(staging).unwrap().next().is_none());
}

#[test]
fn install_from_directory_reads_and_validates() {
    let root = TempDir::new("fromdir-root");
    let src = TempDir::new("fromdir-src");
    std::fs::create_dir_all(src.path().join("inject")).unwrap();
    let pkg = package("2.0.0", "");
    for (rel, bytes) in pkg.files() {
        std::fs::write(src.path().join(rel), bytes).unwrap();
    }
    let from_dir = ExtensionPackage::from_dir(src.path()).unwrap();
    let mut host = open_host(root.path());
    host.install(&from_dir, true).unwrap();
    assert_eq!(host.registry().get(ID).unwrap().version, "2.0.0");
}

#[test]
fn requests_are_validated_before_anything_else() {
    let root = TempDir::new("validate");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();
    host.install(&package("1.0.0", r#","permissions":["storage"]"#), true)
        .unwrap();

    let raw = |host: &mut ExtensionHost, b: &mut FakeBrowserHost, s: &[u8]| -> Value {
        serde_json::from_str(&host.handle_request(ID, s, b)).unwrap()
    };
    assert_eq!(
        err_code(&raw(&mut host, &mut b, b"garbage")),
        "invalid_request"
    );
    assert_eq!(
        err_code(&raw(&mut host, &mut b, b"[1,2]")),
        "invalid_request"
    );
    // 自己申告の拡張機能 ID は拒否 (呼び出し元は Rust が決める)。
    let spoof = json!({"api_version":1,"id":1,"method":"storage.get","extension_id":"com.victim"});
    assert_eq!(
        err_code(&raw(&mut host, &mut b, spoof.to_string().as_bytes())),
        "invalid_request"
    );
    // 版の交渉。
    for v in [0, 2, 99] {
        let r = raw(
            &mut host,
            &mut b,
            json!({"api_version": v, "id": 1, "method": "runtime.getInfo"})
                .to_string()
                .as_bytes(),
        );
        assert_eq!(err_code(&r), "unsupported_version", "v={v}");
    }
    let r = raw(&mut host, &mut b, br#"{"id":1,"method":"runtime.getInfo"}"#);
    assert_eq!(err_code(&r), "unsupported_version");
    // 未知のメソッド・不正な params。
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "cookies.getAll", json!({}))),
        "unknown_method"
    );
    assert_eq!(
        err_code(&call(
            &mut host,
            &mut b,
            ID,
            "storage.get",
            json!({"keys": 5})
        )),
        "invalid_params"
    );
    // サイズ上限 (パース前)。
    let big = json!({"api_version":1,"id":1,"method":"storage.set","params":{"items":{"k":"x".repeat(70_000)}}});
    assert_eq!(
        err_code(&raw(&mut host, &mut b, big.to_string().as_bytes())),
        "invalid_request"
    );
    // 相関 ID は応答へ返る。
    let r = raw(
        &mut host,
        &mut b,
        json!({"api_version":1,"id":42,"method":"runtime.getInfo"})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(r["id"], 42);
    // 他の拡張機能 ID・未インストールの呼び出し元は拒否。
    let r = call(
        &mut host,
        &mut b,
        "com.not.installed",
        "runtime.getInfo",
        json!({}),
    );
    assert_eq!(err_code(&r), "extension_disabled");
}

#[test]
fn every_permissioned_method_is_denied_without_its_permission() {
    let root = TempDir::new("denied");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();
    // 何の権限も持たない拡張機能。
    host.install(&package("1.0.0", ""), true).unwrap();
    for (method, params) in [
        ("storage.get", json!({})),
        ("storage.set", json!({"items": {"a": 1}})),
        ("storage.remove", json!({"keys": ["a"]})),
        ("storage.clear", json!({})),
        ("storage.getBytesInUse", json!({})),
        ("tabs.query", json!({})),
        ("tabs.get", json!({"tab_id": 1})),
        (
            "scripting.executeScript",
            json!({"tab_id": 1, "file": "inject/hello.js"}),
        ),
        ("permissions.request", json!({"permissions": ["tabs"]})),
    ] {
        let r = call(&mut host, &mut b, ID, method, params);
        assert_eq!(err_code(&r), "permission_denied", "{method}");
    }
    // 権限不要のものは通る。
    ok(&call(&mut host, &mut b, ID, "runtime.getInfo", json!({})));
    ok(&call(
        &mut host,
        &mut b,
        ID,
        "permissions.getAll",
        json!({}),
    ));
    assert!(b.consent_requests.is_empty());
}

#[test]
fn storage_is_isolated_per_extension_and_quota_is_enforced() {
    let root = TempDir::new("storage");
    let limits = StorageLimits {
        max_key_bytes: 16,
        max_value_bytes: 64,
        max_items: 3,
        max_total_bytes: 128,
    };
    let mut host = open_host_with(root.path(), limits);
    let mut b = FakeBrowserHost::with_tabs();
    let a = "com.example.a";
    let c = "com.example.c";
    for id in [a, c] {
        host.install(
            &package_with(id, "1.0.0", r#","permissions":["storage"]"#),
            true,
        )
        .unwrap();
    }
    ok(&call(
        &mut host,
        &mut b,
        a,
        "storage.set",
        json!({"items": {"secret": "a-only"}}),
    ));
    // 別の拡張機能からは見えない。
    let r = call(&mut host, &mut b, c, "storage.get", json!({}));
    assert_eq!(ok(&r)["items"], json!({}));
    let r = call(
        &mut host,
        &mut b,
        c,
        "storage.get",
        json!({"keys": ["secret"]}),
    );
    assert_eq!(ok(&r)["items"], json!({}));
    assert_ne!(
        host.registry().storage_dir(a),
        host.registry().storage_dir(c)
    );

    // クォータ: キー長・値サイズ・件数・総量。超過しても既存データは無傷。
    let too_long_key = "k".repeat(17);
    let r = call(
        &mut host,
        &mut b,
        a,
        "storage.set",
        json!({"items": {too_long_key: 1}}),
    );
    assert_eq!(err_code(&r), "invalid_params");
    let r = call(
        &mut host,
        &mut b,
        a,
        "storage.set",
        json!({"items": {"v": "x".repeat(100)}}),
    );
    assert_eq!(err_code(&r), "quota_exceeded");
    let r = call(
        &mut host,
        &mut b,
        a,
        "storage.set",
        json!({"items": {"a": 1, "b": 2, "c": 3}}),
    );
    assert_eq!(err_code(&r), "quota_exceeded"); // secret を含め 4 件 > 3
    ok(&call(
        &mut host,
        &mut b,
        a,
        "storage.set",
        json!({"items": {"p": "x".repeat(50)}}),
    ));
    let r = call(
        &mut host,
        &mut b,
        a,
        "storage.set",
        json!({"items": {"q": "y".repeat(60)}}),
    );
    assert_eq!(err_code(&r), "quota_exceeded"); // 総量 128 バイト超
    let r = call(&mut host, &mut b, a, "storage.get", json!({}));
    assert_eq!(
        ok(&r)["items"],
        json!({"secret": "a-only", "p": "x".repeat(50)})
    );
    let r = call(&mut host, &mut b, a, "storage.getBytesInUse", json!({}));
    assert!(ok(&r)["bytes"].as_u64().unwrap() > 50);
    ok(&call(
        &mut host,
        &mut b,
        a,
        "storage.remove",
        json!({"keys": ["p"]}),
    ));
    ok(&call(&mut host, &mut b, a, "storage.clear", json!({})));

    // 更新してもストレージは保たれる。
    ok(&call(
        &mut host,
        &mut b,
        c,
        "storage.set",
        json!({"items": {"keep": 1}}),
    ));
    host.update(&package_with(c, "1.1.0", r#","permissions":["storage"]"#))
        .unwrap();
    let r = call(&mut host, &mut b, c, "storage.get", json!({}));
    assert_eq!(ok(&r)["items"], json!({"keep": 1}));
}

#[test]
fn update_with_new_permission_requires_reconsent() {
    let root = TempDir::new("update");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();
    host.install(&package("1.0.0", r#","permissions":["storage"]"#), true)
        .unwrap();
    ok(&call(
        &mut host,
        &mut b,
        ID,
        "storage.set",
        json!({"items": {"k": "v"}}),
    ));
    let _ = events_of(&mut host);

    // ダウングレード・同版は拒否 (状態は変わらない)。
    for v in ["0.9.0", "1.0.0"] {
        assert!(matches!(
            host.update(&package(v, r#","permissions":["storage"]"#)),
            Err(RegistryError::NotNewer { .. })
        ));
    }
    assert_eq!(host.registry().get(ID).unwrap().version, "1.0.0");

    // tabs を新たに要求する更新: 適用されるが無効化される。
    let out = host
        .update(&package("1.1.0", r#","permissions":["storage","tabs"]"#))
        .unwrap();
    assert!(out.needs_reconsent);
    assert_eq!(out.added_permissions, vec!["tabs".to_owned()]);
    assert_eq!(host.registry().get(ID).unwrap().version, "1.1.0");
    assert_eq!(
        host.registry().get(ID).unwrap().state,
        ExtensionState::Disabled(DisabledReason::NeedsReconsent)
    );
    assert_eq!(
        events_of(&mut host),
        vec![
            LifecycleEvent::Updated {
                from: "1.0.0".into(),
                to: "1.1.0".into()
            },
            LifecycleEvent::ConsentRequired {
                added_permissions: vec!["tabs".into()],
                added_hosts: vec![]
            },
            LifecycleEvent::Disabled(DisabledReason::NeedsReconsent),
        ]
    );
    // 再承認まで API は使えず、enable でも戻せない。
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "storage.get", json!({}))),
        "extension_disabled"
    );
    assert!(host.enable(ID).is_err());
    assert_eq!(
        events_of(&mut host),
        Vec::<LifecycleEvent>::new(),
        "失敗した enable はイベントを出さない"
    );

    // 再承認で有効化。データは保たれ、新しい権限が使える。
    host.approve(ID).unwrap();
    let r = call(&mut host, &mut b, ID, "storage.get", json!({}));
    assert_eq!(ok(&r)["items"], json!({"k": "v"}));
    ok(&call(&mut host, &mut b, ID, "tabs.query", json!({})));

    // 権限が減る更新は黙って適用され、失効した権限は即座に使えなくなる。
    let out = host
        .update(&package("1.2.0", r#","permissions":["storage"]"#))
        .unwrap();
    assert!(!out.needs_reconsent);
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "tabs.query", json!({}))),
        "permission_denied"
    );
    ok(&call(&mut host, &mut b, ID, "storage.get", json!({})));

    // 新しいホストを足す更新も再承認が必要。
    let out = host
        .update(&package(
            "1.3.0",
            r#","permissions":["storage"],"host_permissions":["https://new.example.com/*"]"#,
        ))
        .unwrap();
    assert_eq!(
        out.added_hosts,
        vec!["https://new.example.com/*".to_owned()]
    );
    assert!(!host.registry().is_enabled(ID));
}

#[test]
fn scripting_requires_permission_and_host_access_at_call_time() {
    let root = TempDir::new("scripting");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();
    host.install(
        &package(
            "1.0.0",
            r#","permissions":["scripting"],"host_permissions":["https://example.com/*"]"#,
        ),
        true,
    )
    .unwrap();
    let inject = |host: &mut ExtensionHost, b: &mut FakeBrowserHost, tab: u64| {
        call(
            host,
            b,
            ID,
            "scripting.executeScript",
            json!({"tab_id": tab, "file": "inject/hello.js"}),
        )
    };
    // ホストが一致するタブへは注入できる。
    ok(&inject(&mut host, &mut b, 1));
    assert_eq!(
        b.injected,
        vec![(ID.to_owned(), 1, "/* hello 1.0.0 */".to_owned())]
    );
    // 一致しないタブ・内部ページ・存在しないタブは拒否。
    assert_eq!(err_code(&inject(&mut host, &mut b, 2)), "permission_denied");
    assert_eq!(err_code(&inject(&mut host, &mut b, 3)), "not_found");
    assert_eq!(err_code(&inject(&mut host, &mut b, 99)), "not_found");
    // ナビゲーションで一致しなくなったら、次の呼び出しから拒否 (使用時検査)。
    b.navigate(1, "https://evil.test/");
    assert_eq!(err_code(&inject(&mut host, &mut b, 1)), "permission_denied");
    b.navigate(1, "https://example.com.evil.test/");
    assert_eq!(err_code(&inject(&mut host, &mut b, 1)), "permission_denied");
    assert_eq!(b.injected.len(), 1);
    // パッケージ外・不正なパスは拒否。
    b.navigate(1, "https://example.com/");
    for file in [
        "../manifest.json",
        "/etc/passwd",
        "inject/missing.js",
        "manifest.json",
    ] {
        let r = call(
            &mut host,
            &mut b,
            ID,
            "scripting.executeScript",
            json!({"tab_id": 1, "file": file}),
        );
        assert!(
            matches!(err_code(&r), "invalid_params" | "not_found"),
            "{file}: {r}"
        );
    }
    assert_eq!(b.injected.len(), 1);
}

#[test]
fn active_tab_grants_one_tab_until_navigation() {
    let root = TempDir::new("activetab");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();
    host.install(
        &package("1.0.0", r#","permissions":["active_tab","scripting"]"#),
        true,
    )
    .unwrap();
    let get = |host: &mut ExtensionHost, b: &mut FakeBrowserHost, tab: u64| {
        call(host, b, ID, "tabs.get", json!({"tab_id": tab}))
    };
    let exec = |host: &mut ExtensionHost, b: &mut FakeBrowserHost, tab: u64| {
        call(
            host,
            b,
            ID,
            "scripting.executeScript",
            json!({"tab_id": tab, "file": "inject/hello.js"}),
        )
    };
    // 付与前は何もできない。
    assert_eq!(err_code(&get(&mut host, &mut b, 1)), "permission_denied");
    assert_eq!(err_code(&exec(&mut host, &mut b, 1)), "permission_denied");
    // ユーザが起動したタブ 1 だけ。
    host.grant_active_tab(ID, 1, &b).unwrap();
    assert_eq!(
        ok(&get(&mut host, &mut b, 1))["url"],
        "https://example.com/page"
    );
    ok(&exec(&mut host, &mut b, 1));
    assert_eq!(err_code(&get(&mut host, &mut b, 2)), "permission_denied");
    assert_eq!(err_code(&exec(&mut host, &mut b, 2)), "permission_denied");
    // ナビゲーションで失効する。
    b.navigate(1, "https://example.com/next");
    assert_eq!(err_code(&get(&mut host, &mut b, 1)), "permission_denied");
    // 内部ページには付与できない。無効化でも失効する。
    assert!(host.grant_active_tab(ID, 3, &b).is_err());
    host.grant_active_tab(ID, 1, &b).unwrap();
    host.disable(ID).unwrap();
    host.enable(ID).unwrap();
    assert_eq!(err_code(&get(&mut host, &mut b, 1)), "permission_denied");
}

#[test]
fn optional_permissions_need_activation_consent_and_can_be_revoked() {
    let root = TempDir::new("optional");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();
    host.install(
        &package(
            "1.0.0",
            r#","permissions":["storage"],"optional_permissions":["tabs"],
               "optional_host_permissions":["https://opt.example.org/*"]"#,
        ),
        true,
    )
    .unwrap();
    let request = json!({"permissions": ["tabs"]});
    // tabs はまだ使えない。
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "tabs.query", json!({}))),
        "permission_denied"
    );
    // 宣言外は要求できない (scripting は optional に無い)。
    b.activations = 5;
    b.consent_answer = true;
    let r = call(
        &mut host,
        &mut b,
        ID,
        "permissions.request",
        json!({"permissions": ["scripting"]}),
    );
    assert_eq!(err_code(&r), "permission_denied");
    assert!(b.consent_requests.is_empty());
    // ユーザ操作が無ければ要求できない。
    b.activations = 0;
    assert_eq!(
        err_code(&call(
            &mut host,
            &mut b,
            ID,
            "permissions.request",
            request.clone()
        )),
        "permission_denied"
    );
    // ユーザが拒否 → granted:false (正常系)。
    b.activations = 1;
    b.consent_answer = false;
    let r = call(
        &mut host,
        &mut b,
        ID,
        "permissions.request",
        request.clone(),
    );
    assert_eq!(ok(&r)["granted"], false);
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "tabs.query", json!({}))),
        "permission_denied"
    );
    // ユーザが承認 → 使える。永続する。
    b.activations = 1;
    b.consent_answer = true;
    let r = call(
        &mut host,
        &mut b,
        ID,
        "permissions.request",
        request.clone(),
    );
    assert_eq!(ok(&r)["granted"], true);
    assert_eq!(b.consent_requests.last().unwrap().permissions.len(), 1);
    ok(&call(&mut host, &mut b, ID, "tabs.query", json!({})));
    let r = call(
        &mut host,
        &mut b,
        ID,
        "permissions.contains",
        request.clone(),
    );
    assert_eq!(ok(&r)["result"], true);
    drop(host);
    let mut host = open_host(root.path());
    ok(&call(&mut host, &mut b, ID, "tabs.query", json!({})));
    // すでに付与済みなら再度プロンプトしない。
    let before = b.consent_requests.len();
    let r = call(
        &mut host,
        &mut b,
        ID,
        "permissions.request",
        request.clone(),
    );
    assert_eq!(ok(&r)["granted"], true);
    assert_eq!(b.consent_requests.len(), before);
    // 取り消しは即時に効く。必須の権限は取り消せない。
    let r = call(&mut host, &mut b, ID, "permissions.remove", request);
    assert_eq!(ok(&r)["removed"], true);
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "tabs.query", json!({}))),
        "permission_denied"
    );
    let r = call(
        &mut host,
        &mut b,
        ID,
        "permissions.remove",
        json!({"permissions": ["storage"]}),
    );
    assert_eq!(ok(&r)["removed"], false);
    ok(&call(&mut host, &mut b, ID, "storage.get", json!({})));
    // 任意ホストも同様。
    b.activations = 1;
    let r = call(
        &mut host,
        &mut b,
        ID,
        "permissions.request",
        json!({"origins": ["https://opt.example.org/*"]}),
    );
    assert_eq!(ok(&r)["granted"], true);
    let r = call(&mut host, &mut b, ID, "permissions.getAll", json!({}));
    assert!(ok(&r)["origins"]
        .as_array()
        .unwrap()
        .contains(&json!("https://opt.example.org/*")));
}

#[test]
fn state_survives_restart_and_disabled_state_persists() {
    let root = TempDir::new("restart");
    let mut b = FakeBrowserHost::with_tabs();
    {
        let mut host = open_host(root.path());
        host.install(&package("1.0.0", r#","permissions":["storage"]"#), true)
            .unwrap();
        ok(&call(
            &mut host,
            &mut b,
            ID,
            "storage.set",
            json!({"items": {"x": 1}}),
        ));
        host.disable(ID).unwrap();
    }
    let mut host = open_host(root.path());
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "storage.get", json!({}))),
        "extension_disabled"
    );
    host.enable(ID).unwrap();
    assert_eq!(
        ok(&call(&mut host, &mut b, ID, "storage.get", json!({})))["items"],
        json!({"x": 1})
    );
}

#[test]
fn corrupted_storage_is_recovered_without_affecting_others() {
    let root = TempDir::new("corrupt-storage");
    let mut b = FakeBrowserHost::with_tabs();
    let (a, c) = ("com.example.a", "com.example.c");
    let dir_a;
    {
        let mut host = open_host(root.path());
        for id in [a, c] {
            host.install(
                &package_with(id, "1.0.0", r#","permissions":["storage"]"#),
                true,
            )
            .unwrap();
            ok(&call(
                &mut host,
                &mut b,
                id,
                "storage.set",
                json!({"items": {"v": id}}),
            ));
        }
        dir_a = host.registry().storage_dir(a);
    }
    std::fs::write(dir_a.join("storage.json"), b"\x00\x01 not json").unwrap();
    let mut host = open_host(root.path());
    // 壊れた側は空で続行し (退避される)、書き込みもできる。
    assert_eq!(
        ok(&call(&mut host, &mut b, a, "storage.get", json!({})))["items"],
        json!({})
    );
    assert!(dir_a.join("storage.json.corrupt").exists());
    ok(&call(
        &mut host,
        &mut b,
        a,
        "storage.set",
        json!({"items": {"fresh": true}}),
    ));
    // もう一方は無傷。
    assert_eq!(
        ok(&call(&mut host, &mut b, c, "storage.get", json!({})))["items"],
        json!({"v": c})
    );
}

#[test]
fn corrupted_registry_index_is_backed_up_and_extensions_need_reapproval() {
    let root = TempDir::new("corrupt-index");
    let mut b = FakeBrowserHost::with_tabs();
    {
        let mut host = open_host(root.path());
        host.install(&package("1.0.0", r#","permissions":["storage"]"#), true)
            .unwrap();
        ok(&call(
            &mut host,
            &mut b,
            ID,
            "storage.set",
            json!({"items": {"x": 1}}),
        ));
    }
    std::fs::write(
        root.path().join("index.json"),
        b"{\"schema_version\": 1, \"extensions\": [",
    )
    .unwrap();
    let mut host = open_host(root.path());
    let report = host.registry().recovery_report();
    assert!(report.index_corrupt);
    assert_eq!(report.rebuilt, vec![ID.to_owned()]);
    assert!(root.path().join("index.json.corrupt").exists());
    // 起動は止まらず、承認は推測で復元されない (無効・要再承認)。
    assert_eq!(
        err_code(&call(&mut host, &mut b, ID, "storage.get", json!({}))),
        "extension_disabled"
    );
    assert!(host.enable(ID).is_err());
    host.approve(ID).unwrap();
    // ストレージは残っている。
    assert_eq!(
        ok(&call(&mut host, &mut b, ID, "storage.get", json!({})))["items"],
        json!({"x": 1})
    );
    // 復旧後の索引は正常で、次回起動では何も復旧しない。
    drop(host);
    let host = open_host(root.path());
    assert!(!host.registry().recovery_report().index_corrupt);
    assert!(host.registry().is_enabled(ID));
}

#[test]
fn corrupted_package_quarantines_only_that_extension() {
    let root = TempDir::new("corrupt-pkg");
    let mut b = FakeBrowserHost::with_tabs();
    let (a, c) = ("com.example.a", "com.example.c");
    {
        let mut host = open_host(root.path());
        for id in [a, c] {
            host.install(
                &package_with(id, "1.0.0", r#","permissions":["storage"]"#),
                true,
            )
            .unwrap();
        }
    }
    std::fs::remove_file(root.path().join("packages").join(a).join("bg.js")).unwrap();
    let mut host = open_host(root.path());
    assert_eq!(
        host.registry().get(a).unwrap().state,
        ExtensionState::Disabled(DisabledReason::Corrupt)
    );
    assert!(events_of(&mut host)
        .iter()
        .any(|e| matches!(e, LifecycleEvent::Quarantined { .. })));
    ok(&call(&mut host, &mut b, c, "storage.get", json!({})));
    assert_eq!(
        err_code(&call(&mut host, &mut b, a, "storage.get", json!({}))),
        "extension_disabled"
    );
    assert!(host.enable(a).is_err());
    // 更新 (再インストール) で修復でき、その後は明示的に有効化する。
    host.update(&package_with(a, "1.0.1", r#","permissions":["storage"]"#))
        .unwrap();
    host.enable(a).unwrap();
    ok(&call(&mut host, &mut b, a, "storage.get", json!({})));
}

#[test]
fn crash_leftovers_do_not_break_startup_or_leak_into_installs() {
    let root = TempDir::new("crash");
    {
        let mut host = open_host(root.path());
        host.install(&package("1.0.0", ""), true).unwrap();
    }
    // クラッシュの残骸: 作業領域、索引にないパッケージ、書きかけの索引一時ファイル。
    std::fs::create_dir_all(root.path().join("staging/com.example.x.7/inject")).unwrap();
    std::fs::create_dir_all(root.path().join("packages/com.example.ghost")).unwrap();
    std::fs::write(root.path().join("index.json.tmp"), b"half").unwrap();
    let host = open_host(root.path());
    let report = host.registry().recovery_report();
    assert_eq!(report.removed_orphans, vec!["com.example.ghost".to_owned()]);
    assert_eq!(report.staging_cleaned, 1);
    assert!(host.registry().is_enabled(ID));
    assert!(!root.path().join("packages/com.example.ghost").exists());
}

#[test]
fn schema_and_method_table_agree() {
    use velox::browser::extensions::api::{schema_json, API_VERSION, METHODS};
    let s = schema_json();
    assert_eq!(s["api_version"], API_VERSION);
    assert_eq!(s["methods"].as_array().unwrap().len(), METHODS.len());
    // 権限を要するメソッドは、その権限が無い拡張機能では必ず拒否される
    // (`every_permissioned_method_is_denied_without_its_permission` と対)。
    let root = TempDir::new("schema");
    let mut host = open_host(root.path());
    let mut b = FakeBrowserHost::with_tabs();
    host.install(&package("1.0.0", ""), true).unwrap();
    for m in METHODS.iter().filter(|m| m.permission.is_some()) {
        let params = match m.name {
            "storage.set" => json!({"items": {"a": 1}}),
            "storage.remove" => json!({"keys": ["a"]}),
            "scripting.executeScript" => json!({"tab_id": 1, "file": "inject/hello.js"}),
            _ => json!({}),
        };
        let r = call(&mut host, &mut b, ID, m.name, params);
        assert_eq!(err_code(&r), "permission_denied", "{}", m.name);
    }
}
