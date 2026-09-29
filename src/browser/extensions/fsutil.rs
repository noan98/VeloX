//! 拡張機能ホストが使う小さなファイル操作 (Issue #84)。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `path` へ原子的に書く。同じディレクトリの一時ファイルへ書いて `sync` し、
/// `rename` で置き換える。途中で落ちても `path` は「旧内容」か「新内容」の
/// どちらかで、半端な内容にはならない。一時ファイルが残っても次回書き込みで
/// 上書きされ、読み出し側は `path` しか見ない。
pub(super) fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub(super) fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

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
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".corrupt");
    let dest = path.with_file_name(name);
    if fs::rename(path, &dest).is_err() {
        let _ = fs::remove_file(path);
    }
}
