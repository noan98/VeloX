//! Bookmarks: a pure, in-memory collection of saved pages.
//!
//! Unlike [`crate::browser::history`], entries are de-duplicated by URL —
//! a page is either bookmarked or it is not, there is no concept of visiting
//! a bookmark "again". File IO lives in [`crate::browser::persistence`].

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
}

/// An ordered collection of [`BookmarkEntry`] values, in creation order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkStore {
    entries: Vec<BookmarkEntry>,
    next_id: u64,
}

impl Default for BookmarkStore {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }
}

impl BookmarkStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[BookmarkEntry] {
        &self.entries
    }

    /// Entries, most recently created first — the order the UI panel lists
    /// them in.
    pub fn entries_newest_first(&self) -> impl Iterator<Item = &BookmarkEntry> {
        self.entries.iter().rev()
    }

    /// Whether `url` is already bookmarked.
    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.entries.iter().any(|entry| entry.url == url)
    }

    /// Bookmark `url`, returning its id.
    ///
    /// De-duplicates by URL: bookmarking an already-bookmarked page updates
    /// its title (when `title` is `Some`) instead of creating a second
    /// entry, and its id and position are unchanged.
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
}
