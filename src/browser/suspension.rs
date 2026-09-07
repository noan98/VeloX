//! Adaptive tab suspension policy (Issue #63).
//!
//! Before this module, automatic suspension had exactly one input: how long
//! a background tab had sat idle (`Config::auto_suspend_after`, D9). That is
//! a poor proxy for the thing suspension is actually for — memory. A user
//! with three tabs open for a day loses scroll position and form state for
//! nothing, while a user who opens twenty tabs in five minutes gets no
//! relief at all. This module replaces the single knob with a policy that
//! combines three independent signals and protects tabs that would be
//! visibly disruptive or wasteful to suspend:
//!
//! | Signal | Knob | What it does |
//! | --- | --- | --- |
//! | idle time | [`SuspensionPolicy::idle_after`] | every background tab idle at least this long is suspended (the pre-#63 behavior, unchanged) |
//! | tab count | [`SuspensionPolicy::max_live_tabs`] | when more than this many tabs have a live webview, the least recently used background tabs are suspended until the count fits |
//! | memory | [`SuspensionPolicy::memory_budget_bytes`] | when the process tree's memory (PSS, `metrics::sample_process_tree_rss`) exceeds the budget, enough least recently used background tabs are suspended to be expected to bring it back under (see [`ESTIMATED_BYTES_PER_TAB`]) — the further over budget, the more tabs go in one sweep, which is the "adaptive" part |
//!
//! Every signal is optional (`None` = off). **As of Issue #184
//! (docs/decisions.md D90), the default policy has the memory-budget signal
//! on** ([`DEFAULT_MEMORY_BUDGET_BYTES`], 700 MiB) while idle time and
//! tab-count stay off — D9's "never suspend a tab the user did not ask for"
//! rule still holds for those two, but D90 concluded a memory-budget-only
//! default does not actually violate it in practice: with few tabs open the
//! process tree never crosses the budget, so nothing is ever suspended,
//! exactly like the old fully-off default. Set `VELOX_MEMORY_BUDGET_MB=0`
//! (or the settings screen's Performance tab) to turn even that off. Every
//! knob is a configuration choice (`VELOX_AUTO_SUSPEND_AFTER_MS`,
//! `VELOX_MAX_LIVE_TABS`, `VELOX_MEMORY_BUDGET_MB` — see `config`).
//!
//! **Protections** — a tab is never suspended automatically when it is:
//!
//! - the active tab (the visible tab always needs a live webview; enforced
//!   again by `Tabs::suspend`/`BrowserWindow::suspend_tab`, this module just
//!   never nominates it);
//! - still loading (dropping a webview mid-load throws away the work the
//!   engine has already done and guarantees the same cost again on resume —
//!   the worst possible restore-cost trade);
//! - protected by the caller ([`Candidate::protected`]), which `app.rs` sets
//!   for a tab that is playing audio (`BrowserWindow::is_playing_audio`) —
//!   suspending it would cut the sound off, the most user-visible way a
//!   background tab can be disrupted.
//!
//! **Ordering — the unit of reclaim is a web process, not a tab.** D54
//! packs up to four tabs into one `WebKitWebProcess`, and D56 measured
//! that dropping a webview inside a process that keeps running reclaims
//! only a fraction of that page's memory (the freed heap stays resident
//! in the process; a process that hosted 15 pages over time still held
//! 496 MiB for its 3 live tabs), whereas a process whose last webview is
//! dropped exits and returns everything. So when the tab-count or memory
//! signal asks for tabs to go, [`plan`] first empties whole process groups
//! — every tab of the least recently used *group* that contains no
//! ineligible tab (active, loading, protected) — even when that overshoots
//! the demand, and only then falls back to individual least recently used
//! tabs from groups it cannot empty (a partial reclaim, better than
//! nothing). Within a group, and for the idle signal (which is per tab by
//! definition), the least recently used tab goes first. A group's own
//! recency is that of its most recently used tab: a group whose newest
//! tab is older than every tab of another group goes first.
//!
//! This module is pure, clock-injected Rust with no UI/engine dependency
//! (D20): it never reads a clock or `/proc` itself. [`plan`] takes a
//! snapshot of every live tab ([`Candidate`], including the active one and
//! which process group each is in) and an optional fresh memory sample,
//! and returns which tabs to suspend and why ([`SuspendReason`]). Acting on
//! that (dropping webviews, logging) is `app.rs`'s job.

use std::time::Duration;

use super::tab::TabId;

/// Rough memory reclaimed by suspending one background tab, used only to
/// turn "we are N bytes over budget" into "so suspend about this many tabs
/// in this sweep". D54 measured 63.3 MiB per additional tab on
/// `minimal.html` under WebKitGTK in the reference environment
/// (`docs/memory-analysis.md` §10.2), rounded to 64 MiB here.
///
/// This is a tuning knob, not a promise: a heavier page frees more, a
/// lighter one less. Getting it wrong only changes how many sweeps
/// convergence takes — [`plan`] runs again on the next memory sample, so an
/// underestimate suspends a few more tabs next time and an overestimate
/// stops early. It is deliberately *not* refined from measurements at
/// runtime (e.g. sampling before/after each suspension), which would add
/// `/proc` walks on the hot path for a second-order improvement.
pub const ESTIMATED_BYTES_PER_TAB: u64 = 64 * 1024 * 1024;

/// The default policy's memory budget (Issue #184, docs/decisions.md D90):
/// 700 MiB, the same figure D56 measured (`VELOX_MEMORY_BUDGET_MB=700`) to
/// land within budget at 1/5/10/20 tabs on `minimal.html` in this project's
/// reference Linux/WebKitGTK environment (407 / 660 / 476 / 615 MiB —
/// `docs/performance-targets.md` §12). A single, absolute, compiled-in
/// number, not scaled to the machine's installed RAM — D90's Revisit
/// condition records that limitation (Issue #176 is where a RAM-relative
/// budget belongs).
///
/// `browser::settings::PerformanceSettings::default`'s `memory_budget_mb`
/// mirrors this value in MiB so `Config::default().to_settings()` and
/// `Settings::default()` agree (see that constant's doc comment).
pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 700 * 1024 * 1024;

/// The automatic suspension policy — three independent, individually
/// optional signals plus how often memory is checked. See the module doc
/// comment for what each does and [`plan`] for how they combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuspensionPolicy {
    /// Suspend a background tab once it has been idle (not the active tab)
    /// for at least this long. `None` disables the idle signal. This is
    /// the pre-#63 `Config::auto_suspend_after`, unchanged in meaning.
    pub idle_after: Option<Duration>,
    /// Keep at most this many tabs alive (with a webview) at once; the
    /// active tab always counts as one of them, so `Some(1)` means "every
    /// background tab is suspended as soon as it is eligible". `None`
    /// disables the tab-count signal. A value of `0` is treated as `1`
    /// (the active tab can never be suspended, so there is no way to have
    /// zero live tabs).
    pub max_live_tabs: Option<usize>,
    /// Suspend background tabs whenever the process tree's memory exceeds
    /// this many bytes. `None` disables the memory signal, and no memory
    /// sampling happens at all.
    pub memory_budget_bytes: Option<u64>,
    /// How often the process tree's memory is sampled while
    /// `memory_budget_bytes` is set. Irrelevant (and no sampler runs)
    /// otherwise.
    pub memory_check_interval: Duration,
}

impl SuspensionPolicy {
    /// The default interval between memory samples. Two seconds is slow
    /// enough that walking `/proc` (every process on the machine, see
    /// `metrics::sample_process_tree_rss`) is negligible, and fast enough
    /// that a burst of new tabs is reined in within a few seconds rather
    /// than sitting over budget for a long time.
    pub const DEFAULT_MEMORY_CHECK_INTERVAL: Duration = Duration::from_secs(2);

    /// Whether any signal is on at all. When `false`, the event loop has
    /// nothing to sweep and no deadline to wake up for.
    pub fn is_enabled(&self) -> bool {
        self.idle_after.is_some()
            || self.max_live_tabs.is_some()
            || self.memory_budget_bytes.is_some()
    }
}

impl Default for SuspensionPolicy {
    /// Memory budget on ([`DEFAULT_MEMORY_BUDGET_BYTES`], 700 MiB), idle
    /// time and tab-count off (Issue #184, docs/decisions.md D90 — this
    /// decided D56's Revisit condition (3), superseding D9's original
    /// "every signal off" default without rewriting D9/D56 themselves).
    /// `max_live_tabs` is not defaulted on: D90 records why (a process-
    /// group-unit cap loses up to `max_tabs_per_web_process` tabs' worth of
    /// scroll/form state the moment it is exceeded, and D56's own Revisit
    /// condition (2) says its recommended value should come from real-site
    /// measurement, not `minimal.html`). `idle_after` is not defaulted on
    /// either — it is a poor proxy for memory pressure by itself (see the
    /// module doc comment) and D9's original reasoning for leaving it off
    /// was never revisited.
    fn default() -> Self {
        Self {
            idle_after: None,
            max_live_tabs: None,
            memory_budget_bytes: Some(DEFAULT_MEMORY_BUDGET_BYTES),
            memory_check_interval: Self::DEFAULT_MEMORY_CHECK_INTERVAL,
        }
    }
}

/// Why [`plan`] chose to suspend a tab. Logged (as the `reason` field of
/// the `tab_suspend` perf event) so a benchmark or a user reading the log
/// can tell an idle-time suspension from a memory-pressure one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendReason {
    /// Idle longer than [`SuspensionPolicy::idle_after`].
    Idle,
    /// More live tabs than [`SuspensionPolicy::max_live_tabs`].
    TabCount,
    /// Process-tree memory over [`SuspensionPolicy::memory_budget_bytes`].
    Memory,
}

impl SuspendReason {
    /// Stable lowercase name, as written into perf logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SuspendReason::Idle => "idle",
            SuspendReason::TabCount => "tab_count",
            SuspendReason::Memory => "memory",
        }
    }
}

/// One live (not suspended) tab as [`plan`] sees it — the active tab
/// included, flagged, so the policy knows which process group it pins.
/// Built by the caller from `Tabs` plus whatever the engine side knows
/// (`protected`, `process_group`), so this module needs neither `Tabs` nor
/// a webview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub id: TabId,
    /// Whether this is the active (visible) tab. Never suspended, and it
    /// pins its process group: a group holding the active tab can never be
    /// emptied.
    pub active: bool,
    /// How long this tab has been in the background (`Tab::idle_for`);
    /// meaningless (and unused) when `active`.
    pub idle: Duration,
    /// Whether the tab's page is still loading (`Tab::is_loading`). Never
    /// suspended — see the module doc comment.
    pub loading: bool,
    /// Whether the caller wants this tab kept alive regardless of the
    /// signals (today: it is playing audio). Never suspended.
    pub protected: bool,
    /// Which web process this tab's webview lives in (D54's process group
    /// id, `BrowserWindow::process_group_of`). `None` when the caller does
    /// not know (a platform without process groups, or a tab the window
    /// does not track): such a tab is treated as its own, never-emptyable
    /// group, i.e. it only ever goes through the per-tab fallback path.
    pub process_group: Option<u64>,
}

impl Candidate {
    /// Whether the policy may suspend this tab at all.
    fn eligible(&self) -> bool {
        !self.active && !self.loading && !self.protected
    }
}

/// A fresh memory sample for [`plan`]'s memory signal: the process tree's
/// total memory in bytes (PSS where available, see
/// `app::spawn_memory_pressure_sampler`). Passed only when a *new* sample
/// has arrived since the last sweep — re-using a stale sample would keep
/// suspending more tabs on every pass before the effect of the previous
/// sweep is even visible in the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySample {
    pub total_bytes: u64,
}

/// Decide which tabs to suspend right now.
///
/// - `candidates`: every live tab (not already suspended), the active one
///   included, in any order. Their count is what the tab-count signal
///   compares against [`SuspensionPolicy::max_live_tabs`].
/// - `memory`: a fresh memory sample, or `None` to skip the memory signal
///   this sweep (no new sample, or memory checking is off).
///
/// Returns the tabs to suspend, in the order they should be suspended,
/// each tagged with the signal that demanded it. The active tab and tabs
/// that are loading or protected are never returned. The signals combine
/// as follows:
///
/// 1. every eligible tab idle for at least `idle_after` is returned (idle
///    signal — per tab, by definition);
/// 2. the tab-count and memory demands ("how many more tabs must go") are
///    computed, the idle tabs already taken are subtracted from both, and
///    the larger remaining demand (not the sum — suspending one tab
///    satisfies both) is met by walking [`reclaim_order`]: whole emptyable
///    process groups first, least recently used group first. Starting a
///    group means taking *all* of it, so the demand may be overshot by up
///    to a group's worth of tabs — that overshoot is the point (see the
///    module doc comment: only an exiting process gives the memory back);
///    tabs from groups that cannot be emptied come last, one at a time.
pub fn plan(
    policy: &SuspensionPolicy,
    candidates: &[Candidate],
    memory: Option<MemorySample>,
) -> Vec<(TabId, SuspendReason)> {
    if !policy.is_enabled() {
        return Vec::new();
    }
    let live_tabs = candidates.len();

    let mut planned: Vec<(TabId, SuspendReason)> = Vec::new();
    if let Some(idle_after) = policy.idle_after {
        // Least recently used first; a stable sort keeps the caller's
        // order for ties (equal idle times — e.g. tabs opened in one
        // burst).
        let mut idle: Vec<&Candidate> = candidates
            .iter()
            .filter(|tab| tab.eligible() && tab.idle >= idle_after)
            .collect();
        idle.sort_by_key(|tab| std::cmp::Reverse(tab.idle));
        planned.extend(idle.iter().map(|tab| (tab.id, SuspendReason::Idle)));
    }

    let count_demand = policy
        .max_live_tabs
        .map(|max| live_tabs.saturating_sub(max.max(1)))
        .unwrap_or(0);
    let memory_demand = match (policy.memory_budget_bytes, memory) {
        (Some(budget), Some(sample)) => tabs_to_free(sample.total_bytes, budget),
        _ => 0,
    };
    // Whatever the idle signal already takes counts toward both demands.
    let already = planned.len();
    let count_remaining = count_demand.saturating_sub(already);
    let memory_remaining = memory_demand.saturating_sub(already);
    let extra = count_remaining.max(memory_remaining);
    if extra > 0 {
        let mut taken = 0;
        for chunk in reclaim_order(candidates) {
            if taken >= extra {
                break;
            }
            // Attribute the whole chunk to the signal that still needed
            // more tabs when the chunk was started: the tab-count signal
            // while its demand is unmet, the memory signal after that. A
            // group's overshoot is credited to the same signal as its
            // first tab — it went for that signal's sake.
            let reason = if taken < count_remaining {
                SuspendReason::TabCount
            } else {
                SuspendReason::Memory
            };
            for tab in chunk {
                if planned.iter().any(|(id, _)| *id == tab.id) {
                    continue;
                }
                planned.push((tab.id, reason));
                taken += 1;
            }
        }
    }
    planned
}

/// The order in which eligible tabs should be reclaimed to free the most
/// memory soonest, as a list of chunks that must be taken whole:
///
/// 1. one chunk per *emptyable* process group — a group (`Some` id) whose
///    every tab is eligible, so suspending all of them makes the process
///    exit — least recently used group first (by its most recently used
///    tab), each chunk's tabs least recently used first;
/// 2. then one single-tab chunk per remaining eligible tab (a tab in a
///    group pinned by the active/a loading/a protected tab, or with no
///    known group), least recently used first.
///
/// Public for the same reason as [`plan`]: `app.rs` never calls it, but a
/// caller that wants to explain *why* a tab went (a debug view) can.
pub fn reclaim_order(candidates: &[Candidate]) -> Vec<Vec<&Candidate>> {
    use std::collections::BTreeMap;

    // group id -> (its tabs, whether every one of them is eligible).
    let mut groups: BTreeMap<u64, (Vec<&Candidate>, bool)> = BTreeMap::new();
    let mut ungrouped: Vec<&Candidate> = Vec::new();
    for tab in candidates {
        match tab.process_group {
            Some(group) => {
                let entry = groups.entry(group).or_insert((Vec::new(), true));
                entry.0.push(tab);
                entry.1 &= tab.eligible();
            }
            None => ungrouped.push(tab),
        }
    }

    let mut emptyable: Vec<Vec<&Candidate>> = Vec::new();
    let mut leftovers: Vec<&Candidate> = ungrouped.into_iter().filter(|t| t.eligible()).collect();
    for (_, (mut tabs, all_eligible)) in groups {
        if all_eligible && !tabs.is_empty() {
            tabs.sort_by_key(|tab| std::cmp::Reverse(tab.idle));
            emptyable.push(tabs);
        } else {
            leftovers.extend(tabs.into_iter().filter(|t| t.eligible()));
        }
    }
    // A group's recency is its *most* recently used tab (the smallest
    // idle, i.e. the last element after the sort above): the group whose
    // newest tab is the oldest goes first. `BTreeMap` iteration made the
    // input order deterministic, and the sort is stable, so ties (equal
    // recency) resolve by ascending group id.
    emptyable
        .sort_by_key(|tabs| std::cmp::Reverse(tabs.last().map(|tab| tab.idle).unwrap_or_default()));
    leftovers.sort_by_key(|tab| std::cmp::Reverse(tab.idle));

    let mut order = emptyable;
    order.extend(leftovers.into_iter().map(|tab| vec![tab]));
    order
}

/// How many tabs' worth of memory `total` is over `budget`, rounded up —
/// zero when at or under budget. The adaptive core of the memory signal:
/// 10 MiB over frees one tab, 200 MiB over frees four in the same sweep.
fn tabs_to_free(total: u64, budget: u64) -> usize {
    let excess = total.saturating_sub(budget);
    if excess == 0 {
        return 0;
    }
    let tabs = excess.div_ceil(ESTIMATED_BYTES_PER_TAB).max(1);
    usize::try_from(tabs).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// A background tab in its own process group (id = tab id), so the
    /// per-signal tests below behave exactly as a per-tab policy would:
    /// every group is a one-tab emptyable group.
    fn tab(id: u64, idle_secs: u64) -> Candidate {
        Candidate {
            id: TabId::from(id),
            active: false,
            idle: Duration::from_secs(idle_secs),
            loading: false,
            protected: false,
            process_group: Some(id),
        }
    }

    /// A background tab in process group `group`.
    fn grouped(id: u64, idle_secs: u64, group: u64) -> Candidate {
        Candidate {
            process_group: Some(group),
            ..tab(id, idle_secs)
        }
    }

    /// The active tab (never suspended; pins its group).
    fn active(id: u64, group: u64) -> Candidate {
        Candidate {
            active: true,
            ..grouped(id, 0, group)
        }
    }

    /// `candidates` plus one active tab (id 1000, its own group), so the
    /// live count `plan` derives is "active + these", as in `app.rs`.
    fn with_active(candidates: &[Candidate]) -> Vec<Candidate> {
        let mut all = candidates.to_vec();
        all.push(active(1000, 1000));
        all
    }

    fn ids(planned: &[(TabId, SuspendReason)]) -> Vec<u64> {
        planned.iter().map(|(id, _)| id.get()).collect()
    }

    /// Baseline for testing one signal in isolation: every signal off. This
    /// is deliberately *not* [`SuspensionPolicy::default`] — since D90 (Issue
    /// #184) the compiled-in default has the memory signal on, and the
    /// per-signal tests below (`..disabled_policy()`) need a genuinely inert
    /// starting point so they exercise exactly the one signal each is named
    /// after, not "that signal plus whatever the compiled default happens to
    /// enable".
    fn disabled_policy() -> SuspensionPolicy {
        SuspensionPolicy {
            idle_after: None,
            max_live_tabs: None,
            memory_budget_bytes: None,
            memory_check_interval: SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL,
        }
    }

    #[test]
    fn default_policy_enables_only_the_memory_budget_signal() {
        // Issue #184 / docs/decisions.md D90: automatic suspension is on by
        // default, but only the memory-budget signal — idle time and
        // tab-count stay opt-in. See D90 for why tab-count specifically is
        // not defaulted on.
        let policy = SuspensionPolicy::default();
        assert!(policy.is_enabled());
        assert_eq!(policy.idle_after, None);
        assert_eq!(policy.max_live_tabs, None);
        assert_eq!(
            policy.memory_budget_bytes,
            Some(DEFAULT_MEMORY_BUDGET_BYTES)
        );
        assert_eq!(policy.memory_budget_bytes, Some(700 * MIB));
        assert_eq!(
            policy.memory_check_interval,
            SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL
        );
    }

    #[test]
    fn default_policy_stays_inert_with_few_tabs_and_no_over_budget_sample() {
        // The property that keeps D90's default from being a surprise: with
        // few tabs open (the common case) there is either no fresh memory
        // sample yet, or the sample is comfortably under budget, so `plan`
        // returns nothing — bit-for-bit the same as the old fully-off
        // default for everyone who never approaches 700 MiB.
        let candidates = [tab(1, 3600), tab(2, 3600)];
        assert!(plan(
            &SuspensionPolicy::default(),
            &with_active(&candidates),
            None
        )
        .is_empty());
        let under_budget = Some(MemorySample {
            total_bytes: 400 * MIB,
        });
        assert!(plan(
            &SuspensionPolicy::default(),
            &with_active(&candidates),
            under_budget
        )
        .is_empty());
    }

    #[test]
    fn default_policy_actually_suspends_once_over_budget() {
        // The other half: D90's default is not just "enabled" in name —
        // fed a real over-budget sample, it plans suspensions, tagged with
        // the memory reason, exactly like an explicit
        // `VELOX_MEMORY_BUDGET_MB` would.
        let candidates = [tab(1, 5), tab(2, 10)];
        let over_budget = Some(MemorySample {
            total_bytes: 900 * MIB,
        });
        let planned = plan(
            &SuspensionPolicy::default(),
            &with_active(&candidates),
            over_budget,
        );
        assert!(!planned.is_empty());
        assert!(planned.iter().all(|(_, r)| *r == SuspendReason::Memory));
    }

    #[test]
    fn disabled_policy_never_plans_anything() {
        let candidates = [tab(1, 3600), tab(2, 3600)];
        let memory = Some(MemorySample {
            total_bytes: 10_000 * MIB,
        });
        assert!(plan(&disabled_policy(), &with_active(&candidates), memory).is_empty());
    }

    // -- idle signal (pre-#63 behavior) ------------------------------------

    #[test]
    fn idle_signal_suspends_every_tab_past_the_threshold() {
        let policy = SuspensionPolicy {
            idle_after: Some(Duration::from_secs(60)),
            ..disabled_policy()
        };
        let candidates = [tab(1, 10), tab(2, 60), tab(3, 600)];
        let planned = plan(&policy, &with_active(&candidates), None);
        // Longest idle first; the tab under the threshold is left alone.
        assert_eq!(ids(&planned), vec![3, 2]);
        assert!(planned.iter().all(|(_, r)| *r == SuspendReason::Idle));
    }

    // -- tab-count signal ------------------------------------------------

    #[test]
    fn tab_count_signal_suspends_lru_tabs_until_the_count_fits() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(3),
            ..disabled_policy()
        };
        // 5 live tabs (active + 4 background): two must go.
        let candidates = [tab(1, 5), tab(2, 50), tab(3, 1), tab(4, 20)];
        let planned = plan(&policy, &with_active(&candidates), None);
        assert_eq!(ids(&planned), vec![2, 4]);
        assert!(planned.iter().all(|(_, r)| *r == SuspendReason::TabCount));
    }

    #[test]
    fn tab_count_signal_is_satisfied_when_within_the_limit() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(3),
            ..disabled_policy()
        };
        let candidates = [tab(1, 5), tab(2, 50)];
        assert!(plan(&policy, &with_active(&candidates), None).is_empty());
    }

    #[test]
    fn max_live_tabs_of_zero_behaves_as_one() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(0),
            ..disabled_policy()
        };
        let candidates = [tab(1, 5), tab(2, 50)];
        // live = active + 2 background = 3; with a floor of 1 live tab,
        // exactly the two background tabs go (never "3").
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), None)),
            vec![2, 1]
        );
    }

    // -- memory signal ---------------------------------------------------

    #[test]
    fn memory_signal_scales_with_how_far_over_budget() {
        let policy = SuspensionPolicy {
            memory_budget_bytes: Some(500 * MIB),
            ..disabled_policy()
        };
        let candidates = [tab(1, 1), tab(2, 2), tab(3, 3), tab(4, 4), tab(5, 5)];
        let over_by = |mib: u64| {
            Some(MemorySample {
                total_bytes: (500 + mib) * MIB,
            })
        };
        // Just over: one tab. 64 MiB over: still one. 65 MiB over: two.
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), over_by(1))),
            vec![5]
        );
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), over_by(64))),
            vec![5]
        );
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), over_by(65))),
            vec![5, 4]
        );
        // 200 MiB over: four tabs in one sweep.
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), over_by(200))),
            vec![5, 4, 3, 2]
        );
        // Far more than there are tabs to free: everything eligible, no
        // panic.
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), over_by(100_000))),
            vec![5, 4, 3, 2, 1]
        );
    }

    #[test]
    fn memory_signal_does_nothing_at_or_under_budget() {
        let policy = SuspensionPolicy {
            memory_budget_bytes: Some(500 * MIB),
            ..disabled_policy()
        };
        let candidates = [tab(1, 1), tab(2, 2)];
        for total in [0, 100 * MIB, 500 * MIB] {
            let memory = Some(MemorySample { total_bytes: total });
            assert!(plan(&policy, &with_active(&candidates), memory).is_empty());
        }
    }

    #[test]
    fn memory_signal_needs_a_fresh_sample() {
        let policy = SuspensionPolicy {
            memory_budget_bytes: Some(1),
            ..disabled_policy()
        };
        let candidates = [tab(1, 1), tab(2, 2)];
        // Budget is effectively zero, but with no sample this sweep the
        // memory signal must stay quiet.
        assert!(plan(&policy, &with_active(&candidates), None).is_empty());
    }

    #[test]
    fn tabs_to_free_rounds_up_and_never_returns_zero_when_over() {
        assert_eq!(tabs_to_free(100, 100), 0);
        assert_eq!(tabs_to_free(99, 100), 0);
        assert_eq!(tabs_to_free(101, 100), 1);
        assert_eq!(tabs_to_free(100 + ESTIMATED_BYTES_PER_TAB, 100), 1);
        assert_eq!(tabs_to_free(101 + ESTIMATED_BYTES_PER_TAB, 100), 2);
        // Absurd excess: no overflow, no panic, just "a lot".
        assert_eq!(
            tabs_to_free(u64::MAX, 0),
            u64::MAX.div_ceil(ESTIMATED_BYTES_PER_TAB) as usize
        );
    }

    // -- protections -------------------------------------------------------

    #[test]
    fn loading_and_protected_tabs_are_never_suspended() {
        let policy = SuspensionPolicy {
            idle_after: Some(Duration::from_secs(1)),
            max_live_tabs: Some(1),
            memory_budget_bytes: Some(1),
            ..disabled_policy()
        };
        let candidates = [
            Candidate {
                loading: true,
                ..tab(1, 1000)
            },
            Candidate {
                protected: true,
                ..tab(2, 1000)
            },
            tab(3, 1000),
        ];
        let memory = Some(MemorySample {
            total_bytes: 10_000 * MIB,
        });
        // Every signal is screaming, yet only the plain tab goes.
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), memory)),
            vec![3]
        );
    }

    // -- combining signals -------------------------------------------------

    #[test]
    fn idle_tabs_count_toward_the_other_signals_demands() {
        let policy = SuspensionPolicy {
            idle_after: Some(Duration::from_secs(100)),
            max_live_tabs: Some(3),
            ..disabled_policy()
        };
        // Live = 5, limit 3 -> two must go. Tab 4 is idle anyway, so the
        // tab-count signal only needs one more (the next LRU: tab 2).
        let candidates = [tab(1, 5), tab(2, 50), tab(3, 1), tab(4, 500)];
        let planned = plan(&policy, &with_active(&candidates), None);
        assert_eq!(
            planned,
            vec![
                (TabId::from(4), SuspendReason::Idle),
                (TabId::from(2), SuspendReason::TabCount),
            ]
        );
    }

    #[test]
    fn count_and_memory_demands_take_the_larger_not_the_sum() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(4),
            memory_budget_bytes: Some(500 * MIB),
            ..disabled_policy()
        };
        // Live = 6, limit 4 -> count wants 2. 150 MiB over -> memory wants 3.
        let candidates = [tab(1, 1), tab(2, 2), tab(3, 3), tab(4, 4), tab(5, 5)];
        let memory = Some(MemorySample {
            total_bytes: 650 * MIB,
        });
        let planned = plan(&policy, &with_active(&candidates), memory);
        assert_eq!(
            planned,
            vec![
                (TabId::from(5), SuspendReason::TabCount),
                (TabId::from(4), SuspendReason::TabCount),
                (TabId::from(3), SuspendReason::Memory),
            ]
        );
    }

    #[test]
    fn stable_order_for_equal_idle_times() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(2),
            ..disabled_policy()
        };
        // All opened in one burst (equal idle): the caller's order wins,
        // deterministically.
        let candidates = [tab(7, 10), tab(8, 10), tab(9, 10)];
        assert_eq!(
            ids(&plan(&policy, &with_active(&candidates), None)),
            vec![7, 8]
        );
    }

    // -- process-unit reclaim (D56) ----------------------------------------

    #[test]
    fn an_emptyable_group_is_taken_whole_even_when_it_overshoots_the_demand() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(5),
            ..disabled_policy()
        };
        // Group 0: four background tabs. Group 1: the active tab plus one
        // background tab (pinned). Live = 6, limit 5 -> demand 1, but the
        // whole of group 0 goes so its process can exit.
        let candidates = [
            grouped(1, 40, 0),
            grouped(2, 30, 0),
            grouped(3, 20, 0),
            grouped(4, 10, 0),
            grouped(5, 50, 1),
            active(6, 1),
        ];
        let planned = plan(&policy, &candidates, None);
        assert_eq!(ids(&planned), vec![1, 2, 3, 4]);
        // The overshoot is credited to the signal the group went for.
        assert!(planned.iter().all(|(_, r)| *r == SuspendReason::TabCount));
    }

    #[test]
    fn groups_go_least_recently_used_group_first_by_their_newest_tab() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(1),
            ..disabled_policy()
        };
        // Group 0's newest tab (idle 5) is newer than group 1's newest
        // (idle 8), even though group 0 also holds the oldest tab of all.
        let candidates = [
            grouped(1, 100, 0),
            grouped(2, 5, 0),
            grouped(3, 9, 1),
            grouped(4, 8, 1),
            active(9, 7),
        ];
        let planned = plan(&policy, &candidates, None);
        assert_eq!(ids(&planned), vec![3, 4, 1, 2]);
    }

    #[test]
    fn a_group_pinned_by_an_ineligible_tab_falls_back_to_per_tab_lru_last() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(1),
            ..disabled_policy()
        };
        let candidates = [
            // Group 0 pinned by the active tab: its background tabs are
            // leftovers.
            active(0, 0),
            grouped(1, 500, 0),
            // Group 1 pinned by a loading tab.
            Candidate {
                loading: true,
                ..grouped(2, 400, 1)
            },
            grouped(3, 300, 1),
            // Group 2 emptyable, but its tabs are the newest of all.
            grouped(4, 2, 2),
            grouped(5, 1, 2),
            // No known group: never emptyable, a leftover.
            Candidate {
                process_group: None,
                ..tab(6, 450)
            },
        ];
        let planned = plan(&policy, &candidates, None);
        // Emptyable group 2 first (whole), then leftovers by idle: 1
        // (500), 6 (450), 3 (300). Tab 2 (loading) never.
        assert_eq!(ids(&planned), vec![4, 5, 1, 6, 3]);
    }

    #[test]
    fn reclaim_order_stops_at_the_demand_between_groups() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(3),
            ..disabled_policy()
        };
        // Two emptyable one-tab groups and a pinned group. Live = 5,
        // limit 3 -> demand 2: both single-tab groups, nothing from the
        // pinned group.
        let candidates = [
            grouped(1, 10, 0),
            grouped(2, 20, 1),
            grouped(3, 30, 2),
            grouped(4, 40, 2),
            active(5, 3),
        ];
        // Groups 0 and 1 are single-tab; group 2 is emptyable too and its
        // newest tab (30) is older than groups 0 (10) and 1 (20), so it
        // goes first — and whole (its own tabs least recently used first):
        // demand 2 is met by it alone.
        let planned = plan(&policy, &candidates, None);
        assert_eq!(ids(&planned), vec![4, 3]);
    }

    #[test]
    fn the_active_tab_is_never_planned_even_when_alone_in_its_group() {
        let policy = SuspensionPolicy {
            idle_after: Some(Duration::ZERO),
            max_live_tabs: Some(1),
            memory_budget_bytes: Some(1),
            ..disabled_policy()
        };
        let candidates = [active(0, 0)];
        let memory = Some(MemorySample {
            total_bytes: 10_000 * MIB,
        });
        assert!(plan(&policy, &candidates, memory).is_empty());
    }

    #[test]
    fn reason_names_are_stable() {
        assert_eq!(SuspendReason::Idle.as_str(), "idle");
        assert_eq!(SuspendReason::TabCount.as_str(), "tab_count");
        assert_eq!(SuspendReason::Memory.as_str(), "memory");
    }
}
