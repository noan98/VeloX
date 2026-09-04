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
//! Every signal is optional (`None` = off) and the default policy has all
//! three off, so a fresh checkout still never suspends a tab the user did
//! not ask to suspend (D9's rule stands). Turning any of them on is a
//! configuration choice (`VELOX_AUTO_SUSPEND_AFTER_MS`,
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
//! **Ordering** — among eligible tabs, the least recently used (longest
//! idle) is always suspended first, for every signal. That is the tab whose
//! state the user is least likely to miss, and it keeps the three signals
//! from disagreeing about *which* tabs go — they only ever disagree about
//! *how many*.
//!
//! This module is pure, clock-injected Rust with no UI/engine dependency
//! (D20): it never reads a clock or `/proc` itself. [`plan`] takes a
//! snapshot of the background tabs ([`Candidate`]), the live tab count, and
//! an optional fresh memory sample, and returns which tabs to suspend and
//! why ([`SuspendReason`]). Acting on that (dropping webviews, logging) is
//! `app.rs`'s job.

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
    /// Every signal off — automatic suspension stays opt-in (D9).
    fn default() -> Self {
        Self {
            idle_after: None,
            max_live_tabs: None,
            memory_budget_bytes: None,
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

/// One background, not-yet-suspended tab as [`plan`] sees it. Built by the
/// caller from `Tabs` plus whatever the engine side knows (`protected`),
/// so this module needs neither `Tabs` nor a webview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub id: TabId,
    /// How long this tab has been in the background (`Tab::idle_for`).
    pub idle: Duration,
    /// Whether the tab's page is still loading (`Tab::is_loading`). Never
    /// suspended — see the module doc comment.
    pub loading: bool,
    /// Whether the caller wants this tab kept alive regardless of the
    /// signals (today: it is playing audio). Never suspended.
    pub protected: bool,
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
/// - `candidates`: every background tab that is not already suspended, in
///   any order.
/// - `live_tabs`: how many tabs currently have a webview — the active tab
///   plus every candidate (suspended tabs are not counted). Passed
///   separately rather than derived as `candidates.len() + 1` so the
///   caller's notion of "live" (which includes loading/protected tabs
///   this function will refuse to suspend) is the one the tab-count signal
///   compares against.
/// - `memory`: a fresh memory sample, or `None` to skip the memory signal
///   this sweep (no new sample, or memory checking is off).
///
/// Returns the tabs to suspend, least recently used first, each tagged
/// with the signal that demanded it. Tabs that are loading or protected
/// are never returned. The three signals combine as follows: every idle
/// tab is returned (idle signal); then, if the tab-count or memory signal
/// still wants more tabs gone *after* those idle ones are subtracted, the
/// next least recently used eligible tabs are added, up to the larger of
/// the two demands (they are not additive — both are estimates of "how
/// many tabs need to go", and suspending one tab satisfies both).
pub fn plan(
    policy: &SuspensionPolicy,
    candidates: &[Candidate],
    live_tabs: usize,
    memory: Option<MemorySample>,
) -> Vec<(TabId, SuspendReason)> {
    if !policy.is_enabled() {
        return Vec::new();
    }
    // Least recently used first; a stable sort keeps the caller's order
    // for ties (equal idle times — e.g. tabs opened in one burst).
    let mut eligible: Vec<&Candidate> = candidates
        .iter()
        .filter(|tab| !tab.loading && !tab.protected)
        .collect();
    eligible.sort_by(|a, b| b.idle.cmp(&a.idle));

    let mut planned: Vec<(TabId, SuspendReason)> = Vec::new();
    if let Some(idle_after) = policy.idle_after {
        planned.extend(
            eligible
                .iter()
                .filter(|tab| tab.idle >= idle_after)
                .map(|tab| (tab.id, SuspendReason::Idle)),
        );
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
        let taken: Vec<(TabId, SuspendReason)> = eligible
            .iter()
            .filter(|tab| !planned.iter().any(|(id, _)| *id == tab.id))
            .take(extra)
            .enumerate()
            .map(|(i, tab)| {
                // Attribute each extra tab to the signal that still needed
                // it: the first `count_remaining` go to the tab-count
                // signal, anything beyond that only the memory signal
                // asked for.
                let reason = if i < count_remaining {
                    SuspendReason::TabCount
                } else {
                    SuspendReason::Memory
                };
                (tab.id, reason)
            })
            .collect();
        planned.extend(taken);
    }
    planned
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

    fn tab(id: u64, idle_secs: u64) -> Candidate {
        Candidate {
            id: TabId::from(id),
            idle: Duration::from_secs(idle_secs),
            loading: false,
            protected: false,
        }
    }

    fn ids(planned: &[(TabId, SuspendReason)]) -> Vec<u64> {
        planned.iter().map(|(id, _)| id.get()).collect()
    }

    fn policy() -> SuspensionPolicy {
        SuspensionPolicy::default()
    }

    #[test]
    fn default_policy_is_fully_disabled() {
        let policy = SuspensionPolicy::default();
        assert!(!policy.is_enabled());
        assert_eq!(policy.idle_after, None);
        assert_eq!(policy.max_live_tabs, None);
        assert_eq!(policy.memory_budget_bytes, None);
        assert_eq!(
            policy.memory_check_interval,
            SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL
        );
    }

    #[test]
    fn disabled_policy_never_plans_anything() {
        let candidates = [tab(1, 3600), tab(2, 3600)];
        let memory = Some(MemorySample {
            total_bytes: 10_000 * MIB,
        });
        assert!(plan(&policy(), &candidates, 3, memory).is_empty());
    }

    // -- idle signal (pre-#63 behavior) ------------------------------------

    #[test]
    fn idle_signal_suspends_every_tab_past_the_threshold() {
        let policy = SuspensionPolicy {
            idle_after: Some(Duration::from_secs(60)),
            ..policy()
        };
        let candidates = [tab(1, 10), tab(2, 60), tab(3, 600)];
        let planned = plan(&policy, &candidates, 4, None);
        // Longest idle first; the tab under the threshold is left alone.
        assert_eq!(ids(&planned), vec![3, 2]);
        assert!(planned.iter().all(|(_, r)| *r == SuspendReason::Idle));
    }

    // -- tab-count signal ------------------------------------------------

    #[test]
    fn tab_count_signal_suspends_lru_tabs_until_the_count_fits() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(3),
            ..policy()
        };
        // 5 live tabs (active + 4 background): two must go.
        let candidates = [tab(1, 5), tab(2, 50), tab(3, 1), tab(4, 20)];
        let planned = plan(&policy, &candidates, 5, None);
        assert_eq!(ids(&planned), vec![2, 4]);
        assert!(planned.iter().all(|(_, r)| *r == SuspendReason::TabCount));
    }

    #[test]
    fn tab_count_signal_is_satisfied_when_within_the_limit() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(3),
            ..policy()
        };
        let candidates = [tab(1, 5), tab(2, 50)];
        assert!(plan(&policy, &candidates, 3, None).is_empty());
    }

    #[test]
    fn max_live_tabs_of_zero_behaves_as_one() {
        let policy = SuspensionPolicy {
            max_live_tabs: Some(0),
            ..policy()
        };
        let candidates = [tab(1, 5), tab(2, 50)];
        // live = active + 2 background = 3; with a floor of 1 live tab,
        // exactly the two background tabs go (never "3").
        assert_eq!(ids(&plan(&policy, &candidates, 3, None)), vec![2, 1]);
    }

    // -- memory signal ---------------------------------------------------

    #[test]
    fn memory_signal_scales_with_how_far_over_budget() {
        let policy = SuspensionPolicy {
            memory_budget_bytes: Some(500 * MIB),
            ..policy()
        };
        let candidates = [tab(1, 1), tab(2, 2), tab(3, 3), tab(4, 4), tab(5, 5)];
        let over_by = |mib: u64| {
            Some(MemorySample {
                total_bytes: (500 + mib) * MIB,
            })
        };
        // Just over: one tab. 64 MiB over: still one. 65 MiB over: two.
        assert_eq!(ids(&plan(&policy, &candidates, 6, over_by(1))), vec![5]);
        assert_eq!(ids(&plan(&policy, &candidates, 6, over_by(64))), vec![5]);
        assert_eq!(ids(&plan(&policy, &candidates, 6, over_by(65))), vec![5, 4]);
        // 200 MiB over: four tabs in one sweep.
        assert_eq!(
            ids(&plan(&policy, &candidates, 6, over_by(200))),
            vec![5, 4, 3, 2]
        );
        // Far more than there are tabs to free: everything eligible, no
        // panic.
        assert_eq!(
            ids(&plan(&policy, &candidates, 6, over_by(100_000))),
            vec![5, 4, 3, 2, 1]
        );
    }

    #[test]
    fn memory_signal_does_nothing_at_or_under_budget() {
        let policy = SuspensionPolicy {
            memory_budget_bytes: Some(500 * MIB),
            ..policy()
        };
        let candidates = [tab(1, 1), tab(2, 2)];
        for total in [0, 100 * MIB, 500 * MIB] {
            let memory = Some(MemorySample { total_bytes: total });
            assert!(plan(&policy, &candidates, 3, memory).is_empty());
        }
    }

    #[test]
    fn memory_signal_needs_a_fresh_sample() {
        let policy = SuspensionPolicy {
            memory_budget_bytes: Some(1),
            ..policy()
        };
        let candidates = [tab(1, 1), tab(2, 2)];
        // Budget is effectively zero, but with no sample this sweep the
        // memory signal must stay quiet.
        assert!(plan(&policy, &candidates, 3, None).is_empty());
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
            ..policy()
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
        assert_eq!(ids(&plan(&policy, &candidates, 4, memory)), vec![3]);
    }

    // -- combining signals -------------------------------------------------

    #[test]
    fn idle_tabs_count_toward_the_other_signals_demands() {
        let policy = SuspensionPolicy {
            idle_after: Some(Duration::from_secs(100)),
            max_live_tabs: Some(3),
            ..policy()
        };
        // Live = 5, limit 3 -> two must go. Tab 4 is idle anyway, so the
        // tab-count signal only needs one more (the next LRU: tab 2).
        let candidates = [tab(1, 5), tab(2, 50), tab(3, 1), tab(4, 500)];
        let planned = plan(&policy, &candidates, 5, None);
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
            ..policy()
        };
        // Live = 6, limit 4 -> count wants 2. 150 MiB over -> memory wants 3.
        let candidates = [tab(1, 1), tab(2, 2), tab(3, 3), tab(4, 4), tab(5, 5)];
        let memory = Some(MemorySample {
            total_bytes: 650 * MIB,
        });
        let planned = plan(&policy, &candidates, 6, memory);
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
            ..policy()
        };
        // All opened in one burst (equal idle): the caller's order wins,
        // deterministically.
        let candidates = [tab(7, 10), tab(8, 10), tab(9, 10)];
        assert_eq!(ids(&plan(&policy, &candidates, 4, None)), vec![7, 8]);
    }

    #[test]
    fn reason_names_are_stable() {
        assert_eq!(SuspendReason::Idle.as_str(), "idle");
        assert_eq!(SuspendReason::TabCount.as_str(), "tab_count");
        assert_eq!(SuspendReason::Memory.as_str(), "memory");
    }
}
