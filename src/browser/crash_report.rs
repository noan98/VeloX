//! クラッシュレポートの純粋ロジック (Issue #89, docs/decisions.md D156)。
//!
//! ファイル名の規則・古いレポートの間引き・本文の整形・プライバシー保護の
//! ためのサニタイズ・起動時に出す通知文だけを持つ。ファイルの読み書きと
//! パニックフックの設置は [`crate::browser::crash_store`] にあり、そちらは
//! 意図的に薄い IO 層に留めている (`persistence` / `perf_log` と同じ流儀)。
//!
//! ## 何を書かないか
//!
//! レポートは**ローカルのファイルに書くだけ**で、外部へは一切送らない
//! (送信は Issue #93 の別判断)。書くのは時刻・バージョン・OS・panic の
//! 場所とメッセージ・バックトレースだけで、**閲覧中の URL・ページの内容・
//! タブのタイトルは持たない** (そもそも受け取る型に欄がない)。panic の
//! メッセージやバックトレースに紛れ込みうる URL とホームディレクトリの
//! パスは [`sanitize`] が潰し、長さも [`MAX_FIELD_BYTES`] で切る。

use super::util::truncate_utf8;

/// 残すレポートの最大件数。これを超えた分は古いものから消す。
pub const MAX_REPORTS: usize = 10;

/// メッセージ・場所など 1 項目あたりの最大バイト数。
pub const MAX_FIELD_BYTES: usize = 1024;

/// バックトレースの最大バイト数。
pub const MAX_BACKTRACE_BYTES: usize = 16 * 1024;

const FILE_PREFIX: &str = "crash-";
const FILE_SUFFIX: &str = ".txt";

/// `VELOX_CRASH_REPORTS` の値からレポート機能が有効かを決める。
///
/// 未設定 (`None`) は有効。`0` / `off` / `false` / `no` (大文字小文字を
/// 区別しない) で無効になる。無効なときはパニックフックも実行中マーカー
/// も設置しない。
pub fn reports_enabled(env_value: Option<&str>) -> bool {
    match env_value {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        ),
    }
}

/// レポートの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashKind {
    /// VeloX 本体の panic。
    Panic,
    /// 埋め込み WebView のプロセス異常 (Windows / WebView2 のみ検知できる)。
    WebViewProcess,
}

impl CrashKind {
    fn label(self) -> &'static str {
        match self {
            CrashKind::Panic => "panic",
            CrashKind::WebViewProcess => "webview-process",
        }
    }
}

/// WebView2 の `ProcessFailedKind` を VeloX 側の語彙に落としたもの
/// (`COREWEBVIEW2_PROCESS_FAILED_KIND` の値と 1 対 1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessFailureKind {
    BrowserProcessExited,
    RenderProcessExited,
    RenderProcessUnresponsive,
    FrameRenderProcessExited,
    UtilityProcessExited,
    SandboxHelperProcessExited,
    GpuProcessExited,
    PpapiPluginProcessExited,
    PpapiBrokerProcessExited,
    UnknownProcessExited,
}

/// [`ProcessFailureKind`] に対して VeloX が取る動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// そのタブを作り直す (アクティブならリロード、背景なら次の切替で再生成)。
    RecoverTab,
    /// 通知だけ出す。
    NoticeOnly,
    /// ログとレポートのみ (エンジンが自力で復旧する種類)。
    LogOnly,
}

impl ProcessFailureKind {
    /// WebView2 の列挙値から変換する。未知の値は `UnknownProcessExited`。
    pub fn from_webview2_code(code: i32) -> Self {
        match code {
            0 => Self::BrowserProcessExited,
            1 => Self::RenderProcessExited,
            2 => Self::RenderProcessUnresponsive,
            3 => Self::FrameRenderProcessExited,
            4 => Self::UtilityProcessExited,
            5 => Self::SandboxHelperProcessExited,
            6 => Self::GpuProcessExited,
            7 => Self::PpapiPluginProcessExited,
            8 => Self::PpapiBrokerProcessExited,
            _ => Self::UnknownProcessExited,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::BrowserProcessExited => "browser process exited",
            Self::RenderProcessExited => "render process exited",
            Self::RenderProcessUnresponsive => "render process unresponsive",
            Self::FrameRenderProcessExited => "frame render process exited",
            Self::UtilityProcessExited => "utility process exited",
            Self::SandboxHelperProcessExited => "sandbox helper process exited",
            Self::GpuProcessExited => "gpu process exited",
            Self::PpapiPluginProcessExited => "ppapi plugin process exited",
            Self::PpapiBrokerProcessExited => "ppapi broker process exited",
            Self::UnknownProcessExited => "unknown process exited",
        }
    }

    /// この種類に対する動作。ページを描画しているプロセスが落ちたときだけ
    /// タブを作り直す。応答なし (`RenderProcessUnresponsive`) は待てば戻る
    /// ことがあるので、ユーザーのページを勝手に捨てず通知に留める。
    /// ブラウザプロセスの終了は webview 全体が死んでいて VeloX 側から直せない
    /// ため通知のみ (再起動を促す)。
    pub fn recovery(self) -> Recovery {
        match self {
            Self::RenderProcessExited => Recovery::RecoverTab,
            Self::BrowserProcessExited | Self::RenderProcessUnresponsive => Recovery::NoticeOnly,
            _ => Recovery::LogOnly,
        }
    }
}

/// 1 件のクラッシュレポート。URL・ページ内容・タイトルを入れる欄は無い。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReport {
    pub unix_time: u64,
    pub pid: u32,
    pub kind: CrashKind,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub thread: Option<String>,
    pub location: Option<String>,
    pub message: String,
    pub backtrace: Option<String>,
}

impl CrashReport {
    /// ファイル名 (`crash-<unix>-<pid>.txt`)。
    pub fn file_name(&self) -> String {
        report_file_name(self.unix_time, self.pid)
    }

    /// 人が読むプレーンテキストに整形する。すべての自由記述は [`sanitize`]
    /// を通す。`home_dirs` は `~` に置き換えるパスの前置。
    pub fn format(&self, home_dirs: &[String]) -> String {
        let field = |s: &str| sanitize(s, home_dirs, MAX_FIELD_BYTES);
        let mut out = String::new();
        out.push_str("VeloX crash report\n");
        out.push_str(&format!("kind: {}\n", self.kind.label()));
        out.push_str(&format!("version: {}\n", field(&self.version)));
        out.push_str(&format!("os: {} {}\n", field(&self.os), field(&self.arch)));
        out.push_str(&format!("time_unix: {}\n", self.unix_time));
        out.push_str(&format!("pid: {}\n", self.pid));
        if let Some(thread) = &self.thread {
            out.push_str(&format!("thread: {}\n", field(thread)));
        }
        if let Some(location) = &self.location {
            out.push_str(&format!("location: {}\n", field(location)));
        }
        out.push_str(&format!("message: {}\n", field(&self.message)));
        if let Some(bt) = &self.backtrace {
            out.push_str("backtrace:\n");
            out.push_str(&sanitize(bt, home_dirs, MAX_BACKTRACE_BYTES));
            out.push('\n');
        }
        out
    }
}

/// `crash-<unix>-<pid>.txt`。
pub fn report_file_name(unix_time: u64, pid: u32) -> String {
    format!("{FILE_PREFIX}{unix_time}-{pid}{FILE_SUFFIX}")
}

/// [`report_file_name`] の逆。規則に合わない名前は `None`。
pub fn parse_report_file_name(name: &str) -> Option<(u64, u32)> {
    let body = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    let (time, pid) = body.split_once('-')?;
    Some((time.parse().ok()?, pid.parse().ok()?))
}

/// `names` のうち、新しい順に `keep` 件を残したうえで**消すべき**名前を返す。
/// 規則に合わない名前 (ユーザーが置いたファイルなど) は対象にしない。
/// 並びは (時刻, pid) の昇順で、古いものが先。
pub fn reports_to_delete<'a>(names: &[&'a str], keep: usize) -> Vec<&'a str> {
    let mut parsed: Vec<((u64, u32), &'a str)> = names
        .iter()
        .filter_map(|n| parse_report_file_name(n).map(|k| (k, *n)))
        .collect();
    parsed.sort();
    let excess = parsed.len().saturating_sub(keep);
    parsed.into_iter().take(excess).map(|(_, n)| n).collect()
}

/// `since` (unix 秒) 以降に書かれたレポートの件数。
pub fn count_reports_since(names: &[String], since: u64) -> usize {
    names
        .iter()
        .filter_map(|n| parse_report_file_name(n))
        .filter(|(time, _)| *time >= since)
        .count()
}

/// 実行中マーカーの中身 (`<pid> <started_unix>`)。
pub fn marker_contents(pid: u32, started_unix: u64) -> String {
    format!("{pid} {started_unix}\n")
}

/// [`marker_contents`] から開始時刻を取り出す。空・壊れた内容は `None`。
pub fn parse_marker_started(contents: &str) -> Option<u64> {
    contents.split_whitespace().nth(1)?.parse().ok()
}

/// 前回の実行の終わり方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousRun {
    /// 正常終了 (または初回起動、または別のインスタンスが動作中)。
    Clean,
    /// 正常終了していない。`started` は前回の開始時刻 (読めれば)。
    Unclean { started: Option<u64> },
}

/// 起動時にユーザーへ出す通知文。通知すべきことが無ければ `None`。
/// `reports` は前回の実行中 (`started` 以降) に書かれたレポートの件数。
pub fn startup_notice(previous: PreviousRun, reports: usize) -> Option<String> {
    match previous {
        PreviousRun::Clean => None,
        PreviousRun::Unclean { .. } if reports > 0 => Some(format!(
            "前回は異常終了しました。クラッシュレポートを {reports} 件保存しました (crash-reports フォルダ)。"
        )),
        PreviousRun::Unclean { .. } => {
            Some("前回は正常に終了しなかったようです (原因の記録はありません)。".to_owned())
        }
    }
}

/// レポートに載せる文字列から個人情報になりうるものを取り除く。
///
/// 1. `home_dirs` に一致するパスの前置を `~` に置き換える (環境変数の値
///    そのままで比べ、`/` と `\` の違いは同一視しない)。
/// 2. `scheme://...` 形式の URL を `<url>` に置き換える。
/// 3. 改行とタブ以外の制御文字を除く。
/// 4. `max_bytes` を超える分を切り、切ったことを末尾に示す。
pub fn sanitize(text: &str, home_dirs: &[String], max_bytes: usize) -> String {
    let mut s = text.to_owned();
    for home in home_dirs {
        if home.len() >= 2 {
            s = s.replace(home.as_str(), "~");
        }
    }
    let s = redact_urls(&s);
    let s: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    if s.len() <= max_bytes {
        s
    } else {
        format!("{}…(truncated)", truncate_utf8(&s, max_bytes))
    }
}

fn redact_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("://") {
        let before = &rest[..pos];
        // スキーム部分 (英数字と `+.-`) を後ろから拾う。
        let scheme_len = before
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
            .map(char::len_utf8)
            .sum::<usize>();
        out.push_str(&before[..before.len() - scheme_len]);
        let after = &rest[pos + 3..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | ',' | '`'))
            .unwrap_or(after.len());
        out.push_str("<url>");
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CrashReport {
        CrashReport {
            unix_time: 1_700_000_000,
            pid: 42,
            kind: CrashKind::Panic,
            version: "0.1.0".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
            thread: Some("main".into()),
            location: Some("src/app.rs:10:5".into()),
            message: "boom".into(),
            backtrace: Some("0: velox::app::run".into()),
        }
    }

    #[test]
    fn enabled_by_default_and_disabled_by_off_values() {
        assert!(reports_enabled(None));
        assert!(reports_enabled(Some("1")));
        assert!(reports_enabled(Some("")));
        for v in ["0", "off", "OFF", "False", " no "] {
            assert!(!reports_enabled(Some(v)), "{v}");
        }
    }

    #[test]
    fn file_name_round_trips() {
        assert_eq!(report_file_name(123, 7), "crash-123-7.txt");
        assert_eq!(parse_report_file_name("crash-123-7.txt"), Some((123, 7)));
        assert_eq!(sample().file_name(), "crash-1700000000-42.txt");
    }

    #[test]
    fn foreign_file_names_are_not_reports() {
        for n in [
            "crash-x-7.txt",
            "crash-1.txt",
            "notes.txt",
            "crash-1-2.log",
            "",
        ] {
            assert_eq!(parse_report_file_name(n), None, "{n}");
        }
    }

    #[test]
    fn deletes_oldest_beyond_the_cap_and_ignores_foreign_files() {
        let names = [
            "crash-30-1.txt",
            "crash-10-1.txt",
            "readme.txt",
            "crash-20-1.txt",
            "crash-20-0.txt",
        ];
        assert_eq!(
            reports_to_delete(&names, 2),
            vec!["crash-10-1.txt", "crash-20-0.txt"]
        );
        assert!(reports_to_delete(&names, 4).is_empty());
        assert!(reports_to_delete(&names, 10).is_empty());
        assert_eq!(reports_to_delete(&names, 0).len(), 4);
    }

    #[test]
    fn counts_reports_since_a_start_time() {
        let names: Vec<String> = ["crash-5-1.txt", "crash-10-1.txt", "crash-11-2.txt", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(count_reports_since(&names, 10), 2);
        assert_eq!(count_reports_since(&names, 100), 0);
    }

    #[test]
    fn marker_round_trips_and_rejects_garbage() {
        assert_eq!(parse_marker_started(&marker_contents(9, 1234)), Some(1234));
        assert_eq!(parse_marker_started(""), None);
        assert_eq!(parse_marker_started("abc"), None);
        assert_eq!(parse_marker_started("1 x"), None);
    }

    #[test]
    fn notice_only_after_an_unclean_run() {
        assert_eq!(startup_notice(PreviousRun::Clean, 3), None);
        let with = startup_notice(PreviousRun::Unclean { started: Some(1) }, 2).unwrap();
        assert!(with.contains("2 件"));
        let without = startup_notice(PreviousRun::Unclean { started: None }, 0).unwrap();
        assert!(without.contains("正常に終了しなかった"));
    }

    #[test]
    fn urls_are_redacted() {
        let out = sanitize(
            "failed to load https://example.com/a?q=secret&x=1 now, (file:///C:/x.html) ok",
            &[],
            1024,
        );
        assert_eq!(out, "failed to load <url> now, (<url>) ok");
        assert!(!out.contains("secret"));
    }

    #[test]
    fn home_dir_is_replaced() {
        let homes = vec!["C:\\Users\\alice".to_owned(), "/home/alice".to_owned()];
        assert_eq!(
            sanitize(
                "C:\\Users\\alice\\src\\a.rs and /home/alice/b.rs",
                &homes,
                1024
            ),
            "~\\src\\a.rs and ~/b.rs"
        );
        // 短すぎる (空や "/") 前置は全文置換を招くので無視する。
        assert_eq!(
            sanitize("a/b", &["/".to_owned(), String::new()], 1024),
            "a/b"
        );
    }

    #[test]
    fn control_chars_are_dropped_and_length_is_capped() {
        assert_eq!(
            sanitize("a\u{0}b\u{1b}[31mc\nd\te", &[], 1024),
            "ab[31mc\nd\te"
        );
        let long = "あ".repeat(1000);
        let out = sanitize(&long, &[], 10);
        assert!(out.ends_with("…(truncated)"));
        assert!(out.starts_with("あああ"));
    }

    #[test]
    fn format_contains_fields_and_never_leaks_urls() {
        let mut r = sample();
        r.message = "cannot open https://secret.example/path".into();
        let text = r.format(&[]);
        assert!(text.starts_with("VeloX crash report\n"));
        assert!(text.contains("kind: panic\n"));
        assert!(text.contains("version: 0.1.0\n"));
        assert!(text.contains("os: windows x86_64\n"));
        assert!(text.contains("location: src/app.rs:10:5\n"));
        assert!(text.contains("message: cannot open <url>\n"));
        assert!(text.contains("backtrace:\n0: velox::app::run\n"));
        assert!(!text.contains("secret.example"));
    }

    #[test]
    fn process_failure_mapping_and_recovery() {
        assert_eq!(
            ProcessFailureKind::from_webview2_code(1),
            ProcessFailureKind::RenderProcessExited
        );
        assert_eq!(
            ProcessFailureKind::from_webview2_code(99),
            ProcessFailureKind::UnknownProcessExited
        );
        assert_eq!(
            ProcessFailureKind::RenderProcessExited.recovery(),
            Recovery::RecoverTab
        );
        assert_eq!(
            ProcessFailureKind::RenderProcessUnresponsive.recovery(),
            Recovery::NoticeOnly
        );
        assert_eq!(
            ProcessFailureKind::BrowserProcessExited.recovery(),
            Recovery::NoticeOnly
        );
        assert_eq!(
            ProcessFailureKind::GpuProcessExited.recovery(),
            Recovery::LogOnly
        );
    }
}
