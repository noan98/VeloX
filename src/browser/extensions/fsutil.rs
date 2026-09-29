//! 拡張機能ホストが使う小さなファイル操作 (Issue #84)。
//!
//! 原子的書き込みは `browser` 層の永続化全体で共有するため
//! [`crate::browser::fsutil`] へ移した (Issue #92)。ここはその再輸出と、
//! 拡張機能ホスト固有の操作だけを持つ。

use std::fs;
use std::path::Path;

pub(super) use crate::browser::fsutil::{atomic_write, tmp_path};

/// ディレクトリを消す。存在しないのは成功扱い。
pub(super) fn remove_dir_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// 壊れたファイルを `<name>.corrupt` へ退避する (既存の退避は上書き)。
/// 退避できなければ削除する。どちらも失敗しても呼び出し側は空状態で続行する。
pub(super) fn quarantine_file(path: &Path) {
    let dest = crate::browser::fsutil::sibling_with_suffix(path, ".corrupt");
    if fs::rename(path, &dest).is_err() {
        let _ = fs::remove_file(path);
    }
}
