//! クラッシュレポートの IO 層 (Issue #89, docs/decisions.md D156)。
//!
//! パニックフックの設置・レポートの書き出しと間引き・実行中マーカーの
//! 取得/解放だけを行う薄い層で、判断ロジックは
//! [`crate::browser::crash_report`] にある。呼び出し側は失敗をすべて
//! 非致命として扱う (書けなくてもブラウザは動く)。**フックの中で panic
//! しないこと**が最優先で、フック内の失敗は握りつぶす。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use super::crash_report::{
    self, count_reports_since, marker_contents, parse_marker_started, reports_to_delete, CrashKind,
    CrashReport, PreviousRun, MAX_REPORTS,
};
use super::util::now_unix;

/// データディレクトリ直下のレポート置き場。
const REPORT_DIR: &str = "crash-reports";
const MARKER_FILE: &str = "running.lock";

pub fn report_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(REPORT_DIR)
}

fn home_dirs() -> Vec<String> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .collect()
}

/// `report` を `<data_dir>/crash-reports/` に書き、上限を超えた古い
/// レポートを消す。
pub fn write_report(data_dir: &Path, report: &CrashReport) -> std::io::Result<PathBuf> {
    let dir = report_dir(data_dir);
    fs::create_dir_all(&dir)?;
    let path = dir.join(report.file_name());
    fs::write(&path, report.format(&home_dirs()))?;
    let _ = prune(&dir, MAX_REPORTS);
    Ok(path)
}

/// レポートのファイル名一覧 (規則に合うものだけ)。
pub fn list_reports(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(report_dir(data_dir)) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| crash_report::parse_report_file_name(n).is_some())
        .collect()
}

fn prune(dir: &Path, keep: usize) -> std::io::Result<()> {
    let names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    for name in reports_to_delete(&refs, keep) {
        let _ = fs::remove_file(dir.join(name));
    }
    Ok(())
}

/// panic を `data_dir` にレポートとして書くフックを設置する。以前のフック
/// (既定では stderr へのメッセージ出力) は**そのまま呼び続ける**。
pub fn install_panic_hook(data_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "(non-string panic payload)".to_owned()
        };
        let report = CrashReport {
            unix_time: now_unix(),
            pid: std::process::id(),
            kind: CrashKind::Panic,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            thread: std::thread::current().name().map(str::to_owned),
            location: info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
            message,
            backtrace: Some(std::backtrace::Backtrace::force_capture().to_string()),
        };
        // フックの中での失敗は握りつぶす (二重 panic は abort になる)。
        let _ = write_report(&data_dir, &report);
    }));
}

/// WebView プロセスの異常を (URL 等を含めず) レポートとして残す。
pub fn write_process_failure(data_dir: &Path, label: &str) -> std::io::Result<PathBuf> {
    write_report(
        data_dir,
        &CrashReport {
            unix_time: now_unix(),
            pid: std::process::id(),
            kind: CrashKind::WebViewProcess,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            thread: None,
            location: None,
            message: label.to_owned(),
            backtrace: None,
        },
    )
}

/// 実行中であることを示すマーカー。プロセスが生きている間だけ OS の
/// ファイルロックを保持する。正常終了では [`RunMarker::release`] が中身を
/// 空にし、異常終了 (panic・kill・電源断) では中身が残る。ロックは
/// プロセスの死とともに OS が解放するので、次の起動は「ロックが取れて
/// 中身が残っている」ことで異常終了を判定できる。別インスタンスが
/// 動作中ならロックが取れず、異常終了とは見なさない。
pub struct RunMarker {
    file: File,
}

impl RunMarker {
    /// マーカーを取得し、前回の終わり方を返す。マーカーを取れなかった
    /// (別インスタンスが動作中、または IO 失敗) ときは `None`。
    pub fn acquire(data_dir: &Path) -> (Option<RunMarker>, PreviousRun) {
        let open = || -> std::io::Result<Option<(File, String)>> {
            fs::create_dir_all(data_dir)?;
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(data_dir.join(MARKER_FILE))?;
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(e)) => return Err(e),
            }
            let mut previous = String::new();
            let _ = file.read_to_string(&mut previous);
            file.set_len(0)?;
            file.rewind()?;
            file.write_all(marker_contents(std::process::id(), now_unix()).as_bytes())?;
            file.flush()?;
            Ok(Some((file, previous)))
        };
        match open() {
            Ok(Some((file, previous))) => {
                let state = if previous.trim().is_empty() {
                    PreviousRun::Clean
                } else {
                    PreviousRun::Unclean {
                        started: parse_marker_started(&previous),
                    }
                };
                (Some(RunMarker { file }), state)
            }
            Ok(None) => (None, PreviousRun::Clean),
            Err(err) => {
                eprintln!("velox: 実行中マーカーを作成できません: {err}");
                (None, PreviousRun::Clean)
            }
        }
    }

    /// 正常終了の印 (中身を空にする) を付けて解放する。
    pub fn release(mut self) {
        let _ = self.file.set_len(0);
        let _ = self.file.flush();
        let _ = self.file.unlock();
    }
}

/// 前回異常終了だったときの通知文 (前回の開始以降に書かれたレポート数を
/// 数えて [`crash_report::startup_notice`] に渡す)。
pub fn startup_notice(data_dir: Option<&Path>, previous: PreviousRun) -> Option<String> {
    let reports = match (previous, data_dir) {
        (PreviousRun::Unclean { started }, Some(dir)) => {
            count_reports_since(&list_reports(dir), started.unwrap_or(0))
        }
        _ => 0,
    };
    crash_report::startup_notice(previous, reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::util::unique_temp_path;

    fn report(time: u64) -> CrashReport {
        CrashReport {
            unix_time: time,
            pid: 1,
            kind: CrashKind::Panic,
            version: "t".into(),
            os: "o".into(),
            arch: "a".into(),
            thread: None,
            location: None,
            message: "m".into(),
            backtrace: None,
        }
    }

    #[test]
    fn write_report_caps_the_number_of_files() {
        let dir = unique_temp_path("velox-crash-cap");
        for t in 1..=(MAX_REPORTS as u64 + 3) {
            write_report(&dir, &report(t)).unwrap();
        }
        let mut names = list_reports(&dir);
        names.sort();
        assert_eq!(names.len(), MAX_REPORTS);
        assert!(names.contains(&"crash-4-1.txt".to_owned()));
        assert!(!names.contains(&"crash-3-1.txt".to_owned()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_detects_unclean_shutdown_and_clean_release() {
        let dir = unique_temp_path("velox-crash-marker");
        let (first, prev) = RunMarker::acquire(&dir);
        assert_eq!(prev, PreviousRun::Clean);
        let first = first.expect("marker");
        // 生きている間の 2 つ目は異常終了と見なさない。
        let (second, prev2) = RunMarker::acquire(&dir);
        assert!(second.is_none());
        assert_eq!(prev2, PreviousRun::Clean);
        first.release();
        let (third, prev3) = RunMarker::acquire(&dir);
        assert_eq!(prev3, PreviousRun::Clean);
        // release せずに落とす (= ロックだけ OS が解放し中身が残る)。
        drop(third);
        let (_fourth, prev4) = RunMarker::acquire(&dir);
        assert!(matches!(prev4, PreviousRun::Unclean { started: Some(_) }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_notice_counts_reports_since_previous_start() {
        let dir = unique_temp_path("velox-crash-notice");
        write_report(&dir, &report(5)).unwrap();
        write_report(&dir, &report(50)).unwrap();
        let n = startup_notice(Some(&dir), PreviousRun::Unclean { started: Some(10) }).unwrap();
        assert!(n.contains("1 件"));
        assert_eq!(startup_notice(Some(&dir), PreviousRun::Clean), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
