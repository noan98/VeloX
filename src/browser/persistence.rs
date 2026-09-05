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
use super::session::SessionSnapshot;

const HISTORY_FILE: &str = "history.json";
const BOOKMARKS_FILE: &str = "bookmarks.json";
const INPUT_HISTORY_FILE: &str = "input_history.json";
const SESSION_FILE: &str = "session.json";

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
pub fn load_bookmarks(dir: &Path) -> BookmarkStore {
    read_json(&dir.join(BOOKMARKS_FILE)).unwrap_or_default()
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

/// Load the last-saved tab session from `dir` (Issue #25 — see
/// docs/decisions.md D65). `None` for anything that does not parse into a
/// well-formed [`SessionSnapshot`]: a missing file (first run, or the
/// feature was just turned on), an unreadable one, truncated/corrupt JSON,
/// or JSON of the wrong shape entirely (e.g. a bare array or a number where
/// an object is expected) — `serde_json` simply fails to deserialize and
/// [`read_json`] turns that into `None`, same as every other store here.
/// The caller (`app::run`) still runs this through
/// [`SessionSnapshot::sanitize`] before trusting it further; this function
/// only answers "did a session file parse at all".
pub fn load_session(dir: &Path) -> Option<SessionSnapshot> {
    read_json(&dir.join(SESSION_FILE))
}

/// Persist the current tab session to `dir`, creating the directory if
/// needed. Called after essentially every tab-affecting change (see
/// `app::persist_session`'s call sites) rather than only at exit, so a
/// session started before an unclean shutdown (a crash, `kill -9`, a power
/// loss) still has something recent to restore from next launch — an exit
/// hook alone would never run in exactly the cases session restore is
/// supposed to help with.
pub fn save_session(dir: &Path, snapshot: &SessionSnapshot) -> std::io::Result<()> {
    write_json(dir, &dir.join(SESSION_FILE), snapshot)
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
    use super::super::session::SavedTab;
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

    // --- Session (Issue #25, D65) ---

    #[test]
    fn missing_session_file_loads_as_none() {
        let dir = unique_temp_dir("velox-persist-session-missing");
        assert_eq!(load_session(&dir), None);
    }

    #[test]
    fn session_round_trips_through_disk() {
        let dir = unique_temp_dir("velox-persist-session");
        let snapshot = SessionSnapshot {
            tabs: vec![
                SavedTab {
                    url: "https://a.example/".to_owned(),
                    title: Some("A".to_owned()),
                    favicon: None,
                },
                SavedTab {
                    url: "https://b.example/".to_owned(),
                    title: None,
                    favicon: Some("https://b.example/favicon.ico".to_owned()),
                },
            ],
            active_index: 1,
        };

        save_session(&dir, &snapshot).expect("save_session should succeed");
        let loaded = load_session(&dir);
        assert_eq!(loaded, Some(snapshot));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_session_file_loads_as_none_not_a_panic() {
        let dir = unique_temp_dir("velox-persist-session-corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SESSION_FILE), "not json").unwrap();
        assert_eq!(load_session(&dir), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_session_file_loads_as_none() {
        let dir = unique_temp_dir("velox-persist-session-truncated");
        fs::create_dir_all(&dir).unwrap();
        // A well-formed session, chopped off mid-object — simulates a write
        // interrupted by a crash or power loss.
        let full = serde_json::to_string(&SessionSnapshot {
            tabs: vec![SavedTab {
                url: "https://a.example/".to_owned(),
                title: Some("A very very very long title indeed".to_owned()),
                favicon: None,
            }],
            active_index: 0,
        })
        .unwrap();
        let truncated = &full[..full.len() / 2];
        fs::write(dir.join(SESSION_FILE), truncated).unwrap();
        assert_eq!(load_session(&dir), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_file_of_the_wrong_json_shape_loads_as_none() {
        let dir = unique_temp_dir("velox-persist-session-wrong-shape");
        fs::create_dir_all(&dir).unwrap();
        // A bare array/number/string instead of the expected object.
        for wrong in ["[1,2,3]", "42", "\"hello\"", "null"] {
            fs::write(dir.join(SESSION_FILE), wrong).unwrap();
            assert_eq!(load_session(&dir), None, "input was {wrong:?}");
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_file_with_wrong_field_types_loads_as_none() {
        let dir = unique_temp_dir("velox-persist-session-wrong-types");
        fs::create_dir_all(&dir).unwrap();
        // `active_index` as a string, `tabs` as an object instead of an
        // array — both should fail to deserialize rather than panicking or
        // silently coercing into something unintended.
        fs::write(
            dir.join(SESSION_FILE),
            r#"{"tabs":"not-an-array","active_index":"zero"}"#,
        )
        .unwrap();
        assert_eq!(load_session(&dir), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn very_large_session_file_still_round_trips() {
        // "巨大なファイル" from the issue's acceptance criteria — a session
        // with many tabs must not fail to load or blow up memory unusually;
        // `serde_json` streams through `fs::read_to_string` just like every
        // other store here, so this exercises that path at a size no real
        // user session would ever reach.
        let dir = unique_temp_dir("velox-persist-session-large");
        let tabs: Vec<SavedTab> = (0..20_000)
            .map(|i| SavedTab {
                url: format!("https://{i}.example/"),
                title: Some(format!("Tab {i}")),
                favicon: None,
            })
            .collect();
        let snapshot = SessionSnapshot {
            tabs,
            active_index: 10_000,
        };

        save_session(&dir, &snapshot).expect("save_session should succeed");
        let loaded = load_session(&dir).expect("a large well-formed file should still load");
        assert_eq!(loaded.tabs.len(), 20_000);
        assert_eq!(loaded.active_index, 10_000);

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
