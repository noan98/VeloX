//! Thin file-backed persistence for [`HistoryStore`], [`BookmarkStore`],
//! [`InputHistoryStore`], and [`SitePermissionStore`].
//!
//! All the collection logic (de-duplication, caps, ordering) lives in
//! [`crate::browser::history`] / [`crate::browser::bookmarks`] /
//! [`crate::browser::input_history`] / [`crate::browser::site_permissions`]
//! and is unit-tested in isolation; this
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
use super::settings::Settings;
use super::site_permissions::SitePermissionStore;

const HISTORY_FILE: &str = "history.json";
const BOOKMARKS_FILE: &str = "bookmarks.json";
const INPUT_HISTORY_FILE: &str = "input_history.json";
const SESSION_FILE: &str = "session.json";
const SITE_PERMISSIONS_FILE: &str = "site_permissions.json";
const SETTINGS_FILE: &str = "settings.json";

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

/// Load the site permission store from `dir` (Issue #24 — see
/// docs/decisions.md D60). Any failure (missing file, unreadable, malformed
/// JSON) yields an empty store — every site simply starts back at "ask"
/// rather than the browser failing to start over a damaged permissions
/// file, same as [`load_history`]/[`load_bookmarks`].
pub fn load_site_permissions(dir: &Path) -> SitePermissionStore {
    read_json(&dir.join(SITE_PERMISSIONS_FILE)).unwrap_or_default()
}

/// Persist the site permission store to `dir`, creating the directory if
/// needed.
pub fn save_site_permissions(dir: &Path, store: &SitePermissionStore) -> std::io::Result<()> {
    write_json(dir, &dir.join(SITE_PERMISSIONS_FILE), store)
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

/// Load persisted user settings from `dir` (Issue #30 — see
/// docs/decisions.md D67). `None` for anything that does not parse into a
/// well-formed [`Settings`] (missing file, unreadable, truncated/corrupt
/// JSON, or JSON of the wrong shape) — exactly [`load_session`]'s contract.
/// The caller (`app::run`) falls back to [`Settings::default`] in that case
/// and always runs the result through [`Settings::sanitize`] besides, so a
/// well-formed but hostile/out-of-range value never reaches this far either.
pub fn load_settings(dir: &Path) -> Option<Settings> {
    read_json(&dir.join(SETTINGS_FILE))
}

/// Persist `settings` to `dir`, creating the directory if needed. Called
/// whenever the settings screen's "保存" action succeeds, so a crash or
/// `kill -9` right after saving still leaves the new value in place next
/// launch — the same reasoning `save_session` documents for writing on every
/// change rather than only at exit.
pub fn save_settings(dir: &Path, settings: &Settings) -> std::io::Result<()> {
    write_json(dir, &dir.join(SETTINGS_FILE), settings)
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
        assert_eq!(load_site_permissions(&dir), SitePermissionStore::new());
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

    #[test]
    fn site_permissions_round_trip_through_disk() {
        use super::super::site_permissions::{PermissionDecision, PermissionKind};

        let dir = unique_temp_dir("velox-persist-site-permissions");
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            100,
        );
        store.set(
            "https://tracker.example",
            PermissionKind::Notifications,
            PermissionDecision::Block,
            200,
        );

        save_site_permissions(&dir, &store).expect("save_site_permissions should succeed");
        let loaded = load_site_permissions(&dir);
        assert_eq!(loaded, store);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_site_permissions_file_falls_back_to_an_empty_store() {
        let dir = unique_temp_dir("velox-persist-corrupt-permissions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SITE_PERMISSIONS_FILE), "not json").unwrap();
        assert_eq!(load_site_permissions(&dir), SitePermissionStore::new());

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

    // --- Settings (Issue #30, D67) ---

    #[test]
    fn missing_settings_file_loads_as_none() {
        let dir = unique_temp_dir("velox-persist-settings-missing");
        assert_eq!(load_settings(&dir), None);
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let dir = unique_temp_dir("velox-persist-settings");
        let mut settings = Settings::default();
        settings.general.homepage = "https://example.com/".to_owned();
        settings.privacy.content_blocking_site_exceptions = vec!["example.com".to_owned()];

        save_settings(&dir, &settings).expect("save_settings should succeed");
        let loaded = load_settings(&dir);
        assert_eq!(loaded, Some(settings));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_settings_file_loads_as_none_not_a_panic() {
        let dir = unique_temp_dir("velox-persist-settings-corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SETTINGS_FILE), "not json").unwrap();
        assert_eq!(load_settings(&dir), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_settings_file_loads_as_none() {
        let dir = unique_temp_dir("velox-persist-settings-truncated");
        fs::create_dir_all(&dir).unwrap();
        let full = serde_json::to_string(&Settings::default()).unwrap();
        let truncated = &full[..full.len() / 2];
        fs::write(dir.join(SETTINGS_FILE), truncated).unwrap();
        assert_eq!(load_settings(&dir), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_settings_file_from_an_older_velox_missing_new_fields_still_loads() {
        // Simulates upgrading across a version that added a whole new
        // category (`#[serde(default)]` on every field is what makes this
        // work) — the issue's own "デフォルト値/マイグレーション" criterion.
        let dir = unique_temp_dir("velox-persist-settings-old-shape");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(SETTINGS_FILE),
            r#"{"general":{"homepage":"https://old.example/"}}"#,
        )
        .unwrap();
        let loaded = load_settings(&dir).expect("a partial but valid object should still parse");
        assert_eq!(loaded.general.homepage, "https://old.example/");
        assert_eq!(
            loaded.performance,
            super::super::settings::PerformanceSettings::default()
        );

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
