//! Per-tab browser state and its lifecycle state machine.
//!
//! The web engine owns the real session history (used for back/forward); this
//! struct mirrors just enough state to drive the UI (address bar contents,
//! title, favicon, loading indicator, lifecycle state) and to let future
//! work (tab suspension policy, session restore) build on a stable model.
//! Multiple tabs are tracked by [`super::tabs::Tabs`], which owns a `Vec<Tab>`
//! plus which one is active; a `Tab` itself only knows about its own page.
//!
//! **Ownership boundary** (see docs/decisions.md D20 and
//! docs/architecture.md): a `Tab` never holds a web engine handle of any
//! kind. `browser::` as a whole has no dependency on `wry`/`tao`/`gtk` — the
//! actual content `WebView` for a tab is owned exclusively by
//! `ui::window::BrowserWindow`, keyed by the same [`TabId`]. `Tab::state`
//! below tracks this tab's *logical* lifecycle (is it visible, backgrounded,
//! or has its webview been reclaimed) without ever referencing the webview
//! itself; `ui::window` is responsible for keeping its own `Option<WebView>`
//! in sync with what `Tab::state` says should be true.

use std::time::{Duration, Instant};

/// Opaque, stable identifier for a tab.
///
/// Assigned once by [`super::tabs::Tabs`] when a tab is opened and never
/// reused, so a stale id (e.g. a `close_tab` message racing a second close)
/// simply refers to nothing rather than to the wrong tab. Sent to/from the
/// toolbar webview as a plain JSON number, and used by `ui::window` as the
/// key into its own `WebView` map — the one place a `TabId` and a live
/// webview are associated.
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

/// A tab's lifecycle state.
///
/// Exactly one tab in a given [`super::tabs::Tabs`] is ever `Active` or
/// `Restoring` at a time (the one [`super::tabs::Tabs::active_id`] points
/// at); every other tab is `Background` or `Suspended`. `Tabs` is the sole
/// mutator of this state and is responsible for upholding that invariant —
/// see its module doc comment.
///
/// ```text
///        ┌────────────┐  another tab activated   ┌────────────┐
///        │   Active    │ ────────────────────────►│ Background │
///        │ (visible,   │◄──────────────────────── │ (awake,    │
///        │  webview    │   this tab activated      │  webview   │
///        │  live)      │                           │  live)     │
///        └──────┬──────┘                           └──────┬─────┘
///               │ (only reachable via Restoring)          │ idle timeout /
///               │                                          │ manual suspend
///        ┌──────┴──────┐   webview rebuilt          ┌──────▼─────┐
///        │  Restoring  │◄────────────────────────── │  Suspended │
///        │ (selected,  │   this tab selected         │ (webview   │
///        │  webview    │                             │  dropped)  │
///        │  rebuilding)│                             └────────────┘
///        └─────────────┘
/// ```
///
/// `Restoring` is a real, distinct state — not a synonym for `Active` —
/// because rebuilding a webview is not guaranteed to be instantaneous in
/// general (today it happens to be, since `wry` builds a webview
/// synchronously; see docs/decisions.md D20). It exists now so a future
/// asynchronous session restore (#25) has a state to represent "selected,
/// but not yet actually showing anything" instead of forcing that work to
/// invent one later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabState {
    /// The visible tab; its content webview is live and shown.
    Active,
    /// Not the visible tab, but its content webview is still alive (e.g.
    /// scroll position and in-progress form input survive).
    Background,
    /// This tab's content webview has been dropped to reclaim memory (see
    /// docs/decisions.md D9). Only this `Tab`'s own state survives; the
    /// engine-side scroll position, form input, and session history are
    /// gone.
    Suspended,
    /// This tab has been selected to become active again after being
    /// suspended, and its webview is being rebuilt. Transient today (see
    /// the type doc comment); every current caller passes through it and
    /// lands on `Active` within the same function call.
    Restoring,
}

/// A state transition that is not one of [`TabState`]'s defined edges (see
/// its diagram) was attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTabTransition {
    pub from: TabState,
    pub to: TabState,
}

impl std::fmt::Display for InvalidTabTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid tab state transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for InvalidTabTransition {}

impl TabState {
    /// Whether this state is [`TabState::Suspended`].
    pub fn is_suspended(self) -> bool {
        matches!(self, TabState::Suspended)
    }

    /// `Active -> Background`: this tab stopped being the visible tab.
    /// Valid only from `Active`.
    fn background(self) -> Result<Self, InvalidTabTransition> {
        match self {
            TabState::Active => Ok(TabState::Background),
            _ => Err(InvalidTabTransition {
                from: self,
                to: TabState::Background,
            }),
        }
    }

    /// `Background -> Active` or `Restoring -> Active`: this tab became (or
    /// finished becoming) the visible tab. A `Suspended` tab must pass
    /// through [`Self::begin_restore`] first — there is no direct
    /// `Suspended -> Active` edge, since resuming requires rebuilding a
    /// webview, which `browser::` itself has no part in.
    fn activate(self) -> Result<Self, InvalidTabTransition> {
        match self {
            TabState::Background | TabState::Restoring => Ok(TabState::Active),
            _ => Err(InvalidTabTransition {
                from: self,
                to: TabState::Active,
            }),
        }
    }

    /// `Background -> Suspended`. Valid only from `Background` — the active
    /// tab must always keep a live webview, and an already-suspended tab
    /// has nothing left to drop, so both are rejected by construction
    /// rather than by a separate guard at the call site.
    fn suspend(self) -> Result<Self, InvalidTabTransition> {
        match self {
            TabState::Background => Ok(TabState::Suspended),
            _ => Err(InvalidTabTransition {
                from: self,
                to: TabState::Suspended,
            }),
        }
    }

    /// `Suspended -> Restoring`: this tab was selected while suspended;
    /// resuming (rebuilding its webview) is about to start. Valid only from
    /// `Suspended`.
    fn begin_restore(self) -> Result<Self, InvalidTabTransition> {
        match self {
            TabState::Suspended => Ok(TabState::Restoring),
            _ => Err(InvalidTabTransition {
                from: self,
                to: TabState::Restoring,
            }),
        }
    }
}

/// A tab's favicon.
///
/// Fetching and rendering a favicon is out of scope for this issue (tracked
/// as part of #11); this only gives that work a field and a type to fill in
/// without another state-model change. `Unknown` covers both "never looked
/// up" and "this page has none".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Favicon {
    #[default]
    Unknown,
    /// A URL a future favicon fetcher would load from.
    Url(String),
}

/// State of a single browser tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    id: TabId,
    current_url: String,
    /// The page's title, as last reported by the engine (see
    /// `ui::window::BrowserWindow::fetch_page_title`). `None` until a title
    /// arrives for the current page — cleared on every new navigation so a
    /// stale title from the previous page is never shown as if it were
    /// current. Also part of the state a future session restore (#25) needs
    /// to show a tab strip before every suspended tab has reloaded.
    title: Option<String>,
    /// This tab's favicon; see [`Favicon`]. Cleared on every new navigation
    /// for the same reason as `title`.
    favicon: Favicon,
    loading: bool,
    /// Number of navigations content blocking has refused since this tab
    /// was created.
    blocked_count: u32,
    /// This tab's lifecycle state — see [`TabState`]. `Tabs` is the sole
    /// mutator; every transition here goes through one of `Tab`'s wrapper
    /// methods, which themselves defer to `TabState`'s transition methods,
    /// so an invalid transition is rejected by construction rather than by
    /// a separate runtime check.
    state: TabState,
    /// The last moment this tab stopped being the active tab (or, for a tab
    /// that has never yet been backgrounded, the moment it was created).
    /// Used only to compute how long a *background* tab has sat idle for
    /// the automatic-suspension policy — irrelevant while a tab is active,
    /// since the active tab is never a suspension candidate. Also the
    /// "last active" timestamp a future session restore (#25) would want to
    /// decide which tabs to restore eagerly vs. lazily.
    last_active: Instant,
}

impl Tab {
    /// Create a tab that is about to load `initial_url`.
    ///
    /// Every current caller ([`super::tabs::Tabs::new`],
    /// [`super::tabs::Tabs::open`]) makes the new tab active immediately, so
    /// it starts in [`TabState::Active`] — there is currently no path that
    /// opens a tab directly into the background.
    pub fn new(id: TabId, initial_url: impl Into<String>) -> Self {
        Self {
            id,
            current_url: initial_url.into(),
            title: None,
            favicon: Favicon::default(),
            loading: true,
            blocked_count: 0,
            state: TabState::Active,
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

    /// The page title last reported for the current page, if any has
    /// arrived yet.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Record a page title for the current page (see
    /// `UserEvent::PageTitleResolved`).
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }

    /// This tab's favicon (see [`Favicon`]).
    pub fn favicon(&self) -> &Favicon {
        &self.favicon
    }

    /// Record a favicon URL for the current page.
    pub fn set_favicon_url(&mut self, url: impl Into<String>) {
        self.favicon = Favicon::Url(url.into());
    }

    /// Whether the engine is currently loading a page. Always `false` while
    /// suspended (nothing is loading — there is no webview).
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Number of navigations content blocking has refused since this tab
    /// was created.
    pub fn blocked_count(&self) -> u32 {
        self.blocked_count
    }

    /// This tab's current lifecycle state.
    pub fn state(&self) -> TabState {
        self.state
    }

    /// Whether this tab is suspended (its content webview has been
    /// dropped). Shorthand for `self.state() == TabState::Suspended`, kept
    /// because it is by far the most common state query at call sites.
    pub fn is_suspended(&self) -> bool {
        self.state.is_suspended()
    }

    /// `Active -> Background`. See [`TabState::background`].
    pub(super) fn background(&mut self) -> Result<(), InvalidTabTransition> {
        self.state = self.state.background()?;
        Ok(())
    }

    /// `Background -> Active`. Does not touch `loading` — an awake
    /// background tab keeps whatever loading state it already had. See
    /// [`TabState::activate`].
    pub(super) fn activate(&mut self) -> Result<(), InvalidTabTransition> {
        self.state = self.state.activate()?;
        Ok(())
    }

    /// Suspend this tab (`Background -> Suspended`): it is dormant until
    /// [`Self::resume`]. Only `current_url` (plus the metadata fields, which
    /// were never engine-owned in the first place) is preserved; `loading`
    /// is cleared since no webview is loading anything anymore. See
    /// [`TabState::suspend`] for why the active tab and an already-suspended
    /// tab both reject this.
    pub(super) fn suspend(&mut self) -> Result<(), InvalidTabTransition> {
        self.state = self.state.suspend()?;
        self.loading = false;
        Ok(())
    }

    /// Resume a suspended tab (`Suspended -> Restoring -> Active`): it is
    /// about to reload `current_url` in a freshly rebuilt webview (built by
    /// `ui::window`, not here), so it starts out loading again.
    ///
    /// This collapses both edges of the diagram in [`TabState`]'s doc
    /// comment into one call because today's webview rebuild is synchronous
    /// (docs/decisions.md D20) — every caller observes `Active` by the time
    /// this returns, never `Restoring`. The intermediate `begin_restore`
    /// step is still real (not skipped): only a currently-`Suspended` tab
    /// can be resumed, exactly as [`TabState::begin_restore`] requires.
    pub(super) fn resume(&mut self) -> Result<(), InvalidTabTransition> {
        self.state = self.state.begin_restore()?;
        // `Restoring -> Active` cannot fail (see `TabState::activate`), so
        // this is not a second fallible call — assigning directly avoids an
        // `unwrap`/`expect` for a step that is guaranteed by the type.
        self.state = TabState::Active;
        self.loading = true;
        Ok(())
    }

    /// Record `now` as the moment this tab stopped being the active tab.
    /// Called by [`super::tabs::Tabs`] on the outgoing tab whenever a
    /// different tab becomes active. Purely a clock stamp for the idle-based
    /// auto-suspend policy — independent of the `state` transition above,
    /// which some callers (tests that don't care about the idle clock)
    /// intentionally skip.
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
    /// back/forward — the engine does not distinguish for us). Clears the
    /// previous page's title/favicon, since neither describes the page now
    /// loading.
    pub fn on_navigation_started(&mut self, url: &str) {
        self.current_url = url.to_owned();
        self.loading = true;
        self.title = None;
        self.favicon = Favicon::Unknown;
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

    /// Content blocking refused a main-frame navigation to `url`; the
    /// address bar and loading state are left untouched since the current
    /// page never actually navigated away.
    pub fn on_navigation_blocked(&mut self, _url: &str) {
        self.blocked_count += 1;
    }

    /// Content blocking refused a subresource request (image/script/
    /// XHR/fetch/...) in this tab (Issue #22, Windows/WebView2 only — see
    /// docs/decisions.md D59). Shares `blocked_count` with
    /// [`Self::on_navigation_blocked`]: both are "content blocking stopped a
    /// request in this tab" from the toolbar badge's point of view, and the
    /// badge does not distinguish which layer did the blocking.
    pub fn on_subresource_blocked(&mut self, _url: &str) {
        self.blocked_count += 1;
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
    fn new_tab_has_no_blocked_navigations() {
        let tab = Tab::new(TabId::from(0), "https://example.com/");
        assert_eq!(tab.blocked_count(), 0);
    }

    #[test]
    fn blocked_navigation_increments_the_counter_without_changing_the_page() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_load_finished("https://example.com/");

        tab.on_navigation_blocked("https://doubleclick.net/");
        assert_eq!(tab.blocked_count(), 1);
        assert_eq!(tab.current_url(), "https://example.com/");
        assert!(!tab.is_loading());

        tab.on_navigation_blocked("https://googlesyndication.com/");
        assert_eq!(tab.blocked_count(), 2);
    }

    #[test]
    fn blocked_subresource_shares_the_counter_with_blocked_navigations() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_load_finished("https://example.com/");

        tab.on_subresource_blocked("https://ads.example/banner.js");
        assert_eq!(tab.blocked_count(), 1);
        assert_eq!(tab.current_url(), "https://example.com/");

        tab.on_navigation_blocked("https://doubleclick.net/");
        assert_eq!(tab.blocked_count(), 2);
    }

    #[test]
    fn new_tab_is_active_and_not_suspended() {
        let tab = Tab::new(TabId::from(0), "https://example.com/");
        assert_eq!(tab.state(), TabState::Active);
        assert!(!tab.is_suspended());
    }

    #[test]
    fn new_tab_has_no_title_or_favicon_yet() {
        let tab = Tab::new(TabId::from(0), "https://example.com/");
        assert_eq!(tab.title(), None);
        assert_eq!(tab.favicon(), &Favicon::Unknown);
    }

    #[test]
    fn title_and_favicon_can_be_set_and_read_back() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.set_title("Example Domain");
        tab.set_favicon_url("https://example.com/favicon.ico");

        assert_eq!(tab.title(), Some("Example Domain"));
        assert_eq!(
            tab.favicon(),
            &Favicon::Url("https://example.com/favicon.ico".to_owned())
        );
    }

    #[test]
    fn navigation_clears_the_previous_pages_title_and_favicon() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.set_title("Example Domain");
        tab.set_favicon_url("https://example.com/favicon.ico");

        tab.on_navigation_started("https://rust-lang.org/");

        assert_eq!(tab.title(), None);
        assert_eq!(tab.favicon(), &Favicon::Unknown);
    }

    #[test]
    fn suspend_clears_loading_and_sets_suspended_state() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_load_finished("https://example.com/"); // stop loading first
        tab.on_navigation_started("https://example.com/still-loading");
        // A freshly created tab is Active; it must background first.
        tab.background().unwrap();

        tab.suspend().unwrap();

        assert_eq!(tab.state(), TabState::Suspended);
        assert!(tab.is_suspended());
        assert!(!tab.is_loading());
        // The URL to reload on resume is preserved.
        assert_eq!(tab.current_url(), "https://example.com/still-loading");
    }

    #[test]
    fn resume_clears_suspended_state_and_starts_loading() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.on_load_finished("https://example.com/");
        tab.background().unwrap();
        tab.suspend().unwrap();

        tab.resume().unwrap();

        assert_eq!(tab.state(), TabState::Active);
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

    // --- TabState transition table: every (state, transition) pair. ---
    // Five valid edges (Active->Background, Background->Active,
    // Restoring->Active, Background->Suspended, Suspended->Restoring);
    // everything else — including every self-transition — must be rejected.

    #[test]
    fn background_only_succeeds_from_active() {
        assert_eq!(TabState::Active.background(), Ok(TabState::Background));
        for from in [
            TabState::Background,
            TabState::Suspended,
            TabState::Restoring,
        ] {
            assert_eq!(
                from.background(),
                Err(InvalidTabTransition {
                    from,
                    to: TabState::Background
                })
            );
        }
    }

    #[test]
    fn activate_succeeds_from_background_and_restoring_only() {
        assert_eq!(TabState::Background.activate(), Ok(TabState::Active));
        assert_eq!(TabState::Restoring.activate(), Ok(TabState::Active));
        for from in [TabState::Active, TabState::Suspended] {
            assert_eq!(
                from.activate(),
                Err(InvalidTabTransition {
                    from,
                    to: TabState::Active
                })
            );
        }
    }

    #[test]
    fn suspend_only_succeeds_from_background() {
        assert_eq!(TabState::Background.suspend(), Ok(TabState::Suspended));
        for from in [TabState::Active, TabState::Suspended, TabState::Restoring] {
            assert_eq!(
                from.suspend(),
                Err(InvalidTabTransition {
                    from,
                    to: TabState::Suspended
                })
            );
        }
    }

    #[test]
    fn begin_restore_only_succeeds_from_suspended() {
        assert_eq!(TabState::Suspended.begin_restore(), Ok(TabState::Restoring));
        for from in [TabState::Active, TabState::Background, TabState::Restoring] {
            assert_eq!(
                from.begin_restore(),
                Err(InvalidTabTransition {
                    from,
                    to: TabState::Restoring
                })
            );
        }
    }

    #[test]
    fn invalid_transition_display_names_both_states() {
        let err = InvalidTabTransition {
            from: TabState::Active,
            to: TabState::Suspended,
        };
        assert_eq!(
            err.to_string(),
            "invalid tab state transition: Active -> Suspended"
        );
    }

    #[test]
    fn tab_rejects_suspending_the_active_state() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        assert_eq!(tab.state(), TabState::Active);

        let err = tab.suspend().unwrap_err();
        assert_eq!(err.from, TabState::Active);
        // Rejected: state (and loading) are unchanged.
        assert_eq!(tab.state(), TabState::Active);
    }

    #[test]
    fn tab_rejects_suspending_an_already_suspended_tab() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        tab.background().unwrap();
        tab.suspend().unwrap();

        let err = tab.suspend().unwrap_err();
        assert_eq!(err.from, TabState::Suspended);
    }

    #[test]
    fn tab_rejects_resuming_a_tab_that_is_not_suspended() {
        let mut tab = Tab::new(TabId::from(0), "https://example.com/");
        let err = tab.resume().unwrap_err();
        assert_eq!(err.from, TabState::Active);
        assert_eq!(err.to, TabState::Restoring);
    }
}
