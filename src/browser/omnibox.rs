//! Omnibox candidate list: the pure logic behind what the address bar's
//! dropdown shows and what selecting a row does. UI/engine-independent, like
//! the rest of `browser::` — `ui::toolbar`/`ui::window` only render
//! [`Candidate`]s this module produces and forward the row the user picked
//! back as an ordinary `navigate` command (see `app.rs`).
//!
//! This issue (#15) implements only the two built-in candidates every input
//! can produce — "load this as a URL" and "search for this text" — via
//! [`build_candidates`]. Issue #20 (history/bookmark candidates and ranking)
//! plugs in through [`CandidateSource`] without this module's public
//! surface changing again; see that trait's doc comment for exactly what it
//! needs to implement.

use crate::browser::navigation::{self, Intent};

/// Default cap on how many rows [`build_candidates`] returns. Generous
/// enough for a handful of history/bookmark matches (#20) on top of the two
/// built-in candidates, without letting one wildly popular source of
/// candidates make the dropdown unusably long.
pub const DEFAULT_CANDIDATE_LIMIT: usize = 8;

/// What a [`Candidate`] represents — decides the icon/style the toolbar
/// renders it with, and (indirectly, since [`Candidate::target_url`] is
/// already fully resolved either way) nothing about how it is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// The literal input, interpreted as a URL to load.
    NavigateUrl,
    /// The literal input, sent to the configured search engine as a query.
    Search,
    /// Reserved for #20: a match against browsing history.
    History,
    /// Reserved for #20: a match against bookmarks.
    Bookmark,
}

/// One row the omnibox dropdown can show and let the user select.
///
/// `target_url` is always a fully resolved, already-normalized URL — never
/// the raw address-bar text — so a caller can execute a candidate the exact
/// same way it executes a plain Enter-with-no-selection: hand `target_url`
/// to the same `navigate` path (which re-normalizes it, a no-op for an
/// already-normalized URL; see `app.rs`'s `ToolbarCommand::Navigate`
/// handler).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub target_url: String,
    /// Primary label the dropdown row shows.
    pub label: String,
    /// Secondary text shown alongside `label` (e.g. which search engine, or
    /// — for #20 — a visit timestamp). `None` when `label` already says
    /// everything worth showing.
    pub detail: Option<String>,
}

/// A source of extra omnibox candidates beyond the built-in URL/search pair
/// [`build_candidates`] always considers — e.g. history or bookmark matches
/// (#20).
///
/// # What #20 implements
///
/// One `impl CandidateSource for <your type>` per source (history,
/// bookmarks, or a single type covering both), with:
///
/// ```ignore
/// fn candidates(&self, input: &str, limit: usize) -> Vec<Candidate>;
/// ```
///
/// - `input` is the raw, not-yet-trimmed address-bar text exactly as the
///   user typed it (the same string [`build_candidates`] itself received);
///   matching/ranking against history or bookmark entries is entirely up to
///   the implementation.
/// - `limit` is the most candidates [`build_candidates`] has room left for
///   from this source; returning more than `limit` is safe (the extra ones
///   are dropped) but wasteful.
/// - Return candidates already ranked, best match first — `build_candidates`
///   appends them in the order returned and does not re-sort.
/// - Every returned [`Candidate::target_url`] must already be a normalized,
///   loadable URL (typically the history/bookmark entry's stored URL passed
///   straight through — no re-parsing needed there), matching every other
///   candidate this module produces.
/// - `app.rs` is where a source gets wired in: pass `&[&history_source,
///   &bookmark_source]` as `build_candidates`'s `sources` argument instead
///   of today's `&[]`.
pub trait CandidateSource {
    fn candidates(&self, input: &str, limit: usize) -> Vec<Candidate>;
}

/// Build the omnibox candidate list for the current address-bar text.
///
/// Always starts from what [`navigation::classify_input`] decides:
/// - [`Intent::Url`] contributes a [`CandidateKind::NavigateUrl`] candidate
///   at the resolved URL, *plus* a [`CandidateKind::Search`] candidate for
///   the same raw text (in case the input was meant as a search after all —
///   mirrors how mainstream browsers offer both for input that is
///   URL-shaped but ambiguous).
/// - [`Intent::Search`] contributes just the one `Search` candidate.
/// - `None` (empty input, or input `classify_input` refuses outright, e.g.
///   a rejected scheme) contributes nothing.
///
/// `sources` are then asked for candidates in order (see
/// [`CandidateSource`]), appended after the built-in ones, and the combined
/// list is truncated to `limit`.
pub fn build_candidates(
    input: &str,
    search_engine_name: &str,
    search_query_template: &str,
    sources: &[&dyn CandidateSource],
    limit: usize,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    let build_search_candidate = |query: &str| -> Option<Candidate> {
        navigation::build_search_url(search_query_template, query).map(|target_url| Candidate {
            kind: CandidateKind::Search,
            target_url,
            label: query.to_owned(),
            detail: Some(format!("{search_engine_name} で検索")),
        })
    };

    match navigation::classify_input(input) {
        Some(Intent::Url(url)) => {
            candidates.push(Candidate {
                kind: CandidateKind::NavigateUrl,
                target_url: url.clone(),
                label: url,
                detail: None,
            });
            let raw = input.trim();
            if let Some(candidate) = build_search_candidate(raw) {
                candidates.push(candidate);
            }
        }
        Some(Intent::Search(query)) => {
            if let Some(candidate) = build_search_candidate(&query) {
                candidates.push(candidate);
            }
        }
        None => {}
    }

    for source in sources {
        if candidates.len() >= limit {
            break;
        }
        let remaining = limit - candidates.len();
        candidates.extend(source.candidates(input, remaining));
    }

    candidates.truncate(limit);
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDG: &str = "https://duckduckgo.com/?q={}";

    #[test]
    fn url_input_yields_navigate_and_search_candidates() {
        let candidates = build_candidates("example.com", "DuckDuckGo", DDG, &[], 8);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].kind, CandidateKind::NavigateUrl);
        assert_eq!(candidates[0].target_url, "https://example.com/");
        assert_eq!(candidates[1].kind, CandidateKind::Search);
        assert_eq!(
            candidates[1].target_url,
            "https://duckduckgo.com/?q=example.com"
        );
        assert_eq!(candidates[1].label, "example.com");
    }

    #[test]
    fn search_input_yields_a_single_search_candidate() {
        let candidates = build_candidates("rust ownership", "DuckDuckGo", DDG, &[], 8);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, CandidateKind::Search);
        assert_eq!(
            candidates[0].target_url,
            "https://duckduckgo.com/?q=rust+ownership"
        );
        assert_eq!(candidates[0].detail.as_deref(), Some("DuckDuckGo で検索"));
    }

    #[test]
    fn empty_input_yields_no_candidates() {
        assert!(build_candidates("", "DuckDuckGo", DDG, &[], 8).is_empty());
        assert!(build_candidates("   ", "DuckDuckGo", DDG, &[], 8).is_empty());
    }

    #[test]
    fn rejected_scheme_yields_no_candidates() {
        assert!(build_candidates("javascript://alert(1)", "DuckDuckGo", DDG, &[], 8).is_empty());
    }

    #[test]
    fn explicit_search_prefix_yields_a_single_search_candidate() {
        let candidates = build_candidates("?rust", "DuckDuckGo", DDG, &[], 8);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, CandidateKind::Search);
        assert_eq!(candidates[0].label, "rust");
    }

    struct FixedSource(Vec<Candidate>);

    impl CandidateSource for FixedSource {
        fn candidates(&self, _input: &str, limit: usize) -> Vec<Candidate> {
            self.0.iter().take(limit).cloned().collect()
        }
    }

    fn history_candidate(n: usize) -> Candidate {
        Candidate {
            kind: CandidateKind::History,
            target_url: format!("https://example.com/{n}"),
            label: format!("Example {n}"),
            detail: None,
        }
    }

    #[test]
    fn sources_are_appended_after_the_built_in_candidates() {
        let source = FixedSource(vec![history_candidate(1), history_candidate(2)]);
        let candidates = build_candidates("example.com", "DuckDuckGo", DDG, &[&source], 8);
        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates[0].kind, CandidateKind::NavigateUrl);
        assert_eq!(candidates[1].kind, CandidateKind::Search);
        assert_eq!(candidates[2].kind, CandidateKind::History);
        assert_eq!(candidates[3].kind, CandidateKind::History);
    }

    #[test]
    fn total_candidates_are_truncated_to_the_limit() {
        let source = FixedSource((0..10).map(history_candidate).collect());
        let candidates = build_candidates("example.com", "DuckDuckGo", DDG, &[&source], 5);
        assert_eq!(candidates.len(), 5);
    }

    #[test]
    fn a_source_is_not_asked_for_more_than_the_remaining_room() {
        struct RecordingSource {
            requested_limit: std::cell::Cell<usize>,
        }
        impl CandidateSource for RecordingSource {
            fn candidates(&self, _input: &str, limit: usize) -> Vec<Candidate> {
                self.requested_limit.set(limit);
                Vec::new()
            }
        }
        let source = RecordingSource {
            requested_limit: std::cell::Cell::new(0),
        };
        // "example.com" contributes 2 built-in candidates; limit is 5, so 3
        // should remain for the source.
        build_candidates("example.com", "DuckDuckGo", DDG, &[&source], 5);
        assert_eq!(source.requested_limit.get(), 3);
    }
}
