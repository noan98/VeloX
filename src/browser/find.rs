//! In-page find (Issue #43, Ctrl/Cmd+F) — the UI/engine-independent half.
//!
//! No wry-supported engine exposes a public "find text in this webview" API
//! `BrowserWindow` could call directly (see docs/decisions.md D69 for the
//! survey of wry 0.56/webview2-com-sys/WKWebView that ruled this out), so
//! the actual text search and DOM highlighting is implemented as JavaScript,
//! injected/evaluated by `ui::window` into the *content* webview. What can
//! be expressed as plain data — the query text, whether the search is
//! case-sensitive, and where we are in the result set (the match count the
//! DOM reported back, which match is active, and cyclic next/previous
//! navigation) — belongs in `browser::` per the four-layer split in
//! `docs/architecture.md`, and lives here so it is unit-tested without a
//! webview.

use super::TabId;

/// Normalizes raw find-bar input into a search query, or `None` for
/// empty/whitespace-only input. `None` means "nothing to search for" — the
/// caller should clear any existing highlight rather than search for an
/// empty string (an empty pattern would otherwise "match" everywhere).
pub fn normalize_query(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// One tab's in-page find session: the query currently searched for
/// (already normalized — never empty), whether it is case-sensitive, and
/// bookkeeping for the result set the content webview's DOM search reported
/// back (see `ui::window::BrowserWindow::search_in_page`).
///
/// Only one session exists at a time, for whichever tab was active when the
/// find bar was opened (`app::open_find_bar`) — switching tabs or
/// navigating that tab away closes it (see `app.rs`). This is a deliberate
/// MVP simplification, not a structural limit of this type: nothing here
/// prevents keying a `HashMap<TabId, FindState>` later if per-tab find
/// sessions are wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindState {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
    /// Total matches last reported by the DOM for `query`. `0` until the
    /// first `set_total` call after a search starts.
    total: usize,
    /// 0-based index of the currently active (highlighted) match, or `None`
    /// when there are no matches (including "no search has run yet").
    active: Option<usize>,
}

impl FindState {
    /// A fresh session for `tab_id`, with an empty query and no results yet
    /// (`app::open_find_bar` creates one right when the find bar opens,
    /// before the user has typed anything).
    pub fn new(tab_id: TabId) -> Self {
        Self {
            tab_id,
            query: String::new(),
            case_sensitive: false,
            total: 0,
            active: None,
        }
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// 0-based index of the active match, or `None` when there is none.
    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// Starts a fresh search: `query` should already be normalized (see
    /// [`normalize_query`]) — this does not re-validate it. Resets
    /// total/active to "no results yet" until [`Self::set_total`] applies
    /// what the DOM actually found; a stale count from the *previous* query
    /// must never be shown against the new one, even for the instant before
    /// the DOM search script's callback returns.
    pub fn set_query(&mut self, query: String, case_sensitive: bool) {
        self.query = query;
        self.case_sensitive = case_sensitive;
        self.total = 0;
        self.active = None;
    }

    /// Applies a freshly-reported match count from the DOM. The first match
    /// becomes active automatically — mainstream find bars show "1/N"
    /// immediately after a search, without requiring an explicit "next".
    pub fn set_total(&mut self, total: usize) {
        self.total = total;
        self.active = if total == 0 { None } else { Some(0) };
    }

    /// Cyclic "next match": wraps from the last match back to the first.
    /// A no-op (returns `None`) when there are no matches.
    pub fn next_match(&mut self) -> Option<usize> {
        self.active = if self.total == 0 {
            None
        } else {
            Some(match self.active {
                Some(i) => (i + 1) % self.total,
                None => 0,
            })
        };
        self.active
    }

    /// Cyclic "previous match": wraps from the first match to the last.
    /// A no-op (returns `None`) when there are no matches.
    pub fn previous_match(&mut self) -> Option<usize> {
        self.active = if self.total == 0 {
            None
        } else {
            Some(match self.active {
                Some(i) => (i + self.total - 1) % self.total,
                None => self.total - 1,
            })
        };
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(n: u64) -> TabId {
        TabId::from(n)
    }

    #[test]
    fn normalize_query_trims_and_rejects_empty_or_whitespace() {
        assert_eq!(normalize_query("hello"), Some("hello".to_owned()));
        assert_eq!(normalize_query("  hello  "), Some("hello".to_owned()));
        assert_eq!(normalize_query(""), None);
        assert_eq!(normalize_query("   "), None);
        assert_eq!(normalize_query("\t\n"), None);
    }

    #[test]
    fn normalize_query_keeps_internal_whitespace() {
        assert_eq!(
            normalize_query("  hello world  "),
            Some("hello world".to_owned())
        );
    }

    #[test]
    fn new_state_starts_with_no_query_and_no_results() {
        let state = FindState::new(tab(1));
        assert_eq!(state.tab_id(), tab(1));
        assert_eq!(state.query(), "");
        assert!(!state.case_sensitive());
        assert_eq!(state.total(), 0);
        assert_eq!(state.active(), None);
    }

    #[test]
    fn set_query_resets_total_and_active() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(5);
        assert_eq!(state.active(), Some(0));

        // A new query (even before the DOM reports back) must not keep
        // showing the previous query's count.
        state.set_query("bar".to_owned(), true);
        assert_eq!(state.query(), "bar");
        assert!(state.case_sensitive());
        assert_eq!(state.total(), 0);
        assert_eq!(state.active(), None);
    }

    #[test]
    fn set_total_zero_clears_active() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(3);
        assert_eq!(state.active(), Some(0));
        state.set_total(0);
        assert_eq!(state.active(), None);
    }

    #[test]
    fn set_total_selects_first_match() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(1);
        assert_eq!(state.active(), Some(0));
        assert_eq!(state.total(), 1);
    }

    #[test]
    fn next_match_advances_and_wraps_around() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(3);
        assert_eq!(state.active(), Some(0));
        assert_eq!(state.next_match(), Some(1));
        assert_eq!(state.next_match(), Some(2));
        // Wraps back to the first match.
        assert_eq!(state.next_match(), Some(0));
    }

    #[test]
    fn previous_match_retreats_and_wraps_around() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(3);
        assert_eq!(state.active(), Some(0));
        // Wraps to the last match immediately.
        assert_eq!(state.previous_match(), Some(2));
        assert_eq!(state.previous_match(), Some(1));
        assert_eq!(state.previous_match(), Some(0));
    }

    #[test]
    fn next_and_previous_are_inverses() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(4);
        let start = state.active();
        state.next_match();
        state.next_match();
        state.previous_match();
        state.previous_match();
        assert_eq!(state.active(), start);
    }

    #[test]
    fn single_match_stays_on_itself() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(1);
        assert_eq!(state.next_match(), Some(0));
        assert_eq!(state.previous_match(), Some(0));
    }

    #[test]
    fn next_and_previous_are_no_ops_with_no_matches() {
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        state.set_total(0);
        assert_eq!(state.next_match(), None);
        assert_eq!(state.previous_match(), None);
    }

    #[test]
    fn next_match_from_no_active_starts_at_first() {
        // Reachable if a caller calls `next_match`/`previous_match` before
        // any `set_total` (e.g. right after `set_query`, before the DOM
        // callback arrives) — should not panic, and should behave as if
        // starting fresh once matches do exist by the time it's called.
        let mut state = FindState::new(tab(1));
        state.set_query("foo".to_owned(), false);
        // Simulate `set_total` having been skipped by driving `total`
        // through the public API only: total starts at 0 with no matches,
        // then a later count arrives without resetting `active` back to
        // `None` explicitly by the caller — `set_total` already handles
        // that, so exercise `next_match` right after `set_query` (active is
        // `None`, total is `0`) to confirm it stays a safe no-op.
        assert_eq!(state.next_match(), None);
        assert_eq!(state.previous_match(), None);
    }
}
