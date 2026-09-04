//! A collection of tabs plus which one is active.
//!
//! [`Tab`] only knows about a single page; `Tabs` is what turns that into
//! "multiple tabs, one of them active" — the piece the app needs to drive a
//! tab strip. This is plain UI/engine-independent Rust, like the rest of
//! `browser::`, so open/close/activate logic is unit-tested without a
//! window or a webview.
//!
//! **Invariant**: exactly one tab — the one at `self.active` — is ever in
//! [`TabState::Active`] or [`TabState::Restoring`]; every other tab is
//! [`TabState::Background`] or [`TabState::Suspended`]. `Tabs` is the only
//! type that mutates a `Tab`'s state (its transition methods are
//! `pub(super)`), and every method below that changes which tab is active
//! upholds this invariant by construction — see [`Self::resolve_activation`].

use std::time::{Duration, Instant};

use super::suspension::Candidate;
use super::tab::{Tab, TabId, TabState};

/// What the caller must do on the *webview* side after a [`Tabs`] operation
/// changes which tab is active.
///
/// `browser::tabs` decides *that* a tab became active and *how* (straight
/// from `Background`, or resumed from `Suspended`); it has no way to act on
/// a webview itself (see the module doc comment's ownership invariant), so
/// it reports which of the two happened and leaves the actual webview call
/// (`ui::window::BrowserWindow::activate_tab` / `resume_tab`) to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationEffect {
    /// The newly active tab already had a live webview; the caller only
    /// needs to change which one is visible.
    Switch,
    /// The newly active tab was suspended; the caller must rebuild its
    /// webview.
    Resume,
}

/// A bounded LIFO stack of recently closed tabs' URLs, for
/// Ctrl/Cmd+Shift+T ("reopen closed tab").
///
/// Deliberately just a list of URLs: a closed tab's webview (scroll
/// position, in-progress form input, JS-side session history) is already
/// gone by the time it lands here (see [`Tabs::close`]), so reopening always
/// starts a fresh tab at that URL rather than restoring anything richer.
/// Pure and UI-independent, like the rest of this module, so the cap and
/// LIFO ordering are unit-tested without a `Tabs`.
#[derive(Debug, Default)]
struct ClosedTabs {
    /// Oldest entry first, most recently closed last (so `pop` — via
    /// `Vec::pop` — returns the most recently closed tab, matching a LIFO
    /// stack). Trimmed from the front once [`Self::CAP`] is exceeded.
    urls: Vec<String>,
}

impl ClosedTabs {
    /// Maximum number of closed tabs remembered at once. An arbitrary but
    /// generous bound — comfortably more than a user would reopen in one
    /// sitting — that keeps this from growing without limit across a long
    /// session of opening and closing many tabs.
    const CAP: usize = 20;

    /// Record a just-closed tab's URL. If already at capacity, the oldest
    /// remembered entry is dropped to make room.
    fn push(&mut self, url: impl Into<String>) {
        if self.urls.len() >= Self::CAP {
            self.urls.remove(0);
        }
        self.urls.push(url.into());
    }

    /// Take the most recently closed URL, if any, removing it from the
    /// stack (reopening a tab does not leave it available to reopen again).
    fn pop(&mut self) -> Option<String> {
        self.urls.pop()
    }
}

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
    /// Recently closed tabs' URLs, for [`Self::reopen_closed`]. See
    /// [`ClosedTabs`].
    closed: ClosedTabs,
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
            closed: ClosedTabs::default(),
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
    ///
    /// Returns `None` for an unknown or already-closed id rather than
    /// panicking, so a stale event addressed to a `TabId` that no longer
    /// exists is safely ignored by every caller that goes through this
    /// (see the `stale_tab_id` tests below).
    pub fn get(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id() == id)
    }

    /// Look up a tab by id, mutably. Same "unknown id is `None`, never a
    /// panic" contract as [`Self::get`].
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
        // The outgoing active tab is always `Active` by the module
        // invariant, so this cannot fail; a failure is silently ignored
        // (leaving the tab's state as-is) rather than panicking, since a
        // stale state here is a `browser::tabs` bug to catch in tests, not
        // something that should ever crash the browser.
        let _ = self.active_mut().background();
        // `Tab::new` starts a tab in `TabState::Active`, matching it
        // becoming the new active tab below.
        self.tabs.push(Tab::new(id, url));
        self.active = self.tabs.len() - 1;
        id
    }

    /// Like [`Self::open`], but first records `now` as the moment the
    /// previously active tab went to the background — see
    /// [`Self::idle_background_tabs`]. Prefer this over `open` wherever a
    /// real clock is available (i.e. everywhere but tests that don't care
    /// about the auto-suspend idle clock).
    pub fn open_at(&mut self, url: impl Into<String>, now: Instant) -> TabId {
        self.active_mut().mark_backgrounded(now);
        self.open(url)
    }

    /// Close the tab `id`.
    ///
    /// Returns `None`, leaving the collection untouched, when `id` is
    /// unknown or it is the only remaining tab. Otherwise returns the id of
    /// the tab that is active afterwards (unchanged, with
    /// [`ActivationEffect::Switch`], if a background tab was closed) plus
    /// what the caller must do on the webview side.
    pub fn close(&mut self, id: TabId) -> Option<(TabId, ActivationEffect)> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let index = self.index_of(id)?;
        let closing_active = index == self.active;
        // Every successful close — from the tab strip's own "x", or from a
        // keyboard shortcut (`CloseActiveTab`/`ContentShortcut::CloseTab`) —
        // goes through this one method, so this is the single choke point
        // that feeds `reopen_closed` (Ctrl/Cmd+Shift+T).
        self.closed.push(self.tabs[index].current_url().to_owned());
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
        let effect = if closing_active {
            // A different tab is taking over as active; it may have been
            // suspended (a background tab can be suspended while the tab in
            // front of it is closed), so run it through the same
            // state-resolution `activate` does.
            self.resolve_activation(self.active)
        } else {
            // The active tab's identity didn't change — only its index may
            // have shifted — so there is nothing to resolve.
            ActivationEffect::Switch
        };
        Some((self.active_id(), effect))
    }

    /// Make `id` the active tab, resuming it first if it was suspended.
    /// Returns `None`, leaving the active tab unchanged, when `id` is
    /// unknown.
    pub fn activate(&mut self, id: TabId) -> Option<ActivationEffect> {
        let index = self.index_of(id)?;
        // See `open`: the outgoing tab is always `Active` by the module
        // invariant. If `id` is already the active tab this is a harmless
        // Active -> Background -> Active round trip.
        let _ = self.active_mut().background();
        self.active = index;
        Some(self.resolve_activation(index))
    }

    /// Like [`Self::activate`], but first records `now` as the moment the
    /// previously active tab went to the background — see
    /// [`Self::idle_background_tabs`]. Returns `None` (like `activate`)
    /// when `id` is unknown, in which case nothing is marked either.
    pub fn activate_at(&mut self, id: TabId, now: Instant) -> Option<ActivationEffect> {
        self.index_of(id)?;
        self.active_mut().mark_backgrounded(now);
        self.activate(id)
    }

    /// Activate the tab at display-order index `index` (0-based), marking
    /// the outgoing tab backgrounded at `now` — the shared implementation
    /// behind [`Self::activate_relative`], [`Self::activate_by_position`]
    /// and [`Self::activate_last`]. `None` if `index` is out of range.
    fn activate_index_at(&mut self, index: usize, now: Instant) -> Option<ActivationEffect> {
        let id = self.tabs.get(index)?.id();
        self.activate_at(id, now)
    }

    /// Activate the tab `delta` positions away from the currently active tab
    /// in display order, wrapping around at either end (e.g. `delta = 1` is
    /// "next tab" — Ctrl/Cmd+Tab; `delta = -1` is "previous tab" —
    /// Ctrl/Cmd+Shift+Tab). `Tabs` is never empty, so this always succeeds.
    pub fn activate_relative(&mut self, delta: i64, now: Instant) -> Option<ActivationEffect> {
        let len = self.tabs.len() as i64;
        // Double-mod (`% len` then `+ len) % len`) to land in `0..len` even
        // when `self.active as i64 + delta` is negative, since Rust's `%`
        // keeps the sign of its left operand rather than always returning a
        // non-negative result.
        let index = ((self.active as i64 + delta) % len + len) % len;
        self.activate_index_at(index as usize, now)
    }

    /// Activate the tab at 1-based display-order position `position` (the
    /// Ctrl/Cmd+1..8 shortcuts). `None` if there is no tab at that position
    /// (fewer tabs open than `position`, or `position == 0`).
    pub fn activate_by_position(
        &mut self,
        position: usize,
        now: Instant,
    ) -> Option<ActivationEffect> {
        let index = position.checked_sub(1)?;
        self.activate_index_at(index, now)
    }

    /// Activate the last tab in display order (the Ctrl/Cmd+9 shortcut,
    /// mirroring most browsers' "always the last tab, however many there
    /// are" behavior for that key rather than a literal "9th tab").
    pub fn activate_last(&mut self, now: Instant) -> Option<ActivationEffect> {
        self.activate_index_at(self.tabs.len() - 1, now)
    }

    /// Reopen the most recently closed tab (Ctrl/Cmd+Shift+T), making it the
    /// active tab — exactly like [`Self::open_at`], since a reopened tab is
    /// a brand new tab/webview at a remembered URL, not a restoration of the
    /// old one's session state (see [`ClosedTabs`]). `None`, leaving `Tabs`
    /// untouched, when nothing has been closed yet (or every closed tab has
    /// already been reopened).
    pub fn reopen_closed(&mut self, now: Instant) -> Option<TabId> {
        let url = self.closed.pop()?;
        Some(self.open_at(url, now))
    }

    /// Bring the tab at `index` — already installed as [`Self::active`] by
    /// the caller — into a state consistent with being the active tab, and
    /// report which effect the webview-owning caller needs to apply.
    ///
    /// A suspended tab is resumed (`Suspended -> Restoring -> Active`, via
    /// [`Tab::resume`]); anything else (only ever `Background` in practice,
    /// by the module invariant) is activated directly
    /// (`Background -> Active`). Both `Tab` calls are infallible in
    /// practice here — `Tabs` only ever calls this for a tab it just
    /// confirmed is not the outgoing active tab — but any failure is
    /// ignored rather than panicking, consistent with the rest of this
    /// type's "never crash on an inconsistent id/state" contract.
    fn resolve_activation(&mut self, index: usize) -> ActivationEffect {
        let tab = &mut self.tabs[index];
        if tab.is_suspended() {
            let _ = tab.resume();
            ActivationEffect::Resume
        } else {
            let _ = tab.activate();
            ActivationEffect::Switch
        }
    }

    /// Suspend tab `id`: mark it dormant so its content webview can be
    /// dropped (see `ui::window::BrowserWindow::suspend_tab`). Refuses —
    /// returning `false`, leaving every tab unchanged — for the active tab
    /// (the visible tab always needs a live webview), an already-suspended
    /// tab, or an unknown id. The first two are enforced by
    /// [`TabState::suspend`]'s transition rules, not by a separate check
    /// here.
    pub fn suspend(&mut self, id: TabId) -> bool {
        match self.get_mut(id) {
            Some(tab) => tab.suspend().is_ok(),
            None => false,
        }
    }

    /// Background (non-active) tabs that are not yet suspended and have
    /// been idle for at least `idle_after` as of `now`. The active tab is
    /// never a candidate: suspending the tab the user is looking at would
    /// be visibly disruptive, not a background memory optimization. This
    /// falls directly out of the module invariant — only a `Background`
    /// tab can ever be a candidate, so there is no separate "is this the
    /// active tab" check needed here.
    ///
    /// Pure and clock-injected on purpose, so the auto-suspend policy is
    /// unit-testable without sleeping a real thread — see the tests below.
    pub fn idle_background_tabs(&self, now: Instant, idle_after: Duration) -> Vec<TabId> {
        self.tabs
            .iter()
            .filter(|tab| tab.state() == TabState::Background && tab.idle_for(now) >= idle_after)
            .map(Tab::id)
            .collect()
    }

    /// How many tabs currently have a live content webview: every tab that
    /// is not `Suspended` (the active tab, plus every `Background` tab).
    /// The number `browser::suspension::SuspensionPolicy::max_live_tabs`
    /// is compared against.
    pub fn live_tab_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|tab| tab.state() != TabState::Suspended)
            .count()
    }

    /// Every live (not suspended) tab as a suspension [`Candidate`] for
    /// [`super::suspension::plan`] (Issue #63): whether it is the active
    /// tab, its idle time as of `now`, whether it is still loading, and
    /// what the caller knows from the engine side — `protect(id)` (e.g. it
    /// is playing audio) and `process_group(id)` (which web process its
    /// webview is in, D54) — neither of which `Tabs` can know itself.
    /// Suspended tabs are never candidates.
    ///
    /// Pure and clock-injected like [`Self::idle_background_tabs`]; the
    /// returned order is display order (the policy orders by itself).
    pub fn suspension_candidates(
        &self,
        now: Instant,
        protect: impl Fn(TabId) -> bool,
        process_group: impl Fn(TabId) -> Option<u64>,
    ) -> Vec<Candidate> {
        self.tabs
            .iter()
            .filter(|tab| tab.state() != TabState::Suspended)
            .map(|tab| Candidate {
                id: tab.id(),
                active: tab.state() != TabState::Background,
                idle: tab.idle_for(now),
                loading: tab.is_loading(),
                protected: protect(tab.id()),
                process_group: process_group(tab.id()),
            })
            .collect()
    }

    /// The earliest instant at which a currently-awake background tab will
    /// next become eligible for automatic suspension, for scheduling the
    /// next idle check (e.g. `tao::event_loop::ControlFlow::WaitUntil`).
    /// `None` when there is no such tab (e.g. a single-tab window, or every
    /// background tab is already suspended) — nothing to wait for.
    pub fn next_idle_deadline(&self, idle_after: Duration) -> Option<Instant> {
        self.tabs
            .iter()
            .filter(|tab| tab.state() == TabState::Background)
            .map(|tab| tab.last_active_at() + idle_after)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tab::Favicon;
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
        assert_eq!(tabs.active().state(), TabState::Active);
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
        // The tab left behind is Background, the new one is Active.
        assert_eq!(tabs.get(first).unwrap().state(), TabState::Background);
        assert_eq!(tabs.get(second).unwrap().state(), TabState::Active);
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
        // it but still points at C, and nothing needed on the webview side
        // beyond what's already showing.
        let (new_active, effect) = tabs.close(a).unwrap();
        assert_eq!(new_active, c);
        assert_eq!(effect, ActivationEffect::Switch);
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
        let (new_active, effect) = tabs.close(b).unwrap();

        assert_eq!(new_active, c);
        assert_eq!(effect, ActivationEffect::Switch);
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

        let (new_active, effect) = tabs.close(c).unwrap();

        assert_eq!(new_active, b);
        assert_eq!(effect, ActivationEffect::Switch);
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

        let (new_active, _effect) = tabs.close(a).unwrap();

        assert_eq!(new_active, b);
        assert_eq!(tabs.active_id(), b);
    }

    #[test]
    fn closing_the_active_tab_resumes_a_suspended_replacement() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/"); // active
        tabs.activate(a); // b now background
        assert!(tabs.suspend(b));
        let c = tabs.open("https://c.example/"); // active; order: a, b, c
        assert_eq!(ids(&tabs), vec![a, b, c]);

        // Close the active tab (c); the tab sliding into its place (b) is
        // suspended, so the caller must resume it.
        let (new_active, effect) = tabs.close(c).unwrap();

        assert_eq!(new_active, b);
        assert_eq!(effect, ActivationEffect::Resume);
        assert_eq!(tabs.get(b).unwrap().state(), TabState::Active);
        assert!(!tabs.get(b).unwrap().is_suspended());
    }

    #[test]
    fn activate_switches_active_tab_and_rejects_unknown_ids() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        assert_eq!(tabs.active_id(), b);

        assert_eq!(tabs.activate(a), Some(ActivationEffect::Switch));
        assert_eq!(tabs.active_id(), a);

        assert_eq!(tabs.activate(TabId::from(999)), None);
        assert_eq!(tabs.active_id(), a);
    }

    #[test]
    fn activating_the_already_active_tab_is_a_harmless_no_op() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();

        assert_eq!(tabs.activate(a), Some(ActivationEffect::Switch));
        assert_eq!(tabs.active_id(), a);
        assert_eq!(tabs.active().state(), TabState::Active);
    }

    #[test]
    fn activating_a_suspended_tab_resumes_it() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/"); // active
        tabs.activate(a); // b now background
        assert!(tabs.suspend(b));
        assert!(tabs.get(b).unwrap().is_suspended());

        let effect = tabs.activate(b);

        assert_eq!(effect, Some(ActivationEffect::Resume));
        assert_eq!(tabs.active_id(), b);
        assert_eq!(tabs.get(b).unwrap().state(), TabState::Active);
        assert!(tabs.get(b).unwrap().is_loading());
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

    #[test]
    fn stale_tab_id_lookups_return_none_instead_of_panicking() {
        // A `TabId` that never existed and one for a tab that has since
        // been closed must both behave the same way: `None`, no panic —
        // the shape a stale toolbar/webview event's `TabId` takes.
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        tabs.activate(a);
        tabs.close(b).unwrap();
        let closed = b;
        let never_existed = TabId::from(9999);

        for stale in [closed, never_existed] {
            assert!(tabs.get(stale).is_none());
            assert!(tabs.get_mut(stale).is_none());
            assert_eq!(tabs.activate(stale), None);
            assert_eq!(tabs.activate_at(stale, Instant::now()), None);
            assert!(!tabs.suspend(stale));
            assert_eq!(tabs.close(stale), None);
        }
        // The collection itself is unaffected by any of the above.
        assert_eq!(ids(&tabs), vec![a]);
    }

    #[test]
    fn suspend_refuses_the_active_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let active = tabs.active_id();

        assert!(!tabs.suspend(active));
        assert!(!tabs.get(active).unwrap().is_suspended());
    }

    #[test]
    fn suspend_refuses_an_unknown_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        assert!(!tabs.suspend(TabId::from(999)));
    }

    #[test]
    fn suspend_marks_a_background_tab_dormant() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/"); // active
        tabs.activate(a); // b is now the background tab

        assert!(tabs.suspend(b));
        assert!(tabs.get(b).unwrap().is_suspended());
        // A second suspend is a no-op (already suspended).
        assert!(!tabs.suspend(b));
    }

    #[test]
    fn activate_at_marks_the_outgoing_tab_backgrounded() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/"); // active

        let t0 = Instant::now();
        assert_eq!(tabs.activate_at(a, t0), Some(ActivationEffect::Switch));

        // b just went to the background at t0, so it is not yet idle...
        assert!(tabs
            .idle_background_tabs(t0, Duration::from_secs(1))
            .is_empty());
        // ...but it is after enough time passes.
        assert_eq!(
            tabs.idle_background_tabs(t0 + Duration::from_secs(1), Duration::from_secs(1)),
            vec![b]
        );
    }

    #[test]
    fn activate_at_is_a_no_op_for_an_unknown_id() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();

        assert_eq!(tabs.activate_at(TabId::from(999), Instant::now()), None);
        assert_eq!(tabs.active_id(), a);
    }

    #[test]
    fn open_at_marks_the_previously_active_tab_backgrounded() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();

        let t0 = Instant::now();
        let b = tabs.open_at("https://b.example/", t0);

        assert_eq!(tabs.active_id(), b);
        assert_eq!(
            tabs.idle_background_tabs(t0 + Duration::from_secs(5), Duration::from_secs(5)),
            vec![a]
        );
    }

    #[test]
    fn idle_background_tabs_excludes_the_active_and_already_suspended_tabs() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/"); // active

        let t0 = Instant::now();
        tabs.activate_at(a, t0); // backgrounds c
        tabs.activate_at(b, t0); // backgrounds a; b is now active

        let long_after = t0 + Duration::from_secs(3600);
        // a and c are both idle background tabs...
        let mut idle = tabs.idle_background_tabs(long_after, Duration::from_secs(1));
        idle.sort();
        let mut expected = vec![a, c];
        expected.sort();
        assert_eq!(idle, expected);

        // ...but not once c is suspended, and never b (it's active).
        assert!(tabs.suspend(c));
        assert_eq!(
            tabs.idle_background_tabs(long_after, Duration::from_secs(1)),
            vec![a]
        );
        assert!(!tabs
            .idle_background_tabs(long_after, Duration::from_secs(1))
            .contains(&b));
    }

    #[test]
    fn idle_background_tabs_respects_the_threshold() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let t0 = Instant::now();
        tabs.activate_at(a, t0); // backgrounds b at t0

        let idle_after = Duration::from_secs(60);
        assert!(tabs
            .idle_background_tabs(t0 + Duration::from_secs(30), idle_after)
            .is_empty());
        assert_eq!(
            tabs.idle_background_tabs(t0 + Duration::from_secs(60), idle_after),
            vec![b]
        );
    }

    #[test]
    fn next_idle_deadline_is_none_with_no_background_tabs() {
        let tabs = Tabs::new("https://a.example/");
        assert_eq!(tabs.next_idle_deadline(Duration::from_secs(60)), None);
    }

    #[test]
    fn next_idle_deadline_tracks_the_soonest_background_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let t0 = Instant::now();
        tabs.activate_at(a, t0); // backgrounds b at t0

        let idle_after = Duration::from_secs(60);
        assert_eq!(
            tabs.next_idle_deadline(idle_after),
            Some(t0 + Duration::from_secs(60))
        );

        // Once suspended, b no longer contributes a deadline.
        assert!(tabs.suspend(b));
        assert_eq!(tabs.next_idle_deadline(idle_after), None);
    }

    // --- Closed-tab stack (Ctrl/Cmd+Shift+T) ---

    #[test]
    fn closed_tabs_stack_pushes_and_pops_lifo() {
        let mut closed = ClosedTabs::default();
        assert_eq!(closed.pop(), None);

        closed.push("https://a.example/");
        closed.push("https://b.example/");
        closed.push("https://c.example/");

        // Most recently pushed comes back first.
        assert_eq!(closed.pop().as_deref(), Some("https://c.example/"));
        assert_eq!(closed.pop().as_deref(), Some("https://b.example/"));
        assert_eq!(closed.pop().as_deref(), Some("https://a.example/"));
        assert_eq!(closed.pop(), None);
    }

    #[test]
    fn closed_tabs_stack_drops_the_oldest_entry_once_over_capacity() {
        let mut closed = ClosedTabs::default();
        for i in 0..ClosedTabs::CAP + 3 {
            closed.push(format!("https://{i}.example/"));
        }
        // The three oldest (0, 1, 2) were evicted; the most recent CAP
        // entries survive, most-recently-closed first.
        for i in (3..ClosedTabs::CAP + 3).rev() {
            assert_eq!(closed.pop(), Some(format!("https://{i}.example/")));
        }
        assert_eq!(closed.pop(), None);
    }

    #[test]
    fn closing_a_tab_makes_it_reopenable() {
        let mut tabs = Tabs::new("https://a.example/");
        let b = tabs.open("https://b.example/");
        tabs.close(b).unwrap();

        let reopened = tabs.reopen_closed(Instant::now()).unwrap();

        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs.active_id(), reopened);
        assert_eq!(
            tabs.get(reopened).unwrap().current_url(),
            "https://b.example/"
        );
        // Not the same id as the tab that was closed — ids are never reused.
        assert_ne!(reopened, b);
    }

    #[test]
    fn reopen_closed_is_none_when_nothing_has_been_closed() {
        let mut tabs = Tabs::new("https://a.example/");
        assert_eq!(tabs.reopen_closed(Instant::now()), None);
        assert_eq!(tabs.len(), 1);
    }

    #[test]
    fn reopen_closed_replays_closes_in_lifo_order() {
        let mut tabs = Tabs::new("https://a.example/");
        let b = tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/");
        tabs.close(b).unwrap();
        tabs.close(c).unwrap();

        // c was closed last, so it comes back first.
        let first = tabs.reopen_closed(Instant::now()).unwrap();
        assert_eq!(tabs.get(first).unwrap().current_url(), "https://c.example/");
        let second = tabs.reopen_closed(Instant::now()).unwrap();
        assert_eq!(
            tabs.get(second).unwrap().current_url(),
            "https://b.example/"
        );
        assert_eq!(tabs.reopen_closed(Instant::now()), None);
    }

    #[test]
    fn closing_the_only_other_tab_does_not_leave_it_reopenable_when_close_is_refused() {
        // Closing the last remaining tab is refused (see
        // `closing_the_last_tab_is_a_no_op`); it must not have been pushed
        // onto the closed-tab stack either.
        let mut tabs = Tabs::new("https://a.example/");
        let only = tabs.active_id();
        assert_eq!(tabs.close(only), None);
        assert_eq!(tabs.reopen_closed(Instant::now()), None);
    }

    // --- Relative/positional activation (keyboard shortcuts) ---

    #[test]
    fn activate_relative_moves_forward_and_wraps_around() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/"); // active
        assert_eq!(tabs.active_id(), c);

        assert_eq!(
            tabs.activate_relative(1, Instant::now()),
            Some(ActivationEffect::Switch)
        );
        assert_eq!(tabs.active_id(), a); // wrapped past the end
        assert_eq!(
            tabs.activate_relative(1, Instant::now()),
            Some(ActivationEffect::Switch)
        );
        assert_eq!(tabs.active_id(), b);
    }

    #[test]
    fn activate_relative_moves_backward_and_wraps_around() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let _b = tabs.open("https://b.example/");
        let _c = tabs.open("https://c.example/");
        tabs.activate(a);

        assert_eq!(
            tabs.activate_relative(-1, Instant::now()),
            Some(ActivationEffect::Switch)
        );
        // Wrapped backward past the start, landing on the last tab.
        assert_eq!(tabs.active_id(), _c);
    }

    #[test]
    fn activate_relative_is_a_no_op_with_a_single_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let only = tabs.active_id();
        assert_eq!(
            tabs.activate_relative(1, Instant::now()),
            Some(ActivationEffect::Switch)
        );
        assert_eq!(tabs.active_id(), only);
    }

    #[test]
    fn activate_by_position_uses_one_based_display_order() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/");
        assert_eq!(ids(&tabs), vec![a, b, c]);

        assert_eq!(
            tabs.activate_by_position(1, Instant::now()),
            Some(ActivationEffect::Switch)
        );
        assert_eq!(tabs.active_id(), a);
        assert_eq!(
            tabs.activate_by_position(3, Instant::now()),
            Some(ActivationEffect::Switch)
        );
        assert_eq!(tabs.active_id(), c);
    }

    #[test]
    fn activate_by_position_is_none_out_of_range() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        tabs.open("https://b.example/");

        assert_eq!(tabs.activate_by_position(0, Instant::now()), None);
        assert_eq!(tabs.activate_by_position(99, Instant::now()), None);
        // Neither out-of-range attempt changed the active tab.
        assert_eq!(tabs.active_id(), tabs.iter().nth(1).unwrap().id());
        let _ = a;
    }

    #[test]
    fn activate_last_targets_the_rightmost_tab_regardless_of_count() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/");
        tabs.activate(a);

        assert_eq!(
            tabs.activate_last(Instant::now()),
            Some(ActivationEffect::Switch)
        );
        assert_eq!(tabs.active_id(), c);
    }

    #[test]
    fn activate_relative_resumes_a_suspended_target() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/"); // active
        tabs.activate(a); // b now background
        assert!(tabs.suspend(b));

        assert_eq!(
            tabs.activate_relative(1, Instant::now()),
            Some(ActivationEffect::Resume)
        );
        assert_eq!(tabs.active_id(), b);
        assert!(!tabs.get(b).unwrap().is_suspended());
    }

    // --- Many tabs: acceptance criterion "10個以上のタブを安定して操作できる" ---

    #[test]
    fn many_tabs_stay_internally_consistent() {
        let mut tabs = Tabs::new("https://0.example/");
        let mut opened = vec![tabs.active_id()];
        for i in 1..15 {
            opened.push(tabs.open(format!("https://{i}.example/")));
        }
        assert_eq!(tabs.len(), 15);
        // Every id is distinct.
        let mut sorted = opened.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 15);
        // Exactly one tab reports itself Active; the rest are Background.
        let active_count = tabs
            .iter()
            .filter(|t| t.state() == TabState::Active)
            .count();
        assert_eq!(active_count, 1);
        assert_eq!(
            tabs.iter()
                .filter(|t| t.state() == TabState::Background)
                .count(),
            14
        );

        // Close every other tab and keep checking the same invariant holds.
        for &id in opened.iter().step_by(2) {
            tabs.close(id);
            assert_eq!(
                tabs.iter()
                    .filter(|t| t.state() == TabState::Active)
                    .count(),
                1
            );
        }
        // 15 tabs, every other one closed (indices 0,2,..,14 — 8 tabs): 7 left.
        assert_eq!(tabs.len(), 7);
    }

    // --- Isolation between tabs: acceptance criterion
    // "各タブの状態が混線しない" ---

    #[test]
    fn mutating_one_tab_never_touches_another_tabs_state() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open("https://b.example/");
        let c = tabs.open("https://c.example/"); // active

        // Drive very different state into each tab.
        tabs.get_mut(a)
            .unwrap()
            .on_navigation_blocked("https://ad.example/");
        tabs.get_mut(b).unwrap().set_title("B's title");
        tabs.get_mut(b)
            .unwrap()
            .set_favicon_url("https://b.example/favicon.ico");
        tabs.get_mut(c)
            .unwrap()
            .on_navigation_started("https://c.example/next");

        // a: only the block counter moved.
        let ta = tabs.get(a).unwrap();
        assert_eq!(ta.blocked_count(), 1);
        assert_eq!(ta.current_url(), "https://a.example/");
        assert_eq!(ta.title(), None);
        assert_eq!(ta.favicon(), &Favicon::Unknown);

        // b: only the title/favicon moved, and it is still Background.
        let tb = tabs.get(b).unwrap();
        assert_eq!(tb.blocked_count(), 0);
        assert_eq!(tb.title(), Some("B's title"));
        assert_eq!(
            tb.favicon(),
            &Favicon::Url("https://b.example/favicon.ico".to_owned())
        );
        assert_eq!(tb.state(), TabState::Background);

        // c: only the URL/loading state moved (navigation clears
        // title/favicon, but c never had any set).
        let tc = tabs.get(c).unwrap();
        assert_eq!(tc.current_url(), "https://c.example/next");
        assert!(tc.is_loading());
        assert_eq!(tc.blocked_count(), 0);
        assert_eq!(tabs.active_id(), c);
    }

    #[test]
    fn background_tab_navigation_does_not_disturb_the_active_tab() {
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        tabs.get_mut(a)
            .unwrap()
            .on_load_finished("https://a.example/");
        let b = tabs.open("https://b.example/"); // active
        tabs.activate(a); // b now background

        // A background tab keeps loading/navigating on its own.
        tabs.get_mut(b)
            .unwrap()
            .on_navigation_started("https://b.example/other");
        tabs.get_mut(b)
            .unwrap()
            .on_load_finished("https://b.example/other");

        assert_eq!(tabs.active_id(), a);
        assert_eq!(tabs.active().current_url(), "https://a.example/");
        assert!(!tabs.active().is_loading());
        assert_eq!(
            tabs.get(b).unwrap().current_url(),
            "https://b.example/other"
        );
    }
    // -- Issue #63: live_tab_count / suspension_candidates -----------------

    #[test]
    fn live_tab_count_excludes_suspended_tabs() {
        let mut tabs = Tabs::new("https://a.example/");
        assert_eq!(tabs.live_tab_count(), 1);
        let b = tabs.open("https://b.example/");
        let _c = tabs.open("https://c.example/");
        assert_eq!(tabs.live_tab_count(), 3);
        assert!(tabs.suspend(b));
        assert_eq!(tabs.live_tab_count(), 2);
        // Resuming (by activating) brings it back.
        tabs.activate(b);
        assert_eq!(tabs.live_tab_count(), 3);
    }

    #[test]
    fn suspension_candidates_are_the_live_tabs_with_their_idle_and_flags() {
        let t0 = Instant::now();
        let mut tabs = Tabs::new("https://a.example/");
        let a = tabs.active_id();
        let b = tabs.open_at("https://b.example/", t0);
        let c = tabs.open_at("https://c.example/", t0 + Duration::from_secs(10));
        // b finished loading; a (still `loading` from `Tab::new`) did not.
        tabs.get_mut(b)
            .unwrap()
            .on_load_finished("https://b.example/");
        let now = t0 + Duration::from_secs(30);

        let candidates = tabs.suspension_candidates(now, |id| id == b, |id| Some(id.get() / 2));
        // Display order: a, b, c — c is the active tab, flagged as such.
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].id, a);
        assert!(!candidates[0].active);
        assert_eq!(candidates[0].idle, Duration::from_secs(30));
        assert!(candidates[0].loading);
        assert!(!candidates[0].protected);
        assert_eq!(candidates[0].process_group, Some(0));
        assert_eq!(candidates[1].id, b);
        assert!(!candidates[1].active);
        assert_eq!(candidates[1].idle, Duration::from_secs(20));
        assert!(!candidates[1].loading);
        assert!(candidates[1].protected);
        assert_eq!(candidates[1].process_group, Some(0));
        assert_eq!(candidates[2].id, c);
        assert!(candidates[2].active);
        assert_eq!(candidates[2].process_group, Some(1));

        // A suspended tab drops out of the candidates.
        assert!(tabs.suspend(a));
        let candidates = tabs.suspension_candidates(now, |_| false, |_| None);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].id, b);
        assert_eq!(candidates[0].process_group, None);
        assert!(candidates[1].active);
    }
}
