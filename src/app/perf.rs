//! 性能計測まわり (Issue #13/#59/#62/#66/#67/#69 など): `PerfLog` への
//! 書き出し、起動時間・ページロード時間の集計、RSS サンプラ。
//! `config.perf_metrics` が無効なら、ここの関数はクロックも読まない。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

/// What tab-latency logging needs: where to write records, and the epoch
/// (`process_start`) their `ts_ms` timestamps are relative to. Kept
/// separate from `PerfLog` itself so a `None` here (metrics off) costs
/// nothing beyond the `Option`.
pub(super) struct PerfContext {
    pub(super) process_start: Instant,
    pub(super) log: Arc<PerfLog>,
}

impl PerfContext {
    /// Adapt this context to the shape `ui::window::BrowserWindow` needs
    /// for its own Rust → JS IPC instrumentation (Issue #66) — same
    /// `(Arc<PerfLog>, Instant)` pair, wrapped in the `pub` type `ui::window`
    /// can actually depend on (see `IpcLog`'s doc comment for why this is
    /// not just `PerfContext` reused directly). Called from
    /// `open_new_window`, which only has `state.perf` (not the `ipc_log`
    /// local `run` built before `state` existed) to build a new window's
    /// own `ipc_log` from.
    pub(super) fn to_ipc_log(&self) -> IpcLog {
        IpcLog::new(Arc::clone(&self.log), self.process_start)
    }

    /// `record` を、`now` を `process_start` 起点の経過時間に直して書き出す。
    /// `record_tab_latency`/`record_state_write`/`record_tab_suspend` が
    /// 共通で使う。
    fn write_at(&self, record: &metrics::PerfRecord, now: Instant) {
        self.log
            .write(record, now.saturating_duration_since(self.process_start));
    }
}

/// In-flight page-load timers, one per open tab across every window
/// (`NavigationStarted` inserts/restarts an entry, `LoadStarted` marks its
/// mid-checkpoint — Issue #69 — `LoadFinished` consumes and removes it) —
/// keyed by `(WindowId, TabId)`, not `TabId` alone, since a `TabId` is only
/// unique within its own window (Issue #29/D68).
///
/// **Lifetime (Issue #62).** Every entry must be removed once it stops being
/// useful, or this map grows without bound for the life of the process: both
/// `WindowId` and `TabId` are monotonically increasing counters that are
/// never reused (`browser::tabs::Tabs::take_id`,
/// `browser::windows::Windows::push_window`), so a stale entry left behind
/// by a closed tab is never overwritten by a later one — it just sits there
/// forever. `record_perf_event` removes the entry as soon as its load
/// finishes (the common case); `close_tab`/`close_window_by_tao_id` also
/// remove it on tab/window close so a load abandoned mid-flight (the tab is
/// closed before `LoadFinished` ever arrives) does not leave an orphaned
/// entry either. See docs/decisions.md D79 for the audit that found this.
pub(super) type PageLoadTimers = HashMap<(WindowId, TabId), metrics::PageLoadTimer>;

/// Build the [`PerfLog`] performance events are written through: a file at
/// `config.perf_output_path` if one was requested and could be opened,
/// stderr otherwise. Only called when `config.perf_metrics` is on.
pub(super) fn build_perf_log(config: &Config) -> Arc<PerfLog> {
    match &config.perf_output_path {
        Some(path) => match PerfLog::to_file(config.perf_format, Path::new(path)) {
            Ok(log) => Arc::new(log),
            Err(err) => {
                eprintln!(
                    "velox: failed to open perf output file {path:?}: {err}; \
                     falling back to stderr"
                );
                Arc::new(PerfLog::stderr(config.perf_format))
            }
        },
        None => Arc::new(PerfLog::stderr(config.perf_format)),
    }
}

/// Update startup/page-load metrics state for one [`UserEvent`], writing a
/// [`metrics::PerfRecord`] through `perf_log` whenever a measurement
/// completes. Only called when `config.perf_metrics` is on (see `run`'s
/// event loop), so every branch here is allowed a clock read — the off-path
/// never reaches this function at all.
pub(super) fn record_perf_event(
    startup: &mut Option<metrics::StartupTimestamps>,
    page_load_timers: &mut PageLoadTimers,
    perf_log: &PerfLog,
    process_start: Instant,
    event: &UserEvent,
) {
    match event {
        UserEvent::ToolbarMessage(_, body) => {
            // Issue #66: every toolbar IPC message (JS → Rust), regardless
            // of what it is, is counted/sized/timed here — the one choke
            // point every `window.ipc.postMessage` call already funnels
            // through (`UserEvent::ToolbarMessage`), so this needs no new
            // call site anywhere else. `command_name` is a second, shallow
            // parse of `body` (see its doc comment for why it is not just
            // `parse_command(body).ok().map(...)`): it still labels a
            // message that fails the real parse below, which a metrics
            // consumer wants to see rather than silently lose. Gated by the
            // same `perf_log.is_some()` check every other branch below
            // already runs under (`app::run`'s event loop), so this never
            // reads a clock or allocates on the metrics-off path.
            let started = Instant::now();
            let name = toolbar::command_name(body);
            let parsed = toolbar::parse_command(body);
            // `PerfRecord::ipc`'s `Instant::elapsed()` call happens here,
            // after both the shallow tag read above and the real parse
            // below — so `duration` reflects the full Rust-side cost of
            // turning this message into a `ToolbarCommand`, matching
            // `PerfRecord::Ipc`'s doc comment.
            perf_log.write(
                &metrics::PerfRecord::ipc(metrics::IpcDirection::In, name, body.len(), started),
                started.saturating_duration_since(process_start),
            );
            match parsed {
                Ok(ToolbarCommand::ScriptStarted) => {
                    mark_startup(
                        startup,
                        perf_log,
                        process_start,
                        metrics::StartupTimestamps::mark_toolbar_script_started,
                    );
                }
                Ok(ToolbarCommand::Ready) => {
                    mark_startup(
                        startup,
                        perf_log,
                        process_start,
                        metrics::StartupTimestamps::mark_toolbar_ready,
                    );
                }
                _ => {}
            }
        }
        UserEvent::NavigationStarted(window_id, id, _) => {
            page_load_timers
                .entry((*window_id, *id))
                .or_default()
                .start(Instant::now());
        }
        // Issue #69: the mid-checkpoint that splits `page_load_ms` into a
        // VeloX-side dispatch portion and an engine (black-box, Epic #57
        // rule 3) portion — see `metrics::PageLoadTimer`'s doc comment.
        // Listed before the catch-all arm below, which used to cover this
        // variant as a no-op.
        UserEvent::LoadStarted(window_id, id, _) => {
            if let Some(timer) = page_load_timers.get_mut(&(*window_id, *id)) {
                timer.mark_load_started(Instant::now());
            }
        }
        UserEvent::LoadFinished(window_id, id, url) => {
            let now = Instant::now();
            // Removed, not just looked up (Issue #62/D79): once this load
            // has finished there is nothing left in the entry worth keeping
            // — a later navigation of the same tab recreates it fresh via
            // `NavigationStarted`'s `.entry().or_default()`. Leaving a
            // "finished" entry behind here was the dominant leak this map
            // had: `WindowId`/`TabId` never repeat, so every tab that ever
            // finished loading a page left one entry behind forever.
            if let Some(outcome) = page_load_timers
                .remove(&(*window_id, *id))
                .and_then(|mut timer| timer.finish(now))
            {
                let elapsed = now.saturating_duration_since(process_start);
                perf_log.write(
                    &metrics::PerfRecord::page_load(url.as_str(), outcome.total, outcome.engine),
                    elapsed,
                );
            }
            // The first page to finish anywhere is time-to-first-page; a
            // background tab cannot beat the initial one to it, since it
            // can only be opened after the window is up.
            mark_startup(
                startup,
                perf_log,
                process_start,
                metrics::StartupTimestamps::mark_first_load_finished,
            );
        }
        // The `mark` command (Issue #60) is the one automation command
        // that *is* a perf event of its own: it writes the `measure_start`
        // marker `benchmark::aggregate_trials` cuts each trial at. Listed
        // before the catch-all arm below, which still covers every other
        // `Automation` variant.
        UserEvent::Automation(AutomationCommand::Mark) => {
            let elapsed = Instant::now().saturating_duration_since(process_start);
            perf_log.write(&metrics::PerfRecord::measure_start(), elapsed);
        }
        UserEvent::NavigationBlocked(..)
        | UserEvent::SubresourceBlocked(..)
        | UserEvent::PageTitleResolved { .. }
        | UserEvent::FaviconResolved { .. }
        | UserEvent::OpenDevtoolsRequested(_)
        | UserEvent::ContentShortcut(..)
        | UserEvent::NewTabRequested(..)
        | UserEvent::DownloadStarted { .. }
        | UserEvent::DownloadCompleted { .. }
        // Issue #46's save-page flow is not a perf-tracked operation either
        // (same reasoning as `FindMatchesUpdated` below) — nothing to log
        // here.
        | UserEvent::SavePageStarted { .. }
        | UserEvent::SavePageFinished { .. }
        // `handle_automation_command` calls the same tab-management
        // functions the toolbar path does, which already call
        // `record_tab_latency` themselves — nothing extra to log here.
        | UserEvent::Automation(_)
        // Suspensions the sample leads to are logged by `sweep_tabs`
        // (`record_tab_suspend`); the sample itself is not a perf event
        // (the perf RSS sampler already logs `rss` on its own schedule).
        | UserEvent::MemorySampled(_)
        // Issue #272: fires on every keystroke in a form, so logging it
        // would both flood the perf log and measure the user's typing
        // rate rather than anything VeloX does. `Tab::mark_form_input`
        // is a bool store; there is no duration worth recording.
        | UserEvent::FormInputDetected(..)
        // Issue #43's in-page find is not a perf-tracked operation (no
        // `docs/performance-targets.md` budget calls for it) — nothing to
        // log here.
        | UserEvent::FindMatchesUpdated { .. }
        // Same for Issue #40's PDF export — no performance budget calls
        // for it either.
        | UserEvent::PdfExportFinished { .. }
        // Issue #243's freeze result is not perf-tracked either: what the
        // measurement wants is the RSS and `tab_resume_ms` the existing
        // events already carry, not a timestamp for the COM round-trip.
        | UserEvent::TabFreezeFinished { .. }
        // Same for Issue #45's View Source: no performance budget calls for
        // it either.
        | UserEvent::ViewSourceReady { .. }
        // Issue #39's context menu is not a perf-tracked operation either.
        | UserEvent::ContextMenuRequested { .. }
        | UserEvent::ContextMenuActionSelected { .. }
        | UserEvent::ContextMenuClosed(..)
        // Issue #149: reopening the previous session's extra windows is not
        // a perf-tracked operation. `process_start` → `window_created`
        // (#182) is about the *first* window only, and these deliberately
        // sit outside it (see `UserEvent::RestoreWindows`).
        | UserEvent::RestoreWindows(..) => {}
    }
}

/// Apply one startup-checkpoint mark and, once the full report is
/// available, write it through `perf_log` and clear `startup` so it is only
/// reported once.
pub(super) fn mark_startup(
    startup: &mut Option<metrics::StartupTimestamps>,
    perf_log: &PerfLog,
    process_start: Instant,
    mark: fn(&mut metrics::StartupTimestamps, Instant),
) {
    let Some(timestamps) = startup.as_mut() else {
        return;
    };
    let now = Instant::now();
    mark(timestamps, now);
    if let Some(report) = timestamps.report() {
        let elapsed = now.saturating_duration_since(process_start);
        perf_log.write(&metrics::PerfRecord::startup(report), elapsed);
        *startup = None;
    }
}

/// Log a tab-create/switch latency record if performance metrics are on
/// (`state.perf` is `Some`); a single `Option::is_none` check with no
/// `Instant::now()` call otherwise. `started` is the timestamp the caller
/// captured right before the operation it is timing.
pub(super) fn record_tab_latency(
    state: &AppState,
    kind: metrics::TabLatencyKind,
    id: TabId,
    started: Instant,
) {
    let Some(perf) = &state.perf else {
        return;
    };
    let now = Instant::now();
    let duration = now.saturating_duration_since(started);
    perf.write_at(
        &metrics::PerfRecord::tab_latency(kind, id.get(), duration),
        now,
    );
}

/// Log a `persistence::save_*` disk-write record if performance metrics
/// are on — Issue #67's counterpart to [`record_tab_latency`], same
/// "single `Option::is_none` check when metrics are off" shape. `started`
/// is the timestamp the caller captured right before the `save_*` call
/// (never around the dedup check that may skip it — a skipped write has no
/// disk-I/O cost worth recording).
pub(super) fn record_state_write(
    state: &AppState,
    kind: metrics::StateWriteKind,
    started: Instant,
) {
    let Some(perf) = &state.perf else {
        return;
    };
    let now = Instant::now();
    let duration = now.saturating_duration_since(started);
    perf.write_at(&metrics::PerfRecord::state_write(kind, duration), now);
}

/// Spawn a background thread that periodically samples this process's
/// (and its descendants') RSS and writes it through `log`. Runs for the
/// lifetime of the process; only ever spawned when `config.perf_metrics`
/// and `config.perf_rss_interval` are both set, so it costs nothing
/// otherwise.
pub(super) fn spawn_rss_sampler(interval: Duration, log: Arc<PerfLog>, process_start: Instant) {
    let pid = std::process::id();
    std::thread::spawn(move || {
        // The previous sample and when it was taken, so each pass can turn
        // two cumulative CPU readings into a rate over the interval that
        // actually elapsed (Issue #64). The real elapsed time is used
        // rather than `interval`, because a loaded machine can stretch the
        // sleep and a fixed divisor would then overstate CPU use.
        let mut previous: Option<(metrics::RssSample, Instant)> = None;
        loop {
            match metrics::sample_process_tree_rss(pid) {
                Ok(sample) => {
                    let now = Instant::now();
                    let elapsed = now.saturating_duration_since(process_start);
                    if let Some((prev_sample, prev_at)) = &previous {
                        if let Some(percent) = metrics::PerfRecord::cpu_percent_between(
                            prev_sample,
                            &sample,
                            now.saturating_duration_since(*prev_at),
                        ) {
                            log.write(&metrics::PerfRecord::cpu(percent), elapsed);
                        }
                    }
                    log.write(&metrics::PerfRecord::rss(sample), elapsed);
                    previous = Some((sample, now));
                }
                Err(err) => eprintln!("velox: rss sampling failed: {err}"),
            }
            std::thread::sleep(interval);
        }
    });
}

/// Log a `tab_suspend` perf event (Issue #63) — a no-op when metrics are
/// off, like [`record_tab_latency`].
pub(super) fn record_tab_suspend(state: &AppState, id: TabId, reason: SuspendReason) {
    let Some(perf) = &state.perf else {
        return;
    };
    perf.write_at(
        &metrics::PerfRecord::tab_suspend(id.get(), reason),
        Instant::now(),
    );
}
