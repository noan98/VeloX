//! [`CandidateSource`] implementations for the omnibox dropdown (Issue
//! #20): history/bookmark matches and previously-submitted search queries.
//! `app.rs` builds one of each fresh per `ToolbarCommand::OmniboxInput` and
//! passes them to `omnibox::build_candidates`; the actual scoring/ranking
//! lives in `browser::ranking`, kept separate so it stays testable without
//! constructing a `Candidate` at all. See docs/decisions.md D36-D38 and
//! docs/architecture.md's "Omnibox and search" section.

use crate::browser::bookmarks::BookmarkStore;
use crate::browser::history::HistoryStore;
use crate::browser::input_history::InputHistoryStore;
use crate::browser::navigation;
use crate::browser::omnibox::{Candidate, CandidateKind, CandidateSource};
use crate::browser::ranking;

/// Combined history+bookmark candidate source — one source, not two, so
/// de-duplication (a URL that is both visited and bookmarked) happens once,
/// with the full picture, instead of each store guessing at what the other
/// might also contain (see docs/decisions.md D37 and
/// `ranking::merge_entries`'s doc comment).
///
/// Reads `history`/`bookmarks` as they stand right now; nothing here mutates
/// either store, so building this fresh on every keystroke (as `app.rs`
/// does) is just borrowing, not copying.
pub struct HistoryBookmarkSource<'a> {
    pub history: &'a HistoryStore,
    pub bookmarks: &'a BookmarkStore,
    /// "Now", injected rather than read internally, so recency scoring is
    /// deterministic and testable — same pattern as
    /// `history::date_bucket`/`group_by_date`.
    pub now: u64,
}

impl CandidateSource for HistoryBookmarkSource<'_> {
    fn candidates(&self, input: &str, limit: usize) -> Vec<Candidate> {
        if limit == 0 || input.trim().is_empty() {
            return Vec::new();
        }

        let merged = ranking::merge_entries(self.history.entries(), self.bookmarks.entries());
        ranking::rank_page_entries(merged, input, self.now, limit)
            .into_iter()
            .map(|entry| {
                let title = entry.title.map(str::to_owned);
                Candidate {
                    kind: if entry.is_bookmark {
                        CandidateKind::Bookmark
                    } else {
                        CandidateKind::History
                    },
                    target_url: entry.url.to_owned(),
                    label: title.clone().unwrap_or_else(|| entry.url.to_owned()),
                    // Show the actual destination URL as the secondary
                    // line whenever the label is showing a title instead of
                    // the URL itself — the user is one click away from
                    // loading it, and a page's own <title> is not a
                    // trustworthy stand-in for "where this will take you"
                    // (deliberately not a visit timestamp; see
                    // docs/decisions.md D37).
                    detail: title.is_some().then(|| entry.url.to_owned()),
                }
            })
            .collect()
    }
}

/// Previously-submitted search queries, resurfaced as `Search` candidates
/// when the current input is a prefix of (or appears in) one. See
/// docs/decisions.md D38.
pub struct InputHistorySource<'a> {
    pub store: &'a InputHistoryStore,
    pub search_engine_name: &'a str,
    pub search_query_template: &'a str,
    pub now: u64,
}

impl CandidateSource for InputHistorySource<'_> {
    fn candidates(&self, input: &str, limit: usize) -> Vec<Candidate> {
        let query = input.trim();
        if limit == 0 || query.is_empty() {
            return Vec::new();
        }
        let query_lower = query.to_lowercase();

        ranking::rank_input_history(self.store.entries(), query, self.now, limit)
            .into_iter()
            // Do not also suggest re-running the exact text
            // `build_candidates`'s own built-in `Search` candidate already
            // offers for this input (see docs/decisions.md D38) — this can
            // occasionally hand back one fewer candidate than `limit` even
            // though `store` had more matches; `build_candidates` never
            // re-asks, so a duplicate-looking row is preferred over none by
            // mainstream omniboxes, but here it is simply skipped.
            .filter(|entry| entry.text.to_lowercase() != query_lower)
            .filter_map(|entry| {
                navigation::build_search_url(self.search_query_template, &entry.text).map(
                    |target_url| Candidate {
                        kind: CandidateKind::Search,
                        target_url,
                        label: entry.text.clone(),
                        detail: Some(format!("{} で検索", self.search_engine_name)),
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{BookmarkStore, HistoryStore};

    #[test]
    fn history_bookmark_source_yields_no_candidates_for_blank_input() {
        let history = HistoryStore::new();
        let bookmarks = BookmarkStore::new();
        let source = HistoryBookmarkSource {
            history: &history,
            bookmarks: &bookmarks,
            now: 1_000,
        };
        assert!(source.candidates("", 8).is_empty());
        assert!(source.candidates("   ", 8).is_empty());
    }

    #[test]
    fn history_bookmark_source_yields_no_candidates_when_limit_is_zero() {
        let mut history = HistoryStore::new();
        history.record_visit("https://example.com/", Some("Example".to_owned()), 1, 0);
        let bookmarks = BookmarkStore::new();
        let source = HistoryBookmarkSource {
            history: &history,
            bookmarks: &bookmarks,
            now: 1_000,
        };
        assert!(source.candidates("example", 0).is_empty());
    }

    #[test]
    fn history_bookmark_source_labels_and_details_a_titled_match() {
        let mut history = HistoryStore::new();
        history.record_visit(
            "https://example.com/",
            Some("Example Domain".to_owned()),
            1,
            0,
        );
        let bookmarks = BookmarkStore::new();
        let source = HistoryBookmarkSource {
            history: &history,
            bookmarks: &bookmarks,
            now: 1_000,
        };
        let candidates = source.candidates("example", 8);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, CandidateKind::History);
        assert_eq!(candidates[0].target_url, "https://example.com/");
        assert_eq!(candidates[0].label, "Example Domain");
        assert_eq!(
            candidates[0].detail.as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn history_bookmark_source_falls_back_to_the_url_as_label_with_no_detail_when_untitled() {
        let mut history = HistoryStore::new();
        history.record_visit("https://example.com/", None, 1, 0);
        let bookmarks = BookmarkStore::new();
        let source = HistoryBookmarkSource {
            history: &history,
            bookmarks: &bookmarks,
            now: 1_000,
        };
        let candidates = source.candidates("example", 8);
        assert_eq!(candidates[0].label, "https://example.com/");
        assert_eq!(candidates[0].detail, None);
    }

    #[test]
    fn history_bookmark_source_marks_a_bookmarked_url_as_bookmark_kind() {
        let history = HistoryStore::new();
        let mut bookmarks = BookmarkStore::new();
        bookmarks.add("https://example.com/", Some("Example".to_owned()), 1);
        let source = HistoryBookmarkSource {
            history: &history,
            bookmarks: &bookmarks,
            now: 1_000,
        };
        let candidates = source.candidates("example", 8);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, CandidateKind::Bookmark);
    }

    #[test]
    fn history_bookmark_source_deduplicates_a_url_that_is_both_visited_and_bookmarked() {
        let mut history = HistoryStore::new();
        history.record_visit("https://example.com/", Some("Example".to_owned()), 1, 0);
        let mut bookmarks = BookmarkStore::new();
        bookmarks.add("https://example.com/", Some("Example".to_owned()), 1);
        let source = HistoryBookmarkSource {
            history: &history,
            bookmarks: &bookmarks,
            now: 1_000,
        };
        assert_eq!(source.candidates("example", 8).len(), 1);
    }

    #[test]
    fn history_bookmark_source_respects_the_remaining_limit() {
        let mut history = HistoryStore::new();
        for i in 0..5 {
            history.record_visit(&format!("https://example{i}.test/"), None, i, 0);
        }
        let bookmarks = BookmarkStore::new();
        let source = HistoryBookmarkSource {
            history: &history,
            bookmarks: &bookmarks,
            now: 1_000,
        };
        assert_eq!(source.candidates("example", 2).len(), 2);
    }

    const DDG: &str = "https://duckduckgo.com/?q={}";

    #[test]
    fn input_history_source_suggests_a_matching_past_query() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 1, 0);
        let source = InputHistorySource {
            store: &store,
            search_engine_name: "DuckDuckGo",
            search_query_template: DDG,
            now: 1_000,
        };
        let candidates = source.candidates("rust", 8);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, CandidateKind::Search);
        assert_eq!(candidates[0].label, "rust ownership");
        assert_eq!(
            candidates[0].target_url,
            "https://duckduckgo.com/?q=rust+ownership"
        );
        assert_eq!(candidates[0].detail.as_deref(), Some("DuckDuckGo で検索"));
    }

    #[test]
    fn input_history_source_skips_a_query_identical_to_the_current_input() {
        let mut store = InputHistoryStore::new();
        store.record("rust", 1, 0);
        store.record("rust ownership", 1, 0);
        let source = InputHistorySource {
            store: &store,
            search_engine_name: "DuckDuckGo",
            search_query_template: DDG,
            now: 1_000,
        };
        let candidates = source.candidates("rust", 8);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "rust ownership");
    }

    #[test]
    fn input_history_source_yields_no_candidates_for_blank_input() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 1, 0);
        let source = InputHistorySource {
            store: &store,
            search_engine_name: "DuckDuckGo",
            search_query_template: DDG,
            now: 1_000,
        };
        assert!(source.candidates("", 8).is_empty());
    }
}
