//! 拡張機能パッケージ (インストール前の、まだ信頼していないファイル一式)。
//!
//! docs/extensions.md §5 install の (1)(2)(3): サイズ・ファイル数の上限、全パスの
//! 検査 (zip slip: 絶対パス・`..`・`\`・隠しファイルを含むパッケージは**丸ごと拒否**)、
//! `manifest.json` の検証。パッケージは「パス → バイト列」のメモリ上の表現で、
//! ディレクトリからの読み込み (`from_dir`) はシンボリックリンクを拒否する。
//! アーカイブ形式 (zip 等) の展開はここでは扱わない (依存を増やさない。D163)。

use std::collections::BTreeMap;
use std::path::Path;

use crate::browser::extension_manifest::{parse_manifest, Manifest, ManifestError};

pub const MANIFEST_FILE: &str = "manifest.json";
/// パッケージ内のファイル数の上限。
pub const MAX_PACKAGE_FILES: usize = 256;
/// 1 ファイルの最大バイト数。
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// パッケージ全体の最大バイト数 (zip bomb 対策)。
pub const MAX_PACKAGE_BYTES: usize = 8 * 1024 * 1024;
/// パッケージ内パスの最大バイト数。
pub const MAX_PACKAGE_PATH_LEN: usize = 200;
/// ディレクトリの最大深さ。
pub const MAX_PACKAGE_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageError {
    InvalidPath(String),
    /// 大文字小文字だけが違うパス (Windows / macOS で衝突する)、または
    /// ファイルとディレクトリが衝突するパス。
    ConflictingPath(String),
    TooManyFiles,
    FileTooLarge(String),
    PackageTooLarge,
    MissingManifest,
    Manifest(ManifestError),
    /// マニフェストが宣言したスクリプトがパッケージに無い。
    MissingResource(String),
    /// ディレクトリからの読み込みでシンボリックリンク等を見つけた。
    UnsupportedFileType(String),
    Io(String),
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageError::InvalidPath(p) => write!(f, "パッケージ内のパス `{p}` が不正"),
            PackageError::ConflictingPath(p) => write!(f, "パス `{p}` が他のパスと衝突する"),
            PackageError::TooManyFiles => {
                write!(f, "ファイル数が上限 {MAX_PACKAGE_FILES} を超える")
            }
            PackageError::FileTooLarge(p) => write!(f, "`{p}` が大きすぎる"),
            PackageError::PackageTooLarge => write!(f, "パッケージが大きすぎる"),
            PackageError::MissingManifest => write!(f, "{MANIFEST_FILE} が無い"),
            PackageError::Manifest(e) => write!(f, "{e}"),
            PackageError::MissingResource(p) => write!(f, "宣言されたファイル `{p}` が無い"),
            PackageError::UnsupportedFileType(p) => {
                write!(f, "`{p}` は通常のファイルではない")
            }
            PackageError::Io(e) => write!(f, "読み込みに失敗: {e}"),
        }
    }
}

impl std::error::Error for PackageError {}

/// Windows で予約されたデバイス名 (拡張子付きでも開けない)。
const RESERVED_DEVICE_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// パッケージ内パスの形を検査する。相対・`/` 区切り・`[A-Za-z0-9._-]` のみ・
/// 隠しファイル (先頭 `.`) 不可・Windows の予約名と末尾 `.` 不可。
pub fn validate_package_path(path: &str) -> Result<(), PackageError> {
    let bad = || PackageError::InvalidPath(path.chars().take(64).collect());
    if path.is_empty() || path.len() > MAX_PACKAGE_PATH_LEN {
        return Err(bad());
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_'))
    {
        return Err(bad());
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > MAX_PACKAGE_DEPTH {
        return Err(bad());
    }
    for seg in segments {
        if seg.is_empty() || seg.starts_with('.') || seg.ends_with('.') {
            return Err(bad());
        }
        let stem = seg.split('.').next().unwrap_or(seg).to_ascii_lowercase();
        if RESERVED_DEVICE_NAMES.contains(&stem.as_str()) {
            return Err(bad());
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionPackage {
    files: BTreeMap<String, Vec<u8>>,
    total_bytes: usize,
}

impl ExtensionPackage {
    pub fn new() -> ExtensionPackage {
        ExtensionPackage::default()
    }

    /// ファイルを 1 つ足す。パス・サイズ・件数・衝突を検査し、違反は**パッケージ
    /// ごと拒否**するのが呼び出し側の責務 (エラーを受けたらパッケージを捨てる)。
    pub fn insert(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), PackageError> {
        validate_package_path(path)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(PackageError::FileTooLarge(path.chars().take(64).collect()));
        }
        let lower = path.to_ascii_lowercase();
        for existing in self.files.keys() {
            let e = existing.to_ascii_lowercase();
            let conflict = e == lower
                || lower.starts_with(&format!("{e}/"))
                || e.starts_with(&format!("{lower}/"));
            if conflict {
                return Err(PackageError::ConflictingPath(
                    path.chars().take(64).collect(),
                ));
            }
        }
        if self.files.len() >= MAX_PACKAGE_FILES {
            return Err(PackageError::TooManyFiles);
        }
        if self.total_bytes + bytes.len() > MAX_PACKAGE_BYTES {
            return Err(PackageError::PackageTooLarge);
        }
        self.total_bytes += bytes.len();
        self.files.insert(path.to_owned(), bytes);
        Ok(())
    }

    pub fn with_file(
        mut self,
        path: &str,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<Self, PackageError> {
        self.insert(path, bytes.into())?;
        Ok(self)
    }

    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }

    /// ディレクトリから読む。シンボリックリンクなど通常のファイル/ディレクトリ
    /// 以外は拒否する (パッケージ外のファイルを取り込ませない)。
    pub fn from_dir(dir: &Path) -> Result<ExtensionPackage, PackageError> {
        let mut pkg = ExtensionPackage::new();
        Self::walk(dir, "", 0, &mut pkg)?;
        Ok(pkg)
    }

    fn walk(
        dir: &Path,
        prefix: &str,
        depth: usize,
        pkg: &mut ExtensionPackage,
    ) -> Result<(), PackageError> {
        if depth > MAX_PACKAGE_DEPTH {
            return Err(PackageError::InvalidPath(prefix.chars().take(64).collect()));
        }
        let rd = std::fs::read_dir(dir).map_err(|e| PackageError::Io(e.to_string()))?;
        for entry in rd {
            let entry = entry.map_err(|e| PackageError::Io(e.to_string()))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| PackageError::InvalidPath("(non-UTF-8 name)".to_owned()))?;
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let meta = std::fs::symlink_metadata(entry.path())
                .map_err(|e| PackageError::Io(e.to_string()))?;
            if meta.is_dir() {
                validate_package_path(&rel)?;
                Self::walk(&entry.path(), &rel, depth + 1, pkg)?;
            } else if meta.is_file() {
                validate_package_path(&rel)?;
                // 読む前にサイズを見る (巨大ファイルをメモリに載せない)。
                if meta.len() > MAX_FILE_BYTES as u64 {
                    return Err(PackageError::FileTooLarge(rel.chars().take(64).collect()));
                }
                let bytes =
                    std::fs::read(entry.path()).map_err(|e| PackageError::Io(e.to_string()))?;
                pkg.insert(&rel, bytes)?;
            } else {
                return Err(PackageError::UnsupportedFileType(
                    rel.chars().take(64).collect(),
                ));
            }
        }
        Ok(())
    }

    /// マニフェストを検証して返し、宣言されたスクリプトがすべて在ることを確かめる。
    pub fn validate(&self) -> Result<Manifest, PackageError> {
        let bytes = self
            .files
            .get(MANIFEST_FILE)
            .ok_or(PackageError::MissingManifest)?;
        let manifest = parse_manifest(bytes).map_err(PackageError::Manifest)?;
        check_declared_resources(&manifest, |p| self.files.contains_key(p))?;
        Ok(manifest)
    }

    /// `dir` (存在しない新規ディレクトリ) へ全ファイルを書く。原子性は呼び出し側が
    /// 一時領域 → rename で確保する。
    pub(super) fn write_to(&self, dir: &Path) -> std::io::Result<()> {
        for (rel, bytes) in &self.files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        // 空のパッケージ (あり得ないが) でもディレクトリは作る。
        std::fs::create_dir_all(dir)
    }
}

/// マニフェストが宣言した `content_scripts.js` / `background.script` の実在を検査する。
pub(super) fn check_declared_resources(
    manifest: &Manifest,
    exists: impl Fn(&str) -> bool,
) -> Result<(), PackageError> {
    let declared = manifest
        .content_scripts
        .iter()
        .flat_map(|c| c.js.iter())
        .chain(manifest.background.iter().map(|b| &b.script));
    for path in declared {
        if !exists(path) {
            return Err(PackageError::MissingResource(path.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::util::unique_temp_path;

    fn manifest_json() -> &'static str {
        r#"{"manifest_version":1,"id":"com.example.pkg","name":"Pkg","version":"1.0.0",
            "background":{"script":"bg.js"}}"#
    }

    #[test]
    fn valid_package_validates() {
        let pkg = ExtensionPackage::new()
            .with_file("manifest.json", manifest_json())
            .unwrap()
            .with_file("bg.js", "// bg")
            .unwrap();
        assert_eq!(pkg.validate().unwrap().id, "com.example.pkg");
    }

    #[test]
    fn missing_manifest_and_missing_declared_script_are_rejected() {
        assert_eq!(
            ExtensionPackage::new().validate(),
            Err(PackageError::MissingManifest)
        );
        let pkg = ExtensionPackage::new()
            .with_file("manifest.json", manifest_json())
            .unwrap();
        assert_eq!(
            pkg.validate(),
            Err(PackageError::MissingResource("bg.js".to_owned()))
        );
    }

    #[test]
    fn path_traversal_and_odd_paths_are_rejected() {
        for bad in [
            "../evil.js",
            "a/../../evil.js",
            "/abs.js",
            "C:/x.js",
            "a\\b.js",
            ".hidden",
            "dir/.git/config",
            "a//b.js",
            "trailing./x.js",
            "con.js",
            "dir/NUL.txt",
            "sp ace.js",
            "",
        ] {
            let mut pkg = ExtensionPackage::new();
            assert!(
                matches!(pkg.insert(bad, vec![]), Err(PackageError::InvalidPath(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn case_and_file_dir_conflicts_are_rejected() {
        let mut pkg = ExtensionPackage::new();
        pkg.insert("a/b.js", vec![]).unwrap();
        assert!(matches!(
            pkg.insert("A/B.js", vec![]),
            Err(PackageError::ConflictingPath(_))
        ));
        assert!(matches!(
            pkg.insert("a", vec![]),
            Err(PackageError::ConflictingPath(_))
        ));
        assert!(matches!(
            pkg.insert("a/b.js/c", vec![]),
            Err(PackageError::ConflictingPath(_))
        ));
    }

    #[test]
    fn size_and_count_limits() {
        let mut pkg = ExtensionPackage::new();
        assert!(matches!(
            pkg.insert("big.js", vec![0; MAX_FILE_BYTES + 1]),
            Err(PackageError::FileTooLarge(_))
        ));
        let mut pkg = ExtensionPackage::new();
        for i in 0..MAX_PACKAGE_BYTES / MAX_FILE_BYTES {
            pkg.insert(&format!("f{i}.bin"), vec![0; MAX_FILE_BYTES])
                .unwrap();
        }
        assert_eq!(
            pkg.insert("one-more.bin", vec![0; 1]),
            Err(PackageError::PackageTooLarge)
        );
        let mut pkg = ExtensionPackage::new();
        for i in 0..MAX_PACKAGE_FILES {
            pkg.insert(&format!("f{i}.txt"), vec![]).unwrap();
        }
        assert_eq!(pkg.insert("x.txt", vec![]), Err(PackageError::TooManyFiles));
    }

    #[test]
    fn from_dir_reads_nested_files_and_write_to_round_trips() {
        let src = unique_temp_path("velox-ext-pkg-src");
        std::fs::create_dir_all(src.join("content")).unwrap();
        std::fs::write(src.join("manifest.json"), manifest_json()).unwrap();
        std::fs::write(src.join("bg.js"), "// bg").unwrap();
        std::fs::write(src.join("content/main.js"), "// c").unwrap();
        let pkg = ExtensionPackage::from_dir(&src).unwrap();
        assert_eq!(pkg.files().len(), 3);
        assert!(pkg.validate().is_ok());

        let out = unique_temp_path("velox-ext-pkg-out");
        pkg.write_to(&out).unwrap();
        assert_eq!(ExtensionPackage::from_dir(&out).unwrap(), pkg);
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn from_dir_rejects_bad_names_inside_the_directory() {
        let src = unique_temp_path("velox-ext-pkg-bad");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".hidden"), "x").unwrap();
        assert!(matches!(
            ExtensionPackage::from_dir(&src),
            Err(PackageError::InvalidPath(_))
        ));
        std::fs::remove_dir_all(&src).ok();
    }

    #[cfg(unix)]
    #[test]
    fn from_dir_rejects_symlinks() {
        let src = unique_temp_path("velox-ext-pkg-link");
        std::fs::create_dir_all(&src).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.join("link.txt")).unwrap();
        assert!(matches!(
            ExtensionPackage::from_dir(&src),
            Err(PackageError::UnsupportedFileType(_))
        ));
        std::fs::remove_dir_all(&src).ok();
    }
}
