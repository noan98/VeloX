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
    /// A URL the history panel can point an `<img>` at for this entry's
    /// favicon, filled in asynchronously the same way `title` is (see
    /// `UserEvent::FaviconResolved` / `HistoryStore::update_favicon`).
    /// `None` until one arrives, or forever for a URL the resolver never
    /// found an icon for. `#[serde(default)]` lets a `history.json` written
    /// by the pre-#18 store (Issue #4) — which has no `favicon` key at all —
    /// deserialize cleanly instead of failing the whole file (see
    /// docs/decisions.md D27).
    #[serde(default)]
    pub favicon: Option<String>,
    /// How many times this entry has absorbed a visit — see
    /// [`HistoryStore::record_visit`]'s doc comment and docs/decisions.md
    /// D27 for what this does and does not count. `#[serde(default)]` maps a
    /// pre-#18 entry (no `visit_count` key) to `1`, same reasoning as
    /// `favicon` above: it was visited at least once, or it would not exist.
    #[serde(default = "default_visit_count")]
    pub visit_count: u32,
}

fn default_visit_count() -> u32 {
    1
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
    /// `Some`, `visit_count` incremented) rather than creating a duplicate.
    /// Otherwise a new entry is appended with `visit_count: 1` and the store
    /// is trimmed to `max_entries` by dropping the oldest entries.
    ///
    /// See docs/decisions.md D27 for why `visit_count` counts *consecutive*
    /// re-visits of the same in-place entry (reloads, or the engine
    /// re-reporting the same page) rather than every historical visit to
    /// `url` anywhere in the log: browsing away and back still starts a
    /// fresh entry at `1`, exactly as it always has for `title`/
    /// `visited_at`, so this is additive to the existing de-duplication
    /// rather than a behavior change to it.
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
                last.visit_count = last.visit_count.saturating_add(1);
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
            favicon: None,
            visit_count: 1,
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

    /// Update the favicon URL of the entry with the given `id`, if it still
    /// exists. Returns `true` when an entry was updated. Mirrors
    /// [`Self::update_title`] — `app::UserEvent::FaviconResolved` calls this
    /// the same way `PageTitleResolved` calls `update_title`.
    pub fn update_favicon(&mut self, id: u64, url: String) -> bool {
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.favicon = Some(url);
                true
            }
            None => false,
        }
    }

    /// Entries whose URL or title contains `query` (case-insensitive),
    /// most-recently-visited first. Thin wrapper around the free function
    /// [`search`] over this store's own entries — see its doc comment for
    /// the matching rule.
    pub fn search(&self, query: &str) -> Vec<&HistoryEntry> {
        search(self.entries_newest_first(), query)
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

/// Entries from `entries` whose URL or title contains `query`
/// (case-insensitive substring match), in the order given. An empty `query`
/// matches nothing (mirrors "no search active" rather than "show
/// everything" — callers that want the unfiltered list already have
/// [`HistoryStore::entries_newest_first`] for that, so an empty query here
/// unambiguously means "nothing matched" instead of aliasing two different
/// UI states onto the same return value).
///
/// A free function over `&HistoryEntry` (not just a `HistoryStore` method)
/// so it is a pure, independently unit-testable building block — see
/// docs/decisions.md D30 — and [`HistoryStore::search`] is a one-line
/// wrapper over it for callers that do have a store handy.
pub fn search<'a>(
    entries: impl IntoIterator<Item = &'a HistoryEntry>,
    query: &str,
) -> Vec<&'a HistoryEntry> {
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    entries
        .into_iter()
        .filter(|entry| {
            entry.url.to_lowercase().contains(&query)
                || entry
                    .title
                    .as_deref()
                    .is_some_and(|title| title.to_lowercase().contains(&query))
        })
        .collect()
}

/// One second, in days — used to turn unix timestamps into calendar-day
/// numbers for [`date_bucket`].
const SECS_PER_DAY: u64 = 86_400;

/// Which of the history panel's date sections an entry falls into, relative
/// to some "now". See [`date_bucket`] and docs/decisions.md D29 for the
/// exact boundaries and why they are computed in UTC rather than the user's
/// local timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDateBucket {
    Today,
    Yesterday,
    #[serde(rename = "last_7_days")]
    Last7Days,
    Older,
}

/// Classify a visit at unix-seconds timestamp `visited_at` into a
/// [`HistoryDateBucket`], relative to unix-seconds "now" `now`.
///
/// A pure function of its two arguments — no `SystemTime::now()`/`Instant`
/// call inside it — so a test can pin `now` and assert boundaries exactly
/// (see docs/decisions.md D29 for why the issue asked for this shape).
/// Day boundaries are computed as `timestamp / 86_400` — i.e. **UTC**
/// calendar days, not the machine's local timezone (`std::time` alone has
/// no timezone database to consult, and this repo does not depend on
/// `chrono`/`tz`-aware crates — see D10/D29). A `visited_at` after `now`
/// (clock skew, or a system clock that moved backward between recording and
/// display) is clamped to [`HistoryDateBucket::Today`] rather than
/// underflowing.
pub fn date_bucket(visited_at: u64, now: u64) -> HistoryDateBucket {
    let today = now / SECS_PER_DAY;
    let day = visited_at / SECS_PER_DAY;
    if day >= today {
        return HistoryDateBucket::Today;
    }
    match today - day {
        1 => HistoryDateBucket::Yesterday,
        2..=6 => HistoryDateBucket::Last7Days,
        _ => HistoryDateBucket::Older,
    }
}

/// A contiguous run of entries sharing one [`HistoryDateBucket`], in the
/// order [`group_by_date`] encountered them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryGroup<'a> {
    pub bucket: HistoryDateBucket,
    pub entries: Vec<&'a HistoryEntry>,
}

/// Group `entries` (assumed newest-first, e.g. from
/// [`HistoryStore::entries_newest_first`] or [`search`]) into
/// [`HistoryGroup`]s by [`date_bucket`], relative to `now`, preserving
/// input order and starting a new group only when the bucket actually
/// changes from the previous entry. For newest-first input this yields at
/// most one group per bucket (today/yesterday/last-7-days/older, in that
/// order); a caller that passes differently-ordered entries still gets a
/// correct, non-panicking grouping — just possibly more than one group per
/// bucket — since this only ever compares each entry to its immediate
/// predecessor.
pub fn group_by_date<'a>(
    entries: impl IntoIterator<Item = &'a HistoryEntry>,
    now: u64,
) -> Vec<HistoryGroup<'a>> {
    let mut groups: Vec<HistoryGroup<'a>> = Vec::new();
    for entry in entries {
        let bucket = date_bucket(entry.visited_at, now);
        match groups.last_mut() {
            Some(group) if group.bucket == bucket => group.entries.push(entry),
            _ => groups.push(HistoryGroup {
                bucket,
                entries: vec![entry],
            }),
        }
    }
    groups
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
        assert_eq!(store.entries()[0].favicon, None);
        assert_eq!(store.entries()[0].visit_count, 1);
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
        // D27: a consecutive re-visit bumps visit_count on the same entry.
        assert_eq!(store.entries()[0].visit_count, 2);
    }

    #[test]
    fn does_not_collapse_non_consecutive_repeat_visits() {
        let mut store = HistoryStore::new();
        store.record_visit("https://a.example/", None, 1, 0);
        store.record_visit("https://b.example/", None, 2, 0);
        store.record_visit("https://a.example/", None, 3, 0);
        assert_eq!(store.entries().len(), 3);
        // D27: browsing away and back starts a fresh entry at visit_count 1,
        // it does not add to the earlier entry for the same URL.
        assert_eq!(store.entries()[0].visit_count, 1);
        assert_eq!(store.entries()[2].visit_count, 1);
    }

    #[test]
    fn visit_count_keeps_incrementing_across_several_reloads() {
        let mut store = HistoryStore::new();
        store.record_visit("https://example.com/", None, 1, 0);
        store.record_visit("https://example.com/", None, 2, 0);
        store.record_visit("https://example.com/", None, 3, 0);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].visit_count, 3);
    }

    #[test]
    fn updates_favicon_by_id() {
        let mut store = HistoryStore::new();
        let id = store.record_visit("https://example.com/", None, 1, 0);
        assert_eq!(store.entries()[0].favicon, None);
        assert!(store.update_favicon(id, "https://example.com/favicon.ico".to_owned()));
        assert_eq!(
            store.entries()[0].favicon.as_deref(),
            Some("https://example.com/favicon.ico")
        );
    }

    #[test]
    fn updating_favicon_of_missing_id_is_a_noop() {
        let mut store = HistoryStore::new();
        store.record_visit("https://example.com/", None, 1, 0);
        assert!(!store.update_favicon(999, "https://example.com/favicon.ico".to_owned()));
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

    #[test]
    fn pre_issue_18_history_json_without_favicon_or_visit_count_still_loads() {
        // Shape of a `history.json` written by the Issue #4 store: no
        // `favicon`/`visit_count` keys at all. `#[serde(default)]` must fill
        // them in rather than fail deserialization of the whole file.
        let json = r#"{
            "entries": [
                {"id": 1, "url": "https://example.com/", "title": "Example", "visited_at": 100}
            ],
            "next_id": 2
        }"#;
        let store: HistoryStore = serde_json::from_str(json).expect("old-format JSON should parse");
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].favicon, None);
        assert_eq!(store.entries()[0].visit_count, 1);
    }

    #[test]
    fn search_matches_url_or_title_case_insensitively() {
        let mut store = HistoryStore::new();
        store.record_visit(
            "https://rust-lang.org/",
            Some("The Rust Programming Language".to_owned()),
            1,
            0,
        );
        store.record_visit(
            "https://example.com/",
            Some("Example Domain".to_owned()),
            2,
            0,
        );

        let by_url = store.search("RUST-lang");
        assert_eq!(by_url.len(), 1);
        assert_eq!(by_url[0].url, "https://rust-lang.org/");

        let by_title = store.search("domain");
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].url, "https://example.com/");

        assert!(store.search("nonexistent").is_empty());
    }

    #[test]
    fn search_with_empty_query_matches_nothing() {
        let mut store = HistoryStore::new();
        store.record_visit("https://example.com/", None, 1, 0);
        assert!(store.search("").is_empty());
    }

    #[test]
    fn search_ignores_entries_with_no_title() {
        let mut store = HistoryStore::new();
        store.record_visit("https://example.com/", None, 1, 0);
        // The URL itself does not contain "hello", and there is no title to
        // match against — this must not panic on the `None` title.
        assert!(store.search("hello").is_empty());
    }

    #[test]
    fn date_bucket_classifies_today_yesterday_last_7_days_and_older() {
        // "now" pinned to day 100 (an arbitrary multiple of a day so the
        // math is exact), noon UTC.
        let now = 100 * SECS_PER_DAY + 12 * 3600;
        assert_eq!(date_bucket(now, now), HistoryDateBucket::Today);
        assert_eq!(
            date_bucket(100 * SECS_PER_DAY, now),
            HistoryDateBucket::Today
        );
        assert_eq!(
            date_bucket(99 * SECS_PER_DAY, now),
            HistoryDateBucket::Yesterday
        );
        assert_eq!(
            date_bucket(98 * SECS_PER_DAY, now),
            HistoryDateBucket::Last7Days
        );
        assert_eq!(
            date_bucket(94 * SECS_PER_DAY, now),
            HistoryDateBucket::Last7Days
        );
        assert_eq!(
            date_bucket(93 * SECS_PER_DAY, now),
            HistoryDateBucket::Older
        );
        assert_eq!(date_bucket(0, now), HistoryDateBucket::Older);
    }

    #[test]
    fn date_bucket_clamps_future_timestamps_to_today() {
        let now = 10 * SECS_PER_DAY;
        assert_eq!(
            date_bucket(now + SECS_PER_DAY, now),
            HistoryDateBucket::Today
        );
    }

    #[test]
    fn group_by_date_groups_newest_first_entries_by_bucket() {
        let mut store = HistoryStore::new();
        let now = 100 * SECS_PER_DAY;
        // Recorded oldest-to-newest, as real visits are (`entries_newest_first`
        // just reverses insertion order — see its doc comment).
        store.record_visit("https://old.example/", None, now - 30 * SECS_PER_DAY, 0);
        store.record_visit(
            "https://last-week.example/",
            None,
            now - 3 * SECS_PER_DAY,
            0,
        );
        store.record_visit("https://yesterday.example/", None, now - SECS_PER_DAY, 0);
        store.record_visit("https://today.example/", None, now, 0);

        let groups = group_by_date(store.entries_newest_first(), now);
        let buckets: Vec<HistoryDateBucket> = groups.iter().map(|g| g.bucket).collect();
        assert_eq!(
            buckets,
            [
                HistoryDateBucket::Today,
                HistoryDateBucket::Yesterday,
                HistoryDateBucket::Last7Days,
                HistoryDateBucket::Older,
            ]
        );
        for group in &groups {
            assert_eq!(group.entries.len(), 1);
        }
    }

    #[test]
    fn group_by_date_merges_consecutive_entries_in_the_same_bucket() {
        let mut store = HistoryStore::new();
        let now = 100 * SECS_PER_DAY;
        store.record_visit("https://a.example/", None, now, 0);
        store.record_visit("https://b.example/", None, now + 1, 0);

        let groups = group_by_date(store.entries_newest_first(), now);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].bucket, HistoryDateBucket::Today);
        assert_eq!(groups[0].entries.len(), 2);
    }

    #[test]
    fn group_by_date_of_no_entries_is_empty() {
        assert!(group_by_date(std::iter::empty(), 0).is_empty());
    }
}
