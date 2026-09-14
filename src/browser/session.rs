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

/// One window's persisted tabs, in display order, plus which one was active
/// (Issue #149).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SavedWindow {
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub active_index: usize,
}

/// A whole session: every open window, in the order they were opened, each
/// with its own tabs. `#[serde(default)]` on every field means a file
/// missing a field entirely (an older schema, or a hand-edited partial
/// file) still parses — see [`Self::sanitize`] for the rest of the
/// corruption-tolerance story.
///
/// ## Reading a file written before Issue #149
///
/// Until #149 this struct *was* one window: `{"tabs": [...],
/// "active_index": N}`. Those two fields are still here, and still
/// deserialize, **purely so an existing `session.json` keeps working** —
/// nothing writes them any more ([`Self::from_windows`] fills `windows`
/// only, and they serialize as an empty list / `0`).
///
/// [`Self::sanitize`] is where the old shape becomes the new one: when
/// `windows` is empty and the legacy fields are not, they become the single
/// window. **That is the whole migration**, and it happens on the one path
/// that already exists to distrust this file, rather than in a separate
/// step something could skip. A user who upgrades mid-session gets their
/// tabs back and the next write is in the new shape.
///
/// Both shapes present at once (a hand-edited file) resolves to `windows`,
/// and the legacy fields are ignored rather than appended — guessing which
/// the user meant would risk silently doubling their tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionSnapshot {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<SavedWindow>,
    /// Pre-#149 schema, read-only. See the struct's doc comment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<SavedTab>,
    /// Pre-#149 schema, read-only. See the struct's doc comment.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active_index: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl SavedWindow {
    /// Capture one window's tab set, in display order. Pure — reads only
    /// `Tabs`'/`Tab`'s existing public getters, the same state the tab strip
    /// itself renders from (`app::sync_tab_strip`'s `TabSummary`), so what
    /// gets saved is exactly what the user currently sees.
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

    /// [`SessionSnapshot::sanitize`]'s per-window half: re-validate every
    /// URL and re-resolve `active_index`. `None` when nothing usable
    /// survives, so the caller drops this window rather than restoring an
    /// empty one (`Tabs` is never empty — see `browser::tabs`'s module doc).
    fn sanitize(mut self) -> Option<Self> {
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

impl SessionSnapshot {
    /// Capture every window that should be restorable, in the order given.
    ///
    /// The caller decides *which* windows those are — this module never
    /// learns what a private window is (D20). `app::persist_session` passes
    /// only the non-private ones, so a private window is absent from
    /// `session.json` rather than present-and-empty.
    pub fn from_windows<'a>(windows: impl IntoIterator<Item = &'a Tabs>) -> Self {
        Self {
            windows: windows.into_iter().map(SavedWindow::from_tabs).collect(),
            ..Self::default()
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
        // Issue #149: a file written before this issue has no `windows`.
        // Fold its single window in here — on the one path that already
        // exists to distrust this file, so nothing can skip the migration.
        if self.windows.is_empty() && !self.tabs.is_empty() {
            self.windows = vec![SavedWindow {
                tabs: std::mem::take(&mut self.tabs),
                active_index: self.active_index,
            }];
        }
        // Whichever shape it arrived in, the legacy fields are spent now:
        // leaving them populated would round-trip them back to disk.
        self.tabs = Vec::new();
        self.active_index = 0;

        self.windows = self
            .windows
            .into_iter()
            .filter_map(SavedWindow::sanitize)
            .collect();
        if self.windows.is_empty() {
            return None;
        }
        Some(self)
    }

    /// The first window's tabs — what `app::run` restores the primary
    /// window from before the event loop starts. Never empty for a snapshot
    /// that came out of [`Self::sanitize`].
    pub fn primary(&self) -> Option<&SavedWindow> {
        self.windows.first()
    }

    /// Every window after the first, in order (Issue #149). These cannot be
    /// opened until the event loop is running (a `BrowserWindow` needs an
    /// `EventLoopWindowTarget`), so `app::run` hands them to the loop rather
    /// than building them up front.
    pub fn secondary(&self) -> &[SavedWindow] {
        self.windows.get(1..).unwrap_or(&[])
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

    /// A snapshot in the **current** shape: one or more windows.
    fn snapshot(windows: Vec<SavedWindow>) -> SessionSnapshot {
        SessionSnapshot {
            windows,
            ..SessionSnapshot::default()
        }
    }

    fn window(tabs: Vec<SavedTab>, active_index: usize) -> SavedWindow {
        SavedWindow { tabs, active_index }
    }

    /// A snapshot in the **pre-#149** shape: tabs at the top level, no
    /// `windows`. What an existing `session.json` on a user's disk looks
    /// like.
    fn legacy(tabs: Vec<SavedTab>, active_index: usize) -> SessionSnapshot {
        SessionSnapshot {
            tabs,
            active_index,
            ..SessionSnapshot::default()
        }
    }

    /// The single window a snapshot sanitized down to, for the many tests
    /// below that only ever have one.
    fn only_window(snapshot: &SessionSnapshot) -> &SavedWindow {
        assert_eq!(snapshot.windows.len(), 1, "{snapshot:?}");
        &snapshot.windows[0]
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

        let saved = SavedWindow::from_tabs(&tabs);

        assert_eq!(saved.tabs.len(), 2);
        assert_eq!(saved.active_index, 0);
        assert_eq!(saved.tabs[0].url, "https://a.example/");
        assert_eq!(saved.tabs[0].title, None);
        assert_eq!(saved.tabs[1].url, "https://b.example/");
        assert_eq!(saved.tabs[1].title.as_deref(), Some("B"));
        assert_eq!(
            saved.tabs[1].favicon.as_deref(),
            Some("https://b.example/favicon.ico")
        );
    }

    // --- Issue #149: 複数ウィンドウ -----------------------------------

    #[test]
    fn from_windows_keeps_every_window_in_the_order_given() {
        let first = Tabs::new("https://a.example/");
        let mut second = Tabs::new("https://b.example/");
        second.open("https://c.example/");

        let snapshot = SessionSnapshot::from_windows([&first, &second]);

        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].tabs[0].url, "https://a.example/");
        assert_eq!(snapshot.windows[1].tabs.len(), 2);
        // 旧スキーマのフィールドには**何も書かない。**
        assert!(snapshot.tabs.is_empty());
        assert_eq!(snapshot.active_index, 0);
    }

    #[test]
    fn primary_and_secondary_split_the_windows() {
        let sanitized = snapshot(vec![
            window(vec![saved("https://a.example/")], 0),
            window(vec![saved("https://b.example/")], 0),
            window(vec![saved("https://c.example/")], 0),
        ])
        .sanitize()
        .unwrap();

        assert_eq!(
            sanitized.primary().unwrap().tabs[0].url,
            "https://a.example/"
        );
        let rest: Vec<&str> = sanitized
            .secondary()
            .iter()
            .map(|w| w.tabs[0].url.as_str())
            .collect();
        assert_eq!(rest, vec!["https://b.example/", "https://c.example/"]);
    }

    #[test]
    fn a_single_window_snapshot_has_no_secondary_windows() {
        let sanitized = snapshot(vec![window(vec![saved("https://a.example/")], 0)])
            .sanitize()
            .unwrap();
        assert!(sanitized.secondary().is_empty());
        assert!(sanitized.primary().is_some());
    }

    #[test]
    fn sanitize_drops_a_window_whose_every_url_is_rejected_and_keeps_the_rest() {
        // 1 つのウィンドウが丸ごと駄目でも、他のウィンドウは復元する。
        let sanitized = snapshot(vec![
            window(vec![saved("https://a.example/")], 0),
            window(vec![saved("javascript:alert(1)"), saved("  ")], 0),
            window(vec![saved("https://c.example/")], 0),
        ])
        .sanitize()
        .unwrap();

        let urls: Vec<&str> = sanitized
            .windows
            .iter()
            .map(|w| w.tabs[0].url.as_str())
            .collect();
        assert_eq!(urls, vec!["https://a.example/", "https://c.example/"]);
    }

    // --- Issue #149: 旧スキーマとの後方互換 ---------------------------

    #[test]
    fn a_pre_149_snapshot_becomes_one_window() {
        // 利用者のディスクに既にある `session.json` の形。**アップグレード
        // したらタブが消える**という壊れ方をしないことが、この Issue の
        // 受け入れ条件そのもの。
        let sanitized = legacy(
            vec![saved("https://a.example/"), saved("https://b.example/")],
            1,
        )
        .sanitize()
        .unwrap();

        let only = only_window(&sanitized);
        assert_eq!(only.tabs.len(), 2);
        assert_eq!(only.active_index, 1);
    }

    #[test]
    fn a_pre_149_snapshot_deserializes_from_the_old_json() {
        // 形そのものをここで固定する。上のテストは構造体を直接組んで
        // いるので、`serde` の受け口が変わっても気付けない。
        let json = r#"{"tabs":[{"url":"https://a.example/"}],"active_index":0}"#;
        let snapshot: SessionSnapshot = serde_json::from_str(json).unwrap();
        assert!(snapshot.windows.is_empty());
        let sanitized = snapshot.sanitize().unwrap();
        assert_eq!(only_window(&sanitized).tabs[0].url, "https://a.example/");
    }

    #[test]
    fn the_legacy_fields_are_spent_by_sanitize_and_never_round_trip() {
        // 移行したあと旧フィールドが残っていると、次の書き込みで
        // ディスクに戻ってしまい、同じタブが 2 つの形で並ぶ。
        let sanitized = legacy(vec![saved("https://a.example/")], 0)
            .sanitize()
            .unwrap();
        assert!(sanitized.tabs.is_empty());
        assert_eq!(sanitized.active_index, 0);

        // トップレベルに旧フィールドが無いことを、文字列ではなく構造で
        // 見る。`"tabs"` はウィンドウの中には正当に現れるので、素朴な
        // `contains` では区別できない。
        let json = serde_json::to_string(&sanitized).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let top = value.as_object().expect("an object");
        assert!(top.contains_key("windows"), "{json}");
        assert!(!top.contains_key("tabs"), "{json}");
        assert!(!top.contains_key("active_index"), "{json}");
    }

    #[test]
    fn both_shapes_at_once_resolves_to_windows_without_appending() {
        // 手で編集されたファイル。どちらを意図したか当てに行かない —
        // 足すと利用者のタブが黙って倍になる。
        let mixed = SessionSnapshot {
            windows: vec![window(vec![saved("https://new.example/")], 0)],
            tabs: vec![saved("https://old.example/")],
            active_index: 0,
        };
        let sanitized = mixed.sanitize().unwrap();
        let only = only_window(&sanitized);
        assert_eq!(only.tabs.len(), 1);
        assert_eq!(only.tabs[0].url, "https://new.example/");
    }

    // --- 既存の 1 ウィンドウ分の検査 (#25/D65 から) --------------------

    #[test]
    fn sanitize_keeps_a_well_formed_snapshot_unchanged_in_meaning() {
        let sanitized = snapshot(vec![window(
            vec![saved("https://a.example/"), saved("https://b.example/")],
            1,
        )])
        .sanitize()
        .unwrap();
        let only = only_window(&sanitized);
        assert_eq!(only.tabs.len(), 2);
        assert_eq!(only.active_index, 1);
    }

    #[test]
    fn sanitize_normalizes_urls_like_the_address_bar() {
        let sanitized = snapshot(vec![window(vec![saved("a.example")], 0)])
            .sanitize()
            .unwrap();
        assert_eq!(only_window(&sanitized).tabs[0].url, "https://a.example/");
    }

    #[test]
    fn sanitize_drops_entries_with_rejected_urls() {
        let sanitized = snapshot(vec![window(
            vec![
                saved("https://a.example/"),
                saved("javascript:alert(1)"),
                saved("https://b.example/"),
            ],
            0,
        )])
        .sanitize()
        .unwrap();
        assert_eq!(
            only_window(&sanitized)
                .tabs
                .iter()
                .map(|t| t.url.as_str())
                .collect::<Vec<_>>(),
            vec!["https://a.example/", "https://b.example/"]
        );
    }

    #[test]
    fn sanitize_returns_none_when_every_url_is_rejected() {
        let snapshot = snapshot(vec![window(
            vec![saved("javascript:alert(1)"), saved("   ")],
            0,
        )]);
        assert_eq!(snapshot.sanitize(), None);
    }

    #[test]
    fn sanitize_returns_none_for_an_empty_snapshot() {
        assert_eq!(SessionSnapshot::default().sanitize(), None);
    }

    #[test]
    fn sanitize_clamps_an_out_of_range_active_index() {
        let sanitized = snapshot(vec![window(
            vec![saved("https://a.example/"), saved("https://b.example/")],
            999,
        )])
        .sanitize()
        .unwrap();
        assert_eq!(only_window(&sanitized).active_index, 0);
    }

    #[test]
    fn sanitize_re_resolves_active_index_when_the_active_tab_itself_is_dropped() {
        let sanitized = snapshot(vec![window(
            vec![
                saved("https://a.example/"),
                saved("javascript:alert(1)"),
                saved("https://c.example/"),
            ],
            1, // the entry that gets dropped
        )])
        .sanitize()
        .unwrap();
        // Neither surviving entry was "active" in a way sanitize can trust,
        // so it falls back to the first surviving tab.
        let only = only_window(&sanitized);
        assert_eq!(only.active_index, 0);
        assert_eq!(only.tabs[0].url, "https://a.example/");
    }

    #[test]
    fn sanitize_follows_the_active_tab_when_earlier_entries_are_dropped() {
        let sanitized = snapshot(vec![window(
            vec![
                saved("javascript:alert(1)"),
                saved("https://a.example/"),
                saved("https://b.example/"),
            ],
            2, // "https://b.example/", which survives
        )])
        .sanitize()
        .unwrap();
        // b.example shifted from index 2 to index 1 once the first entry was
        // dropped; sanitize must follow it by URL, not by the stale index.
        let only = only_window(&sanitized);
        assert_eq!(only.active_index, 1);
        assert_eq!(only.tabs[1].url, "https://b.example/");
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = snapshot(vec![
            window(
                vec![
                    SavedTab {
                        url: "https://a.example/".to_owned(),
                        title: Some("A".to_owned()),
                        favicon: Some("https://a.example/favicon.ico".to_owned()),
                    },
                    saved("https://b.example/"),
                ],
                1,
            ),
            window(vec![saved("https://c.example/")], 0),
        ]);
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snapshot);
    }

    #[test]
    fn missing_optional_fields_deserialize_as_defaults() {
        // A minimal file: no `title`/`favicon` on the tab, no
        // `active_index` at all.
        let json = r#"{"windows":[{"tabs":[{"url":"https://a.example/"}]}]}"#;
        let snapshot: SessionSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].tabs.len(), 1);
        assert_eq!(snapshot.windows[0].tabs[0].title, None);
        assert_eq!(snapshot.windows[0].tabs[0].favicon, None);
        assert_eq!(snapshot.windows[0].active_index, 0);
    }
}
