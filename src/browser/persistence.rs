//! Thin file-backed persistence for [`HistoryStore`], [`BookmarkStore`], and
//! [`InputHistoryStore`].
//!
//! All the collection logic (de-duplication, caps, ordering) lives in
//! [`crate::browser::history`] / [`crate::browser::bookmarks`] /
//! [`crate::browser::input_history`] and is unit-tested in isolation; this
//! module is deliberately "dumb": read a JSON file into a store, or write a
//! store out as JSON. Callers treat every failure here as non-fatal (see
//! `app.rs`'s `log_failure` pattern) — a missing, corrupt, or unwritable
//! data directory degrades to an in-memory, non-persisted session rather
//! than crashing the browser.

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::bookmarks::BookmarkStore;
use super::history::HistoryStore;
use super::input_history::InputHistoryStore;

const HISTORY_FILE: &str = "history.json";
const BOOKMARKS_FILE: &str = "bookmarks.json";
const INPUT_HISTORY_FILE: &str = "input_history.json";

/// Resolve the directory VeloX stores its history/bookmarks files in.
///
/// `VELOX_DATA_DIR`, when set, always wins (used by tests and for portable
/// installs). Otherwise this follows each platform's usual convention for
/// per-user application data, resolved from environment variables that are
/// already present rather than a `dirs`-style crate — see docs/decisions.md
/// D10. Returns `None` when no suitable environment variable is set, in which
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
///
/// Also repairs any entry left with a dangling `folder_id` (a folder id
/// that does not name a folder actually present in the same file) back to
/// the root level — see [`BookmarkStore::repair_dangling_folder_ids`] for
/// why plain deserialization alone cannot be trusted to keep that invariant
/// (Issue #35, docs/decisions.md D62).
pub fn load_bookmarks(dir: &Path) -> BookmarkStore {
    let mut store: BookmarkStore = read_json(&dir.join(BOOKMARKS_FILE)).unwrap_or_default();
    store.repair_dangling_folder_ids();
    store
}

/// Persist the bookmark store to `dir`, creating the directory if needed.
pub fn save_bookmarks(dir: &Path, store: &BookmarkStore) -> std::io::Result<()> {
    write_json(dir, &dir.join(BOOKMARKS_FILE), store)
}

/// Load the typed-search-query history store from `dir` (Issue #20 — see
/// docs/decisions.md D38). Any failure yields an empty store, same as
/// [`load_history`]/[`load_bookmarks`].
pub fn load_input_history(dir: &Path) -> InputHistoryStore {
    read_json(&dir.join(INPUT_HISTORY_FILE)).unwrap_or_default()
}

/// Persist the typed-search-query history store to `dir`, creating the
/// directory if needed.
pub fn save_input_history(dir: &Path, store: &InputHistoryStore) -> std::io::Result<()> {
    write_json(dir, &dir.join(INPUT_HISTORY_FILE), store)
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
        assert_eq!(load_input_history(&dir), InputHistoryStore::new());
    }

    #[test]
    fn input_history_round_trips_through_disk() {
        let dir = unique_temp_dir("velox-persist-input-history");
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 100, 0);
        store.record("rust async", 200, 0);

        save_input_history(&dir, &store).expect("save_input_history should succeed");
        let loaded = load_input_history(&dir);
        assert_eq!(loaded, store);

        fs::remove_dir_all(&dir).ok();
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

    // --- Robustness against a hostile/broken data directory (Issue #35):
    // every one of `load_history`/`load_bookmarks`/`load_input_history`
    // must fall back to an empty store rather than panicking, no matter
    // what is actually on disk. ---

    #[test]
    fn truncated_json_falls_back_to_an_empty_store_for_every_store() {
        let dir = unique_temp_dir("velox-persist-truncated");
        fs::create_dir_all(&dir).unwrap();
        // A file that starts out as valid-looking JSON but is cut off
        // mid-value, e.g. by a crash or a full disk during a previous save.
        fs::write(dir.join(HISTORY_FILE), r#"{"entries":[{"id":1,"url":"#).unwrap();
        fs::write(dir.join(BOOKMARKS_FILE), r#"{"entries":[{"id":1,"#).unwrap();
        fs::write(dir.join(INPUT_HISTORY_FILE), r#"{"entries":[{"text":"#).unwrap();

        assert_eq!(load_history(&dir), HistoryStore::new());
        assert_eq!(load_bookmarks(&dir), BookmarkStore::new());
        assert_eq!(load_input_history(&dir), InputHistoryStore::new());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_falls_back_to_an_empty_store() {
        let dir = unique_temp_dir("velox-persist-empty-file");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(HISTORY_FILE), "").unwrap();
        assert_eq!(load_history(&dir), HistoryStore::new());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn json_of_the_wrong_shape_falls_back_to_an_empty_store() {
        let dir = unique_temp_dir("velox-persist-wrong-shape");
        fs::create_dir_all(&dir).unwrap();
        // Valid JSON, but not the shape any of these stores expect (e.g. a
        // bare array, or an object missing every required field).
        fs::write(dir.join(HISTORY_FILE), "[1,2,3]").unwrap();
        fs::write(dir.join(BOOKMARKS_FILE), r#"{"unexpected":"shape"}"#).unwrap();
        fs::write(dir.join(INPUT_HISTORY_FILE), "\"just a string\"").unwrap();

        assert_eq!(load_history(&dir), HistoryStore::new());
        assert_eq!(load_bookmarks(&dir), BookmarkStore::new());
        assert_eq!(load_input_history(&dir), InputHistoryStore::new());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn json_with_wrong_field_types_falls_back_to_an_empty_store() {
        let dir = unique_temp_dir("velox-persist-wrong-types");
        fs::create_dir_all(&dir).unwrap();
        // `id` should be a `u64`, `visited_at` a `u64` — strings/negative
        // numbers here must fail deserialization cleanly, not panic.
        fs::write(
            dir.join(HISTORY_FILE),
            r#"{"entries":[{"id":"not-a-number","url":"https://example.com/","title":null,"visited_at":-1}],"next_id":2}"#,
        )
        .unwrap();
        assert_eq!(load_history(&dir), HistoryStore::new());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deeply_nested_json_does_not_panic() {
        // Same JSON-bomb shape `ui::toolbar`'s IPC parser is hardened
        // against — the store loader goes through the same `serde_json`
        // parser and must be equally immune to a stack overflow.
        let dir = unique_temp_dir("velox-persist-deep-nesting");
        fs::create_dir_all(&dir).unwrap();
        let depth = 100_000;
        let bomb = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        fs::write(dir.join(HISTORY_FILE), &bomb).unwrap();
        assert_eq!(load_history(&dir), HistoryStore::new());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_huge_but_well_formed_history_file_still_loads() {
        // The inverse of the size-limiting concerns elsewhere in this
        // issue: a large *legitimate* history file (many entries
        // accumulated over a long-lived profile) must still load — nothing
        // here should impose an accidental low ceiling.
        let dir = unique_temp_dir("velox-persist-huge-legit");
        let mut store = HistoryStore::new();
        for i in 0..20_000u64 {
            store.record_visit(&format!("https://example.com/{i}"), None, i, 0);
        }
        save_history(&dir, &store).expect("save_history should succeed");
        let loaded = load_history(&dir);
        assert_eq!(loaded.entries().len(), 20_000);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_bookmarks_repairs_a_dangling_folder_id_on_disk() {
        // End-to-end version of `bookmarks::tests::
        // repair_dangling_folder_ids_reparents_unknown_folders_to_root` —
        // confirms `load_bookmarks` (not just the store method in
        // isolation) actually calls the repair step on a real file (Issue
        // #35, docs/decisions.md D62).
        let dir = unique_temp_dir("velox-persist-dangling-folder");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(BOOKMARKS_FILE),
            r#"{
                "entries": [
                    {"id": 1, "url": "https://example.com/", "title": null, "created_at": 100, "folder_id": 999}
                ],
                "next_id": 2,
                "folders": [],
                "next_folder_id": 1
            }"#,
        )
        .unwrap();

        let store = load_bookmarks(&dir);
        assert_eq!(store.entries()[0].folder_id, None);
        assert_eq!(store.entries_in(None).count(), 1);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_null_byte_in_the_json_file_is_handled_without_panicking() {
        let dir = unique_temp_dir("velox-persist-nul-byte");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(HISTORY_FILE), b"{\"entries\":[]\0,\"next_id\":1}").unwrap();
        // No expectation on the exact outcome, only that loading a file
        // containing a stray NUL byte cannot panic.
        let _ = load_history(&dir);

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
