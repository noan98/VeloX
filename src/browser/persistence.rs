//! Thin file-backed persistence for [`HistoryStore`] and [`BookmarkStore`].
//!
//! All the collection logic (de-duplication, caps, ordering) lives in
//! [`crate::browser::history`] / [`crate::browser::bookmarks`] and is
//! unit-tested in isolation; this module is deliberately "dumb": read a JSON
//! file into a store, or write a store out as JSON. Callers treat every
//! failure here as non-fatal (see `app.rs`'s `log_failure` pattern) — a
//! missing, corrupt, or unwritable data directory degrades to an in-memory,
//! non-persisted session rather than crashing the browser.

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::bookmarks::BookmarkStore;
use super::history::HistoryStore;

const HISTORY_FILE: &str = "history.json";
const BOOKMARKS_FILE: &str = "bookmarks.json";

/// Resolve the directory VeloX stores its history/bookmarks files in.
///
/// `VELOX_DATA_DIR`, when set, always wins (used by tests and for portable
/// installs). Otherwise this follows each platform's usual convention for
/// per-user application data, resolved from environment variables that are
/// already present rather than a `dirs`-style crate — see docs/decisions.md
/// D8. Returns `None` when no suitable environment variable is set, in which
/// case history/bookmarks simply are not persisted for that run.
pub fn default_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VELOX_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    platform_data_dir()
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/VeloX"))
}

#[cfg(target_os = "windows")]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("VeloX"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("velox"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/velox"))
}

/// Load the history store from `dir`. Any failure (missing file, unreadable,
/// malformed JSON) yields an empty store rather than an error.
pub fn load_history(dir: &Path) -> HistoryStore {
    read_json(&dir.join(HISTORY_FILE)).unwrap_or_default()
}

/// Persist the history store to `dir`, creating the directory if needed.
pub fn save_history(dir: &Path, store: &HistoryStore) -> std::io::Result<()> {
    write_json(dir, &dir.join(HISTORY_FILE), store)
}

/// Load the bookmark store from `dir`. Any failure (missing file,
/// unreadable, malformed JSON) yields an empty store rather than an error.
pub fn load_bookmarks(dir: &Path) -> BookmarkStore {
    read_json(&dir.join(BOOKMARKS_FILE)).unwrap_or_default()
}

/// Persist the bookmark store to `dir`, creating the directory if needed.
pub fn save_bookmarks(dir: &Path, store: &BookmarkStore) -> std::io::Result<()> {
    write_json(dir, &dir.join(BOOKMARKS_FILE), store)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn write_json<T: Serialize>(dir: &Path, path: &Path, value: &T) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let data = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_load_as_empty_stores() {
        let dir = unique_temp_dir("velox-persist-missing");
        assert_eq!(load_history(&dir), HistoryStore::new());
        assert_eq!(load_bookmarks(&dir), BookmarkStore::new());
    }

    #[test]
    fn history_round_trips_through_disk() {
        let dir = unique_temp_dir("velox-persist-history");
        let mut store = HistoryStore::new();
        store.record_visit("https://example.com/", Some("Example".to_owned()), 100, 0);
        store.record_visit("https://rust-lang.org/", None, 200, 0);

        save_history(&dir, &store).expect("save_history should succeed");
        let loaded = load_history(&dir);
        assert_eq!(loaded, store);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bookmarks_round_trip_through_disk() {
        let dir = unique_temp_dir("velox-persist-bookmarks");
        let mut store = BookmarkStore::new();
        store.add("https://example.com/", Some("Example".to_owned()), 100);

        save_bookmarks(&dir, &store).expect("save_bookmarks should succeed");
        let loaded = load_bookmarks(&dir);
        assert_eq!(loaded, store);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = unique_temp_dir("velox-persist-mkdir")
            .join("nested")
            .join("data");
        assert!(!dir.exists());
        save_history(&dir, &HistoryStore::new()).expect("save_history should create the dir");
        assert!(dir.join(HISTORY_FILE).exists());

        fs::remove_dir_all(unique_temp_dir("velox-persist-mkdir")).ok();
    }

    #[test]
    fn corrupt_file_falls_back_to_an_empty_store() {
        let dir = unique_temp_dir("velox-persist-corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(HISTORY_FILE), "not json").unwrap();
        assert_eq!(load_history(&dir), HistoryStore::new());

        fs::remove_dir_all(&dir).ok();
    }

    /// A per-test temp directory under the OS temp dir, distinguished by
    /// `label` plus the current thread so parallel tests never collide.
    fn unique_temp_dir(label: &str) -> PathBuf {
        let unique = format!(
            "{label}-{:?}-{}",
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        std::env::temp_dir().join(unique)
    }
}
