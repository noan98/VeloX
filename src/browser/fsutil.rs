//! `browser` 層の永続化が共有する小さなファイル操作。
//!
//! もとは拡張機能ホスト (Issue #84) 専用だった `extensions::fsutil` の
//! 原子的書き込みを、履歴・ブックマーク・設定などのストア
//! (`persistence` / `migration`、Issue #92) からも使えるようにここへ移した。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `path` へ原子的に書く。同じディレクトリの一時ファイルへ書いて `sync` し、
/// `rename` で置き換える。途中で落ちても `path` は「旧内容」か「新内容」の
/// どちらかで、半端な内容にはならない。一時ファイルが残っても次回書き込みで
/// 上書きされ、読み出し側は `path` しか見ない。
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
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

/// [`atomic_write`] が使う一時ファイルのパス (`<name>.tmp`)。
pub(crate) fn tmp_path(path: &Path) -> PathBuf {
    sibling_with_suffix(path, ".tmp")
}

/// `path` と同じディレクトリで、ファイル名の末尾に `suffix` を足したパス。
pub(crate) fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}
