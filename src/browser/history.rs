//! Visit history: a pure, in-memory collection of visited pages.
//!
//! This is deliberately independent from the web engine's own session
//! history (see `browser::navigation` doc comments / D4 in
//! `docs/decisions.md`) — it is the app-level "where have I been" log that
//! survives restarts, not the back/forward stack.
//!
//! Everything here is plain data and pure functions so it can be unit
//! tested without a filesystem or a window; file IO lives in
//! [`crate::browser::persistence`].

use serde::{Deserialize, Serialize};

/// One recorded visit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Stable id, unique within one store, used by the UI to reference an
    /// entry (e.g. for deletion) without relying on URL or position.
    pub id: u64,
    pub url: String,
    /// The page's `document.title`, filled in asynchronously after the load
    /// finishes; `None` until it arrives (see `BrowserWindow::fetch_page_title`).
    pub title: Option<String>,
    /// Unix timestamp (seconds) of the visit.
    pub visited_at: u64,
}

/// An ordered collection of [`HistoryEntry`] values, oldest first.
///
/// De-duplicates consecutive visits to the same URL (a reload, or the engine
/// re-reporting the same page) into a single entry instead of piling up
/// near-duplicates, and enforces a hard cap on the number of entries kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryStore {
    entries: Vec<HistoryEntry>,
    next_id: u64,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }
}

impl HistoryStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Entries, most recently visited first — the order the UI panel lists
    /// them in.
    pub fn entries_newest_first(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter().rev()
    }

    /// Record a visit to `url` at `visited_at` (unix seconds), returning the
    /// id of the (possibly reused) entry.
    ///
    /// If the most recent entry is already for the same URL, it is updated
    /// in place (timestamp bumped, title replaced only when `title` is
    /// `Some`) rather than creating a duplicate. Otherwise a new entry is
    /// appended and the store is trimmed to `max_entries` by dropping the
    /// oldest entries.
    ///
    /// `max_entries` of `0` is treated as "no cap".
    pub fn record_visit(
        &mut self,
        url: &str,
        title: Option<String>,
        visited_at: u64,
        max_entries: usize,
    ) -> u64 {
        if let Some(last) = self.entries.last_mut() {
            if last.url == url {
                if title.is_some() {
                    last.title = title;
                }
                last.visited_at = visited_at;
                return last.id;
            }
        }

        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(HistoryEntry {
            id,
            url: url.to_owned(),
            title,
            visited_at,
        });

        if max_entries > 0 {
            let overflow = self.entries.len().saturating_sub(max_entries);
            if overflow > 0 {
                self.entries.drain(0..overflow);
            }
        }

        id
    }

    /// Update the title of the entry with the given `id`, if it still
    /// exists. Returns `true` when an entry was updated.
    pub fn update_title(&mut self, id: u64, title: String) -> bool {
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.title = Some(title);
                true
            }
            None => false,
        }
    }

    /// Remove one entry by id. Returns `true` when an entry was removed.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// Remove every entry, keeping the id counter monotonic.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_a_visit() {
        let mut store = HistoryStore::new();
        let id = store.record_visit("https://example.com/", None, 100, 0);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].id, id);
        assert_eq!(store.entries()[0].url, "https://example.com/");
        assert_eq!(store.entries()[0].visited_at, 100);
        assert_eq!(store.entries()[0].title, None);
    }

    #[test]
    fn assigns_increasing_ids() {
        let mut store = HistoryStore::new();
        let a = store.record_visit("https://a.example/", None, 1, 0);
        let b = store.record_visit("https://b.example/", None, 2, 0);
        assert_ne!(a, b);
        assert!(b > a);
    }

    #[test]
    fn collapses_consecutive_visits_to_the_same_url() {
        let mut store = HistoryStore::new();
        let first = store.record_visit("https://example.com/", None, 1, 0);
        let second = store.record_visit("https://example.com/", Some("Example".to_owned()), 2, 0);
        assert_eq!(first, second);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].visited_at, 2);
        assert_eq!(store.entries()[0].title.as_deref(), Some("Example"));
    }

    #[test]
    fn does_not_collapse_non_consecutive_repeat_visits() {
        let mut store = HistoryStore::new();
        store.record_visit("https://a.example/", None, 1, 0);
        store.record_visit("https://b.example/", None, 2, 0);
        store.record_visit("https://a.example/", None, 3, 0);
        assert_eq!(store.entries().len(), 3);
    }

    #[test]
    fn record_visit_does_not_overwrite_existing_title_with_none() {
        let mut store = HistoryStore::new();
        store.record_visit("https://example.com/", Some("Example".to_owned()), 1, 0);
        store.record_visit("https://example.com/", None, 2, 0);
        assert_eq!(store.entries()[0].title.as_deref(), Some("Example"));
    }

    #[test]
    fn enforces_a_max_entry_cap_by_dropping_oldest() {
        let mut store = HistoryStore::new();
        store.record_visit("https://a.example/", None, 1, 2);
        store.record_visit("https://b.example/", None, 2, 2);
        store.record_visit("https://c.example/", None, 3, 2);
        assert_eq!(store.entries().len(), 2);
        let urls: Vec<&str> = store.entries().iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls, ["https://b.example/", "https://c.example/"]);
    }

    #[test]
    fn zero_cap_means_unlimited() {
        let mut store = HistoryStore::new();
        for i in 0..10 {
            store.record_visit(&format!("https://{i}.example/"), None, i, 0);
        }
        assert_eq!(store.entries().len(), 10);
    }

    #[test]
    fn newest_first_reverses_visit_order() {
        let mut store = HistoryStore::new();
        store.record_visit("https://a.example/", None, 1, 0);
        store.record_visit("https://b.example/", None, 2, 0);
        let urls: Vec<&str> = store
            .entries_newest_first()
            .map(|e| e.url.as_str())
            .collect();
        assert_eq!(urls, ["https://b.example/", "https://a.example/"]);
    }

    #[test]
    fn updates_title_by_id() {
        let mut store = HistoryStore::new();
        let id = store.record_visit("https://example.com/", None, 1, 0);
        assert!(store.update_title(id, "Example Domain".to_owned()));
        assert_eq!(store.entries()[0].title.as_deref(), Some("Example Domain"));
    }

    #[test]
    fn updating_title_of_missing_id_is_a_noop() {
        let mut store = HistoryStore::new();
        store.record_visit("https://example.com/", None, 1, 0);
        assert!(!store.update_title(999, "Nope".to_owned()));
    }

    #[test]
    fn removes_an_entry_by_id() {
        let mut store = HistoryStore::new();
        let id = store.record_visit("https://example.com/", None, 1, 0);
        assert!(store.remove(id));
        assert!(store.entries().is_empty());
        assert!(!store.remove(id));
    }

    #[test]
    fn clear_empties_the_store() {
        let mut store = HistoryStore::new();
        store.record_visit("https://a.example/", None, 1, 0);
        store.record_visit("https://b.example/", None, 2, 0);
        store.clear();
        assert!(store.entries().is_empty());
    }

    #[test]
    fn ids_stay_unique_across_a_clear() {
        let mut store = HistoryStore::new();
        let first = store.record_visit("https://a.example/", None, 1, 0);
        store.clear();
        let second = store.record_visit("https://a.example/", None, 2, 0);
        assert_ne!(first, second);
    }
}
