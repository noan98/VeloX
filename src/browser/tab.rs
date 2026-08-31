//! Per-tab browser state.
//!
//! The web engine owns the real session history (used for back/forward); this
//! struct mirrors just enough state to drive the UI (address bar contents,
//! loading indicator, suspended flag). Multiple tabs are tracked by
//! [`super::tabs::Tabs`], which owns a `Vec<Tab>` plus which one is active; a
//! `Tab` itself only knows about its own page.

use std::time::{Duration, Instant};

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
    /// Whether this tab is suspended: its content webview has been dropped
    /// to reclaim memory (see `ui::window::BrowserWindow::suspend_tab`) and
    /// only this `Tab`'s state — the URL — survives. Scroll position,
    /// in-progress form input and session history (back/forward) are lost;
    /// reactivating the tab rebuilds the webview and reloads `current_url`
    /// from scratch. See docs/decisions.md D9.
    suspended: bool,
    /// The last moment this tab stopped being the active tab (or, for a tab
    /// that has never yet been backgrounded, the moment it was created).
    /// Used only to compute how long a *background* tab has sat idle for
    /// the automatic-suspension policy — irrelevant while a tab is active,
    /// since the active tab is never a suspension candidate.
    last_active: Instant,
}

impl Tab {
    /// Create a tab that is about to load `initial_url`.
    pub fn new(id: TabId, initial_url: impl Into<String>) -> Self {
        Self {
            id,
            current_url: initial_url.into(),
            loading: true,
            suspended: false,
            last_active: Instant::now(),
        }
    }

    /// This tab's stable identifier.
    pub fn id(&self) -> TabId {
        self.id
    }

    /// The URL shown in the address bar. While suspended this is the URL
    /// that will be reloaded on reactivation, not a live page.
    pub fn current_url(&self) -> &str {
        &self.current_url
    }

    /// Whether the engine is currently loading a page. Always `false` while
    /// suspended (nothing is loading — there is no webview).
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Whether this tab is suspended (see the `suspended` field docs).
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// Suspend this tab: it is dormant until [`Self::resume`]. Only the
    /// `current_url` is preserved; the loading flag is cleared since no
    /// webview is loading anything anymore.
    pub fn suspend(&mut self) {
        self.suspended = true;
        self.loading = false;
    }

    /// Resume a suspended tab: it is about to reload `current_url` in a
    /// freshly rebuilt webview, so it starts out loading again.
    pub fn resume(&mut self) {
        self.suspended = false;
        self.loading = true;
    }

    /// Record `now` as the moment this tab stopped being the active tab.
    /// Called by [`super::tabs::Tabs`] on the outgoing tab whenever a
    /// different tab becomes active.
    pub(super) fn mark_backgrounded(&mut self, now: Instant) {
        self.last_active = now;
    }

    /// How long this tab has sat in the background as of `now`. Meaningless
    /// (and unused) for the currently active tab.
    pub fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_active)
    }

    /// The raw instant recorded by [`Self::mark_backgrounded`] (or tab
    /// creation, if it has never yet been backgrounded). Exposed so
    /// [`super::tabs::Tabs`] can compute *when* a background tab will next
    /// become eligible for auto-suspension, not just how idle it is now.
    pub fn last_active_at(&self) -> Instant {
        self.last_active
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
    fn new_tab_is_not_suspended() {
        let tab = Tab::new(TabId::from(0), "https://example.com/");
        assert!(!tab.is_suspended());
    }

    #[test]
    fn suspend_clears_loading_and_sets_suspended() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_load_finished("https://example.com/"); // stop loading first
        tab.on_navigation_started("https://example.com/still-loading");

        tab.suspend();

        assert!(tab.is_suspended());
        assert!(!tab.is_loading());
        // The URL to reload on resume is preserved.
        assert_eq!(tab.current_url(), "https://example.com/still-loading");
    }

    #[test]
    fn resume_clears_suspended_and_starts_loading() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_load_finished("https://example.com/");
        tab.suspend();

        tab.resume();

        assert!(!tab.is_suspended());
        assert!(tab.is_loading());
        assert_eq!(tab.current_url(), "https://example.com/");
    }

    #[test]
    fn idle_for_measures_time_since_last_backgrounded() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        let t0 = Instant::now();
        tab.mark_backgrounded(t0);

        assert_eq!(tab.idle_for(t0), Duration::ZERO);
        assert_eq!(
            tab.idle_for(t0 + Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn idle_for_never_goes_negative() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        let t0 = Instant::now();
        tab.mark_backgrounded(t0 + Duration::from_secs(10));

        // `now` earlier than `last_active` (should not happen with a
        // monotonic clock, but saturating avoids a panic if it ever does).
        assert_eq!(tab.idle_for(t0), Duration::ZERO);
    }

    #[test]
    fn distinct_ids_are_not_equal() {
        assert_ne!(TabId::from(0), TabId::from(1));
        assert_eq!(TabId::from(7).get(), 7);
    }
}
