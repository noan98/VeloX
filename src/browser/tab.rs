//! Per-tab browser state.
//!
//! The web engine owns the real session history (used for back/forward); this
//! struct mirrors just enough state to drive the UI (address bar contents,
//! loading indicator). Multiple tabs are tracked by [`super::tabs::Tabs`],
//! which owns a `Vec<Tab>` plus which one is active; a `Tab` itself only
//! knows about its own page.

/// Opaque, stable identifier for a tab.
///
/// Assigned once by [`super::tabs::Tabs`] when a tab is opened and never
/// reused, so a stale id (e.g. a `close_tab` message racing a second close)
/// simply refers to nothing rather than to the wrong tab. Sent to/from the
/// toolbar webview as a plain JSON number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(u64);

impl TabId {
    /// The underlying numeric id, e.g. to embed in a toolbar IPC message.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for TabId {
    fn from(id: u64) -> Self {
        TabId(id)
    }
}

/// State of a single browser tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    id: TabId,
    current_url: String,
    loading: bool,
}

impl Tab {
    /// Create a tab that is about to load `initial_url`.
    pub fn new(id: TabId, initial_url: impl Into<String>) -> Self {
        Self {
            id,
            current_url: initial_url.into(),
            loading: true,
        }
    }

    /// This tab's stable identifier.
    pub fn id(&self) -> TabId {
        self.id
    }

    /// The URL shown in the address bar.
    pub fn current_url(&self) -> &str {
        &self.current_url
    }

    /// Whether the engine is currently loading a page.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// The engine started navigating to `url` (typed URL, link click, or
    /// back/forward — the engine does not distinguish for us).
    pub fn on_navigation_started(&mut self, url: &str) {
        self.current_url = url.to_owned();
        self.loading = true;
    }

    /// The engine finished loading `url` (the final URL after redirects).
    pub fn on_load_finished(&mut self, url: &str) {
        self.current_url = url.to_owned();
        self.loading = false;
    }

    /// The load ended without reaching a page (network/TLS failure and the
    /// like). The address bar keeps the URL the user tried to reach.
    pub fn on_load_failed(&mut self) {
        self.loading = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tab_is_loading_its_initial_url() {
        let tab = Tab::new(TabId::from(0), "https://example.com/");
        assert_eq!(tab.current_url(), "https://example.com/");
        assert!(tab.is_loading());
        assert_eq!(tab.id(), TabId::from(0));
    }

    #[test]
    fn navigation_updates_url_and_sets_loading() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_load_finished("https://example.com/");
        assert!(!tab.is_loading());

        tab.on_navigation_started("https://rust-lang.org/");
        assert_eq!(tab.current_url(), "https://rust-lang.org/");
        assert!(tab.is_loading());
    }

    #[test]
    fn failed_load_keeps_attempted_url() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_navigation_started("https://unreachable.invalid/");
        tab.on_load_failed();
        assert_eq!(tab.current_url(), "https://unreachable.invalid/");
        assert!(!tab.is_loading());
    }

    #[test]
    fn finished_load_records_final_url_after_redirects() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_navigation_started("http://example.com/old");
        tab.on_load_finished("https://example.com/new");
        assert_eq!(tab.current_url(), "https://example.com/new");
        assert!(!tab.is_loading());
    }

    #[test]
    fn distinct_ids_are_not_equal() {
        assert_ne!(TabId::from(0), TabId::from(1));
        assert_eq!(TabId::from(7).get(), 7);
    }
}
