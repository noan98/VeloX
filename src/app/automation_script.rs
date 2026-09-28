//! `VELOX_AUTOMATION_SCRIPT` の実行 (Issue #112/#169/#173, docs/decisions.md
//! D44/D84/D85): スクリプトを流すバックグラウンドスレッドと、メイン
//! スレッド側でのコマンド処理・`wait_load`/`wait_startup` の待機管理。

use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;

/// State for `AutomationCommand::WaitLoad`/`WaitStartup` (Issue #169/#173,
/// docs/decisions.md D84/D85): at most one wait is ever in flight at a time
/// — the automation thread blocks on each `wait_load`/`wait_startup` (see
/// [`spawn_automation`]) before sending the next command — so a single
/// `Option` slot is enough for either kind. Threaded through the event loop
/// the same way [`PageLoadTimers`] is.
///
/// `pending`, when `Some`, is resolved from exactly two places (per
/// [`AutomationWaitKind`]): `LoadFinished` for the tab a `wait_load` is
/// waiting on ([`resolve_automation_wait_if_matching`], called from
/// [`handle_user_event`]) or `mark_startup` writing the `startup` perf
/// record for a `wait_startup` ([`resolve_automation_wait_for_startup`],
/// called from `run`'s event loop right after `record_perf_event`) — or,
/// regardless of kind, the deadline passing with no such event
/// ([`poll_automation_wait_timeout`], called once per pass through `run`'s
/// event loop, right alongside the existing tab-suspension sweep). Every
/// path clears `pending` before notifying, so the automation thread's
/// blocked `recv()` (in [`spawn_automation`]) always receives exactly one
/// notification per wait — never zero (which would hang it forever) and
/// never more than one for the same command (which would let a later,
/// unrelated wait resolve prematurely on a stale message still sitting in
/// the channel).
pub(super) struct AutomationWaitState {
    pub(super) pending: Option<AutomationWait>,
    /// Wakes the automation thread's blocked `recv()`. The payload carries
    /// nothing — the thread does not need to know *why* it woke, only that
    /// it may proceed; whichever resolution path fired already logged the
    /// reason to stderr on a timeout (see [`poll_automation_wait_timeout`]).
    pub(super) notify: mpsc::Sender<()>,
    /// Set once `mark_startup` has written the `startup` perf record (Issue
    /// #173) — read by `handle_automation_command`'s `WaitStartup` arm so a
    /// `wait_startup` issued *after* that point resolves immediately
    /// instead of registering a pending wait nothing will ever notify again
    /// (mirroring `WaitLoad`'s "already not loading" case). Set from
    /// `run`'s event loop (see [`resolve_automation_wait_for_startup`]),
    /// not from `mark_startup` itself — `mark_startup` lives in
    /// `record_perf_event`, which stays free of any dependency on the
    /// automation machinery, matching how `Wait`/`Mark` are handled
    /// elsewhere in this file. Never reset back to `false`: `mark_startup`
    /// only ever writes the report once per process (it clears `startup`
    /// right after), so once true it stays true for the rest of the run.
    pub(super) startup_reported: bool,
}

impl AutomationWaitState {
    /// 自動化スレッドの `recv()` を起こす。自動化スレッドが既に終わって
    /// いる (受信側が drop 済み) 場合の送信失敗は無視してよい。
    pub(super) fn wake(&self) {
        let _ = self.notify.send(());
    }

    /// 保留中の待機を片付けてから自動化スレッドを起こす — 「待機 1 つに
    /// つき通知はちょうど 1 回」(上記 doc コメント) を守るため、必ず
    /// `pending` を先に消す。
    fn resolve(&mut self) {
        self.pending = None;
        self.wake();
    }

    /// `kind` の待機を、今から `timeout_ms` 後を期限として登録する。
    fn register(&mut self, kind: AutomationWaitKind, timeout_ms: u64) {
        self.pending = Some(AutomationWait {
            kind,
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
        });
    }
}

/// What a pending [`AutomationWait`] is waiting for.
pub(super) enum AutomationWaitKind {
    /// `wait_load` (Issue #169): `tab_id` (in `window_id`) to finish its
    /// current page load.
    Load { window_id: WindowId, tab_id: TabId },
    /// `wait_startup` (Issue #173): the `startup` perf record to be
    /// written — a broader condition than one tab's `LoadFinished`, since
    /// `mark_startup` requires every `metrics::StartupTimestamps`
    /// checkpoint (including the toolbar webview's independent `ready`
    /// handshake) to have landed first.
    Startup,
}

/// One `wait_load`/`wait_startup` currently blocking the automation thread.
pub(super) struct AutomationWait {
    kind: AutomationWaitKind,
    /// When to give up and unblock the automation thread anyway (Issue
    /// #169's "must never hang" requirement) if the awaited event never
    /// arrives — the tab or window closed, the load/startup genuinely never
    /// completes, or any other case a script did not anticipate.
    deadline: Instant,
}

/// Give up on the pending `wait_load` (Issue #169), if any, once its
/// deadline has passed: log why (mirroring the `log_failure` pattern — this
/// is never fatal, the automation thread simply moves on to its next
/// command) and unblock the automation thread. Returns `Some(deadline)`
/// when a wait is still pending and its deadline has *not* yet passed, so
/// the caller (`run`'s event loop tail) can fold it into `next_wake` the
/// same way [`sweep_tabs`]'s return value already is — otherwise the loop
/// would only notice the timeout whenever some unrelated event next woke
/// it, rather than promptly. Returns `None` when nothing is pending, or
/// once this call has just resolved a timed-out one.
///
/// Looking up the tab's current URL for the log line is best-effort: the
/// tab (or its window) may already be gone by the time this fires, in
/// which case the message simply omits it rather than treating a missing
/// tab as its own error.
pub(super) fn poll_automation_wait_timeout(
    automation_wait: &mut AutomationWaitState,
    state: &AppState,
    now: Instant,
) -> Option<Instant> {
    let pending = automation_wait.pending.as_ref()?;
    if now < pending.deadline {
        return Some(pending.deadline);
    }
    match pending.kind {
        AutomationWaitKind::Load { window_id, tab_id } => {
            let url = state
                .windows
                .tabs(window_id)
                .and_then(|tabs| tabs.get(tab_id))
                .map(|tab| tab.current_url().to_owned());
            eprintln!(
                "velox: automation: wait_load はタイムアウトしました \
                 (window={window_id:?}, tab={tab_id:?}, url={url:?})"
            );
        }
        AutomationWaitKind::Startup => {
            eprintln!(
                "velox: automation: wait_startup はタイムアウトしました \
                 (startup perf レコードがまだ書き込まれていません — \
                 VELOX_PERF_METRICS が有効か確認してください)"
            );
        }
    }
    automation_wait.resolve();
    None
}

/// If `wait_load` (Issue #169) is currently blocked waiting on tab `tab_id`
/// in window `window_id`, unblock it: clear the pending wait and notify the
/// automation thread. A no-op when nothing is pending, when a `wait_startup`
/// is pending instead, or when this `LoadFinished` belongs to some other tab
/// — matching the "exactly one notification per wait" invariant
/// [`AutomationWaitState`]'s doc comment describes.
pub(super) fn resolve_automation_wait_if_matching(
    automation_wait: &mut AutomationWaitState,
    window_id: WindowId,
    tab_id: TabId,
) {
    let matches = automation_wait.pending.as_ref().is_some_and(|pending| {
        matches!(
            pending.kind,
            AutomationWaitKind::Load { window_id: w, tab_id: t } if w == window_id && t == tab_id
        )
    });
    if matches {
        automation_wait.resolve();
    }
}

/// Mark the `startup` perf record as written (Issue #173) and, if a
/// `wait_startup` is currently blocked waiting for exactly that, unblock it:
/// clear the pending wait and notify the automation thread. Called from
/// `run`'s event loop the moment it observes `startup` transition from
/// `Some` to `None` (i.e. `mark_startup` just wrote the record) — see that
/// call site's comment for why the transition is detected there rather than
/// threading `automation_wait` into `record_perf_event`/`mark_startup`.
/// `startup_reported` is set unconditionally (even when nothing is
/// currently pending) so a `wait_startup` issued later in the script still
/// resolves immediately in `handle_automation_command` instead of
/// registering a wait nothing will ever notify again.
pub(super) fn resolve_automation_wait_for_startup(automation_wait: &mut AutomationWaitState) {
    automation_wait.startup_reported = true;
    let matches = matches!(
        automation_wait
            .pending
            .as_ref()
            .map(|pending| &pending.kind),
        Some(AutomationWaitKind::Startup)
    );
    if matches {
        automation_wait.resolve();
    }
}

/// Dispatch one step of a `VELOX_AUTOMATION_SCRIPT` (Issue #112, see
/// docs/decisions.md D44 and `browser::automation`) to the same tab
/// operations `handle_toolbar_command`/`handle_content_shortcut` already
/// use — every branch here mirrors an existing `ToolbarCommand`/
/// `ContentShortcut` arm, exactly like `handle_content_shortcut` itself
/// mirrors `handle_toolbar_command`. `Open`/`Close`/`Switch` address a tab
/// by its position in the tab strip (0-based, matching what a benchmark
/// script author sees on screen), resolved against `window_id`'s `Tabs`
/// (see `tab_id_at`) right here — since by the time this runs, tabs may
/// have been opened/closed
/// since the script was parsed, resolving late (rather than up front) is
/// the only way position `2` reliably means "the third tab, right now".
/// An out-of-range position is a silent no-op (eprintln'd), never a panic
/// or a crash — matching every other `Tabs`/`BrowserWindow` guard in this
/// file.
///
/// `AutomationCommand::Wait` never reaches here (the automation thread
/// sleeps locally instead of sending an event — see `spawn_automation`)
/// and `AutomationCommand::Quit` is intercepted in `run`'s event loop
/// before dispatch (it needs `ControlFlow`, which this function does not
/// have); both arms are still written out explicitly, rather than folded
/// into a wildcard, so a future new `AutomationCommand` variant fails to
/// compile here instead of silently doing nothing.
///
/// `AutomationCommand::WaitLoad` (Issue #169) *does* reach here, unlike
/// `Wait` — see its own match arm and `AutomationWaitState`'s doc comment
/// for why it needs a round trip through the main thread's state instead of
/// a local sleep: whether the active tab is still loading is state only the
/// main thread has (`Tab::is_loading`), and the automation thread must
/// itself stay blocked (via `automation_wait_rx.recv()` in
/// `spawn_automation`) rather than racing ahead — which is also exactly why
/// this cannot become a synchronous, blocking wait *inside* this function:
/// this function runs on the main thread, the same one that would have to
/// deliver the `LoadFinished` this wait is waiting on, so blocking it here
/// would deadlock. `AutomationCommand::WaitStartup` (Issue #173) is the same
/// shape, checking `automation_wait.startup_reported` (set elsewhere, from
/// `run`'s event loop — see `resolve_automation_wait_for_startup`) instead
/// of `Tab::is_loading`.
pub(super) fn handle_automation_command(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    page_load_timers: &mut PageLoadTimers,
    automation_wait: &mut AutomationWaitState,
    command: AutomationCommand,
) {
    match command {
        AutomationCommand::Open { url } => open_new_tab(window, window_id, state, &url),
        AutomationCommand::Navigate { url } => navigate_active_tab(window, window_id, state, &url),
        AutomationCommand::Switch { index } => match tab_id_at(state, window_id, index) {
            Some(id) => activate_tab(window, window_id, state, id),
            None => eprintln!("velox: automation: switch {index} は範囲外です"),
        },
        AutomationCommand::Close { index } => match tab_id_at(state, window_id, index) {
            Some(id) => close_tab(window, window_id, state, page_load_timers, id),
            None => eprintln!("velox: automation: close {index} は範囲外です"),
        },
        AutomationCommand::Suspend { index } => match tab_id_at(state, window_id, index) {
            Some(id) => {
                if suspend_tab(window, window_id, state, id) {
                    sync_tab_strip(window, window_id, state);
                }
                // Otherwise the active or an already-suspended tab: a
                // no-op, exactly like `ToolbarCommand::SuspendTab`.
            }
            None => eprintln!("velox: automation: suspend {index} は範囲外です"),
        },
        // Issue #169: register (or immediately resolve) the wait — see
        // `AutomationWaitState`'s doc comment for how this gets unblocked
        // again. `Tabs::active()` is always `Some`-equivalent (a `Tabs`
        // always has at least one tab), so there is no "no tab" case to
        // handle here, unlike `Switch`/`Close`/`Suspend` above.
        AutomationCommand::WaitLoad { timeout_ms } => {
            let tab_id = tabs_of(state, window_id).active_id();
            if tabs_of(state, window_id).active().is_loading() {
                automation_wait
                    .register(AutomationWaitKind::Load { window_id, tab_id }, timeout_ms);
            } else {
                // Already finished (or never started) loading — resolve
                // immediately, matching the issue's "already loaded ->
                // proceed at once" requirement.
                automation_wait.wake();
            }
        }
        // Issue #173: same shape as `WaitLoad` above, but the condition it
        // polls is `automation_wait.startup_reported` (set from `run`'s
        // event loop the moment `mark_startup` writes the `startup` record
        // — see `resolve_automation_wait_for_startup`) rather than
        // `Tab::is_loading` — there is no single tab/window this wait is
        // scoped to, unlike `WaitLoad`.
        AutomationCommand::WaitStartup { timeout_ms } => {
            if automation_wait.startup_reported {
                automation_wait.wake();
            } else {
                automation_wait.register(AutomationWaitKind::Startup, timeout_ms);
            }
        }
        // The marker is a perf-log record only (`record_perf_event` has
        // already written it by the time dispatch gets here); there is no
        // browser state to change.
        AutomationCommand::Mark => {}
        AutomationCommand::Wait { .. } | AutomationCommand::Quit => {}
        // Intercepted in `handle_user_event` before it ever reaches here —
        // opening a window (unlike every other automation command) is not
        // scoped to "the current window" in the first place, and changes
        // *which* window `automation_window` points at afterwards. See
        // `browser::AutomationCommand::NewWindow`'s doc comment.
        AutomationCommand::NewWindow | AutomationCommand::NewPrivateWindow => {}
    }
}

/// The [`TabId`] currently at tab-strip position `index` (0-based) in window
/// `window_id`, or `None` if `index` is out of range — the shared lookup
/// `handle_automation_command`'s `Switch`/`Close`/`Suspend` arms use to turn
/// a script's positional index into the `TabId` every other tab operation
/// in this file addresses tabs by.
pub(super) fn tab_id_at(state: &mut AppState, window_id: WindowId, index: usize) -> Option<TabId> {
    tabs_of(state, window_id)
        .iter()
        .nth(index)
        .map(|tab| tab.id())
}

/// Spawn the background thread that drives one parsed
/// `VELOX_AUTOMATION_SCRIPT` (Issue #112). Walks `commands` in order:
/// `AutomationCommand::Wait` sleeps this thread (never blocking the main
/// thread, which keeps servicing the webview/UI the whole time);
/// `AutomationCommand::WaitLoad`/`WaitStartup` (Issue #169/#173) send their
/// event exactly like every other command below, then additionally block
/// this thread on `automation_wait_rx` until the main thread resolves them
/// (see `AutomationWaitState`'s doc comment) — still never the main thread,
/// so the webview/UI keeps being serviced while a script waits on a load or
/// on startup; every other command is proxied into the event loop as
/// `UserEvent::Automation`, processed on the main thread exactly like any
/// other `UserEvent` (see docs/decisions.md D44). `EventLoopProxy::send_event`
/// is the same fire-and-forget channel every webview callback already uses
/// to reach the main thread (`ui/window.rs`) — nothing new is introduced
/// here beyond one more sender.
///
/// If the event loop has already gone away (the window was closed before
/// the script finished), `send_event` starts failing and this thread exits
/// early rather than spinning forever. The same is true of
/// `automation_wait_rx.recv()`: once the main thread drops its `Sender`
/// (the event loop is gone), `recv()` returns an error immediately instead
/// of blocking forever.
pub(super) fn spawn_automation(
    proxy: EventLoopProxy<UserEvent>,
    commands: Vec<AutomationCommand>,
    automation_wait_rx: mpsc::Receiver<()>,
) {
    std::thread::spawn(move || {
        for command in commands {
            match command {
                AutomationCommand::Wait { ms } => {
                    std::thread::sleep(Duration::from_millis(ms));
                }
                command @ (AutomationCommand::WaitLoad { .. }
                | AutomationCommand::WaitStartup { .. }) => {
                    if proxy.send_event(UserEvent::Automation(command)).is_err() {
                        return;
                    }
                    if automation_wait_rx.recv().is_err() {
                        return;
                    }
                }
                other => {
                    if proxy.send_event(UserEvent::Automation(other)).is_err() {
                        return;
                    }
                }
            }
        }
    });
}
