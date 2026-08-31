//! A collection of tabs plus which one is active.
//!
//! [`Tab`] only knows about a single page; `Tabs` is what turns that into
//! "multiple tabs, one of them active" — the piece the app needs to drive a
//! tab strip. This is plain UI/engine-independent Rust, like the rest of
//! `browser::`, so open/close/activate logic is unit-tested without a
//! window or a webview.

use super::tab::{Tab, TabId};

/// An ordered set of tabs with exactly one active tab.
///
/// VeloX always keeps at least one tab open: [`Tabs::close`] refuses to
/// close the last remaining tab, mirroring how the window always has
/// exactly one content view to show.
#[derive(Debug)]
pub struct Tabs {
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
}

impl Tabs {
    /// Start with a single tab loading `initial_url`.
    pub fn new(initial_url: impl Into<String>) -> Self {
        let mut next_id = 0;
        let id = Self::take_id(&mut next_id);
        Tabs {
            tabs: vec![Tab::new(id, initial_url)],
            active: 0,
            next_id,
        }
    }

    fn take_id(next_id: &mut u64) -> TabId {
        let id = TabId::from(*next_id);
        *next_id += 1;
        id
    }

    /// Number of open tabs.
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// `Tabs` is never empty; kept for the `len`/`is_empty` clippy pairing.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Tabs in display order (left to right in the tab strip).
    pub fn iter(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    /// The id of the currently active tab.
    pub fn active_id(&self) -> TabId {
        self.tabs[self.active].id()
    }

    /// The currently active tab.
    pub fn active(&self) -> &Tab {
        &self.tabs[self.active]
    }

    /// The currently active tab, mutably.
    pub fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Look up a tab by id, regardless of whether it is active.
    pub fn get(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id() == id)
    }

    /// Look up a tab by id, mutably.
    pub fn get_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id() == id)
    }

    fn index_of(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id() == id)
    }

    /// Open a new tab loading `url`, make it active, and return its id.
    ///
    /// Ids are never reused within a `Tabs`, even across closes.
    pub fn open(&mut self, url: impl Into<String>) -> TabId {
        let id = Self::take_id(&mut self.next_id);
        self.tabs.push(Tab::new(id, url));
        self.active = self.tabs.len() - 1;
        id
    }

    /// Close the tab `id`.
    ///
    /// Returns the id of the tab that is active afterwards (unchanged if a
    /// background tab was closed) when `id` was found and closed. Returns
    /// `None`, leaving the collection untouched, when `id` is unknown or it
    /// is the only remaining tab.
    pub fn close(&mut self, id: TabId) -> Option<TabId> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let index = self.index_of(id)?;
        self.tabs.remove(index);
        if index < self.active {
            // A tab to the left of the active one shifted everything after
            // it down by one slot; follow the active tab to its new index.
            self.active -= 1;
        } else if index == self.active {
            // The active tab itself closed: the tab that slid into its slot
            // becomes active (the next tab), or the new last tab if we just
            // closed the rightmost one.
            self.active = self.active.min(self.tabs.len() - 1);
        }
        Some(self.active_id())
    }

    /// Make `id` the active tab. Returns `false`, leaving the active tab
    /// unchanged, when `id` is unknown.
    pub fn activate(&mut self, id: TabId) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.active = index;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(tabs: &Tabs) -> Vec<TabId> {
        tabs.iter().map(Tab::id).collect()
    }

    #[test]
    fn starts_with_one_active_tab() {
        let tabs = Tabs::new("https://example.com/");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs.active().current_url(), "https://example.com/");
        assert_eq!(tabs.active_id(), tabs.iter().next().unwrap().id());
    }

    #[test]
    fn open_appends_and_activates_new_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let first = tabs.active_id();

        let second = tabs.open("https://b.example/");

        assert_eq!(tabs.len(), 2);
        assert_ne!(first, second);
        assert_eq!(tabs.active_id(), second);
        assert_eq!(tabs.active().current_url(), "https://b.example/");
        assert_eq!(ids(&tabs), vec![first, second]);
    }

    #[test]
    fn closing_the_last_tab_is_a_no_op() {
        let mut tabs = Tabs::new("https://example.com/");
        let only = tabs.active_id();

        assert_eq!(tabs.close(only), None);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs.active_id(), only);
    }

    #[test]
    fn closing_unknown_id_is_a_no_op() {
        let mut tabs = Tabs::new("https://example.com/");
        tabs.open("https://b.example/");
        let before = ids(&tabs);

        assert_eq!(tabs.close(TabId::from(999)), None);
        assert_eq!(ids(&tabs), before);
    }

    #[test]
    fn closing_background_tab_keeps_active_selection() {
        let mut tabs = Tabs::new("https://a.example/"); // index 0
        let a = tabs.active_id();
        tabs.open("https://b.example/"); // index 1, active
        let b = tabs.active_id();
        tabs.open("https://c.example/"); // index 2, active
        let c = tabs.active_id();
        assert_eq!(ids(&tabs), vec![a, b, c]);

        // Close A (to the left of active C): active index shifts left with
        // it but still points at C.
        let new_active = tabs.close(a).unwrap();
        assert_eq!(new_active, c);
        assert_eq!(tabs.active_id(), c);
        assert_eq!(ids(&tabs), vec![b, c]);
    }

    #[test]
    fn closing_active_middle_tab_activates_the_next_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/");
        assert_eq!(ids(&tabs), vec![a, b, c]);

        tabs.activate(b);
        let new_active = tabs.close(b).unwrap();

        assert_eq!(new_active, c);
        assert_eq!(tabs.active_id(), c);
        assert_eq!(ids(&tabs), vec![a, c]);
    }

    #[test]
    fn closing_active_rightmost_tab_activates_the_previous_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/");
        assert_eq!(tabs.active_id(), c);

        let new_active = tabs.close(c).unwrap();

        assert_eq!(new_active, b);
        assert_eq!(tabs.active_id(), b);
        assert_eq!(ids(&tabs), vec![a, b]);
    }

    #[test]
    fn closing_active_leftmost_tab_activates_the_following_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let _c = tabs.open("https://c.example/");
        tabs.activate(a);

        let new_active = tabs.close(a).unwrap();

        assert_eq!(new_active, b);
        assert_eq!(tabs.active_id(), b);
    }

    #[test]
    fn activate_switches_active_tab_and_rejects_unknown_ids() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        assert_eq!(tabs.active_id(), b);

        assert!(tabs.activate(a));
        assert_eq!(tabs.active_id(), a);

        assert!(!tabs.activate(TabId::from(999)));
        assert_eq!(tabs.active_id(), a);
    }

    #[test]
    fn ids_are_never_reused() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        tabs.close(b);
        let c = tabs.open("https://c.example/");

        assert_ne!(c, a);
        assert_ne!(c, b);
    }

    #[test]
    fn get_and_get_mut_find_any_tab_by_id() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");

        assert_eq!(tabs.get(a).unwrap().current_url(), "https://a.example/");
        assert!(tabs.get(TabId::from(999)).is_none());

        tabs.get_mut(b)
            .unwrap()
            .on_load_finished("https://b.example/done");
        assert_eq!(tabs.get(b).unwrap().current_url(), "https://b.example/done");
    }
}
