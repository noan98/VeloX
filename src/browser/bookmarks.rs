//! Bookmarks: a pure, in-memory collection of saved pages, organized into an
//! optional single layer of folders.
//!
//! Unlike [`crate::browser::history`], entries are de-duplicated by URL —
//! a page is either bookmarked or it is not, there is no concept of visiting
//! a bookmark "again". File IO lives in [`crate::browser::persistence`].
//!
//! See docs/decisions.md D32 for why folders are a flat `folder_id`
//! reference (one level, no nesting) rather than a tree, and D34 for the
//! manual-reorder scheme (`move_up`/`move_down`).

use serde::{Deserialize, Serialize};

/// One bookmarked page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkEntry {
    /// Stable id, unique within one store, used by the UI to reference an
    /// entry (e.g. for removal) without relying on URL or position.
    pub id: u64,
    pub url: String,
    pub title: Option<String>,
    /// Unix timestamp (seconds) the bookmark was created.
    pub created_at: u64,
    /// The folder this bookmark belongs to, or `None` for the root level
    /// (see docs/decisions.md D32). Always either `None` or the id of an
    /// entry currently in [`BookmarkStore::folders`] — [`BookmarkStore`]
    /// maintains that invariant itself ([`BookmarkStore::edit`] silently
    /// falls back to `None` for an unknown folder id, and
    /// [`BookmarkStore::remove_folder`] reparents every entry in the
    /// removed folder back to `None`), so no other code needs to re-check
    /// it. `#[serde(default)]` lets a `bookmarks.json` written by the
    /// pre-#19 store (Issue #4) — which has no `folder_id` key at all —
    /// deserialize cleanly instead of failing the whole file (every such
    /// entry lands at the root, which is exactly where it already was).
    #[serde(default)]
    pub folder_id: Option<u64>,
    /// A URL the UI can point an `<img>` at for this bookmark's favicon,
    /// filled in asynchronously the same way `HistoryEntry::favicon` is
    /// (see `UserEvent::FaviconResolved` / `BookmarkStore::update_favicon_by_url`
    /// and docs/decisions.md D34). `None` until one arrives, or forever for
    /// a URL the resolver never found an icon for. `#[serde(default)]` for
    /// the same pre-#19-format reason as `folder_id` above.
    #[serde(default)]
    pub favicon: Option<String>,
}

/// One bookmark folder (see docs/decisions.md D32: a single flat layer,
/// folders cannot contain folders).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkFolder {
    pub id: u64,
    pub name: String,
    /// Unix timestamp (seconds) the folder was created.
    pub created_at: u64,
}

/// Why [`BookmarkStore::edit`] refused to apply an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarkEditError {
    /// No entry with the given id exists.
    NotFound,
    /// The new URL already belongs to a *different* bookmark — applying the
    /// edit would violate the store's URL de-duplication invariant (see the
    /// module doc comment and [`BookmarkStore::add`]).
    DuplicateUrl,
}

/// An ordered collection of [`BookmarkEntry`] values plus their
/// [`BookmarkFolder`]s.
///
/// Entry order within the `entries` vector *is* display/manual order (see
/// docs/decisions.md D34) — new bookmarks are appended at the end, and
/// [`Self::move_up`]/[`Self::move_down`] physically reorder the vector
/// rather than maintaining a separate position field. [`Self::entries_in`]
/// filters by `folder_id` while preserving that order, which is what both
/// the bookmarks panel and the bookmark bar render from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkStore {
    entries: Vec<BookmarkEntry>,
    next_id: u64,
    /// `#[serde(default)]`/`#[serde(default = "default_next_folder_id")]`
    /// give a pre-#19 `bookmarks.json` (no folders at all) an empty folder
    /// list and a `next_folder_id` of `1`, exactly like a freshly created
    /// store — see docs/decisions.md D32.
    #[serde(default)]
    folders: Vec<BookmarkFolder>,
    #[serde(default = "default_next_folder_id")]
    next_folder_id: u64,
}

fn default_next_folder_id() -> u64 {
    1
}

impl Default for BookmarkStore {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
            folders: Vec::new(),
            next_folder_id: 1,
        }
    }
}

impl BookmarkStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// All entries, in display/manual order (see the struct doc comment).
    pub fn entries(&self) -> &[BookmarkEntry] {
        &self.entries
    }

    /// Entries, most recently created first — kept for callers that still
    /// want pure recency order (e.g. omnibox candidate ranking); the
    /// bookmarks panel/bar use [`Self::entries_in`] instead so manual
    /// reordering is visible (see docs/decisions.md D34).
    pub fn entries_newest_first(&self) -> impl Iterator<Item = &BookmarkEntry> {
        self.entries.iter().rev()
    }

    /// Entries belonging to `folder_id` (`None` = root level), in display
    /// order.
    pub fn entries_in(&self, folder_id: Option<u64>) -> impl Iterator<Item = &BookmarkEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.folder_id == folder_id)
    }

    /// Every folder, in creation order.
    pub fn folders(&self) -> &[BookmarkFolder] {
        &self.folders
    }

    /// Whether `url` is already bookmarked.
    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.entries.iter().any(|entry| entry.url == url)
    }

    /// Bookmark `url`, returning its id. Always lands at the root level
    /// (`folder_id: None`) — organizing it into a folder afterward is
    /// [`Self::edit`]'s job, keeping "bookmark this page" a true one-action
    /// operation (see the issue's "現在ページをワンアクションで登録できる").
    ///
    /// De-duplicates by URL: bookmarking an already-bookmarked page updates
    /// its title (when `title` is `Some`) instead of creating a second
    /// entry, and its id, position and folder are unchanged.
    pub fn add(&mut self, url: &str, title: Option<String>, created_at: u64) -> u64 {
        if let Some(existing) = self.entries.iter_mut().find(|entry| entry.url == url) {
            if title.is_some() {
                existing.title = title;
            }
            return existing.id;
        }

        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(BookmarkEntry {
            id,
            url: url.to_owned(),
            title,
            created_at,
            folder_id: None,
            favicon: None,
        });
        id
    }

    /// Remove the bookmark with the given id. Returns `true` when a
    /// bookmark was removed.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// Remove the bookmark for `url`, if any. Returns `true` when a
    /// bookmark was removed.
    pub fn remove_by_url(&mut self, url: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.url != url);
        self.entries.len() != before
    }

    /// Toggle the bookmark state of `url`: removes it if present, otherwise
    /// adds it. Returns the resulting state (`true` = now bookmarked).
    pub fn toggle(&mut self, url: &str, title: Option<String>, created_at: u64) -> bool {
        if self.remove_by_url(url) {
            false
        } else {
            self.add(url, title, created_at);
            true
        }
    }

    /// Update title, URL, and folder of an existing bookmark. `url` is
    /// expected to already be normalized by the caller (see
    /// `browser::navigation::normalize_input` — this store never parses or
    /// rejects a URL itself, that stays the one place scheme allow-listing
    /// happens; docs/decisions.md D33).
    ///
    /// `folder_id`, when `Some` and not a known folder id, is silently
    /// treated as `None` (moved to the root) rather than left dangling —
    /// this keeps `BookmarkEntry::folder_id`'s "always `None` or a real
    /// folder" invariant true no matter what a caller passes in (see
    /// docs/decisions.md D32).
    ///
    /// Refuses (leaving the store unchanged) when `id` is unknown
    /// ([`BookmarkEditError::NotFound`]) or `url` already belongs to a
    /// *different* bookmark ([`BookmarkEditError::DuplicateUrl`]) — the
    /// same de-duplication invariant [`Self::add`] maintains.
    pub fn edit(
        &mut self,
        id: u64,
        title: Option<String>,
        url: String,
        folder_id: Option<u64>,
    ) -> Result<(), BookmarkEditError> {
        if self.entries.iter().any(|e| e.id != id && e.url == url) {
            return Err(BookmarkEditError::DuplicateUrl);
        }
        let folder_id = folder_id.filter(|fid| self.folders.iter().any(|f| f.id == *fid));
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.title = title;
                entry.url = url;
                entry.folder_id = folder_id;
                Ok(())
            }
            None => Err(BookmarkEditError::NotFound),
        }
    }

    /// Move the nearest neighbor, within the same [`Self::entries_in`]
    /// scope (same `folder_id`), a rank earlier. Returns `true` when a swap
    /// happened — `false` for an unknown id or an entry already first in
    /// its folder/root scope.
    pub fn move_up(&mut self, id: u64) -> bool {
        self.swap_with_neighbor(id, Direction::Up)
    }

    /// Same as [`Self::move_up`], one rank later.
    pub fn move_down(&mut self, id: u64) -> bool {
        self.swap_with_neighbor(id, Direction::Down)
    }

    fn swap_with_neighbor(&mut self, id: u64, direction: Direction) -> bool {
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            return false;
        };
        let folder_id = self.entries[index].folder_id;
        let neighbor = match direction {
            Direction::Up => self.entries[..index]
                .iter()
                .rposition(|entry| entry.folder_id == folder_id),
            Direction::Down => self.entries[index + 1..]
                .iter()
                .position(|entry| entry.folder_id == folder_id)
                .map(|offset| index + 1 + offset),
        };
        match neighbor {
            Some(neighbor_index) => {
                self.entries.swap(index, neighbor_index);
                true
            }
            None => false,
        }
    }

    /// Create a new folder, returning its id.
    pub fn create_folder(&mut self, name: String, created_at: u64) -> u64 {
        let id = self.next_folder_id;
        self.next_folder_id += 1;
        self.folders.push(BookmarkFolder {
            id,
            name,
            created_at,
        });
        id
    }

    /// Rename an existing folder. Returns `true` when the folder was found
    /// and renamed.
    pub fn rename_folder(&mut self, id: u64, name: String) -> bool {
        match self.folders.iter_mut().find(|folder| folder.id == id) {
            Some(folder) => {
                folder.name = name;
                true
            }
            None => false,
        }
    }

    /// Remove a folder. Every bookmark that was in it is reparented to the
    /// root (`folder_id: None`) rather than deleted — losing a bookmark's
    /// *organization* is a much smaller surprise than silently losing the
    /// bookmark itself (see docs/decisions.md D32). Returns `true` when the
    /// folder was found and removed.
    pub fn remove_folder(&mut self, id: u64) -> bool {
        let before = self.folders.len();
        self.folders.retain(|folder| folder.id != id);
        let removed = self.folders.len() != before;
        if removed {
            for entry in &mut self.entries {
                if entry.folder_id == Some(id) {
                    entry.folder_id = None;
                }
            }
        }
        removed
    }

    /// Update the favicon URL of the bookmark for `url`, if one exists.
    /// Returns `true` when an entry was updated. Keyed by URL rather than
    /// id — mirrors [`Self::remove_by_url`] — since the favicon resolver
    /// (`ui::window::BrowserWindow::fetch_favicon`) only ever knows which
    /// *page* a favicon belongs to, not whether/which bookmark id that page
    /// happens to have (see docs/decisions.md D34).
    pub fn update_favicon_by_url(&mut self, url: &str, favicon: String) -> bool {
        match self.entries.iter_mut().find(|entry| entry.url == url) {
            Some(entry) => {
                entry.favicon = Some(favicon);
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_a_bookmark() {
        let mut store = BookmarkStore::new();
        let id = store.add("https://example.com/", Some("Example".to_owned()), 100);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].id, id);
        assert_eq!(store.entries()[0].url, "https://example.com/");
        assert_eq!(store.entries()[0].title.as_deref(), Some("Example"));
        assert_eq!(store.entries()[0].folder_id, None);
        assert_eq!(store.entries()[0].favicon, None);
        assert!(store.is_bookmarked("https://example.com/"));
        assert!(!store.is_bookmarked("https://other.example/"));
    }

    #[test]
    fn adding_an_existing_url_updates_title_without_duplicating() {
        let mut store = BookmarkStore::new();
        let first = store.add("https://example.com/", None, 1);
        let second = store.add("https://example.com/", Some("Example".to_owned()), 2);
        assert_eq!(first, second);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].title.as_deref(), Some("Example"));
        // created_at of the original bookmark is preserved, not bumped.
        assert_eq!(store.entries()[0].created_at, 1);
    }

    #[test]
    fn adding_an_existing_url_with_no_title_keeps_previous_title() {
        let mut store = BookmarkStore::new();
        store.add("https://example.com/", Some("Example".to_owned()), 1);
        store.add("https://example.com/", None, 2);
        assert_eq!(store.entries()[0].title.as_deref(), Some("Example"));
    }

    #[test]
    fn removes_by_id() {
        let mut store = BookmarkStore::new();
        let id = store.add("https://example.com/", None, 1);
        assert!(store.remove(id));
        assert!(store.entries().is_empty());
        assert!(!store.remove(id));
    }

    #[test]
    fn removes_by_url() {
        let mut store = BookmarkStore::new();
        store.add("https://example.com/", None, 1);
        assert!(store.remove_by_url("https://example.com/"));
        assert!(!store.is_bookmarked("https://example.com/"));
        assert!(!store.remove_by_url("https://example.com/"));
    }

    #[test]
    fn toggle_adds_then_removes() {
        let mut store = BookmarkStore::new();
        assert!(store.toggle("https://example.com/", Some("Example".to_owned()), 1));
        assert!(store.is_bookmarked("https://example.com/"));
        assert!(!store.toggle("https://example.com/", None, 2));
        assert!(!store.is_bookmarked("https://example.com/"));
    }

    #[test]
    fn newest_first_reverses_creation_order() {
        let mut store = BookmarkStore::new();
        store.add("https://a.example/", None, 1);
        store.add("https://b.example/", None, 2);
        let urls: Vec<&str> = store
            .entries_newest_first()
            .map(|e| e.url.as_str())
            .collect();
        assert_eq!(urls, ["https://b.example/", "https://a.example/"]);
    }

    #[test]
    fn ids_stay_unique_after_removal() {
        let mut store = BookmarkStore::new();
        let first = store.add("https://a.example/", None, 1);
        store.remove(first);
        let second = store.add("https://a.example/", None, 2);
        assert_ne!(first, second);
    }

    // --- Folders (Issue #19, see docs/decisions.md D32) ---

    #[test]
    fn creates_and_lists_folders() {
        let mut store = BookmarkStore::new();
        let id = store.create_folder("仕事".to_owned(), 1);
        assert_eq!(store.folders().len(), 1);
        assert_eq!(store.folders()[0].id, id);
        assert_eq!(store.folders()[0].name, "仕事");
    }

    #[test]
    fn folder_ids_are_unique_and_independent_from_bookmark_ids() {
        let mut store = BookmarkStore::new();
        let bookmark_id = store.add("https://example.com/", None, 1);
        let folder_id = store.create_folder("仕事".to_owned(), 1);
        // Both counters start at 1 independently — this is fine, they are
        // never compared against each other, only used to look up within
        // their own collection.
        assert_eq!(bookmark_id, 1);
        assert_eq!(folder_id, 1);
    }

    #[test]
    fn renames_a_folder() {
        let mut store = BookmarkStore::new();
        let id = store.create_folder("仕事".to_owned(), 1);
        assert!(store.rename_folder(id, "プライベート".to_owned()));
        assert_eq!(store.folders()[0].name, "プライベート");
        assert!(!store.rename_folder(999, "no such folder".to_owned()));
    }

    #[test]
    fn removing_a_folder_reparents_its_entries_to_root() {
        let mut store = BookmarkStore::new();
        let folder = store.create_folder("仕事".to_owned(), 1);
        let entry = store.add("https://example.com/", None, 1);
        store
            .edit(entry, None, "https://example.com/".to_owned(), Some(folder))
            .unwrap();
        assert_eq!(store.entries()[0].folder_id, Some(folder));

        assert!(store.remove_folder(folder));
        assert!(store.folders().is_empty());
        assert_eq!(store.entries()[0].folder_id, None);
    }

    #[test]
    fn removing_an_unknown_folder_is_a_noop() {
        let mut store = BookmarkStore::new();
        assert!(!store.remove_folder(999));
    }

    #[test]
    fn entries_in_filters_by_folder_preserving_order() {
        let mut store = BookmarkStore::new();
        let folder = store.create_folder("仕事".to_owned(), 1);
        let a = store.add("https://a.example/", None, 1);
        let b = store.add("https://b.example/", None, 2);
        let c = store.add("https://c.example/", None, 3);
        store
            .edit(b, None, "https://b.example/".to_owned(), Some(folder))
            .unwrap();

        let root: Vec<u64> = store.entries_in(None).map(|e| e.id).collect();
        assert_eq!(root, [a, c]);
        let in_folder: Vec<u64> = store.entries_in(Some(folder)).map(|e| e.id).collect();
        assert_eq!(in_folder, [b]);
    }

    // --- Editing (Issue #19, see docs/decisions.md D33) ---

    #[test]
    fn edit_updates_title_url_and_folder() {
        let mut store = BookmarkStore::new();
        let folder = store.create_folder("仕事".to_owned(), 1);
        let id = store.add("https://example.com/", Some("Old".to_owned()), 1);

        store
            .edit(
                id,
                Some("New Title".to_owned()),
                "https://example.org/".to_owned(),
                Some(folder),
            )
            .unwrap();

        let entry = &store.entries()[0];
        assert_eq!(entry.title.as_deref(), Some("New Title"));
        assert_eq!(entry.url, "https://example.org/");
        assert_eq!(entry.folder_id, Some(folder));
    }

    #[test]
    fn edit_can_clear_the_title() {
        let mut store = BookmarkStore::new();
        let id = store.add("https://example.com/", Some("Old".to_owned()), 1);
        store
            .edit(id, None, "https://example.com/".to_owned(), None)
            .unwrap();
        assert_eq!(store.entries()[0].title, None);
    }

    #[test]
    fn edit_of_unknown_id_is_rejected() {
        let mut store = BookmarkStore::new();
        let result = store.edit(999, None, "https://example.com/".to_owned(), None);
        assert_eq!(result, Err(BookmarkEditError::NotFound));
    }

    #[test]
    fn edit_to_a_url_already_used_by_another_bookmark_is_rejected() {
        let mut store = BookmarkStore::new();
        store.add("https://a.example/", None, 1);
        let b = store.add("https://b.example/", None, 2);

        let result = store.edit(b, None, "https://a.example/".to_owned(), None);
        assert_eq!(result, Err(BookmarkEditError::DuplicateUrl));
        // Unchanged on rejection.
        assert_eq!(store.entries()[1].url, "https://b.example/");
    }

    #[test]
    fn edit_keeping_the_same_url_on_the_same_entry_is_allowed() {
        let mut store = BookmarkStore::new();
        let id = store.add("https://example.com/", None, 1);
        let result = store.edit(
            id,
            Some("Renamed".to_owned()),
            "https://example.com/".to_owned(),
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn edit_with_an_unknown_folder_id_falls_back_to_root() {
        let mut store = BookmarkStore::new();
        let id = store.add("https://example.com/", None, 1);
        store
            .edit(id, None, "https://example.com/".to_owned(), Some(999))
            .unwrap();
        assert_eq!(store.entries()[0].folder_id, None);
    }

    // --- Manual reordering (Issue #19, see docs/decisions.md D34) ---

    #[test]
    fn move_up_and_down_swap_within_the_same_scope() {
        let mut store = BookmarkStore::new();
        let a = store.add("https://a.example/", None, 1);
        let b = store.add("https://b.example/", None, 2);
        let c = store.add("https://c.example/", None, 3);

        assert!(store.move_up(c));
        assert_eq!(
            store.entries().iter().map(|e| e.id).collect::<Vec<_>>(),
            [a, c, b]
        );
        assert!(store.move_down(a));
        assert_eq!(
            store.entries().iter().map(|e| e.id).collect::<Vec<_>>(),
            [c, a, b]
        );
    }

    #[test]
    fn move_up_at_the_top_of_its_scope_is_a_noop() {
        let mut store = BookmarkStore::new();
        let a = store.add("https://a.example/", None, 1);
        store.add("https://b.example/", None, 2);
        assert!(!store.move_up(a));
    }

    #[test]
    fn move_down_at_the_bottom_of_its_scope_is_a_noop() {
        let mut store = BookmarkStore::new();
        store.add("https://a.example/", None, 1);
        let b = store.add("https://b.example/", None, 2);
        assert!(!store.move_down(b));
    }

    #[test]
    fn move_of_unknown_id_is_a_noop() {
        let mut store = BookmarkStore::new();
        store.add("https://a.example/", None, 1);
        assert!(!store.move_up(999));
        assert!(!store.move_down(999));
    }

    #[test]
    fn moving_ignores_entries_in_a_different_folder() {
        let mut store = BookmarkStore::new();
        let folder = store.create_folder("仕事".to_owned(), 1);
        let a = store.add("https://a.example/", None, 1);
        let b = store.add("https://b.example/", None, 2);
        store
            .edit(b, None, "https://b.example/".to_owned(), Some(folder))
            .unwrap();
        // `a` is the only root-level entry now (`b` moved to `folder`), so
        // there is no root-level neighbor for it to swap with even though
        // `b` sits right next to it in the underlying vector.
        assert!(!store.move_down(a));
    }

    // --- Favicon (Issue #19, see docs/decisions.md D34) ---

    #[test]
    fn updates_favicon_by_url() {
        let mut store = BookmarkStore::new();
        store.add("https://example.com/", None, 1);
        assert!(store.update_favicon_by_url(
            "https://example.com/",
            "https://example.com/favicon.ico".to_owned()
        ));
        assert_eq!(
            store.entries()[0].favicon.as_deref(),
            Some("https://example.com/favicon.ico")
        );
    }

    #[test]
    fn updating_favicon_of_an_unbookmarked_url_is_a_noop() {
        let mut store = BookmarkStore::new();
        assert!(!store.update_favicon_by_url(
            "https://example.com/",
            "https://example.com/favicon.ico".to_owned()
        ));
    }

    // --- Migration from the pre-#19 (Issue #4) format ---

    #[test]
    fn pre_issue_19_bookmarks_json_without_folder_id_or_favicon_still_loads() {
        let json = r#"{
            "entries": [
                {"id": 1, "url": "https://example.com/", "title": "Example", "created_at": 100}
            ],
            "next_id": 2
        }"#;
        let mut store: BookmarkStore =
            serde_json::from_str(json).expect("old-format JSON should parse");
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].folder_id, None);
        assert_eq!(store.entries()[0].favicon, None);
        assert!(store.folders().is_empty());
        // A folder created after loading old data must not collide with
        // anything — `next_folder_id` must default sanely, not to 0.
        assert_eq!(store.create_folder("新規".to_owned(), 1), 1);
    }
}
