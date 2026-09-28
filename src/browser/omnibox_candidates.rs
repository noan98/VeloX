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
use crate::browser::omnibox::{self, Candidate, CandidateKind, CandidateSource};
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
    /// History to draw matches from, or `None` to draw none (Issue #157).
    ///
    /// **`Option` rather than a `bool` beside a `&HistoryStore` on purpose.**
    /// The stores are shared by every window in the process, so a *private*
    /// window asking for candidates must not see what a *normal* window
    /// recorded. With a flag the caller can hand over the store and forget
    /// to set it; with `Option` there is nothing to forget — not passing the
    /// store is the only way to say "no history".
    ///
    /// Bookmarks stay available either way: real browsers do surface
    /// bookmarks in an incognito omnibox, and a bookmark is something the
    /// user saved deliberately rather than a trace of where they have been.
    pub history: Option<&'a HistoryStore>,
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

        let history = self.history.map(HistoryStore::entries).unwrap_or(&[]);
        let merged = ranking::merge_entries(history, self.bookmarks.entries());
        ranking::rank_page_entries(merged, input, self.now, limit)
            .into_iter()
            .map(|entry| Candidate {
                kind: if entry.is_bookmark {
                    CandidateKind::Bookmark
                } else {
                    CandidateKind::History
                },
                target_url: entry.url.to_owned(),
                label: entry.title.unwrap_or(entry.url).to_owned(),
                // Show the actual destination URL as the secondary
                // line whenever the label is showing a title instead of
                // the URL itself — the user is one click away from
                // loading it, and a page's own <title> is not a
                // trustworthy stand-in for "where this will take you"
                // (deliberately not a visit timestamp; see
                // docs/decisions.md D37).
                detail: entry.title.map(|_| entry.url.to_owned()),
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
                omnibox::search_candidate(
                    self.search_engine_name,
                    self.search_query_template,
                    &entry.text,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{BookmarkStore, HistoryStore};

    /// 通常ウィンドウ (履歴あり) 相当の [`HistoryBookmarkSource`]。
    fn history_bookmark_source<'a>(
        history: &'a HistoryStore,
        bookmarks: &'a BookmarkStore,
    ) -> HistoryBookmarkSource<'a> {
        HistoryBookmarkSource {
            history: Some(history),
            bookmarks,
            now: 1_000,
        }
    }

    #[test]
    fn history_bookmark_source_yields_no_candidates_for_blank_input() {
        let history = HistoryStore::new();
        let bookmarks = BookmarkStore::new();
        let source = history_bookmark_source(&history, &bookmarks);
        assert!(source.candidates("", 8).is_empty());
        assert!(source.candidates("   ", 8).is_empty());
    }

    #[test]
    fn history_bookmark_source_yields_no_candidates_when_limit_is_zero() {
        let mut history = HistoryStore::new();
        history.record_visit("https://example.com/", Some("Example".to_owned()), 1, 0);
        let bookmarks = BookmarkStore::new();
        let source = history_bookmark_source(&history, &bookmarks);
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
        let source = history_bookmark_source(&history, &bookmarks);
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
        let source = history_bookmark_source(&history, &bookmarks);
        let candidates = source.candidates("example", 8);
        assert_eq!(candidates[0].label, "https://example.com/");
        assert_eq!(candidates[0].detail, None);
    }

    #[test]
    fn history_bookmark_source_marks_a_bookmarked_url_as_bookmark_kind() {
        let history = HistoryStore::new();
        let mut bookmarks = BookmarkStore::new();
        bookmarks.add("https://example.com/", Some("Example".to_owned()), 1);
        let source = history_bookmark_source(&history, &bookmarks);
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
        let source = history_bookmark_source(&history, &bookmarks);
        assert_eq!(source.candidates("example", 8).len(), 1);
    }

    #[test]
    fn history_bookmark_source_respects_the_remaining_limit() {
        let mut history = HistoryStore::new();
        for i in 0..5 {
            history.record_visit(&format!("https://example{i}.test/"), None, i, 0);
        }
        let bookmarks = BookmarkStore::new();
        let source = history_bookmark_source(&history, &bookmarks);
        assert_eq!(source.candidates("example", 2).len(), 2);
    }

    const DDG: &str = "https://duckduckgo.com/?q={}";

    fn input_history_source(store: &InputHistoryStore) -> InputHistorySource<'_> {
        InputHistorySource {
            store,
            search_engine_name: "DuckDuckGo",
            search_query_template: DDG,
            now: 1_000,
        }
    }

    #[test]
    fn input_history_source_suggests_a_matching_past_query() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 1, 0);
        let source = input_history_source(&store);
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
        let source = input_history_source(&store);
        let candidates = source.candidates("rust", 8);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "rust ownership");
    }

    #[test]
    fn input_history_source_yields_no_candidates_for_blank_input() {
        let mut store = InputHistoryStore::new();
        store.record("rust ownership", 1, 0);
        let source = input_history_source(&store);
        assert!(source.candidates("", 8).is_empty());
    }

    /// Issue #157: a private window must not see what a normal window
    /// recorded.
    ///
    /// The three stores are shared by every window in the process, so the
    /// only thing between a private window's omnibox and another window's
    /// browsing is the caller passing `history: None`. These pin that, and
    /// pin what is *deliberately* still shown (bookmarks).
    mod private_window {
        use super::*;

        fn stores() -> (HistoryStore, BookmarkStore) {
            let mut history = HistoryStore::new();
            history.record_visit(
                "https://secret.example/plans",
                Some("極秘の計画".to_owned()),
                900,
                100,
            );
            let mut bookmarks = BookmarkStore::new();
            bookmarks.add(
                "https://bookmarked.example/docs",
                Some("保存した資料".to_owned()),
                900,
            );
            (history, bookmarks)
        }

        fn candidates(
            history: Option<&HistoryStore>,
            bookmarks: &BookmarkStore,
            input: &str,
        ) -> Vec<Candidate> {
            HistoryBookmarkSource {
                history,
                bookmarks,
                now: 1_000,
            }
            .candidates(input, 8)
        }

        #[test]
        fn history_is_withheld_when_no_store_is_passed() {
            let (_history, bookmarks) = stores();
            let found = candidates(None, &bookmarks, "example");
            assert!(
                found.iter().all(|c| c.kind != CandidateKind::History),
                "private ウィンドウに履歴が漏れている: {found:?}"
            );
            assert!(
                !found.iter().any(|c| c.target_url.contains("secret")),
                "private ウィンドウに通常ウィンドウの訪問先が漏れている: {found:?}"
            );
        }

        #[test]
        fn bookmarks_are_still_shown() {
            let (_history, bookmarks) = stores();
            let found = candidates(None, &bookmarks, "example");
            assert!(
                found
                    .iter()
                    .any(|c| c.kind == CandidateKind::Bookmark
                        && c.target_url.contains("bookmarked")),
                "ブックマークまで消している (実ブラウザは incognito でも出す): {found:?}"
            );
        }

        #[test]
        fn a_normal_window_still_sees_history() {
            let (history, bookmarks) = stores();
            let found = candidates(Some(&history), &bookmarks, "example");
            assert!(
                found
                    .iter()
                    .any(|c| c.kind == CandidateKind::History && c.target_url.contains("secret")),
                "通常ウィンドウの履歴まで止めてしまっている: {found:?}"
            );
        }

        #[test]
        fn withholding_history_does_not_disturb_a_url_that_is_both() {
            // 訪問済みかつブックマーク済みの URL は `merge_entries` が 1 件に
            // 畳む。履歴を外したときに畳み先ごと消えないことを確かめる。
            let mut history = HistoryStore::new();
            history.record_visit("https://both.example/", Some("両方".to_owned()), 900, 100);
            let mut bookmarks = BookmarkStore::new();
            bookmarks.add("https://both.example/", Some("両方".to_owned()), 900);

            let found = candidates(None, &bookmarks, "both");
            assert_eq!(found.len(), 1, "{found:?}");
            assert_eq!(found[0].kind, CandidateKind::Bookmark);
        }
    }
}
