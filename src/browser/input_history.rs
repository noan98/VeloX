//! Typed search-query history: the raw text of search queries the user has
//! actually submitted through the address bar, remembered so a partial
//! retype can resurface a full past query as an omnibox candidate (Issue
//! #20's "入力履歴" requirement). See docs/decisions.md D38 for the scope
//! decision this module implements and why.
//!
//! Deliberately narrow: only text `browser::navigation::classify_input`
//! resolved as [`Intent::Search`](crate::browser::navigation::Intent::Search)
//! belongs here. URL-shaped input the user types is not recorded a second
//! time — loading it already lands it in [`crate::browser::HistoryStore`]
//! once the page finishes loading (`app::record_visit_if_enabled`), and that
//! store already has richer bookkeeping (title, favicon, precise visit
//! semantics) for it. Pure data + pure functions only, like
//! `browser::history`/`browser::bookmarks`; file IO lives in
//! `browser::persistence`, matching their pattern exactly.

use serde::{Deserialize, Serialize};

/// One previously-submitted search query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputHistoryEntry {
    /// The raw query text, trimmed, exactly as classified by
    /// `navigation::classify_input`'s `Intent::Search` — never
    /// percent-encoded or otherwise transformed (that happens at candidate
    /// build time, via `navigation::build_search_url`, same as the
    /// built-in search candidate).
    pub text: String,
    /// Unix timestamp (seconds) this query was last submitted.
    pub last_used_at: u64,
    /// How many times this exact text has been submitted.
    pub use_count: u32,
}

/// Default cap on how many distinct queries [`InputHistoryStore`] keeps —
/// intentionally much smaller than `Config::history_max_entries` (5000):
/// this store exists purely to resurface a *recognizable* past query as you
/// retype the start of it, not to be a complete log, so a few hundred
/// distinct recent queries is already generous. See docs/decisions.md D38.
pub const DEFAULT_MAX_ENTRIES: usize = 200;

/// An ordered-by-nothing-in-particular collection of [`InputHistoryEntry`]
/// values, de-duplicated by exact (trimmed) text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputHistoryStore {
    entries: Vec<InputHistoryEntry>,
}

impl InputHistoryStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// All entries, in no particular order — callers that want a ranking
    /// go through `browser::ranking::rank_input_history`.
    pub fn entries(&self) -> &[InputHistoryEntry] {
        &self.entries
    }

    /// Record a submitted search query at `at` (unix seconds).
    ///
    /// Whitespace-only/empty `text` is ignored (nothing to remember). An
    /// existing entry for the exact same (trimmed) text has `last_used_at`
    /// bumped and `use_count` incremented in place; otherwise a new entry
    /// is appended.
    ///
    /// When appending would exceed `max_entries`, the single
    /// least-recently-used entry is dropped first — unlike
    /// `HistoryStore::record_visit`'s oldest-inserted-first cap (D27), this
    /// store's entire purpose is "what did I search a while back", so a
    /// query re-used last week should survive over one nobody has typed
    /// again since it was first recorded, even if that one is older.
    /// `max_entries` of `0` means "no cap", matching `HistoryStore`.
    pub fn record(&mut self, text: &str, at: u64, max_entries: usize) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.text == text) {
            entry.last_used_at = at;
            entry.use_count = entry.use_count.saturating_add(1);
            return;
        }

        self.entries.push(InputHistoryEntry {
            text: text.to_owned(),
            last_used_at: at,
            use_count: 1,
        });

        if max_entries > 0 && self.entries.len() > max_entries {
            if let Some((index, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used_at)
            {
                self.entries.remove(index);
            }
        }
    }

    /// Remove every entry (mirrors `HistoryStore::clear` /
    /// `ToolbarCommand::ClearHistory` also purging this store — see
    /// docs/decisions.md D38).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_a_new_query() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 100, 0);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].text, "rust ownership");
        assert_eq!(store.entries()[0].last_used_at, 100);
        assert_eq!(store.entries()[0].use_count, 1);
    }

    #[test]
    fn re_recording_the_same_text_bumps_recency_and_count_instead_of_duplicating() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 100, 0);
        store.record("rust ownership", 200, 0);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].last_used_at, 200);
        assert_eq!(store.entries()[0].use_count, 2);
    }

    #[test]
    fn text_is_trimmed_before_recording_and_comparing() {
        let mut store = InputHistoryStore::new();
        store.record("  rust ownership  ", 100, 0);
        store.record("rust ownership", 200, 0);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].text, "rust ownership");
        assert_eq!(store.entries()[0].use_count, 2);
    }

    #[test]
    fn empty_or_whitespace_only_text_is_not_recorded() {
        let mut store = InputHistoryStore::new();
        store.record("", 100, 0);
        store.record("   ", 100, 0);
        assert!(store.entries().is_empty());
    }

    #[test]
    fn distinct_queries_are_kept_separately() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 100, 0);
        store.record("rust async", 200, 0);
        assert_eq!(store.entries().len(), 2);
    }

    #[test]
    fn zero_cap_means_unlimited() {
        let mut store = InputHistoryStore::new();
        for i in 0..10 {
            store.record(&format!("query {i}"), i, 0);
        }
        assert_eq!(store.entries().len(), 10);
    }

    #[test]
    fn exceeding_the_cap_drops_the_least_recently_used_entry() {
        let mut store = InputHistoryStore::new();
        store.record("old", 100, 2);
        store.record("newer", 200, 2);
        // Re-use "old" so it is no longer the least-recently-used entry;
        // without this, an insertion-order cap and an LRU cap would drop
        // the same entry and the test would not distinguish them.
        store.record("old", 300, 2);
        store.record("newest", 400, 2);
        assert_eq!(store.entries().len(), 2);
        let texts: Vec<&str> = store.entries().iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"old"));
        assert!(texts.contains(&"newest"));
        assert!(!texts.contains(&"newer"));
    }

    #[test]
    fn clear_empties_the_store() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 100, 0);
        store.clear();
        assert!(store.entries().is_empty());
    }

    // --- Robustness against extreme/hostile field values (Issue #35) ---

    #[test]
    fn record_does_not_panic_with_an_extremely_long_query() {
        let mut store = InputHistoryStore::new();
        let huge = "検索語".repeat(200_000);
        store.record(&huge, 1, 0);
        assert_eq!(store.entries()[0].text, huge);
    }

    #[test]
    fn record_handles_unicode_and_control_characters() {
        let mut store = InputHistoryStore::new();
        store.record("query\0with\u{202e}control\nchars🚀", 1, 0);
        assert_eq!(store.entries().len(), 1);
    }

    #[test]
    fn use_count_saturates_instead_of_overflowing() {
        let mut store = InputHistoryStore::new();
        store.record("rust", 1, 0);
        store.entries[0].use_count = u32::MAX;
        store.record("rust", 2, 0);
        assert_eq!(store.entries()[0].use_count, u32::MAX);
    }

    #[test]
    fn many_distinct_queries_with_a_tight_cap_does_not_panic() {
        let mut store = InputHistoryStore::new();
        for i in 0..5_000u64 {
            store.record(&format!("query {i}"), i, 50);
        }
        assert_eq!(store.entries().len(), 50);
    }

    #[test]
    fn malformed_json_falls_back_via_persistence_not_a_panic_here() {
        // `InputHistoryStore` itself has no bespoke `Deserialize` impl (it
        // is `#[derive(Deserialize)]`), so malformed-JSON robustness for it
        // is exercised at the `browser::persistence::load_input_history`
        // boundary — this just documents that a directly-malformed
        // deserialize attempt errors cleanly rather than panicking, mirror-
        // ing `history`/`bookmarks`' own direct-deserialize tests.
        assert!(serde_json::from_str::<InputHistoryStore>("not json").is_err());
        assert!(
            serde_json::from_str::<InputHistoryStore>(r#"{"entries":"not an array"}"#).is_err()
        );
    }
}
