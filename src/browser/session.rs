//! Session snapshot: what "前回のタブを復元" (Issue #25) persists and restores.
//!
//! This module only defines the serializable shape and pure
//! snapshot/validation logic; the actual JSON file IO lives in
//! [`super::persistence`] (`load_session`/`save_session`), following the
//! exact same "dumb IO, smart pure logic elsewhere" split as
//! [`super::history`]/[`super::bookmarks`]. Turning a snapshot back into a
//! live [`super::tabs::Tabs`] is [`super::tabs::Tabs::restore`] — this
//! module never touches `Tab`'s private fields itself, only its public
//! getters (via [`SessionSnapshot::from_tabs`]) and the [`SavedTab`] data
//! `Tabs::restore` consumes. See docs/decisions.md D65 for the full design
//! rationale.

use serde::{Deserialize, Serialize};

use super::navigation;
use super::tab::Favicon;
use super::tabs::Tabs;

/// One tab's persisted state: just enough to show a tab strip and reload the
/// page — never scroll position, form input, or JS-side session history
/// (none of that is available once the webview is gone, the same trade-off
/// tab suspension already accepts — see docs/decisions.md D9). `title`/
/// `favicon` are `None` the same way [`super::tab::Tab`]'s are: "never
/// arrived yet" and "this page has none" are not distinguished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub favicon: Option<String>,
}

/// A whole session: every open tab, in display order, plus which one was
/// active. `#[serde(default)]` on every field means a file missing a field
/// entirely (an older schema, or a hand-edited partial file) still parses —
/// see [`Self::sanitize`] for the rest of the corruption-tolerance story.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionSnapshot {
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub active_index: usize,
}

impl SessionSnapshot {
    /// Capture the current tab set as a snapshot to persist, in display
    /// order. Pure — reads only `Tabs`'/`Tab`'s existing public getters, the
    /// same state the tab strip itself renders from
    /// (`app::sync_tab_strip`'s `TabSummary`), so what gets saved is exactly
    /// what the user currently sees.
    pub fn from_tabs(tabs: &Tabs) -> Self {
        let active_id = tabs.active_id();
        let mut active_index = 0;
        let saved = tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                if tab.id() == active_id {
                    active_index = index;
                }
                SavedTab {
                    url: tab.current_url().to_owned(),
                    title: tab.title().map(str::to_owned),
                    favicon: match tab.favicon() {
                        Favicon::Url(url) => Some(url.clone()),
                        Favicon::Unknown => None,
                    },
                }
            })
            .collect();
        Self {
            tabs: saved,
            active_index,
        }
    }

    /// Validate and repair a snapshot freshly deserialized from disk before
    /// it is trusted to rebuild [`Tabs`] from (`Tabs::restore`). A corrupt
    /// or hostile `session.json` must never stop VeloX from starting (the
    /// issue's top acceptance criterion) — this is the one place that
    /// enforces it for session data specifically, the same role
    /// `navigation::normalize_input` already plays for every other URL
    /// entry point (address bar, bookmarks, `VELOX_HOMEPAGE`).
    ///
    /// - Every tab's `url` is re-validated (and normalized) through
    ///   [`navigation::normalize_input`] — never a second copy of the scheme
    ///   rules. An entry whose URL fails (a rejected scheme like
    ///   `javascript:`, unparseable text, or simply empty) is dropped rather
    ///   than discarding the whole session over one bad entry.
    /// - Returns `None` when nothing usable survives — an empty `tabs` list
    ///   to begin with, or every entry's URL was rejected — so the caller
    ///   falls back to a fresh single-tab session at the homepage instead of
    ///   restoring zero tabs (`Tabs` is never empty, see
    ///   `browser::tabs`'s module doc comment).
    /// - `active_index` is clamped/re-resolved rather than trusted as-is: a
    ///   deserialized `usize` can be any value (including one far out of
    ///   range for a hand-edited or truncated file), and even a valid index
    ///   can point at an entry that was just dropped above. The previously
    ///   active tab's URL is looked up in the surviving list by position;
    ///   if it is gone (or the index was already out of range), the first
    ///   surviving tab becomes active instead of panicking on an
    ///   out-of-bounds index later.
    pub fn sanitize(mut self) -> Option<Self> {
        let previously_active_url = self.tabs.get(self.active_index).map(|tab| tab.url.clone());
        self.tabs
            .retain_mut(|tab| match navigation::normalize_input(&tab.url) {
                Some(normalized) => {
                    tab.url = normalized;
                    true
                }
                None => false,
            });
        if self.tabs.is_empty() {
            return None;
        }
        self.active_index = previously_active_url
            .and_then(|url| self.tabs.iter().position(|tab| tab.url == url))
            .unwrap_or(0);
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(url: &str) -> SavedTab {
        SavedTab {
            url: url.to_owned(),
            title: None,
            favicon: None,
        }
    }

    #[test]
    fn from_tabs_captures_url_title_favicon_and_active_index() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        tabs.get_mut(b).unwrap().set_title("B");
        tabs.get_mut(b)
            .unwrap()
            .set_favicon_url("https://b.example/favicon.ico");
        tabs.activate(a);

        let snapshot = SessionSnapshot::from_tabs(&tabs);

        assert_eq!(snapshot.tabs.len(), 2);
        assert_eq!(snapshot.active_index, 0);
        assert_eq!(snapshot.tabs[0].url, "https://a.example/");
        assert_eq!(snapshot.tabs[0].title, None);
        assert_eq!(snapshot.tabs[1].url, "https://b.example/");
        assert_eq!(snapshot.tabs[1].title.as_deref(), Some("B"));
        assert_eq!(
            snapshot.tabs[1].favicon.as_deref(),
            Some("https://b.example/favicon.ico")
        );
    }

    #[test]
    fn sanitize_keeps_a_well_formed_snapshot_unchanged_in_meaning() {
        let snapshot = SessionSnapshot {
            tabs: vec![saved("https://a.example/"), saved("https://b.example/")],
            active_index: 1,
        };
        let sanitized = snapshot.sanitize().unwrap();
        assert_eq!(sanitized.tabs.len(), 2);
        assert_eq!(sanitized.active_index, 1);
    }

    #[test]
    fn sanitize_normalizes_urls_like_the_address_bar() {
        let snapshot = SessionSnapshot {
            tabs: vec![saved("a.example")],
            active_index: 0,
        };
        let sanitized = snapshot.sanitize().unwrap();
        assert_eq!(sanitized.tabs[0].url, "https://a.example/");
    }

    #[test]
    fn sanitize_drops_entries_with_rejected_urls() {
        let snapshot = SessionSnapshot {
            tabs: vec![
                saved("https://a.example/"),
                saved("javascript:alert(1)"),
                saved("https://b.example/"),
            ],
            active_index: 0,
        };
        let sanitized = snapshot.sanitize().unwrap();
        assert_eq!(
            sanitized
                .tabs
                .iter()
                .map(|t| t.url.as_str())
                .collect::<Vec<_>>(),
            vec!["https://a.example/", "https://b.example/"]
        );
    }

    #[test]
    fn sanitize_returns_none_when_every_url_is_rejected() {
        let snapshot = SessionSnapshot {
            tabs: vec![saved("javascript:alert(1)"), saved("   ")],
            active_index: 0,
        };
        assert_eq!(snapshot.sanitize(), None);
    }

    #[test]
    fn sanitize_returns_none_for_an_empty_snapshot() {
        assert_eq!(SessionSnapshot::default().sanitize(), None);
    }

    #[test]
    fn sanitize_clamps_an_out_of_range_active_index() {
        let snapshot = SessionSnapshot {
            tabs: vec![saved("https://a.example/"), saved("https://b.example/")],
            active_index: 999,
        };
        let sanitized = snapshot.sanitize().unwrap();
        assert_eq!(sanitized.active_index, 0);
    }

    #[test]
    fn sanitize_re_resolves_active_index_when_the_active_tab_itself_is_dropped() {
        let snapshot = SessionSnapshot {
            tabs: vec![
                saved("https://a.example/"),
                saved("javascript:alert(1)"),
                saved("https://c.example/"),
            ],
            active_index: 1, // the entry that gets dropped
        };
        let sanitized = snapshot.sanitize().unwrap();
        // Neither surviving entry was "active" in a way sanitize can trust,
        // so it falls back to the first surviving tab.
        assert_eq!(sanitized.active_index, 0);
        assert_eq!(sanitized.tabs[0].url, "https://a.example/");
    }

    #[test]
    fn sanitize_follows_the_active_tab_when_earlier_entries_are_dropped() {
        let snapshot = SessionSnapshot {
            tabs: vec![
                saved("javascript:alert(1)"),
                saved("https://a.example/"),
                saved("https://b.example/"),
            ],
            active_index: 2, // "https://b.example/", which survives
        };
        let sanitized = snapshot.sanitize().unwrap();
        // b.example shifted from index 2 to index 1 once the first entry was
        // dropped; sanitize must follow it by URL, not by the stale index.
        assert_eq!(sanitized.active_index, 1);
        assert_eq!(sanitized.tabs[1].url, "https://b.example/");
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = SessionSnapshot {
            tabs: vec![
                SavedTab {
                    url: "https://a.example/".to_owned(),
                    title: Some("A".to_owned()),
                    favicon: Some("https://a.example/favicon.ico".to_owned()),
                },
                saved("https://b.example/"),
            ],
            active_index: 1,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snapshot);
    }

    #[test]
    fn missing_optional_fields_deserialize_as_defaults() {
        // A minimal/older-schema file: no `title`/`favicon` on the tab, no
        // top-level `active_index` at all.
        let json = r#"{"tabs":[{"url":"https://a.example/"}]}"#;
        let snapshot: SessionSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.tabs.len(), 1);
        assert_eq!(snapshot.tabs[0].title, None);
        assert_eq!(snapshot.tabs[0].favicon, None);
        assert_eq!(snapshot.active_index, 0);
    }
}
