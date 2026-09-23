//! タブの自動休止 (Issue #63/#186, docs/decisions.md D90): メモリ
//! サンプラスレッドと、ウィンドウごとの休止スイープ。

use std::time::{Duration, Instant};

use super::*;

/// Spawn the background thread that feeds the automatic suspension
/// policy's memory signal (Issue #63): every `interval`, sample the whole
/// process tree's memory (`metrics::sample_process_tree_rss`, the same
/// `/proc` walk the perf RSS sampler uses) and send it to the main thread
/// as `UserEvent::MemorySampled`. Only ever spawned when
/// `Config::suspension.memory_budget_bytes` is set.
///
/// Which of the sample's totals is compared against the budget is
/// `input`'s call (`MemoryBudgetInput::pick`, Issue #176 Stage 3 /
/// D151 / D152). With the default (`PrivateCommit`, since D152): on
/// Windows the private commit (`PagefileUsage`, D150) is compared, not
/// the working set — §47.9/§47.10 found the working set of a young
/// renderer growing for ~75 s without any allocation, and the budget
/// discarding tabs to pay for that residency. Elsewhere `PrivateCommit`
/// and `Resident` read the same figure: PSS when the platform can read it
/// (Linux with `smaps_rollup`), because that is what the budget is meant
/// to be compared against (`docs/performance-targets.md` §3.1: RSS
/// double-counts shared pages once per process and would put a
/// multi-process browser "over budget" on shared library pages alone),
/// and the RSS total where PSS is unavailable — an over-estimate, so a
/// budget tuned for PSS will suspend slightly earlier there; documented
/// in D56 (the non-Linux Unix `ps` fallback; PSS has no Windows
/// equivalent either, Issue #136 / D88). `VELOX_MEMORY_BUDGET_INPUT=
/// resident` restores the pre-D151 reading — on Windows the working set
/// via `GetProcessMemoryInfo`. On a platform where RSS itself cannot be
/// read either (`RssError::Unsupported` — today, any OS other than Linux,
/// other Unix, or Windows), the failure is logged once and the thread
/// exits: the memory signal is simply inert, and the idle/tab-count
/// signals keep working.
///
/// Exits when the event loop is gone (`send_event` fails), like
/// `spawn_automation`.
pub(super) fn spawn_memory_pressure_sampler(
    interval: Duration,
    input: MemoryBudgetInput,
    proxy: EventLoopProxy<UserEvent>,
) {
    let pid = std::process::id();
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        let sample = match metrics::sample_process_tree_rss(pid) {
            Ok(sample) => sample,
            Err(err) => {
                eprintln!("velox: memory sampling for tab suspension stopped: {err}");
                return;
            }
        };
        let total_bytes = input.pick(
            sample.total_private_bytes,
            sample.total_pss_bytes,
            sample.total_rss_bytes,
        );
        if proxy
            .send_event(UserEvent::MemorySampled(MemorySample { total_bytes }))
            .is_err()
        {
            return;
        }
    });
}

/// Decide which open window's `sweep_tabs` call should consume this pass's
/// fresh memory sample, and what `AppState::next_memory_sample_window`
/// should become for the *next* one (Issue #186, docs/decisions.md D90).
/// Pure and window/webview-independent — `ids` is whatever `Windows::ids()`
/// currently returns (creation order) and `next` is `AppState::
/// next_memory_sample_window` going in — so the round-robin fairness
/// itself is unit-tested without a real window, matching D20's rule for
/// everything else `browser::`/`app.rs`'s policy logic can keep pure.
///
/// Round-robin, not "hand the same sample to every window" and not
/// "always the first window in `ids`": the former would multiply a single
/// over-budget reading into simultaneous suspensions across every open
/// window (over-reclaim — D56's sawtooth behavior amplified by the window
/// count); the latter is the bug this issue fixes — a window with nothing
/// eligible to suspend (a single always-active tab, say) would silently
/// discard every sample forever, starving every *other* window's memory
/// signal no matter how far over budget the whole process tree was, since
/// it is always first. Round-robin guarantees, by construction, both that
/// **at most one window's tabs are suspended per sample** (the caller only
/// ever passes the sample to the one window this returns) and that no
/// window can be permanently skipped (the cursor always advances past
/// whichever window was just served).
///
/// Returns `(served, next_cursor)`. `served` is `None` only when `ids` is
/// empty (cannot happen while the event loop is running — the app exits
/// once `Windows` is empty — handled defensively rather than assumed
/// away); `next_cursor` is left as `next` unchanged in that case (nothing
/// to advance from). Otherwise `served` is the window at `next`'s position
/// in `ids` (or the first window if `next` is `None` or no longer present
/// — self-healing when the previously-served window has since closed),
/// and `next_cursor` is whichever window comes right after it, wrapping
/// around to the front.
pub(super) fn choose_memory_sample_window(
    ids: &[WindowId],
    next: Option<WindowId>,
) -> (Option<WindowId>, Option<WindowId>) {
    if ids.is_empty() {
        return (None, next);
    }
    let start = next
        .and_then(|id| ids.iter().position(|&w| w == id))
        .unwrap_or(0);
    let served = ids[start];
    let next_index = (start + 1) % ids.len();
    (Some(served), Some(ids[next_index]))
}

/// Run the automatic suspension policy once (Issue #63): suspend every
/// background tab [`suspension::plan`] picks as of `now`, then return when
/// the loop should next check again (the soonest a still-awake background
/// tab would cross `idle_after`). Returns `None` when the policy is fully
/// off, the idle signal is off, or there is no background tab to watch —
/// the caller leaves `control_flow` as `Wait` (the tab-count signal is
/// re-evaluated on the next event anyway, and the memory signal wakes the
/// loop itself via `UserEvent::MemorySampled`).
///
/// `memory` is the fresh sample for *this* window's sweep, or `None` when
/// there is no new sample or (Issue #186) this pass's sample was handed to
/// a different window's sweep instead — the caller (`run`'s event loop)
/// decides that once per pass, round-robin, before calling this for every
/// open window; see `AppState::next_memory_sample_window`'s doc comment. A
/// sample never suspends more than one sweep's worth of tabs, in exactly
/// one window.
pub(super) fn sweep_tabs(
    ui_windows: &mut HashMap<WindowId, BrowserWindow>,
    state: &mut AppState,
    window_id: WindowId,
    policy: &SuspensionPolicy,
    memory: Option<MemorySample>,
    now: Instant,
) -> Option<Instant> {
    if !policy.is_enabled() {
        return None;
    }
    let window = ui_windows.get_mut(&window_id)?;
    let tabs = state.windows.tabs(window_id)?;
    let candidates = tabs.suspension_candidates(
        now,
        |id| window.is_playing_audio(id),
        |id| window.process_group_of(id),
    );
    // 機構は「設定値」ではなく「このウィンドウが実際に使うもの」を渡す
    // (D138 決定3)。メモリ信号の算術は、その休止がメモリを返すかどうかに
    // 依存しているため。
    let planned = suspension::plan(policy, &candidates, memory, window.suspend_mechanism());
    if !planned.is_empty() {
        for (id, reason) in planned {
            if suspend_tab(window, window_id, state, id) {
                record_tab_suspend(state, id, reason);
            }
        }
        sync_tab_strip(window, window_id, state);
    }
    let idle_after = policy.idle_after?;
    state
        .windows
        .tabs(window_id)
        .and_then(|tabs| tabs.next_idle_deadline(idle_after))
}

/// Suspend tab `id` on both sides — `Tabs` state first, then the webview
/// (`BrowserWindow::suspend_tab`) — without touching the tab strip; the
/// caller redraws it once it is done (it may be suspending several tabs).
/// Returns whether the tab was actually suspended: `false` for an unknown
/// id, the active tab, an already-suspended tab, or a pinned tab
/// (`Tabs::suspend`'s guards — Issue #277, D144 added the last one), in
/// which case nothing changed. The one implementation behind the tab
/// strip's suspend button (`ToolbarCommand::SuspendTab`), the
/// `suspend <index>` automation command, and [`sweep_tabs`] — so all three
/// inherit the pinned guard from `Tabs::suspend` itself rather than each
/// needing their own check.
pub(super) fn suspend_tab(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    id: TabId,
) -> bool {
    let Some(tabs) = state.windows.tabs_mut(window_id) else {
        return false;
    };
    if !tabs.suspend(id) {
        return false;
    }
    log_failure("suspend tab", window.suspend_tab(id));
    true
}

/// `UserEvent::TabFreezeFinished` (Issue #243) の処理本体。呼び出し側が
/// `window_id` を生きている `window` に解決済みであること (その順序が
/// 重要な理由は呼び出し側のコメント参照) が前提。
pub(super) fn handle_tab_freeze_finished(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    tab_id: TabId,
    success: bool,
    error: Option<String>,
) {
    // The freeze is asynchronous, so by now the tab may have stopped
    // being suspended: the user can click straight back to it
    // between the `TrySuspend` call and its answer, which resumes it
    // in place — and the engine then reports failure *because* the
    // tab became visible again. Decide once, and let both branches
    // below read it, so neither acts on a stale answer.
    let still_suspended = suspension::late_freeze_failure_may_discard(
        tabs_of(state, window_id).get(tab_id).map(Tab::state),
    );
    if !still_suspended {
        // Nothing to do either way, but say so rather than claiming
        // a freeze that no longer describes the tab: these lines are
        // what a measurement reads to tell whether the `freeze` arm
        // actually froze anything (docs/performance-targets.md §37).
        eprintln!(
            "velox: tab {tab_id:?} の freeze 結果 (success={success}) は届いたが、既に休止が解けている (#243)"
        );
        return;
    }
    if success {
        // Logged, not silent: this is the only positive evidence
        // that the `freeze` arm of a measurement actually froze
        // anything. Suspensions are rare enough (tens per
        // benchmark run) that one line each is not noise.
        eprintln!("velox: tab {tab_id:?} を freeze しました (#243)");
        // The engine's own answer, for runs that are debugging the
        // mechanism rather than measuring it — see
        // `BrowserWindow::engine_reports_tab_suspended` for why this
        // is not asked unconditionally.
        if debug_logging_enabled() {
            match window.engine_reports_tab_suspended(tab_id) {
                Some(engine_state) => {
                    eprintln!("velox: tab {tab_id:?} engine IsSuspended={engine_state} (#243)")
                }
                None => {
                    eprintln!("velox: tab {tab_id:?} engine IsSuspended は取得できません (#243)")
                }
            }
        }
        return;
    }
    // WebView2 declined and the tab really is still suspended, so it
    // is marked suspended while holding a live webview. Fall back to
    // what `Discard` would have done rather than leaving it awake —
    // see the variant's docs.
    match error {
        Some(reason) => eprintln!(
            "velox: tab {tab_id:?} の freeze に失敗したため webview を破棄します (#243): {reason}"
        ),
        None => {
            eprintln!("velox: tab {tab_id:?} の freeze に失敗したため webview を破棄します (#243)")
        }
    }
    log_failure(
        "discard frozen tab webview",
        window.discard_tab_webview(tab_id),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- choose_memory_sample_window (Issue #186): round-robin fairness,
    // and the "no window is ever skipped forever" / "at most one window
    // per sample" guarantees it exists for. ---

    #[test]
    fn one_window_is_always_served_regardless_of_cursor() {
        let ids = [WindowId::from(0)];
        assert_eq!(
            choose_memory_sample_window(&ids, None),
            (Some(WindowId::from(0)), Some(WindowId::from(0)))
        );
        // Even a stale/unknown cursor (e.g. a since-closed window's id)
        // still serves the only window that exists.
        assert_eq!(
            choose_memory_sample_window(&ids, Some(WindowId::from(99))),
            (Some(WindowId::from(0)), Some(WindowId::from(0)))
        );
    }

    #[test]
    fn no_windows_serves_nothing_and_leaves_the_cursor_untouched() {
        // Defensive only — the event loop never calls this with an empty
        // `Windows` in practice (the app exits first), but must not panic
        // or silently invent a window id if it ever did.
        assert_eq!(choose_memory_sample_window(&[], None), (None, None));
        let stale = Some(WindowId::from(7));
        assert_eq!(choose_memory_sample_window(&[], stale), (None, stale));
    }

    #[test]
    fn repeated_calls_round_robin_through_every_window_in_order() {
        // Issue #186's core fairness property: starting from "no
        // preference yet" (`None`, e.g. right after startup), successive
        // samples visit every window in turn and cycle back to the start —
        // no window is served twice before every other window has had its
        // turn, and (equally important) every window under budget-pressure
        // eventually gets a sample rather than being starved forever by
        // whichever window happens to be first.
        let ids = [WindowId::from(0), WindowId::from(1), WindowId::from(2)];
        let mut cursor = None;
        let mut served_order = Vec::new();
        for _ in 0..7 {
            let (served, next) = choose_memory_sample_window(&ids, cursor);
            served_order.push(served.expect("non-empty ids always serves someone"));
            cursor = next;
        }
        assert_eq!(
            served_order,
            vec![
                WindowId::from(0),
                WindowId::from(1),
                WindowId::from(2),
                WindowId::from(0),
                WindowId::from(1),
                WindowId::from(2),
                WindowId::from(0),
            ],
            "7 calls over 3 windows must complete two full round-robin \
             cycles plus one extra, in the same fixed order every time"
        );
    }

    #[test]
    fn a_single_call_never_serves_more_than_one_window() {
        // The structural half of "no over-reclaim": whatever `ids` looks
        // like, exactly one window (never zero-with-panic, never more than
        // one) is named per call — the caller passes the sample to that
        // window's `sweep_tabs` alone, so a single over-budget sample can
        // never suspend more than one window's worth of tabs in one pass.
        for window_count in 1..=5 {
            let ids: Vec<WindowId> = (0..window_count).map(WindowId::from).collect();
            let (served, _) = choose_memory_sample_window(&ids, None);
            assert!(
                served.is_some(),
                "{window_count} window(s) must serve exactly one, got None"
            );
        }
    }

    #[test]
    fn a_served_windows_cursor_survives_closing_a_different_window() {
        // Self-healing when the *previously* served window has since
        // closed: the stored cursor no longer appears in `ids`, so the
        // lookup falls back to the front rather than serving no one or
        // panicking.
        let ids = [WindowId::from(0), WindowId::from(2)]; // window 1 closed
        assert_eq!(
            choose_memory_sample_window(&ids, Some(WindowId::from(1))),
            (Some(WindowId::from(0)), Some(WindowId::from(2)))
        );
    }
}
