//! 拡張機能マニフェスト (`manifest.json`) のスキーマ定義と検証 (Issue #82)。
//!
//! **これは #83 (Extension API) / #84 (Lifecycle / Storage) の土台であり、
//! まだアプリの実行経路には配線されていない。** どの拡張機能も読み込まれず、
//! このモジュールを呼ぶコードは (テストを除き) 存在しない。挙動は変わらない。
//!
//! 設計の全体像・脅威モデル・各制限値の根拠は `docs/extensions.md` と
//! docs/decisions.md D159 を参照。このファイルは、その文書が定めるスキーマの
//! 「機械が検査できる版」である。
//!
//! ## 方針
//!
//! - マニフェストは **悪意ある拡張機能が書いたもの** として扱う。したがって
//!   検証は「許可リスト方式」: 未知のフィールド・未知の権限・未知の
//!   `manifest_version` はすべて拒否する (`deny_unknown_fields`)。
//! - 他の `browser::` モジュールと同じく純粋なロジックのみ (IO・`wry` なし)。
//!   ファイルの読み出しと、`content_scripts.js` などが指すファイルの実在確認は
//!   呼び出し側 (#84) の責務。ここでは **パス文字列の形** だけを検査する。
//! - 上限値は定数として公開し、テストとドキュメントから同じ値を参照する。
//! - `serde_json` は同一キーの重複を後勝ちで受け入れる (エラーにしない)。
//!   これは既知の制約で、検証は「解釈後の値」に対して行う。

use std::fmt;

use serde::Deserialize;

/// このバージョンの VeloX が理解する `manifest_version`。
pub const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// `manifest.json` の最大バイト数 (悪意ある巨大入力で解析コストを払わない)。
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// `id` の長さ (バイト) の範囲。
pub const MIN_ID_LEN: usize = 3;
pub const MAX_ID_LEN: usize = 64;
/// 予約済み `id` 接頭辞。ファーストパーティ拡張機能のなりすましを防ぐ。
pub const RESERVED_ID_PREFIX: &str = "velox.";
/// `name` の最大文字数 (権限プロンプトに 1 行で収まる長さ)。
pub const MAX_NAME_CHARS: usize = 45;
/// `description` の最大文字数。
pub const MAX_DESCRIPTION_CHARS: usize = 132;
/// ホストパターン 1 本の最大バイト数。
pub const MAX_PATTERN_LEN: usize = 512;
/// パターン内パス部の最大バイト数。
pub const MAX_PATH_LEN: usize = 256;
/// `host_permissions` / `optional_host_permissions` それぞれの最大件数。
pub const MAX_HOST_PATTERNS: usize = 50;
/// `content_scripts` の最大件数。
pub const MAX_CONTENT_SCRIPTS: usize = 20;
/// 1 つの `content_scripts` エントリ内の `matches` / `exclude_matches` /
/// `js` それぞれの最大件数。
pub const MAX_ENTRIES_PER_SCRIPT: usize = 20;
/// 拡張機能パッケージ内の相対パスの最大バイト数。
pub const MAX_RESOURCE_PATH_LEN: usize = 128;

/// 拡張機能が要求できる権限 (閉じた列挙)。
///
/// **ここに無い名前は、どれだけ無害に見えてもマニフェストごと拒否する。**
/// 権限を足すのは、脅威モデルの更新と Decision の追記を伴うスキーマ変更
/// であり、文字列を足すだけでは済ませない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    /// 拡張機能ごとに分離された小さなキーバリューストレージ (#84)。
    Storage,
    /// 一定間隔での起床 (最小間隔は実行時に強制する)。
    Alarms,
    /// **全タブの URL・タイトルの閲覧**。ホスト権限とは独立に閲覧履歴に
    /// 近い情報を晒すため高リスク扱い。
    Tabs,
    /// ユーザがその拡張機能を明示的に起動したタブ 1 枚に、一時的にホスト権限を
    /// 与える。常時のホスト権限の代替として推奨する最小権限の入口。
    ActiveTab,
    /// 実行時のスクリプト注入。ホスト権限 (または `active_tab`) と併用。
    Scripting,
    /// コンテキストメニュー項目の追加 (D78 のメニューへ)。
    ContextMenus,
    /// クリップボードへの書き込みのみ。読み取りは提供しない。
    ClipboardWrite,
    /// OS 通知 (表示は VeloX が拡張機能名を付けて装飾する)。
    Notifications,
}

impl Permission {
    /// スキーマ上の全権限 (プロンプト UI・ドキュメント生成・網羅テスト用)。
    pub const ALL: [Permission; 8] = [
        Permission::Storage,
        Permission::Alarms,
        Permission::Tabs,
        Permission::ActiveTab,
        Permission::Scripting,
        Permission::ContextMenus,
        Permission::ClipboardWrite,
        Permission::Notifications,
    ];

    /// マニフェスト上の名前 (snake_case)。
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Storage => "storage",
            Permission::Alarms => "alarms",
            Permission::Tabs => "tabs",
            Permission::ActiveTab => "active_tab",
            Permission::Scripting => "scripting",
            Permission::ContextMenus => "context_menus",
            Permission::ClipboardWrite => "clipboard_write",
            Permission::Notifications => "notifications",
        }
    }

    /// マニフェスト上の名前から権限を引く。未知の名前は `None`。
    pub fn from_name(name: &str) -> Option<Permission> {
        Permission::ALL.into_iter().find(|p| p.as_str() == name)
    }

    /// インストール時のプロンプトで警告を強調すべき権限か。
    pub fn is_sensitive(self) -> bool {
        matches!(
            self,
            Permission::Tabs | Permission::Scripting | Permission::ClipboardWrite
        )
    }
}

/// 「当面提供しない」と決めている権限名。
///
/// 単なる未知の名前と区別して、開発者に「タイプミスではなく意図的に無い」と
/// 伝えるためだけに持つ (どちらでも拒否は同じ)。理由は docs/extensions.md。
pub const RESERVED_PERMISSIONS: [&str; 12] = [
    "cookies",
    "web_request",
    "webrequest",
    "history",
    "bookmarks",
    "native_messaging",
    "nativemessaging",
    "debugger",
    "management",
    "proxy",
    "downloads",
    "declarative_net_request",
];

/// `MAJOR.MINOR.PATCH[-prerelease]` 形式のバージョン。
///
/// 各数値は先頭ゼロ無しの 9 桁以内の 10 進数。順序は数値 → プレリリース
/// (無し > 有り、有り同士は辞書順) で、更新判定 (#84) に使える。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub pre: Option<String>,
}

impl ExtensionVersion {
    /// 文字列から解析する。
    pub fn parse(s: &str) -> Option<ExtensionVersion> {
        if s.len() > 64 {
            return None;
        }
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        if let Some(pre) = pre {
            let ok = !pre.is_empty()
                && pre
                    .split('.')
                    .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()));
            if !ok {
                return None;
            }
        }
        let mut parts = core.split('.');
        let major = parse_version_number(parts.next()?)?;
        let minor = parse_version_number(parts.next()?)?;
        let patch = parse_version_number(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(ExtensionVersion {
            major,
            minor,
            patch,
            pre: pre.map(str::to_owned),
        })
    }
}

fn parse_version_number(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

impl PartialOrd for ExtensionVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExtensionVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl fmt::Display for ExtensionVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

/// ホストパターンのスキーム部。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeMatch {
    Http,
    Https,
    /// `*`。**`http` と `https` のみ**を指す (`file` / `data` / `velox` 等は
    /// 決して含まれない)。
    HttpOrHttps,
}

/// ホストパターンのホスト部。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMatch {
    /// `*` — 全ホスト (要 `allow_all_urls`)。
    Any,
    /// `*.example.com` — `example.com` 自身とそのサブドメイン。
    Subdomains(String),
    /// `example.com` — 完全一致。
    Exact(String),
}

/// `scheme://host[:port]/path` 形式のホストパターン (`<all_urls>` を含む)。
///
/// 一致の意味は `docs/extensions.md` の「ホストパターン」節に定義してある。
/// ポート未指定は **そのスキームの既定ポートのみ** を意味し、任意ポートには
/// ならない (`http://localhost/*` が開発サーバの `:3000` に効いてしまわない
/// ように)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPattern {
    pub scheme: SchemeMatch,
    pub host: HostMatch,
    pub port: Option<u16>,
    /// `/` で始まる。`*` は任意長 (空を含む) の文字列に一致する。
    pub path: String,
}

/// 正規形の文字列 (`parse` に戻すと同じパターンになる)。承認済みの集合を
/// 永続化・比較する (#84) ために使う。`<all_urls>` は `*://*/*` と書く。
impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = match self.scheme {
            SchemeMatch::Http => "http",
            SchemeMatch::Https => "https",
            SchemeMatch::HttpOrHttps => "*",
        };
        write!(f, "{scheme}://")?;
        match &self.host {
            HostMatch::Any => write!(f, "*")?,
            HostMatch::Subdomains(d) => write!(f, "*.{d}")?,
            HostMatch::Exact(h) => write!(f, "{h}")?,
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        write!(f, "{}", self.path)
    }
}

impl HostPattern {
    /// `<all_urls>` と同値のパターン。
    pub fn all_urls() -> HostPattern {
        HostPattern {
            scheme: SchemeMatch::HttpOrHttps,
            host: HostMatch::Any,
            port: None,
            path: "/*".to_owned(),
        }
    }

    /// 全ホストに効く広域パターンか (`allow_all_urls` が必要な範囲)。
    pub fn is_all_hosts(&self) -> bool {
        self.host == HostMatch::Any
    }

    /// パターン文字列を解析・検証する。
    pub fn parse(s: &str) -> Result<HostPattern, PatternError> {
        if s.len() > MAX_PATTERN_LEN {
            return Err(PatternError::TooLong);
        }
        if s == "<all_urls>" {
            return Ok(HostPattern::all_urls());
        }
        let (scheme, rest) = s.split_once("://").ok_or(PatternError::MissingScheme)?;
        let scheme = match scheme {
            "http" => SchemeMatch::Http,
            "https" => SchemeMatch::Https,
            "*" => SchemeMatch::HttpOrHttps,
            other => return Err(PatternError::UnsupportedScheme(other.to_owned())),
        };
        let slash = rest.find('/').ok_or(PatternError::MissingPath)?;
        let (hostport, path) = rest.split_at(slash);
        if path.len() > MAX_PATH_LEN {
            return Err(PatternError::TooLong);
        }
        if path.bytes().any(|b| b <= 0x20 || b == 0x7f || b == b'#') {
            return Err(PatternError::InvalidPath);
        }
        if hostport.contains('@') {
            return Err(PatternError::UserInfo);
        }
        let (host_str, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => {
                let digits = !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
                let port = p
                    .parse::<u32>()
                    .ok()
                    .filter(|n| digits && (1..=65535).contains(n));
                match port {
                    Some(n) => (h, Some(n as u16)),
                    None => return Err(PatternError::InvalidPort),
                }
            }
            None => (hostport, None),
        };
        let host = if host_str == "*" {
            HostMatch::Any
        } else if let Some(domain) = host_str.strip_prefix("*.") {
            // `*.com` のような公開サフィックス丸ごとの指定を避けるため、
            // ドメイン部にはドット (2 ラベル以上) を要求する。
            if !domain.contains('.') {
                return Err(PatternError::WildcardTooBroad);
            }
            validate_domain(domain)?;
            HostMatch::Subdomains(domain.to_owned())
        } else {
            validate_domain(host_str)?;
            HostMatch::Exact(host_str.to_owned())
        };
        Ok(HostPattern {
            scheme,
            host,
            port,
            path: path.to_owned(),
        })
    }

    /// `url` がこのパターンに一致するか。`http` / `https` 以外の URL は
    /// パターンが何であっても一致しない。ユーザ名・パスワードを含む URL も
    /// 一致しない (オリジンの誤認を避ける)。
    pub fn matches(&self, url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        let scheme_ok = matches!(
            (self.scheme, parsed.scheme()),
            (SchemeMatch::Http, "http")
                | (SchemeMatch::Https, "https")
                | (SchemeMatch::HttpOrHttps, "http" | "https")
        );
        if !scheme_ok || !parsed.username().is_empty() || parsed.password().is_some() {
            return false;
        }
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host_ok = match &self.host {
            HostMatch::Any => true,
            HostMatch::Exact(h) => host == h,
            HostMatch::Subdomains(d) => {
                host == d
                    || host
                        .strip_suffix(d.as_str())
                        .is_some_and(|p| p.ends_with('.'))
            }
        };
        if !host_ok {
            return false;
        }
        let default_port = if parsed.scheme() == "https" { 443 } else { 80 };
        if parsed.port_or_known_default() != Some(self.port.unwrap_or(default_port)) {
            return false;
        }
        let mut target = parsed.path().to_owned();
        if let Some(q) = parsed.query() {
            target.push('?');
            target.push_str(q);
        }
        glob_match(&self.path, &target)
    }
}

/// `*` のみをワイルドカード (任意長・空可) とする単純なグロブ。
fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&b| b == b'*')
}

/// 小文字の ASCII ドメイン名 (IDN は punycode 済みであること)。
fn validate_domain(host: &str) -> Result<(), PatternError> {
    if host.is_empty() || host.len() > 253 {
        return Err(PatternError::InvalidHost);
    }
    for label in host.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err(PatternError::InvalidHost);
        }
    }
    Ok(())
}

/// [`HostPattern::parse`] の失敗理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    TooLong,
    MissingScheme,
    UnsupportedScheme(String),
    MissingPath,
    InvalidPath,
    UserInfo,
    InvalidPort,
    InvalidHost,
    WildcardTooBroad,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternError::TooLong => write!(f, "パターンが長すぎる"),
            PatternError::MissingScheme => write!(f, "スキームが無い (`https://...` の形)"),
            PatternError::UnsupportedScheme(s) => {
                write!(f, "未対応のスキーム `{s}` (http / https / * のみ)")
            }
            PatternError::MissingPath => write!(f, "パス部が無い (`/*` などが必要)"),
            PatternError::InvalidPath => write!(f, "パス部に空白・制御文字・`#` は使えない"),
            PatternError::UserInfo => write!(f, "ユーザ情報 (`user@`) は使えない"),
            PatternError::InvalidPort => write!(f, "ポートは 1〜65535 の数字のみ"),
            PatternError::InvalidHost => write!(
                f,
                "ホスト名が不正 (小文字 ASCII のドメイン名のみ。IDN は punycode)"
            ),
            PatternError::WildcardTooBroad => {
                write!(f, "`*.com` のような広すぎるワイルドカードは使えない")
            }
        }
    }
}

/// `content_scripts` の実行タイミング。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAt {
    DocumentStart,
    DocumentEnd,
    DocumentIdle,
}

/// 検証済みの `content_scripts` エントリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentScript {
    pub matches: Vec<HostPattern>,
    pub exclude_matches: Vec<HostPattern>,
    /// パッケージ内の相対パス (形のみ検証済み)。
    pub js: Vec<String>,
    pub run_at: RunAt,
    /// `false` (既定) ならトップフレームのみ。`true` はサブフレームにも注入する。
    pub all_frames: bool,
}

/// `background` (イベントページ)。常駐 (persistent) は提供しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Background {
    pub script: String,
}

/// 検証済みマニフェスト。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: ExtensionVersion,
    pub description: Option<String>,
    pub min_velox_version: Option<ExtensionVersion>,
    pub permissions: Vec<Permission>,
    pub optional_permissions: Vec<Permission>,
    pub host_permissions: Vec<HostPattern>,
    pub optional_host_permissions: Vec<HostPattern>,
    pub content_scripts: Vec<ContentScript>,
    pub background: Option<Background>,
    pub allow_all_urls: bool,
}

impl Manifest {
    /// インストール時にユーザへ提示すべき「必須のホストアクセス」。
    /// `host_permissions` に加え、`content_scripts.matches` も**ホストアクセス
    /// として数える** (スクリプトが走る = そのページの内容を読み書きできる、
    /// ため)。`exclude_matches` は含まない。
    pub fn required_host_patterns(&self) -> Vec<&HostPattern> {
        self.host_permissions
            .iter()
            .chain(self.content_scripts.iter().flat_map(|c| c.matches.iter()))
            .collect()
    }
}

/// 検証失敗の理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    TooLarge(usize),
    /// JSON として解釈できない、未知のフィールドがある、型が違う、など。
    Json(String),
    UnsupportedManifestVersion(u32),
    InvalidId(String),
    InvalidName,
    InvalidDescription,
    InvalidVersion(String),
    UnknownPermission(String),
    /// 意図的に提供しない権限名。
    ReservedPermission(String),
    DuplicatePermission(String),
    /// 必須と任意の両方に同じ権限/パターンがある。
    OverlappingOptional(String),
    InvalidPattern {
        pattern: String,
        reason: PatternError,
    },
    TooManyEntries(&'static str),
    /// 全ホスト対象のパターンがあるのに `allow_all_urls` が真でない。
    AllUrlsNotAllowed(String),
    /// `scripting` にホスト権限も `active_tab` も無い。
    ScriptingWithoutHosts,
    EmptyContentScript,
    InvalidResourcePath(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::TooLarge(n) => write!(
                f,
                "manifest が大きすぎる ({n} バイト > {MAX_MANIFEST_BYTES})"
            ),
            ManifestError::Json(e) => write!(f, "manifest を解釈できない: {e}"),
            ManifestError::UnsupportedManifestVersion(v) => write!(
                f,
                "未対応の manifest_version {v} (対応: {SUPPORTED_MANIFEST_VERSION})"
            ),
            ManifestError::InvalidId(id) => write!(f, "id `{id}` が不正"),
            ManifestError::InvalidName => write!(f, "name が不正 (空・長すぎ・制御文字)"),
            ManifestError::InvalidDescription => write!(f, "description が不正"),
            ManifestError::InvalidVersion(v) => {
                write!(f, "version `{v}` が不正 (MAJOR.MINOR.PATCH[-pre])")
            }
            ManifestError::UnknownPermission(p) => write!(f, "未知の権限 `{p}`"),
            ManifestError::ReservedPermission(p) => {
                write!(f, "権限 `{p}` はこのバージョンでは提供しない")
            }
            ManifestError::DuplicatePermission(p) => write!(f, "`{p}` が重複している"),
            ManifestError::OverlappingOptional(p) => {
                write!(f, "`{p}` が必須と任意の両方に書かれている")
            }
            ManifestError::InvalidPattern { pattern, reason } => {
                write!(f, "パターン `{pattern}` が不正: {reason}")
            }
            ManifestError::TooManyEntries(what) => write!(f, "{what} の件数が上限を超えている"),
            ManifestError::AllUrlsNotAllowed(p) => write!(
                f,
                "`{p}` は全ホスト対象。`allow_all_urls: true` を明示する必要がある"
            ),
            ManifestError::ScriptingWithoutHosts => {
                write!(f, "`scripting` にはホスト権限 (または `active_tab`) が必要")
            }
            ManifestError::EmptyContentScript => {
                write!(f, "content_scripts のエントリに matches / js が無い")
            }
            ManifestError::InvalidResourcePath(p) => {
                write!(f, "リソースパス `{p}` が不正 (相対・`.js`・`..` 不可)")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

// --- 生の (未検証) 形。deny_unknown_fields で未知キーを弾く。 ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    manifest_version: u32,
    id: String,
    name: String,
    version: String,
    description: Option<String>,
    min_velox_version: Option<String>,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    optional_permissions: Vec<String>,
    #[serde(default)]
    host_permissions: Vec<String>,
    #[serde(default)]
    optional_host_permissions: Vec<String>,
    #[serde(default)]
    content_scripts: Vec<RawContentScript>,
    background: Option<RawBackground>,
    #[serde(default)]
    allow_all_urls: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContentScript {
    matches: Vec<String>,
    #[serde(default)]
    exclude_matches: Vec<String>,
    js: Vec<String>,
    run_at: Option<RunAt>,
    #[serde(default)]
    all_frames: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackground {
    script: String,
}

/// `manifest.json` のバイト列を解析して検証する。
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge(bytes.len()));
    }
    let raw: RawManifest =
        serde_json::from_slice(bytes).map_err(|e| ManifestError::Json(e.to_string()))?;
    validate(raw)
}

fn validate(raw: RawManifest) -> Result<Manifest, ManifestError> {
    if raw.manifest_version != SUPPORTED_MANIFEST_VERSION {
        return Err(ManifestError::UnsupportedManifestVersion(
            raw.manifest_version,
        ));
    }
    validate_id(&raw.id)?;
    if !is_display_text(&raw.name, MAX_NAME_CHARS) {
        return Err(ManifestError::InvalidName);
    }
    if let Some(d) = &raw.description {
        if !is_display_text(d, MAX_DESCRIPTION_CHARS) {
            return Err(ManifestError::InvalidDescription);
        }
    }
    let version = ExtensionVersion::parse(&raw.version)
        .ok_or_else(|| ManifestError::InvalidVersion(raw.version.chars().take(64).collect()))?;
    let min_velox_version = match &raw.min_velox_version {
        Some(v) => Some(
            ExtensionVersion::parse(v)
                .ok_or_else(|| ManifestError::InvalidVersion(v.chars().take(64).collect()))?,
        ),
        None => None,
    };

    let permissions = parse_permissions(&raw.permissions)?;
    let optional_permissions = parse_permissions(&raw.optional_permissions)?;
    if let Some(p) = optional_permissions
        .iter()
        .find(|p| permissions.contains(p))
    {
        return Err(ManifestError::OverlappingOptional(p.as_str().to_owned()));
    }

    let host_permissions = parse_patterns(&raw.host_permissions, "host_permissions")?;
    let optional_host_permissions =
        parse_patterns(&raw.optional_host_permissions, "optional_host_permissions")?;
    if let Some(p) = raw
        .optional_host_permissions
        .iter()
        .find(|p| raw.host_permissions.contains(p))
    {
        return Err(ManifestError::OverlappingOptional(
            p.chars().take(80).collect(),
        ));
    }

    if raw.content_scripts.len() > MAX_CONTENT_SCRIPTS {
        return Err(ManifestError::TooManyEntries("content_scripts"));
    }
    let mut content_scripts = Vec::with_capacity(raw.content_scripts.len());
    for cs in &raw.content_scripts {
        if cs.matches.is_empty() || cs.js.is_empty() {
            return Err(ManifestError::EmptyContentScript);
        }
        if cs.js.len() > MAX_ENTRIES_PER_SCRIPT {
            return Err(ManifestError::TooManyEntries("content_scripts.js"));
        }
        for path in &cs.js {
            validate_resource_path(path)?;
        }
        content_scripts.push(ContentScript {
            matches: parse_patterns_limited(&cs.matches, MAX_ENTRIES_PER_SCRIPT, "matches")?,
            exclude_matches: parse_patterns_limited(
                &cs.exclude_matches,
                MAX_ENTRIES_PER_SCRIPT,
                "exclude_matches",
            )?,
            js: cs.js.clone(),
            run_at: cs.run_at.unwrap_or(RunAt::DocumentIdle),
            all_frames: cs.all_frames,
        });
    }

    let background = match raw.background {
        Some(b) => {
            validate_resource_path(&b.script)?;
            Some(Background { script: b.script })
        }
        None => None,
    };

    let manifest = Manifest {
        manifest_version: raw.manifest_version,
        id: raw.id,
        name: raw.name,
        version,
        description: raw.description,
        min_velox_version,
        permissions,
        optional_permissions,
        host_permissions,
        optional_host_permissions,
        content_scripts,
        background,
        allow_all_urls: raw.allow_all_urls,
    };

    // 広域アクセスは明示フラグ必須。required / optional / content script の
    // 「実行される側」(matches) のすべてを見る。exclude は権限を増やさない。
    if !manifest.allow_all_urls {
        let broad = manifest
            .host_permissions
            .iter()
            .chain(&manifest.optional_host_permissions)
            .chain(manifest.content_scripts.iter().flat_map(|c| &c.matches))
            .find(|p| p.is_all_hosts());
        if let Some(p) = broad {
            return Err(ManifestError::AllUrlsNotAllowed(describe_broad(p)));
        }
    }

    let has_hosts = !manifest.host_permissions.is_empty()
        || !manifest.optional_host_permissions.is_empty()
        || manifest.permissions.contains(&Permission::ActiveTab)
        || manifest
            .optional_permissions
            .contains(&Permission::ActiveTab);
    let wants_scripting = manifest.permissions.contains(&Permission::Scripting)
        || manifest
            .optional_permissions
            .contains(&Permission::Scripting);
    if wants_scripting && !has_hosts {
        return Err(ManifestError::ScriptingWithoutHosts);
    }
    Ok(manifest)
}

/// エラーメッセージ用に、広域パターンを文字列へ戻す。
fn describe_broad(p: &HostPattern) -> String {
    if *p == HostPattern::all_urls() {
        return "<all_urls>".to_owned();
    }
    let scheme = match p.scheme {
        SchemeMatch::Http => "http",
        SchemeMatch::Https => "https",
        SchemeMatch::HttpOrHttps => "*",
    };
    match p.port {
        Some(port) => format!("{scheme}://*:{port}{}", p.path),
        None => format!("{scheme}://*{}", p.path),
    }
}

fn validate_id(id: &str) -> Result<(), ManifestError> {
    let bad = || ManifestError::InvalidId(id.chars().take(MAX_ID_LEN).collect());
    if id.len() < MIN_ID_LEN || id.len() > MAX_ID_LEN {
        return Err(bad());
    }
    let bytes = id.as_bytes();
    let charset_ok = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'));
    let edge_ok = bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !id.contains("..");
    if !charset_ok || !edge_ok || id.starts_with(RESERVED_ID_PREFIX) {
        return Err(bad());
    }
    Ok(())
}

/// 権限プロンプトなどにそのまま表示する文字列の検査。空白のみ・制御文字・
/// 双方向制御文字 (表示の入れ替えによるなりすまし)・ゼロ幅文字を拒否する。
fn is_display_text(s: &str, max_chars: usize) -> bool {
    !s.trim().is_empty()
        && s.trim() == s
        && s.chars().count() <= max_chars
        && !s.chars().any(|c| {
            c.is_control()
                || matches!(
                    c,
                    '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{200B}'..='\u{200F}'
                        | '\u{FEFF}'
                )
        })
}

fn parse_permissions(names: &[String]) -> Result<Vec<Permission>, ManifestError> {
    if names.len() > Permission::ALL.len() * 2 {
        return Err(ManifestError::TooManyEntries("permissions"));
    }
    let mut out: Vec<Permission> = Vec::with_capacity(names.len());
    for name in names {
        let perm = match Permission::from_name(name) {
            Some(p) => p,
            None if RESERVED_PERMISSIONS.contains(&name.as_str()) => {
                return Err(ManifestError::ReservedPermission(name.clone()))
            }
            None => {
                let shown: String = name.chars().take(64).collect();
                return Err(ManifestError::UnknownPermission(shown));
            }
        };
        if out.contains(&perm) {
            return Err(ManifestError::DuplicatePermission(name.clone()));
        }
        out.push(perm);
    }
    Ok(out)
}

fn parse_patterns(list: &[String], what: &'static str) -> Result<Vec<HostPattern>, ManifestError> {
    parse_patterns_limited(list, MAX_HOST_PATTERNS, what)
}

fn parse_patterns_limited(
    list: &[String],
    max: usize,
    what: &'static str,
) -> Result<Vec<HostPattern>, ManifestError> {
    if list.len() > max {
        return Err(ManifestError::TooManyEntries(what));
    }
    let mut out = Vec::with_capacity(list.len());
    for (i, s) in list.iter().enumerate() {
        if list[..i].contains(s) {
            return Err(ManifestError::DuplicatePermission(
                s.chars().take(80).collect(),
            ));
        }
        let pattern = HostPattern::parse(s).map_err(|reason| ManifestError::InvalidPattern {
            pattern: s.chars().take(80).collect(),
            reason,
        })?;
        out.push(pattern);
    }
    Ok(out)
}

/// パッケージ内リソース (JS) の相対パスの形を検査する。実在確認は行わない。
pub fn validate_resource_path(path: &str) -> Result<(), ManifestError> {
    let bad = || ManifestError::InvalidResourcePath(path.chars().take(64).collect());
    if path.is_empty() || path.len() > MAX_RESOURCE_PATH_LEN || !path.ends_with(".js") {
        return Err(bad());
    }
    let chars_ok = path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_'));
    let segments_ok = path
        .split('/')
        .all(|seg| !seg.is_empty() && !seg.starts_with('.'));
    if !chars_ok || !segments_ok {
        return Err(bad());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_pattern_display_round_trips_through_parse() {
        for raw in [
            "<all_urls>",
            "https://*.example.com/*",
            "http://localhost:3000/a/*",
            "*://example.org/docs/*",
            "https://127.0.0.1/",
        ] {
            let p = HostPattern::parse(raw).expect(raw);
            let again = HostPattern::parse(&p.to_string()).expect("canonical form parses");
            assert_eq!(p, again, "{raw}");
        }
        assert_eq!(HostPattern::all_urls().to_string(), "*://*/*");
    }

    const MINIMAL: &str = r#"{
        "manifest_version": 1,
        "id": "com.example.hello",
        "name": "Hello",
        "version": "1.0.0"
    }"#;

    fn parse(json: &str) -> Result<Manifest, ManifestError> {
        parse_manifest(json.as_bytes())
    }

    /// 必須フィールドに追加フィールドを足した JSON を作る。
    fn with(extra: &str) -> String {
        format!(
            r#"{{"manifest_version":1,"id":"com.example.hello","name":"Hello","version":"1.0.0",{extra}}}"#
        )
    }

    // --- 基本 ---

    #[test]
    fn minimal_manifest_has_no_privileges() {
        let m = parse(MINIMAL).unwrap();
        assert_eq!(m.id, "com.example.hello");
        assert!(m.permissions.is_empty());
        assert!(m.host_permissions.is_empty());
        assert!(m.content_scripts.is_empty());
        assert!(m.background.is_none());
        assert!(!m.allow_all_urls);
        assert!(m.required_host_patterns().is_empty());
    }

    #[test]
    fn full_manifest_parses() {
        let json = r#"{
            "manifest_version": 1,
            "id": "org.example.tool_1",
            "name": "Tool",
            "version": "2.10.3-beta.1",
            "description": "説明",
            "min_velox_version": "0.5.0",
            "permissions": ["storage", "active_tab"],
            "optional_permissions": ["tabs"],
            "host_permissions": ["https://*.example.com/*"],
            "optional_host_permissions": ["https://example.org/docs/*"],
            "content_scripts": [{
                "matches": ["https://example.com/*"],
                "exclude_matches": ["https://example.com/private/*"],
                "js": ["content/main.js"],
                "run_at": "document_start",
                "all_frames": true
            }],
            "background": {"script": "bg.js"}
        }"#;
        let m = parse(json).unwrap();
        assert_eq!(m.version.to_string(), "2.10.3-beta.1");
        assert_eq!(
            m.permissions,
            vec![Permission::Storage, Permission::ActiveTab]
        );
        assert_eq!(m.optional_permissions, vec![Permission::Tabs]);
        assert_eq!(m.content_scripts[0].run_at, RunAt::DocumentStart);
        assert!(m.content_scripts[0].all_frames);
        // host_permissions 1 + content_scripts.matches 1 (exclude は数えない)
        assert_eq!(m.required_host_patterns().len(), 2);
        assert_eq!(m.background.unwrap().script, "bg.js");
    }

    #[test]
    fn content_script_defaults_are_conservative() {
        let m = parse(&with(
            r#""content_scripts":[{"matches":["https://a.example/*"],"js":["a.js"]}]"#,
        ))
        .unwrap();
        let cs = &m.content_scripts[0];
        assert_eq!(cs.run_at, RunAt::DocumentIdle);
        assert!(!cs.all_frames, "サブフレーム注入は明示オプトイン");
    }

    // --- JSON / 必須フィールド / 未知フィールド ---

    #[test]
    fn rejects_non_json_and_wrong_types() {
        assert!(matches!(parse("not json"), Err(ManifestError::Json(_))));
        assert!(matches!(parse("[]"), Err(ManifestError::Json(_))));
        assert!(matches!(
            parse(r#"{"manifest_version":"1","id":"abc","name":"n","version":"1.0.0"}"#),
            Err(ManifestError::Json(_))
        ));
    }

    #[test]
    fn rejects_missing_required_fields() {
        for missing in ["manifest_version", "id", "name", "version"] {
            let mut v: serde_json::Value = serde_json::from_str(MINIMAL).unwrap();
            v.as_object_mut().unwrap().remove(missing);
            assert!(
                matches!(parse(&v.to_string()), Err(ManifestError::Json(_))),
                "{missing} は必須"
            );
        }
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        // Chrome 由来のフィールドを黙って無視しない (無視すると
        // 「効いていると思っていた設定が無い」ずれを生む)。
        for key in [
            "web_accessible_resources",
            "externally_connectable",
            "content_security_policy",
            "update_url",
            "commands",
        ] {
            let json = with(&format!(r#""{key}": {{}}"#));
            assert!(matches!(parse(&json), Err(ManifestError::Json(_))), "{key}");
        }
    }

    #[test]
    fn rejects_unknown_nested_fields() {
        let cs = with(
            r#""content_scripts":[{"matches":["https://a.example/*"],"js":["a.js"],"world":"MAIN"}]"#,
        );
        assert!(matches!(parse(&cs), Err(ManifestError::Json(_))));
        let bg = with(r#""background":{"script":"b.js","persistent":true}"#);
        assert!(matches!(parse(&bg), Err(ManifestError::Json(_))));
    }

    #[test]
    fn rejects_oversized_manifest() {
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert_eq!(
            parse_manifest(&big),
            Err(ManifestError::TooLarge(MAX_MANIFEST_BYTES + 1))
        );
        // ちょうど上限は (空白なので) サイズ検査は通り、JSON として不正になる。
        let edge = vec![b' '; MAX_MANIFEST_BYTES];
        assert!(matches!(parse_manifest(&edge), Err(ManifestError::Json(_))));
    }

    #[test]
    fn rejects_bad_utf8() {
        assert!(matches!(
            parse_manifest(&[0xff, 0xfe, 0x00]),
            Err(ManifestError::Json(_))
        ));
    }

    // --- manifest_version ---

    #[test]
    fn manifest_version_must_be_supported() {
        for v in [0, 2, 3, 99] {
            let json = MINIMAL.replace(
                r#""manifest_version": 1"#,
                &format!(r#""manifest_version": {v}"#),
            );
            assert_eq!(
                parse(&json),
                Err(ManifestError::UnsupportedManifestVersion(v))
            );
        }
    }

    // --- id ---

    #[test]
    fn id_rules() {
        for id in ["abc", "com.example.hello", "a-b_c.d9", "x1y", "veloxtools"] {
            assert!(validate_id(id).is_ok(), "{id}");
        }
        let long = "a".repeat(MAX_ID_LEN + 1);
        let bad = [
            "",
            "ab",
            "Abc",
            "1abc",
            "-abc",
            ".abc",
            "abc.",
            "abc-",
            "a..b",
            "a b",
            "a/b",
            "拡張機能",
            "velox.official",
            long.as_str(),
        ];
        for id in bad {
            assert!(
                matches!(validate_id(id), Err(ManifestError::InvalidId(_))),
                "{id:?} は拒否"
            );
        }
        assert!(validate_id(&"a".repeat(MAX_ID_LEN)).is_ok());
    }

    // --- name / description ---

    #[test]
    fn display_text_rules() {
        assert!(is_display_text("日本語の名前", MAX_NAME_CHARS));
        assert!(is_display_text(&"a".repeat(MAX_NAME_CHARS), MAX_NAME_CHARS));
        assert!(!is_display_text(
            &"a".repeat(MAX_NAME_CHARS + 1),
            MAX_NAME_CHARS
        ));
        assert!(!is_display_text("", 10));
        assert!(!is_display_text("   ", 10));
        assert!(!is_display_text(" pad", 10));
        assert!(!is_display_text("a\nb", 10));
        assert!(!is_display_text("a\u{0}b", 10));
        // 表示順を入れ替える双方向制御文字・ゼロ幅文字。
        assert!(!is_display_text("ab\u{202E}cd", 10));
        assert!(!is_display_text("ab\u{2066}cd", 10));
        assert!(!is_display_text("ab\u{200B}cd", 10));
    }

    #[test]
    fn invalid_name_and_description_are_rejected() {
        let json = MINIMAL.replace(r#""name": "Hello""#, r#""name": "  ""#);
        assert_eq!(parse(&json), Err(ManifestError::InvalidName));
        let json = MINIMAL.replace(r#""name": "Hello""#, r#""name": """#);
        assert_eq!(parse(&json), Err(ManifestError::InvalidName));
        let json = with(&format!(
            r#""description":"{}""#,
            "x".repeat(MAX_DESCRIPTION_CHARS + 1)
        ));
        assert_eq!(parse(&json), Err(ManifestError::InvalidDescription));
    }

    // --- version ---

    #[test]
    fn version_parsing() {
        for ok in [
            "0.0.0",
            "1.2.3",
            "10.20.30",
            "1.0.0-alpha",
            "1.0.0-rc.1",
            "999999999.0.0",
        ] {
            assert!(ExtensionVersion::parse(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "a.b.c",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3-a_b",
            "v1.2.3",
            "1.2.3+build",
            "-1.2.3",
            "1.2.-3",
            "1000000000.0.0",
            " 1.2.3",
        ] {
            assert!(ExtensionVersion::parse(bad).is_none(), "{bad:?}");
        }
        assert!(ExtensionVersion::parse(&format!("1.0.0-{}", "a".repeat(70))).is_none());
    }

    #[test]
    fn version_ordering() {
        let v = |s: &str| ExtensionVersion::parse(s).unwrap();
        assert!(v("1.0.0") < v("1.0.1"));
        assert!(v("1.9.0") < v("1.10.0"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.0.0-beta") < v("1.0.0"));
        assert!(v("1.0.0-alpha") < v("1.0.0-beta"));
        assert_eq!(v("1.2.3").cmp(&v("1.2.3")), std::cmp::Ordering::Equal);
    }

    #[test]
    fn invalid_version_strings_are_reported() {
        let json = MINIMAL.replace("\"1.0.0\"", "\"1.0\"");
        assert_eq!(
            parse(&json),
            Err(ManifestError::InvalidVersion("1.0".into()))
        );
        let json = with(r#""min_velox_version":"x""#);
        assert!(matches!(
            parse(&json),
            Err(ManifestError::InvalidVersion(_))
        ));
    }

    // --- 権限 ---

    #[test]
    fn permission_names_round_trip() {
        for p in Permission::ALL {
            assert_eq!(Permission::from_name(p.as_str()), Some(p));
        }
        assert_eq!(Permission::from_name("Storage"), None, "大文字小文字は区別");
        assert_eq!(Permission::from_name(""), None);
    }

    #[test]
    fn reserved_permissions_are_not_known_permissions() {
        for name in RESERVED_PERMISSIONS {
            assert_eq!(Permission::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn sensitive_permissions_are_flagged() {
        assert!(Permission::Tabs.is_sensitive());
        assert!(Permission::Scripting.is_sensitive());
        assert!(!Permission::Storage.is_sensitive());
        assert!(!Permission::Alarms.is_sensitive());
    }

    #[test]
    fn rejects_unknown_permissions() {
        for name in ["superpower", "Storage", "storage ", "", "*", "<all_urls>"] {
            let json = with(&format!(r#""permissions":["{name}"]"#));
            assert!(
                matches!(parse(&json), Err(ManifestError::UnknownPermission(_))),
                "{name:?}"
            );
        }
        let json = with(r#""optional_permissions":["superpower"]"#);
        assert!(matches!(
            parse(&json),
            Err(ManifestError::UnknownPermission(_))
        ));
    }

    #[test]
    fn rejects_reserved_permissions_with_distinct_error() {
        for name in [
            "cookies",
            "web_request",
            "history",
            "native_messaging",
            "debugger",
        ] {
            let json = with(&format!(r#""permissions":["{name}"]"#));
            assert_eq!(
                parse(&json),
                Err(ManifestError::ReservedPermission(name.to_owned())),
                "{name}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_and_overlapping_permissions() {
        assert!(matches!(
            parse(&with(r#""permissions":["storage","storage"]"#)),
            Err(ManifestError::DuplicatePermission(_))
        ));
        assert!(matches!(
            parse(&with(
                r#""permissions":["storage"],"optional_permissions":["storage"]"#
            )),
            Err(ManifestError::OverlappingOptional(_))
        ));
        assert!(matches!(
            parse(&with(
                r#""host_permissions":["https://a.example/*"],"optional_host_permissions":["https://a.example/*"]"#
            )),
            Err(ManifestError::OverlappingOptional(_))
        ));
    }

    #[test]
    fn permissions_must_be_strings() {
        assert!(matches!(
            parse(&with(r#""permissions":[1]"#)),
            Err(ManifestError::Json(_))
        ));
        assert!(matches!(
            parse(&with(r#""permissions":"storage""#)),
            Err(ManifestError::Json(_))
        ));
    }

    #[test]
    fn scripting_needs_a_host_source() {
        assert_eq!(
            parse(&with(r#""permissions":["scripting"]"#)),
            Err(ManifestError::ScriptingWithoutHosts)
        );
        assert!(parse(&with(
            r#""permissions":["scripting"],"host_permissions":["https://a.example/*"]"#
        ))
        .is_ok());
        assert!(parse(&with(r#""permissions":["scripting","active_tab"]"#)).is_ok());
        assert!(parse(&with(
            r#""permissions":["scripting"],"optional_host_permissions":["https://a.example/*"]"#
        ))
        .is_ok());
    }

    // --- ホストパターン: 構文 ---

    #[test]
    fn host_pattern_accepts_valid_forms() {
        let ok = [
            "https://example.com/*",
            "http://example.com/a/b",
            "*://example.com/*",
            "https://*.example.com/*",
            "https://example.com:8443/*",
            "http://localhost/*",
            "http://127.0.0.1:3000/*",
            "https://a-b.example.co.jp/x*y",
            "https://example.com/",
        ];
        for s in ok {
            assert!(HostPattern::parse(s).is_ok(), "{s}");
        }
    }

    #[test]
    fn host_pattern_rejects_invalid_forms() {
        use PatternError::*;
        let cases: &[(&str, PatternError)] = &[
            ("", MissingScheme),
            ("example.com/*", MissingScheme),
            ("https://example.com", MissingPath),
            ("file:///etc/passwd", UnsupportedScheme("file".into())),
            ("ftp://example.com/*", UnsupportedScheme("ftp".into())),
            ("data://example.com/*", UnsupportedScheme("data".into())),
            ("velox://settings/*", UnsupportedScheme("velox".into())),
            ("javascript://x/*", UnsupportedScheme("javascript".into())),
            ("https://user@example.com/*", UserInfo),
            ("https://example.com:/*", InvalidPort),
            ("https://example.com:0/*", InvalidPort),
            ("https://example.com:65536/*", InvalidPort),
            ("https://example.com:80a/*", InvalidPort),
            ("https://*.com/*", WildcardTooBroad),
            ("https://*.localhost/*", WildcardTooBroad),
            ("https://ex*mple.com/*", InvalidHost),
            ("https://example.*/*", InvalidHost),
            ("https://**.example.com/*", InvalidHost),
            ("https://Example.com/*", InvalidHost),
            ("https://例え.jp/*", InvalidHost),
            ("https://[::1]/*", InvalidPort),
            ("https://-a.com/*", InvalidHost),
            ("https://a..com/*", InvalidHost),
            ("https://example.com./*", InvalidHost),
            ("https:///*", InvalidHost),
            ("https://example.com/a b", InvalidPath),
            ("https://example.com/a#frag", InvalidPath),
            ("https://example.com/a\u{0}b", InvalidPath),
        ];
        for (s, want) in cases {
            assert_eq!(HostPattern::parse(s), Err(want.clone()), "{s:?}");
        }
    }

    #[test]
    fn host_pattern_length_limits() {
        let long_path = format!("https://example.com/{}", "a".repeat(MAX_PATH_LEN));
        assert_eq!(HostPattern::parse(&long_path), Err(PatternError::TooLong));
        let huge = format!("https://example.com/{}", "a".repeat(MAX_PATTERN_LEN));
        assert_eq!(HostPattern::parse(&huge), Err(PatternError::TooLong));
        let long_label = format!("https://{}.com/*", "a".repeat(64));
        assert_eq!(
            HostPattern::parse(&long_label),
            Err(PatternError::InvalidHost)
        );
    }

    #[test]
    fn all_urls_token_and_equivalents_are_all_hosts() {
        assert!(HostPattern::parse("<all_urls>").unwrap().is_all_hosts());
        assert!(HostPattern::parse("*://*/*").unwrap().is_all_hosts());
        assert!(HostPattern::parse("https://*/*").unwrap().is_all_hosts());
        assert!(!HostPattern::parse("https://*.example.com/*")
            .unwrap()
            .is_all_hosts());
        assert_eq!(
            HostPattern::parse("<all_urls>").unwrap(),
            HostPattern::all_urls()
        );
    }

    // --- ホストパターン: 一致 ---

    fn m(pattern: &str, url: &str) -> bool {
        HostPattern::parse(pattern).unwrap().matches(url)
    }

    #[test]
    fn matching_scheme() {
        assert!(m("https://example.com/*", "https://example.com/"));
        assert!(!m("https://example.com/*", "http://example.com/"));
        assert!(m("http://example.com/*", "http://example.com/x"));
        assert!(m("*://example.com/*", "http://example.com/x"));
        assert!(m("*://example.com/*", "https://example.com/x"));
    }

    #[test]
    fn all_urls_never_matches_non_web_schemes() {
        let p = HostPattern::all_urls();
        assert!(p.matches("https://example.com/x"));
        for url in [
            "file:///etc/passwd",
            "data:text/html,hi",
            "velox://settings/",
            "about:blank",
            "javascript:alert(1)",
            "blob:https://example.com/uuid",
            "ftp://example.com/",
            "not a url",
            "",
        ] {
            assert!(!p.matches(url), "{url:?}");
        }
    }

    #[test]
    fn matching_host() {
        assert!(m("https://example.com/*", "https://example.com/a"));
        assert!(!m("https://example.com/*", "https://www.example.com/a"));
        assert!(m("https://*.example.com/*", "https://www.example.com/a"));
        assert!(m("https://*.example.com/*", "https://a.b.example.com/a"));
        assert!(m("https://*.example.com/*", "https://example.com/a"));
        // 接尾辞が同じだけの別ドメインには一致しない。
        assert!(!m("https://*.example.com/*", "https://evilexample.com/a"));
        assert!(!m(
            "https://*.example.com/*",
            "https://example.com.evil.test/a"
        ));
        assert!(!m(
            "https://example.com/*",
            "https://example.com.evil.test/"
        ));
        // URL 側の大文字は正規化される。
        assert!(m("https://example.com/*", "https://EXAMPLE.com/"));
    }

    #[test]
    fn userinfo_tricks_do_not_match() {
        assert!(!m(
            "https://example.com/*",
            "https://example.com@evil.test/"
        ));
        assert!(!m("https://evil.test/*", "https://example.com@evil.test/"));
        assert!(!m("https://example.com/*", "https://user:pw@example.com/"));
    }

    #[test]
    fn matching_port() {
        assert!(m("https://example.com/*", "https://example.com/"));
        assert!(m("https://example.com/*", "https://example.com:443/"));
        assert!(!m("https://example.com/*", "https://example.com:8443/"));
        assert!(m("https://example.com:8443/*", "https://example.com:8443/"));
        assert!(!m("https://example.com:8443/*", "https://example.com/"));
        assert!(m("http://localhost/*", "http://localhost/"));
        assert!(!m("http://localhost/*", "http://localhost:3000/"));
        assert!(m("http://localhost:3000/*", "http://localhost:3000/x"));
        // `*` スキームの既定ポートはマッチ先のスキームで決まる。
        assert!(m("*://example.com/*", "http://example.com/"));
        assert!(!m("*://example.com/*", "http://example.com:8080/"));
    }

    #[test]
    fn matching_path_and_query() {
        assert!(m("https://example.com/*", "https://example.com/"));
        assert!(m("https://example.com/a/*", "https://example.com/a/b/c"));
        assert!(!m("https://example.com/a/*", "https://example.com/b/c"));
        assert!(m("https://example.com/a", "https://example.com/a"));
        assert!(!m("https://example.com/a", "https://example.com/a/"));
        assert!(!m("https://example.com/a", "https://example.com/ab"));
        assert!(m("https://example.com/a*z", "https://example.com/az"));
        assert!(m("https://example.com/a*z", "https://example.com/a-mid-z"));
        assert!(!m("https://example.com/a*z", "https://example.com/a-mid-y"));
        assert!(m("https://example.com/*?q=1", "https://example.com/p?q=1"));
        assert!(!m("https://example.com/p", "https://example.com/p?q=1"));
        // フラグメントは一致に影響しない。
        assert!(m("https://example.com/p", "https://example.com/p#frag"));
        // `..` はパーサが正規化した後のパスで判定される。
        assert!(!m("https://example.com/a/*", "https://example.com/a/../b"));
    }

    #[test]
    fn glob_edge_cases() {
        assert!(glob_match("*", ""));
        assert!(glob_match("/*", "/"));
        assert!(glob_match("/**", "/x"));
        assert!(glob_match("/a*b*c", "/aXbYc"));
        assert!(!glob_match("/a*b*c", "/aXbY"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    // --- 広域アクセスの明示フラグ ---

    #[test]
    fn all_urls_requires_explicit_flag() {
        for pat in ["<all_urls>", "*://*/*", "https://*/*", "http://*/x"] {
            let json = with(&format!(r#""host_permissions":["{pat}"]"#));
            assert!(
                matches!(parse(&json), Err(ManifestError::AllUrlsNotAllowed(_))),
                "{pat}"
            );
            let json = with(&format!(
                r#""allow_all_urls":true,"host_permissions":["{pat}"]"#
            ));
            assert!(parse(&json).is_ok(), "{pat} + flag");
        }
    }

    #[test]
    fn all_urls_flag_is_checked_for_optional_and_content_scripts() {
        let opt = with(r#""optional_host_permissions":["<all_urls>"]"#);
        assert!(matches!(
            parse(&opt),
            Err(ManifestError::AllUrlsNotAllowed(_))
        ));
        let cs = with(r#""content_scripts":[{"matches":["<all_urls>"],"js":["a.js"]}]"#);
        assert!(matches!(
            parse(&cs),
            Err(ManifestError::AllUrlsNotAllowed(_))
        ));
        let cs_ok = with(
            r#""allow_all_urls":true,"content_scripts":[{"matches":["<all_urls>"],"js":["a.js"]}]"#,
        );
        assert!(parse(&cs_ok).is_ok());
    }

    #[test]
    fn all_urls_error_names_the_pattern() {
        let err = parse(&with(r#""host_permissions":["<all_urls>"]"#)).unwrap_err();
        assert_eq!(err, ManifestError::AllUrlsNotAllowed("<all_urls>".into()));
        let err = parse(&with(r#""host_permissions":["https://*/x*"]"#)).unwrap_err();
        assert_eq!(err, ManifestError::AllUrlsNotAllowed("https://*/x*".into()));
    }

    #[test]
    fn exclude_matches_may_be_broad_without_flag() {
        // exclude は権限を増やさないので広域でも良い。
        let json = with(
            r#""content_scripts":[{"matches":["https://a.example/*"],"exclude_matches":["<all_urls>"],"js":["a.js"]}]"#,
        );
        assert!(parse(&json).is_ok());
    }

    #[test]
    fn invalid_pattern_in_manifest_is_reported() {
        let json = with(r#""host_permissions":["file:///*"]"#);
        assert!(matches!(
            parse(&json),
            Err(ManifestError::InvalidPattern {
                reason: PatternError::UnsupportedScheme(_),
                ..
            })
        ));
    }

    #[test]
    fn rejects_duplicate_patterns() {
        let json = with(r#""host_permissions":["https://a.example/*","https://a.example/*"]"#);
        assert!(matches!(
            parse(&json),
            Err(ManifestError::DuplicatePermission(_))
        ));
    }

    #[test]
    fn host_pattern_count_limits() {
        let many: Vec<String> = (0..=MAX_HOST_PATTERNS)
            .map(|i| format!("\"https://h{i}.example/*\""))
            .collect();
        let json = with(&format!(r#""host_permissions":[{}]"#, many.join(",")));
        assert!(matches!(
            parse(&json),
            Err(ManifestError::TooManyEntries(_))
        ));
        let ok: Vec<String> = (0..MAX_HOST_PATTERNS)
            .map(|i| format!("\"https://h{i}.example/*\""))
            .collect();
        let json = with(&format!(r#""host_permissions":[{}]"#, ok.join(",")));
        assert!(parse(&json).is_ok());
    }

    // --- content_scripts / リソースパス ---

    #[test]
    fn content_script_requires_matches_and_js() {
        let json = with(r#""content_scripts":[{"matches":[],"js":["a.js"]}]"#);
        assert_eq!(parse(&json), Err(ManifestError::EmptyContentScript));
        let json = with(r#""content_scripts":[{"matches":["https://a.example/*"],"js":[]}]"#);
        assert_eq!(parse(&json), Err(ManifestError::EmptyContentScript));
        let json = with(r#""content_scripts":[{"matches":["https://a.example/*"]}]"#);
        assert!(matches!(parse(&json), Err(ManifestError::Json(_))));
    }

    #[test]
    fn content_script_count_limits() {
        let one = r#"{"matches":["https://a.example/*"],"js":["a.js"]}"#;
        let many = vec![one; MAX_CONTENT_SCRIPTS + 1].join(",");
        let json = with(&format!(r#""content_scripts":[{many}]"#));
        assert!(matches!(
            parse(&json),
            Err(ManifestError::TooManyEntries(_))
        ));
        let ok = vec![one; MAX_CONTENT_SCRIPTS].join(",");
        assert!(parse(&with(&format!(r#""content_scripts":[{ok}]"#))).is_ok());
        let js: Vec<String> = (0..=MAX_ENTRIES_PER_SCRIPT)
            .map(|i| format!("\"f{i}.js\""))
            .collect();
        let json = with(&format!(
            r#""content_scripts":[{{"matches":["https://a.example/*"],"js":[{}]}}]"#,
            js.join(",")
        ));
        assert!(matches!(
            parse(&json),
            Err(ManifestError::TooManyEntries(_))
        ));
    }

    #[test]
    fn run_at_must_be_known() {
        let json = with(
            r#""content_scripts":[{"matches":["https://a.example/*"],"js":["a.js"],"run_at":"early"}]"#,
        );
        assert!(matches!(parse(&json), Err(ManifestError::Json(_))));
    }

    #[test]
    fn resource_path_rules() {
        for ok in ["a.js", "dir/a.js", "a-b_c/d.e.js", "x/y/z.js"] {
            assert!(validate_resource_path(ok).is_ok(), "{ok}");
        }
        let long = format!("{}.js", "a".repeat(MAX_RESOURCE_PATH_LEN));
        for bad in [
            "",
            "a.css",
            "a",
            "/abs.js",
            "../a.js",
            "a/../b.js",
            "a/./b.js",
            "a//b.js",
            "a\\b.js",
            "C:/a.js",
            "http://x/a.js",
            "a b.js",
            ".hidden.js",
            "dir/.hidden.js",
            "a\u{0}.js",
            "日本語.js",
            "a.js/",
            long.as_str(),
        ] {
            assert!(
                matches!(
                    validate_resource_path(bad),
                    Err(ManifestError::InvalidResourcePath(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn background_script_path_is_validated() {
        assert!(matches!(
            parse(&with(r#""background":{"script":"../evil.js"}"#)),
            Err(ManifestError::InvalidResourcePath(_))
        ));
        assert!(parse(&with(r#""background":{"script":"bg/main.js"}"#)).is_ok());
    }

    // --- エラー表示 ---

    #[test]
    fn errors_have_readable_messages() {
        let err = parse(&with(r#""permissions":["cookies"]"#)).unwrap_err();
        assert!(err.to_string().contains("cookies"));
        let err = parse("nope").unwrap_err();
        assert!(err.to_string().starts_with("manifest を解釈できない"));
    }

    #[test]
    fn long_bad_inputs_are_truncated_in_errors() {
        let name = "p".repeat(10_000);
        let ManifestError::UnknownPermission(shown) =
            parse(&with(&format!(r#""permissions":["{name}"]"#))).unwrap_err()
        else {
            panic!("expected UnknownPermission");
        };
        assert!(shown.len() <= 64);
    }
}
