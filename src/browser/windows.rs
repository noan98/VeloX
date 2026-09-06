//! A collection of browser windows, each owning its own [`Tabs`] (Issue #29).
//!
//! [`Tabs`] already turns "multiple tabs" into "multiple tabs, one active" —
//! `Windows` does the same one layer up: "multiple windows, each with its own
//! independent `Tabs`". This is plain UI/engine-independent Rust, like the
//! rest of `browser::`, so window open/close and the tab-ownership boundary
//! between windows is unit-tested without a real `tao`/`wry` window. See
//! docs/decisions.md D68 for the full design (in particular *why* this is a
//! flat collection of independent `Tabs`, rather than teaching `Tabs` itself
//! about multiple windows).
//!
//! **Ownership boundary**: exactly like `browser::tab`/`browser::tabs` never
//! reference a `wry`/`tao` `WebView`/`Window` (see docs/decisions.md D20),
//! this module never references one either. `app.rs` pairs each
//! [`WindowId`] here with a real `ui::window::BrowserWindow` in a separate
//! map it owns; `Windows` only tracks the logical, testable half — which
//! windows exist and which tabs belong to which.
//!
//! **`TabId` is *not* globally unique across windows.** Each window's
//! [`Tabs`] issues ids starting from its own `0` (see `Tabs::new`), so two
//! different windows can — and in practice usually do — both have a tab with
//! `TabId(0)`. This is deliberate: giving every window its own tab-id space
//! keeps `Tabs` itself completely unchanged (no new dependency on a
//! cross-window counter, no risk to its existing, heavily-tested behavior),
//! at the cost that a `TabId` is only ever meaningful *together with* the
//! [`WindowId`] of the window it came from. Every per-tab event that can
//! cross the window boundary (`crate::app::UserEvent`'s navigation/load/
//! title/favicon variants) carries both ids for exactly this reason — see
//! docs/decisions.md D68.
//!
//! **In-page find (Issue #43) is per-window, for the same reason.** Each
//! window's own find bar (`ui::toolbar.html`, one per toolbar webview) can
//! have its own session open at once — searching "foo" in one window must
//! never touch, or be closed by, a search for "bar" in another — so
//! [`WindowEntry::find`] lives right alongside that window's `tabs`, not as
//! one global `Option` shared by every window (see docs/decisions.md D68's
//! "複数ウィンドウ × ページ内検索" section, added when Issue #43 (D69) and
//! this issue were integrated).

use super::find::FindState;
use super::session::SavedTab;
use super::tabs::Tabs;
use super::window_id::WindowId;

/// One open window's worth of state this layer tracks: its id, its own
/// independent tab collection, and its own independent in-page find session
/// (if one is currently open).
#[derive(Debug)]
struct WindowEntry {
    id: WindowId,
    tabs: Tabs,
    /// This window's in-page find session (Issue #43), if the find bar is
    /// currently open in it. `None` whenever it is closed. See
    /// `browser::find::FindState`'s doc comment: only one session exists at
    /// a time *per window*, tied to whichever tab was active in that window
    /// when it opened.
    find: Option<FindState>,
    /// Whole-window private browsing (Issue #27, see docs/decisions.md D74).
    /// Set once, at construction (`push_window`), and never flipped
    /// afterwards — a window's privacy is decided the moment it opens
    /// (`Ctrl/Cmd+Shift+N`, `--private`) and stays fixed for its whole
    /// lifetime, exactly like `Config::private` did for the single-window
    /// process D14 originally described. `app.rs` reads this via
    /// [`Windows::is_private`] wherever it used to read a single
    /// process-wide `AppState::history_enabled` bool (history/input-history
    /// recording, session persistence) — see D74 for why per-window is
    /// necessary once two windows with different privacy can coexist.
    private: bool,
}

/// An ordered collection of open windows, each with its own [`Tabs`].
///
/// Unlike [`Tabs`] (which always keeps at least one tab open — closing the
/// last one is a no-op), `Windows` freely allows closing its last remaining
/// window: on the real desktop, closing the last window ends the
/// application (see `app::run`'s `ControlFlow::Exit` once
/// `Windows::is_empty()`), so there is no "always keep one" invariant to
/// enforce here — the caller decides what an empty `Windows` means.
#[derive(Debug)]
pub struct Windows {
    entries: Vec<WindowEntry>,
    next_id: u64,
}

impl Windows {
    /// Start with a single, non-private window, one tab loading
    /// `initial_url`. Convenience wrapper around
    /// [`Self::new_with_privacy`] for every call site that does not care
    /// about private browsing (nearly every existing test in this module) —
    /// see that function for the private-launch (`--private`/`VELOX_PRIVATE`)
    /// case.
    pub fn new(initial_url: impl Into<String>) -> Self {
        Self::new_with_privacy(initial_url, false)
    }

    /// Start with a single window, one tab loading `initial_url`, whose
    /// privacy is `private` (Issue #27, D74). `app::run` calls this for the
    /// very first window with `config.private` — a process launched with
    /// `--private`/`VELOX_PRIVATE` still starts private, exactly as D14
    /// originally specified; this just moves that flag from a whole-process
    /// setting onto the one window that exists at that point.
    pub fn new_with_privacy(initial_url: impl Into<String>, private: bool) -> Self {
        let mut windows = Windows {
            entries: Vec::new(),
            next_id: 0,
        };
        windows.push_window(Tabs::new(initial_url), private);
        windows
    }

    fn take_id(&mut self) -> WindowId {
        let id = WindowId::from(self.next_id);
        self.next_id += 1;
        id
    }

    fn push_window(&mut self, tabs: Tabs, private: bool) -> WindowId {
        let id = self.take_id();
        self.entries.push(WindowEntry {
            id,
            tabs,
            find: None,
            private,
        });
        id
    }

    /// Open a brand new, non-private window with a single tab at
    /// `initial_url` (Ctrl/Cmd+N, `ToolbarCommand::NewWindow`/
    /// `ContentShortcut::NewWindow`). Convenience wrapper around
    /// [`Self::open_window_with_privacy`] — see that function's doc comment,
    /// and docs/decisions.md D74, for why regular Ctrl/Cmd+N does not
    /// literally always pass `false` here (it passes `config.private`
    /// through `app::open_new_window`, so a `--private`-launched process's
    /// windows stay private; only the explicit "false" every test in this
    /// module wants is hardcoded by this wrapper).
    pub fn open_window(&mut self, initial_url: impl Into<String>) -> WindowId {
        self.open_window_with_privacy(initial_url, false)
    }

    /// Open a brand new window with a single tab at `initial_url`, whose
    /// privacy is `private` (Issue #27, D74) — the general form
    /// `app::open_new_window` calls for every one of its three trigger paths
    /// (`ToolbarCommand`/`ContentShortcut`/`AutomationCommand`, each in a
    /// `NewWindow` and a `NewPrivateWindow` flavor). Returns the new
    /// window's id, which the caller pairs with a real
    /// `ui::window::BrowserWindow` built with the same `private` value (see
    /// `ui::window::BrowserWindow::new`'s `private` parameter) — the two
    /// must always agree, since this flag is what gates whether that
    /// window's page visits reach `AppState::history`/`input_history`/
    /// `session.json` (see [`Self::is_private`]).
    pub fn open_window_with_privacy(
        &mut self,
        initial_url: impl Into<String>,
        private: bool,
    ) -> WindowId {
        self.push_window(Tabs::new(initial_url), private)
    }

    /// Open a new, non-private window whose tabs are restored from a
    /// previous session's snapshot (Issue #25's `Tabs::restore`, reused as
    /// is). Only ever used for the *first* window at startup in #29's scope
    /// — see docs/decisions.md D68 for why multi-window session restore is a
    /// follow-up. Always non-private: `app::run` only ever takes this path
    /// when `config.restore_previous_session && !config.private` already
    /// held (D14/D65 — a private launch restores nothing), so there is no
    /// `private` parameter to get wrong here.
    pub fn open_restored_window(&mut self, saved: &[SavedTab], active_index: usize) -> WindowId {
        self.push_window(Tabs::restore(saved, active_index), false)
    }

    /// Whether window `id` is private (Issue #27, D74) — `None` for an
    /// unknown window id, the same "stale id" convention every other
    /// id-addressed lookup here follows (see [`Self::tabs`]). `app.rs` reads
    /// this instead of a single process-wide `AppState::history_enabled`
    /// bool wherever a decision (record a visit, persist the session) needs
    /// to know *this* window's own privacy, since two windows can now
    /// disagree.
    pub fn is_private(&self, id: WindowId) -> Option<bool> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.private)
    }

    /// Close window `id`, dropping its `Tabs` (and every tab in it) along
    /// with it. Returns whether a window was actually removed — `false` for
    /// an unknown id, matching the "stale id is a safe no-op" convention
    /// every other id-addressed operation in `browser::` follows (see
    /// `TabId`'s doc comment).
    ///
    /// Removing the *last* window is allowed and leaves `Windows` empty —
    /// see the struct doc comment for why that differs from `Tabs::close`.
    pub fn close_window(&mut self, id: WindowId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// The tabs belonging to window `id`, or `None` if no such window is
    /// open (already closed, or never existed).
    pub fn tabs(&self, id: WindowId) -> Option<&Tabs> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.tabs)
    }

    /// Mutable version of [`Self::tabs`].
    pub fn tabs_mut(&mut self, id: WindowId) -> Option<&mut Tabs> {
        self.entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .map(|entry| &mut entry.tabs)
    }

    /// Window `id`'s in-page find session (Issue #43), if it currently has
    /// one open. `None` for a closed find bar *or* an unknown window id —
    /// callers that need to tell the two apart already know whether `id` is
    /// open (see `tabs`/`contains`).
    pub fn find(&self, id: WindowId) -> Option<&FindState> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.find.as_ref())
    }

    /// Mutable version of [`Self::find`].
    pub fn find_mut(&mut self, id: WindowId) -> Option<&mut FindState> {
        self.entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.find.as_mut())
    }

    /// Open (or replace) window `id`'s find session — `app::open_find_bar`
    /// starting a fresh search always discards whatever session that window
    /// had before. A no-op for an unknown window id, the same "stale id is a
    /// safe no-op" convention every other id-addressed operation here
    /// follows.
    pub fn set_find(&mut self, id: WindowId, session: FindState) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.find = Some(session);
        }
    }

    /// Close window `id`'s find session, returning it if one was open
    /// (`None` for an unknown window id *or* one with no session open —
    /// same shape as `Tabs::reopen_closed`'s `Option`-returning "take").
    pub fn take_find(&mut self, id: WindowId) -> Option<FindState> {
        self.entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.find.take())
    }

    /// Whether `id` refers to a currently open window.
    pub fn contains(&self, id: WindowId) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    /// Every currently open window's id, in the order the windows were
    /// opened (oldest first).
    pub fn ids(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.entries.iter().map(|entry| entry.id)
    }

    /// How many windows are currently open.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no open windows left — the caller's cue to end the
    /// process (see the struct doc comment).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::tabs::ActivationEffect;

    #[test]
    fn new_starts_with_exactly_one_window_and_one_tab() {
        let windows = Windows::new("https://example.com/");
        assert_eq!(windows.len(), 1);
        let id = windows.ids().next().unwrap();
        assert_eq!(windows.tabs(id).unwrap().len(), 1);
        assert_eq!(
            windows.tabs(id).unwrap().active().current_url(),
            "https://example.com/"
        );
    }

    #[test]
    fn open_window_returns_unique_ids_never_reused() {
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");
        let third = windows.open_window("https://c.example/");
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_ne!(first, third);
        assert_eq!(windows.len(), 3);

        // Closing one and opening another must never resurrect the closed
        // id — the same "never reused" guarantee `TabId` documents.
        assert!(windows.close_window(second));
        let fourth = windows.open_window("https://d.example/");
        assert_ne!(fourth, second);
        assert_ne!(fourth, first);
        assert_ne!(fourth, third);
    }

    #[test]
    fn each_window_has_its_own_independent_tab_id_space() {
        // Deliberately documents the module doc comment's claim: two
        // windows' `Tabs` both start their own tab ids at 0, so the very
        // same `TabId` value legitimately refers to two different tabs in
        // two different windows.
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");

        let first_tab_id = windows.tabs(first).unwrap().active_id();
        let second_tab_id = windows.tabs(second).unwrap().active_id();
        assert_eq!(first_tab_id, second_tab_id);

        // And they are genuinely independent tabs, not aliases of the same
        // one: navigating/opening in one window's `Tabs` never touches the
        // other's.
        windows.tabs_mut(first).unwrap().open("https://a2.example/");
        assert_eq!(windows.tabs(first).unwrap().len(), 2);
        assert_eq!(windows.tabs(second).unwrap().len(), 1);
    }

    #[test]
    fn tabs_and_tabs_mut_return_none_for_an_unknown_window() {
        let mut windows = Windows::new("https://example.com/");
        let unknown = WindowId::from(9999);
        assert!(windows.tabs(unknown).is_none());
        assert!(windows.tabs_mut(unknown).is_none());
        assert!(!windows.contains(unknown));
    }

    #[test]
    fn close_window_removes_only_that_windows_tabs() {
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");
        let third = windows.open_window("https://c.example/");

        assert!(windows.close_window(second));
        assert_eq!(windows.len(), 2);
        assert!(windows.contains(first));
        assert!(!windows.contains(second));
        assert!(windows.contains(third));
        // The surviving windows' own tabs must be untouched.
        assert_eq!(
            windows.tabs(first).unwrap().active().current_url(),
            "https://a.example/"
        );
        assert_eq!(
            windows.tabs(third).unwrap().active().current_url(),
            "https://c.example/"
        );
    }

    #[test]
    fn close_window_on_an_unknown_id_is_a_noop() {
        let mut windows = Windows::new("https://example.com/");
        assert!(!windows.close_window(WindowId::from(9999)));
        assert_eq!(windows.len(), 1);
    }

    #[test]
    fn closing_the_same_window_twice_only_succeeds_once() {
        let mut windows = Windows::new("https://example.com/");
        let extra = windows.open_window("https://b.example/");
        assert!(windows.close_window(extra));
        assert!(!windows.close_window(extra));
    }

    #[test]
    fn closing_the_last_window_is_allowed_and_leaves_windows_empty() {
        // Unlike `Tabs::close` (which refuses to close the last tab),
        // `Windows` has no "always keep one" rule — the caller decides what
        // an empty `Windows` means (ending the process).
        let mut windows = Windows::new("https://example.com/");
        let only = windows.ids().next().unwrap();
        assert!(windows.close_window(only));
        assert!(windows.is_empty());
        assert_eq!(windows.len(), 0);
    }

    #[test]
    fn ids_lists_every_open_window_oldest_first() {
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");
        let third = windows.open_window("https://c.example/");
        assert_eq!(
            windows.ids().collect::<Vec<_>>(),
            vec![first, second, third]
        );
    }

    #[test]
    fn open_restored_window_rebuilds_tabs_from_a_snapshot() {
        let mut windows = Windows::new("https://home.example/");
        let saved = vec![
            SavedTab {
                url: "https://a.example/".to_owned(),
                title: None,
                favicon: None,
            },
            SavedTab {
                url: "https://b.example/".to_owned(),
                title: None,
                favicon: None,
            },
        ];
        let restored = windows.open_restored_window(&saved, 1);
        assert_eq!(windows.len(), 2);
        let tabs = windows.tabs(restored).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs.active().current_url(), "https://b.example/");
    }

    #[test]
    fn mutating_one_windows_tabs_never_leaks_into_another() {
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");

        // Activity in `second` (open, close, activate, suspend) must never
        // change what `first` reports.
        let extra = windows
            .tabs_mut(second)
            .unwrap()
            .open("https://b2.example/");
        assert_eq!(
            windows.tabs_mut(second).unwrap().activate(extra),
            Some(ActivationEffect::Switch)
        );
        windows.tabs_mut(second).unwrap().close(extra);

        assert_eq!(windows.tabs(first).unwrap().len(), 1);
        assert_eq!(
            windows.tabs(first).unwrap().active().current_url(),
            "https://a.example/"
        );
    }

    // --- In-page find (Issue #43) is per-window, not global — see the
    // module doc comment's "複数ウィンドウ × ページ内検索" note and
    // docs/decisions.md D68. ---

    #[test]
    fn a_new_window_has_no_find_session() {
        let windows = Windows::new("https://example.com/");
        let id = windows.ids().next().unwrap();
        assert!(windows.find(id).is_none());
    }

    #[test]
    fn find_sessions_are_independent_per_window() {
        // The exact scenario the multi-window/#43 integration must not get
        // wrong: two windows whose active tabs happen to share the same
        // `TabId` (see `each_window_has_its_own_independent_tab_id_space`)
        // each get their own find session, keyed by `WindowId`, not by the
        // `TabId` alone.
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");
        let first_tab = windows.tabs(first).unwrap().active_id();
        let second_tab = windows.tabs(second).unwrap().active_id();
        assert_eq!(
            first_tab, second_tab,
            "test assumes both windows share a TabId value"
        );

        let mut first_session = FindState::new(first_tab);
        first_session.set_query("foo".to_owned(), false);
        windows.set_find(first, first_session);

        assert_eq!(windows.find(first).unwrap().query(), "foo");
        assert!(
            windows.find(second).is_none(),
            "opening a find session in one window must not leak into another"
        );

        let mut second_session = FindState::new(second_tab);
        second_session.set_query("bar".to_owned(), true);
        windows.set_find(second, second_session);

        // Both sessions coexist, independently, even though their tab ids
        // are numerically identical.
        assert_eq!(windows.find(first).unwrap().query(), "foo");
        assert!(!windows.find(first).unwrap().case_sensitive());
        assert_eq!(windows.find(second).unwrap().query(), "bar");
        assert!(windows.find(second).unwrap().case_sensitive());
    }

    #[test]
    fn taking_one_windows_find_session_never_closes_anothers() {
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");
        let first_tab = windows.tabs(first).unwrap().active_id();
        let second_tab = windows.tabs(second).unwrap().active_id();

        windows.set_find(first, FindState::new(first_tab));
        windows.set_find(second, FindState::new(second_tab));

        let taken = windows.take_find(first);
        assert!(taken.is_some());
        assert!(
            windows.find(first).is_none(),
            "take_find must remove the session it returned"
        );
        assert!(
            windows.find(second).is_some(),
            "closing window 1's find bar must not close window 2's"
        );
    }

    #[test]
    fn find_mut_edits_only_the_targeted_windows_session() {
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let second = windows.open_window("https://b.example/");
        let first_tab = windows.tabs(first).unwrap().active_id();
        let second_tab = windows.tabs(second).unwrap().active_id();

        windows.set_find(first, FindState::new(first_tab));
        windows.set_find(second, FindState::new(second_tab));

        windows
            .find_mut(first)
            .unwrap()
            .set_query("only-first".to_owned(), false);

        assert_eq!(windows.find(first).unwrap().query(), "only-first");
        assert_eq!(windows.find(second).unwrap().query(), "");
    }

    #[test]
    fn set_find_take_find_and_find_mut_are_noops_for_an_unknown_window() {
        let mut windows = Windows::new("https://example.com/");
        let existing_tab = windows.ids().next().unwrap();
        let tab_id = windows.tabs(existing_tab).unwrap().active_id();
        let unknown = WindowId::from(9999);
        windows.set_find(unknown, FindState::new(tab_id));
        assert!(windows.find(unknown).is_none());
        assert!(windows.find_mut(unknown).is_none());
        assert!(windows.take_find(unknown).is_none());
    }

    #[test]
    fn closing_a_window_drops_its_find_session_without_a_panic() {
        let mut windows = Windows::new("https://a.example/");
        let first = windows.ids().next().unwrap();
        let tab = windows.tabs(first).unwrap().active_id();
        windows.set_find(first, FindState::new(tab));
        assert!(windows.close_window(first));
        // `first` is gone entirely now; querying its (former) find session
        // must behave exactly like any other unknown-id lookup, not panic.
        assert!(windows.find(first).is_none());
    }

    // --- Per-window private browsing (Issue #27, see docs/decisions.md D74) ---

    #[test]
    fn a_window_opened_by_new_is_not_private() {
        let windows = Windows::new("https://example.com/");
        let id = windows.ids().next().unwrap();
        assert_eq!(windows.is_private(id), Some(false));
    }

    #[test]
    fn new_with_privacy_marks_the_first_window_private() {
        let windows = Windows::new_with_privacy("https://example.com/", true);
        let id = windows.ids().next().unwrap();
        assert_eq!(windows.is_private(id), Some(true));
    }

    #[test]
    fn open_window_marks_the_new_window_non_private() {
        let mut windows = Windows::new("https://a.example/");
        let second = windows.open_window("https://b.example/");
        assert_eq!(windows.is_private(second), Some(false));
    }

    #[test]
    fn open_window_with_privacy_marks_the_new_window_private() {
        let mut windows = Windows::new("https://a.example/");
        let second = windows.open_window_with_privacy("https://b.example/", true);
        assert_eq!(windows.is_private(second), Some(true));
    }

    #[test]
    fn is_private_returns_none_for_an_unknown_window() {
        let windows = Windows::new("https://example.com/");
        assert_eq!(windows.is_private(WindowId::from(9999)), None);
    }

    #[test]
    fn open_restored_window_is_never_private() {
        let mut windows = Windows::new("https://home.example/");
        let saved = vec![SavedTab {
            url: "https://a.example/".to_owned(),
            title: None,
            favicon: None,
        }];
        let restored = windows.open_restored_window(&saved, 0);
        assert_eq!(windows.is_private(restored), Some(false));
    }

    #[test]
    fn a_normal_and_a_private_window_sharing_the_same_tab_id_keep_independent_privacy() {
        // The exact scenario D74/the PR description calls out: a normal
        // window and a private window opened side by side end up with the
        // same `TabId` (see `each_window_has_its_own_independent_tab_id_space`
        // above), so any code path that keyed privacy off `TabId` alone
        // would confuse the two. `Windows` keys it off `WindowId` instead,
        // so this must never happen.
        let mut windows = Windows::new("https://normal.example/");
        let normal = windows.ids().next().unwrap();
        let private = windows.open_window_with_privacy("https://private.example/", true);

        let normal_tab = windows.tabs(normal).unwrap().active_id();
        let private_tab = windows.tabs(private).unwrap().active_id();
        assert_eq!(
            normal_tab, private_tab,
            "test assumes both windows share a TabId value"
        );

        assert_eq!(windows.is_private(normal), Some(false));
        assert_eq!(windows.is_private(private), Some(true));

        // Activity in the private window (a second tab, closing it again)
        // must not flip the normal window's privacy, and vice versa.
        let extra = windows
            .tabs_mut(private)
            .unwrap()
            .open("https://private2.example/");
        windows.tabs_mut(private).unwrap().close(extra);
        assert_eq!(windows.is_private(normal), Some(false));
        assert_eq!(windows.is_private(private), Some(true));
    }

    #[test]
    fn len_and_is_empty_agree() {
        let mut windows = Windows::new("https://example.com/");
        assert_eq!(windows.len(), 1);
        assert!(!windows.is_empty());
        let extra = windows.open_window("https://b.example/");
        assert_eq!(windows.len(), 2);
        windows.close_window(extra);
        assert_eq!(windows.len(), 1);
        assert!(!windows.is_empty());
    }
}
