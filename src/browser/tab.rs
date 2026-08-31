//! Per-tab browser state.
//!
//! The web engine owns the real session history (used for back/forward); this
//! struct mirrors just enough state to drive the UI (address bar contents,
//! loading indicator). VeloX has a single tab today, but the app already
//! talks to a `Tab` value so a tab strip can be added without reshaping the
//! core.

/// State of a single browser tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    current_url: String,
    loading: bool,
}

impl Tab {
    /// Create a tab that is about to load `initial_url`.
    pub fn new(initial_url: impl Into<String>) -> Self {
        Self {
            current_url: initial_url.into(),
            loading: true,
        }
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
        let tab = Tab::new("https://example.com/");
        assert_eq!(tab.current_url(), "https://example.com/");
        assert!(tab.is_loading());
    }

    #[test]
    fn navigation_updates_url_and_sets_loading() {
        let mut tab = Tab::new("https://example.com/");
        tab.on_load_finished("https://example.com/");
        assert!(!tab.is_loading());

        tab.on_navigation_started("https://rust-lang.org/");
        assert_eq!(tab.current_url(), "https://rust-lang.org/");
        assert!(tab.is_loading());
    }

    #[test]
    fn failed_load_keeps_attempted_url() {
        let mut tab = Tab::new("https://example.com/");
        tab.on_navigation_started("https://unreachable.invalid/");
        tab.on_load_failed();
        assert_eq!(tab.current_url(), "https://unreachable.invalid/");
        assert!(!tab.is_loading());
    }

    #[test]
    fn finished_load_records_final_url_after_redirects() {
        let mut tab = Tab::new("https://example.com/");
        tab.on_navigation_started("http://example.com/old");
        tab.on_load_finished("https://example.com/new");
        assert_eq!(tab.current_url(), "https://example.com/new");
        assert!(!tab.is_loading());
    }
}
