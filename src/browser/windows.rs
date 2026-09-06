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

use super::session::SavedTab;
use super::tabs::Tabs;
use super::window_id::WindowId;

/// One open window's worth of state this layer tracks: its id and its own
/// independent tab collection.
#[derive(Debug)]
struct WindowEntry {
    id: WindowId,
    tabs: Tabs,
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
    /// Start with a single window, one tab loading `initial_url`.
    pub fn new(initial_url: impl Into<String>) -> Self {
        let mut windows = Windows {
            entries: Vec::new(),
            next_id: 0,
        };
        windows.push_window(Tabs::new(initial_url));
        windows
    }

    fn take_id(&mut self) -> WindowId {
        let id = WindowId::from(self.next_id);
        self.next_id += 1;
        id
    }

    fn push_window(&mut self, tabs: Tabs) -> WindowId {
        let id = self.take_id();
        self.entries.push(WindowEntry { id, tabs });
        id
    }

    /// Open a brand new window with a single tab at `initial_url` (Ctrl/Cmd+N,
    /// `ToolbarCommand::NewWindow`/`ContentShortcut::NewWindow`). Returns the
    /// new window's id, which the caller pairs with a real
    /// `ui::window::BrowserWindow` built for it.
    pub fn open_window(&mut self, initial_url: impl Into<String>) -> WindowId {
        self.push_window(Tabs::new(initial_url))
    }

    /// Open a new window whose tabs are restored from a previous session's
    /// snapshot (Issue #25's `Tabs::restore`, reused as-is). Only ever used
    /// for the *first* window at startup in this issue's scope — see
    /// docs/decisions.md D68 for why multi-window session restore is a
    /// follow-up, not part of #29.
    pub fn open_restored_window(&mut self, saved: &[SavedTab], active_index: usize) -> WindowId {
        self.push_window(Tabs::restore(saved, active_index))
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
