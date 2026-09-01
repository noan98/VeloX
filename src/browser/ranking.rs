//! Omnibox candidate ranking: pure scoring/merging/sorting for
//! history+bookmark matches and typed-search-query matches against the
//! current address-bar input.
//!
//! Nothing here touches `HistoryStore`/`BookmarkStore`/`InputHistoryStore`
//! mutably or does any IO — it only reads their entry types, so it stays as
//! unit-testable as the rest of `browser::` (D20). See docs/decisions.md D36
//! (page-entry scoring formula) and D38 (typed-search-query scoring) for the
//! rationale behind the exact numbers below; `browser::omnibox_candidates`
//! is where these functions are wired into `CandidateSource` impls.

use std::collections::HashMap;

use crate::browser::bookmarks::BookmarkEntry;
use crate::browser::history::{date_bucket, HistoryDateBucket, HistoryEntry};
use crate::browser::input_history::InputHistoryEntry;

// --- Shared scoring building blocks ---

/// Points awarded for how often something has been used, capped so one
/// extremely popular entry cannot mathematically bury every text-match
/// distinction below it. A linear-with-cap curve (rather than, say, a log
/// curve) is used deliberately: the cap alone already bounds the top end,
/// and a linear scale keeps every score in this module an exact, easily
/// hand-verified integer-ish `f64` for the unit tests below — see
/// docs/decisions.md D36.
const FREQUENCY_CAP: u32 = 20;
const FREQUENCY_WEIGHT: f64 = 1.5;

fn frequency_score(count: u32) -> f64 {
    count.min(FREQUENCY_CAP) as f64 * FREQUENCY_WEIGHT
}

/// Points awarded for how recently something was used, bucketed onto the
/// exact same today/yesterday/last-7-days/older boundaries the history
/// panel already groups by ([`date_bucket`]/D29) rather than a second,
/// independent notion of "recent" — one recency vocabulary in the whole
/// codebase.
const RECENCY_TODAY: f64 = 30.0;
const RECENCY_YESTERDAY: f64 = 20.0;
const RECENCY_LAST_7_DAYS: f64 = 10.0;
const RECENCY_OLDER: f64 = 0.0;

fn recency_score(timestamp: Option<u64>, now: u64) -> f64 {
    match timestamp {
        None => 0.0,
        Some(ts) => match date_bucket(ts, now) {
            HistoryDateBucket::Today => RECENCY_TODAY,
            HistoryDateBucket::Yesterday => RECENCY_YESTERDAY,
            HistoryDateBucket::Last7Days => RECENCY_LAST_7_DAYS,
            HistoryDateBucket::Older => RECENCY_OLDER,
        },
    }
}

/// Extra points for a match that accounts for a larger share of the
/// matched field — matching all 9 characters of `example.com` is a much
/// more specific signal than matching 2 characters out of a 40-character
/// title. Measured in `char`s (not bytes) so multi-byte text (Japanese
/// titles/queries, which this app expects plenty of) is not penalized
/// relative to ASCII.
const MATCH_RATIO_WEIGHT: f64 = 20.0;

fn match_ratio_bonus(query_len_chars: usize, field_len_chars: usize) -> f64 {
    if field_len_chars == 0 {
        return 0.0;
    }
    let ratio = (query_len_chars as f64 / field_len_chars as f64).min(1.0);
    ratio * MATCH_RATIO_WEIGHT
}

// --- History/bookmark ("page") entries ---

/// One URL's worth of history/bookmark data, reduced to exactly what
/// [`score_page_entry`] needs. Produced by [`merge_entries`] — never built
/// by hand outside this module's own tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankableEntry<'a> {
    pub url: &'a str,
    pub title: Option<&'a str>,
    pub visit_count: u32,
    pub last_visited_at: Option<u64>,
    pub is_bookmark: bool,
}

// Match-location tiers, highest priority first. A host-prefix match (the
// address itself starts with what was typed — e.g. "exa" ->
// "example.com") is the strongest signal a mainstream browser's omnibox
// also treats as king; a title match beats a mere substring match anywhere
// in the host or URL, and matching only somewhere inside the raw URL
// (typically the path/query string) is the weakest still-a-match tier. See
// docs/decisions.md D36.
const TIER_HOST_PREFIX: f64 = 100.0;
const TIER_TITLE_PREFIX: f64 = 90.0;
const TIER_HOST_CONTAINS: f64 = 70.0;
const TIER_TITLE_CONTAINS: f64 = 60.0;
const TIER_URL_CONTAINS: f64 = 40.0;

/// Flat bonus for a URL the user explicitly bookmarked — see
/// docs/decisions.md D36 for why this is a fixed addend rather than a
/// multiplier (a multiplier would also scale up frecency/match noise;
/// "the user saved this on purpose" is a signal of a fixed size, not
/// proportional to how often they happen to have visited it).
const BOOKMARK_BONUS: f64 = 25.0;

/// The host component of `url` for match-tier purposes, lower-cased.
/// Falls back to the whole (lower-cased) URL when it does not parse as a
/// URL with a host — defensive only: every `RankableEntry::url` this module
/// receives already went through `navigation::normalize_input` before it
/// was stored, so this should always succeed in practice.
fn host_lower(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
        .to_lowercase()
}

/// Score `entry` against `query`, or `None` when nothing about it matches
/// `query` at all (in which case it must not appear as a candidate,
/// regardless of how frequently/recently visited or bookmarked it is —
/// frecency/bookmark status only ever break ties among actual matches, they
/// never manufacture one).
pub fn score_page_entry(entry: &RankableEntry<'_>, query: &str, now: u64) -> Option<f64> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let query_lower = query.to_lowercase();
    let query_len = query_lower.chars().count();

    let host = host_lower(entry.url);
    let title_lower = entry.title.map(str::to_lowercase);
    let url_lower = entry.url.to_lowercase();

    let (tier, ratio_bonus) = if host.starts_with(&query_lower) {
        (
            TIER_HOST_PREFIX,
            match_ratio_bonus(query_len, host.chars().count()),
        )
    } else if title_lower
        .as_deref()
        .is_some_and(|title| title.starts_with(&query_lower))
    {
        let title = title_lower.as_deref().unwrap();
        (
            TIER_TITLE_PREFIX,
            match_ratio_bonus(query_len, title.chars().count()),
        )
    } else if host.contains(&query_lower) {
        (
            TIER_HOST_CONTAINS,
            match_ratio_bonus(query_len, host.chars().count()),
        )
    } else if title_lower
        .as_deref()
        .is_some_and(|title| title.contains(&query_lower))
    {
        let title = title_lower.as_deref().unwrap();
        (
            TIER_TITLE_CONTAINS,
            match_ratio_bonus(query_len, title.chars().count()),
        )
    } else if url_lower.contains(&query_lower) {
        (
            TIER_URL_CONTAINS,
            match_ratio_bonus(query_len, url_lower.chars().count()),
        )
    } else {
        return None;
    };

    let bookmark_bonus = if entry.is_bookmark {
        BOOKMARK_BONUS
    } else {
        0.0
    };
    Some(
        tier + ratio_bonus
            + frequency_score(entry.visit_count)
            + recency_score(entry.last_visited_at, now)
            + bookmark_bonus,
    )
}

/// Merge history and bookmark entries into one [`RankableEntry`] per unique
/// URL — see docs/decisions.md D37 for the de-duplication rule this
/// implements: a URL that is both visited and bookmarked becomes a single
/// entry with `is_bookmark: true`, the bookmark's title when it has one
/// (falling back to history's title otherwise), and history's `visit_count`/
/// `last_visited_at` (a bookmark carries neither — it is not a visit log).
///
/// Iteration order of the result is unspecified (it is built through a
/// hash map); callers that care about order — i.e. every caller — always
/// run it through [`rank_page_entries`] afterward, which sorts
/// deterministically.
pub fn merge_entries<'a>(
    history: impl IntoIterator<Item = &'a HistoryEntry>,
    bookmarks: impl IntoIterator<Item = &'a BookmarkEntry>,
) -> Vec<RankableEntry<'a>> {
    let mut by_url: HashMap<&'a str, RankableEntry<'a>> = HashMap::new();

    for entry in history {
        by_url.insert(
            entry.url.as_str(),
            RankableEntry {
                url: &entry.url,
                title: entry.title.as_deref(),
                visit_count: entry.visit_count,
                last_visited_at: Some(entry.visited_at),
                is_bookmark: false,
            },
        );
    }

    for entry in bookmarks {
        by_url
            .entry(entry.url.as_str())
            .and_modify(|existing| {
                existing.is_bookmark = true;
                if entry.title.is_some() {
                    existing.title = entry.title.as_deref();
                }
            })
            .or_insert_with(|| RankableEntry {
                url: &entry.url,
                title: entry.title.as_deref(),
                visit_count: 0,
                last_visited_at: None,
                is_bookmark: true,
            });
    }

    by_url.into_values().collect()
}

/// Score, sort (best first), and truncate `entries` to `limit`. Entries
/// [`score_page_entry`] does not match at all are dropped, never merely
/// scored low.
///
/// Ties (equal score) break first by more-recently-visited, then by URL
/// (lexicographic) purely so the result is deterministic regardless of
/// [`merge_entries`]'s unspecified hash-map ordering — not a meaningful
/// ranking signal on its own.
pub fn rank_page_entries<'a>(
    entries: impl IntoIterator<Item = RankableEntry<'a>>,
    query: &str,
    now: u64,
    limit: usize,
) -> Vec<RankableEntry<'a>> {
    let mut scored: Vec<(RankableEntry<'a>, f64)> = entries
        .into_iter()
        .filter_map(|entry| score_page_entry(&entry, query, now).map(|score| (entry, score)))
        .collect();

    scored.sort_by(|(a_entry, a_score), (b_entry, b_score)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| b_entry.last_visited_at.cmp(&a_entry.last_visited_at))
            .then_with(|| a_entry.url.cmp(b_entry.url))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(entry, _)| entry).collect()
}

// --- Typed search-query ("input history") entries ---

const TIER_TEXT_PREFIX: f64 = 80.0;
const TIER_TEXT_CONTAINS: f64 = 40.0;

fn score_input_history_entry(entry: &InputHistoryEntry, query: &str, now: u64) -> Option<f64> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let query_lower = query.to_lowercase();
    let query_len = query_lower.chars().count();
    let text_lower = entry.text.to_lowercase();

    let (tier, ratio_bonus) = if text_lower.starts_with(&query_lower) {
        (
            TIER_TEXT_PREFIX,
            match_ratio_bonus(query_len, text_lower.chars().count()),
        )
    } else if text_lower.contains(&query_lower) {
        (
            TIER_TEXT_CONTAINS,
            match_ratio_bonus(query_len, text_lower.chars().count()),
        )
    } else {
        return None;
    };

    Some(
        tier + ratio_bonus
            + frequency_score(entry.use_count)
            + recency_score(Some(entry.last_used_at), now),
    )
}

/// Same shape as [`rank_page_entries`], for previously-submitted search
/// queries instead of visited/bookmarked URLs — see docs/decisions.md D38.
pub fn rank_input_history<'a>(
    entries: &'a [InputHistoryEntry],
    query: &str,
    now: u64,
    limit: usize,
) -> Vec<&'a InputHistoryEntry> {
    let mut scored: Vec<(&'a InputHistoryEntry, f64)> = entries
        .iter()
        .filter_map(|entry| {
            score_input_history_entry(entry, query, now).map(|score| (entry, score))
        })
        .collect();

    scored.sort_by(|(a_entry, a_score), (b_entry, b_score)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| b_entry.last_used_at.cmp(&a_entry.last_used_at))
            .then_with(|| a_entry.text.cmp(&b_entry.text))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(entry, _)| entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A "now" that lands mid-day on an arbitrary day boundary, mirroring
    // `history::tests`'s own pinned `now` values so the recency-bucket
    // boundaries below line up with `date_bucket`'s own tests.
    const SECS_PER_DAY: u64 = 86_400;
    fn now() -> u64 {
        100 * SECS_PER_DAY + 12 * 3_600
    }

    fn entry<'a>(
        url: &'a str,
        title: Option<&'a str>,
        visit_count: u32,
        last_visited_at: Option<u64>,
        is_bookmark: bool,
    ) -> RankableEntry<'a> {
        RankableEntry {
            url,
            title,
            visit_count,
            last_visited_at,
            is_bookmark,
        }
    }

    // --- score_page_entry: match tiers ---

    #[test]
    fn host_prefix_match_outranks_every_other_tier() {
        let host_prefix = entry("https://example.com/", None, 0, None, false);
        let title_prefix = entry(
            "https://other.example/",
            Some("Example Domain"),
            0,
            None,
            false,
        );
        let host_contains = entry("https://my-example.net/", None, 0, None, false);

        let s_host_prefix = score_page_entry(&host_prefix, "example", now()).unwrap();
        let s_title_prefix = score_page_entry(&title_prefix, "example", now()).unwrap();
        let s_host_contains = score_page_entry(&host_contains, "example", now()).unwrap();

        assert!(s_host_prefix > s_title_prefix);
        assert!(s_title_prefix > s_host_contains);
    }

    #[test]
    fn host_contains_outranks_title_contains_outranks_url_contains() {
        let host_contains = entry("https://myexample.net/", None, 0, None, false);
        let title_contains = entry(
            "https://other.test/",
            Some("A page about example stuff"),
            0,
            None,
            false,
        );
        let url_contains = entry("https://other.test/path/example", None, 0, None, false);

        let s_host = score_page_entry(&host_contains, "example", now()).unwrap();
        let s_title = score_page_entry(&title_contains, "example", now()).unwrap();
        let s_url = score_page_entry(&url_contains, "example", now()).unwrap();

        assert!(s_host > s_title);
        assert!(s_title > s_url);
    }

    #[test]
    fn no_match_anywhere_yields_none() {
        let e = entry(
            "https://example.com/",
            Some("Example Domain"),
            100,
            Some(now()),
            true,
        );
        assert_eq!(score_page_entry(&e, "zzz-nope", now()), None);
    }

    #[test]
    fn empty_query_never_matches() {
        let e = entry(
            "https://example.com/",
            Some("Example Domain"),
            100,
            Some(now()),
            true,
        );
        assert_eq!(score_page_entry(&e, "", now()), None);
        assert_eq!(score_page_entry(&e, "   ", now()), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let e = entry(
            "https://Example.com/",
            Some("EXAMPLE Domain"),
            0,
            None,
            false,
        );
        assert!(score_page_entry(&e, "EXAMPLE", now()).is_some());
        assert!(score_page_entry(&e, "example", now()).is_some());
    }

    #[test]
    fn a_longer_matched_fraction_of_the_field_scores_higher() {
        // Both host-prefix matches; "example" covers more of "example.io"
        // than it does of "example-with-a-much-longer-domain-name.io".
        let short = entry("https://example.io/", None, 0, None, false);
        let long = entry(
            "https://example-with-a-much-longer-domain-name.io/",
            None,
            0,
            None,
            false,
        );
        let s_short = score_page_entry(&short, "example", now()).unwrap();
        let s_long = score_page_entry(&long, "example", now()).unwrap();
        assert!(s_short > s_long);
    }

    // --- score_page_entry: frecency ---

    #[test]
    fn higher_visit_count_scores_higher_all_else_equal() {
        let popular = entry("https://example.com/", None, 20, Some(now()), false);
        let rare = entry("https://example.com/", None, 1, Some(now()), false);
        assert!(
            score_page_entry(&popular, "example", now()).unwrap()
                > score_page_entry(&rare, "example", now()).unwrap()
        );
    }

    #[test]
    fn visit_count_above_the_cap_does_not_score_any_higher() {
        let at_cap = entry("https://example.com/", None, 20, Some(now()), false);
        let way_above = entry("https://example.com/", None, 5000, Some(now()), false);
        assert_eq!(
            score_page_entry(&at_cap, "example", now()),
            score_page_entry(&way_above, "example", now())
        );
    }

    #[test]
    fn more_recent_visits_score_higher() {
        let today = entry("https://example.com/", None, 1, Some(now()), false);
        let yesterday = entry(
            "https://example.com/",
            None,
            1,
            Some(now() - SECS_PER_DAY),
            false,
        );
        let last_week = entry(
            "https://example.com/",
            None,
            1,
            Some(now() - 5 * SECS_PER_DAY),
            false,
        );
        let ancient = entry(
            "https://example.com/",
            None,
            1,
            Some(now() - 30 * SECS_PER_DAY),
            false,
        );
        let s_today = score_page_entry(&today, "example", now()).unwrap();
        let s_yesterday = score_page_entry(&yesterday, "example", now()).unwrap();
        let s_last_week = score_page_entry(&last_week, "example", now()).unwrap();
        let s_ancient = score_page_entry(&ancient, "example", now()).unwrap();
        assert!(s_today > s_yesterday);
        assert!(s_yesterday > s_last_week);
        assert!(s_last_week > s_ancient);
    }

    #[test]
    fn never_visited_gets_no_recency_bonus_but_can_still_match() {
        let never_visited = entry("https://example.com/", None, 0, None, true);
        assert!(score_page_entry(&never_visited, "example", now()).is_some());
    }

    // --- score_page_entry: bookmark bonus ---

    #[test]
    fn bookmark_bonus_can_move_a_never_visited_bookmark_above_a_weak_history_match() {
        // Both match at the same tier (host-contains); the bookmark has no
        // visit history at all, the history entry has a handful of old
        // visits. The fixed bookmark bonus (25) still needs to clear
        // whatever frecency the history-only entry accumulated.
        let bookmark = entry("https://my-example.net/", None, 0, None, true);
        let history_only = entry(
            "https://an-example.org/",
            None,
            2,
            Some(now() - 10 * SECS_PER_DAY),
            false,
        );
        assert!(
            score_page_entry(&bookmark, "example", now()).unwrap()
                > score_page_entry(&history_only, "example", now()).unwrap()
        );
    }

    #[test]
    fn a_much_more_popular_history_entry_can_still_outrank_a_weaker_tier_bookmark() {
        // Bookmark only matches by URL-substring (weakest tier); the
        // history entry matches by host-prefix (strongest tier) and is
        // visited constantly today. Positional match quality still wins
        // here even without the bookmark bonus in play for the history
        // entry — this is the "strong match beats bookmark bonus" case,
        // the mirror image of the previous test.
        let bookmark = entry("https://other.test/path/example", None, 0, None, true);
        let history = entry("https://example.com/", None, 20, Some(now()), false);
        assert!(
            score_page_entry(&history, "example", now()).unwrap()
                > score_page_entry(&bookmark, "example", now()).unwrap()
        );
    }

    // --- merge_entries ---

    #[test]
    fn merge_keeps_history_only_and_bookmark_only_entries_separate() {
        let history = vec![HistoryEntry {
            id: 1,
            url: "https://a.example/".to_owned(),
            title: Some("A".to_owned()),
            visited_at: 100,
            favicon: None,
            visit_count: 3,
        }];
        let bookmarks = vec![BookmarkEntry {
            id: 1,
            url: "https://b.example/".to_owned(),
            title: Some("B".to_owned()),
            created_at: 100,
            folder_id: None,
            favicon: None,
        }];
        let merged = merge_entries(&history, &bookmarks);
        assert_eq!(merged.len(), 2);
        let a = merged
            .iter()
            .find(|e| e.url == "https://a.example/")
            .unwrap();
        assert!(!a.is_bookmark);
        assert_eq!(a.visit_count, 3);
        let b = merged
            .iter()
            .find(|e| e.url == "https://b.example/")
            .unwrap();
        assert!(b.is_bookmark);
        assert_eq!(b.visit_count, 0);
        assert_eq!(b.last_visited_at, None);
    }

    #[test]
    fn merge_combines_a_url_that_is_both_visited_and_bookmarked_into_one_entry() {
        let history = vec![HistoryEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: Some("History Title".to_owned()),
            visited_at: 500,
            favicon: None,
            visit_count: 7,
        }];
        let bookmarks = vec![BookmarkEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: Some("Bookmark Title".to_owned()),
            created_at: 100,
            folder_id: None,
            favicon: None,
        }];
        let merged = merge_entries(&history, &bookmarks);
        assert_eq!(merged.len(), 1);
        let e = &merged[0];
        assert!(e.is_bookmark);
        // D37: the bookmark's own title wins over history's.
        assert_eq!(e.title, Some("Bookmark Title"));
        // ... but visit stats still come from history — a bookmark is not
        // a visit log.
        assert_eq!(e.visit_count, 7);
        assert_eq!(e.last_visited_at, Some(500));
    }

    #[test]
    fn merge_falls_back_to_the_history_title_when_the_bookmark_has_none() {
        let history = vec![HistoryEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: Some("History Title".to_owned()),
            visited_at: 500,
            favicon: None,
            visit_count: 1,
        }];
        let bookmarks = vec![BookmarkEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: None,
            created_at: 100,
            folder_id: None,
            favicon: None,
        }];
        let merged = merge_entries(&history, &bookmarks);
        assert_eq!(merged[0].title, Some("History Title"));
    }

    // --- rank_page_entries ---

    #[test]
    fn rank_page_entries_sorts_best_match_first() {
        let entries = vec![
            entry("https://weak.test/path/example", None, 0, None, false),
            entry("https://example.com/", None, 0, None, false),
            entry("https://mid-example.test/", None, 0, None, false),
        ];
        let ranked = rank_page_entries(entries, "example", now(), 10);
        assert_eq!(
            ranked.iter().map(|e| e.url).collect::<Vec<_>>(),
            [
                "https://example.com/",
                "https://mid-example.test/",
                "https://weak.test/path/example",
            ]
        );
    }

    #[test]
    fn rank_page_entries_drops_non_matches() {
        let entries = vec![
            entry("https://example.com/", None, 0, None, false),
            entry(
                "https://totally-unrelated.test/",
                None,
                100,
                Some(now()),
                true,
            ),
        ];
        let ranked = rank_page_entries(entries, "example", now(), 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].url, "https://example.com/");
    }

    #[test]
    fn rank_page_entries_truncates_to_the_limit() {
        let entries: Vec<RankableEntry> = (0..10)
            .map(|i| {
                let url: &'static str =
                    Box::leak(format!("https://example{i}.test/").into_boxed_str());
                entry(url, None, 0, None, false)
            })
            .collect();
        let ranked = rank_page_entries(entries, "example", now(), 3);
        assert_eq!(ranked.len(), 3);
    }

    #[test]
    fn rank_page_entries_breaks_exact_ties_deterministically_by_url() {
        // Identical shape, only the URL differs — same score either way.
        let entries = vec![
            entry("https://bbb.test/", None, 0, None, false),
            entry("https://aaa.test/", None, 0, None, false),
        ];
        let ranked = rank_page_entries(entries, "test", now(), 10);
        assert_eq!(
            ranked.iter().map(|e| e.url).collect::<Vec<_>>(),
            ["https://aaa.test/", "https://bbb.test/"]
        );
    }

    // --- input history ---

    fn input_entry(text: &str, use_count: u32, last_used_at: u64) -> InputHistoryEntry {
        InputHistoryEntry {
            text: text.to_owned(),
            use_count,
            last_used_at,
        }
    }

    #[test]
    fn input_history_prefix_match_outranks_contains_match() {
        let entries = vec![
            input_entry("something about rust", 1, now()),
            input_entry("rust ownership", 1, now()),
        ];
        let ranked = rank_input_history(&entries, "rust", now(), 10);
        assert_eq!(ranked[0].text, "rust ownership");
        assert_eq!(ranked[1].text, "something about rust");
    }

    #[test]
    fn input_history_no_match_is_excluded() {
        let entries = vec![input_entry("cats and dogs", 50, now())];
        assert!(rank_input_history(&entries, "rust", now(), 10).is_empty());
    }

    #[test]
    fn input_history_more_frequent_and_recent_ranks_higher() {
        let entries = vec![
            input_entry("rust async", 1, now() - 30 * SECS_PER_DAY),
            input_entry("rust ownership", 10, now()),
        ];
        let ranked = rank_input_history(&entries, "rust", now(), 10);
        assert_eq!(ranked[0].text, "rust ownership");
    }
}
