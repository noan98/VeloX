//! Benchmark aggregation and comparison — the "Measure / Compare" half of
//! Issue #14 (see `docs/benchmarking.md` for the full methodology this
//! module implements, and `src/bin/velox-bench.rs` for the runner that
//! actually launches VeloX and feeds its output through here).
//!
//! Everything in this file is pure Rust with no process spawning, no
//! filesystem access and no WebView dependency, so it is fully exercised by
//! `cargo test` in a headless environment — unlike the runner binary, which
//! needs a real display to launch VeloX and therefore cannot be verified
//! here (see the runner's module doc comment).
//!
//! Pieces:
//!
//! - [`parse_jsonl`] — turns the JSON Lines produced by
//!   `browser::perf_log`/`browser::metrics` (`VELOX_PERF_FORMAT=json`) into
//!   [`serde_json::Value`]s, tolerating blank lines and lines that fail to
//!   parse (a shared stderr stream interleaves plain `velox: ...`
//!   diagnostics with perf records; see `docs/architecture.md`, "Output
//!   format and destination").
//! - [`MetricKey`] — the fixed set of scalar metrics this project measures,
//!   matching `PerfRecord`'s event/field schema one-to-one, plus
//!   [`MetricKey::extract`] to pull every matching value out of one trial's
//!   events.
//! - [`Stats`] / [`compute_stats`] — count/min/max/mean/median/p95/stddev
//!   over a set of samples, satisfying the "中央値またはp95等の代表値を算出
//!   できる" acceptance criterion.
//! - [`aggregate_trials`] — merges N trials' worth of events into one
//!   [`Stats`] per [`MetricKey`], satisfying "同一条件で複数回実行できる".
//! - [`RunEnvironment`] / [`BenchmarkResult`] — the machine-readable,
//!   serde-serializable result shape saved to disk, carrying OS/CPU/commit
//!   metadata alongside the aggregated metrics.
//! - [`compare`] — diffs two saved [`BenchmarkResult`]s metric-by-metric,
//!   satisfying "結果を保存して過去版と比較できる" and giving Issue #36 a
//!   pass/fail signal ([`ComparisonReport::any_regressed`]).
//! - [`scenario`] — the canonical scenario identifiers (cold/warm startup,
//!   navigation, tab create/switch, and the 1/5/10/20/50-tab memory/CPU
//!   scenarios), shared by the runner and by this module's own tests so the
//!   scenario list has one source of truth.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------
// JSON Lines parsing
// ---------------------------------------------------------------------

/// Parse `text` as JSON Lines, keeping only lines that (a) parse as JSON and
/// (b) look like a `PerfRecord` (have a string `"event"` field). Blank
/// lines, non-JSON diagnostic lines, and truncated/corrupted lines (e.g. a
/// log file whose last line was cut off mid-write) are silently skipped
/// rather than failing the whole parse — a benchmark run should not lose 99
/// good records over 1 bad one.
pub fn parse_jsonl(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str::<Value>(trimmed).ok()
        })
        .filter(|value| value.get("event").and_then(Value::as_str).is_some())
        .collect()
}

// ---------------------------------------------------------------------
// Metric extraction
// ---------------------------------------------------------------------

/// The fixed set of scalar metrics this project can extract from perf log
/// events. Mirrors `PerfRecord::event_name()`/`to_json`'s field schema
/// (`docs/architecture.md`, "Output format and destination") one-to-one; a
/// new event kind added there needs a matching variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetricKey {
    /// tao's event loop has been built — see
    /// `metrics::StartupTimestamps::mark_event_loop_built` and
    /// docs/decisions.md D92 (Issue #182). First of four checkpoints
    /// splitting `process_start` → [`MetricKey::StartupWindowCreatedMs`]
    /// into toolkit init / VeloX's own setup / native window / first
    /// webview. Absent (never a fabricated `0`) from a result file produced
    /// by a pre-#182 build, same rule as [`MetricKey::PssTotalBytes`].
    StartupEventLoopMs,
    /// Every Rust-side prerequisite of the first window is loaded and
    /// `BrowserWindow::new` is about to be called — see
    /// `metrics::StartupTimestamps::mark_pre_window_setup_done` and D92.
    /// **The only span of `process_start` → `window_created` that is
    /// VeloX's own code**; the three around it are tao/engine time that
    /// Epic #57 rule 3 treats as a black box.
    StartupPreWindowSetupMs,
    /// tao's `WindowBuilder::build()` has returned — see
    /// `metrics::StartupTimestamps::mark_native_window_built` and D92.
    StartupNativeWindowMs,
    /// The toolbar (first) webview has been attached — see
    /// `metrics::StartupTimestamps::mark_toolbar_webview_built` and D92.
    /// The span before it carries the engine's one-time initialization
    /// (WebView2 environment creation on Windows), which the content
    /// webview built right afterwards does not pay again.
    StartupToolbarWebviewMs,
    StartupWindowCreatedMs,
    /// Right before `app::run` enters the event loop — see
    /// `metrics::StartupTimestamps::mark_rust_setup_done` and
    /// docs/decisions.md D43 (Issue #59). Splits `StartupWindowCreatedMs` →
    /// `StartupToolbarReadyMs` into "VeloX's own synchronous setup" vs.
    /// "inside the toolbar webview".
    StartupRustSetupDoneMs,
    /// The toolbar webview's inline script started executing — see
    /// `metrics::StartupTimestamps::mark_toolbar_script_started` and D43.
    StartupToolbarScriptStartedMs,
    StartupToolbarReadyMs,
    StartupFirstLoadMs,
    PageLoadMs,
    /// `LoadStarted` → `LoadFinished` portion of one `page_load` event
    /// (Issue #69, `docs/decisions.md` D87) — the engine's own resource
    /// loading/parsing/rendering, black-box per Epic #57 rule 3. Absent
    /// from a trial's aggregated metrics for any `page_load` event whose
    /// `LoadStarted` never fired, same "no fabricated 0" rule as
    /// [`MetricKey::PssTotalBytes`].
    PageLoadEngineMs,
    /// `NavigationStarted` → `LoadStarted` portion of the same event.
    /// **Not a VeloX-attributable duration on its own** — see
    /// `metrics::PageLoadTimer`'s doc comment and D87: `LoadStarted` fires
    /// only once the engine has connected/sent the request/started
    /// receiving the response, so this bundles VeloX's own event handling
    /// with real engine/network time. Same absence rule as
    /// [`MetricKey::PageLoadEngineMs`].
    PageLoadDispatchMs,
    TabCreateMs,
    TabSwitchMs,
    /// Restore cost of a suspended tab (Issue #63): `tab_resume` events'
    /// `duration_ms` — switching to a suspended tab, i.e. rebuilding its
    /// webview and making it visible. The page reload that follows is
    /// counted under [`MetricKey::PageLoadMs`] as usual.
    TabResumeMs,
    RssTotalBytes,
    RssProcessCount,
    /// PSS total (Issue #108 / D42): `None`/absent in the source `rss`
    /// event's `total_pss_bytes` field (unsupported platform, old kernel,
    /// permissions) yields no sample for this key, same as any other metric
    /// that never fired — see [`MetricKey::extract`]. Prefer this over
    /// [`MetricKey::RssTotalBytes`] whenever comparing memory footprint
    /// across builds/browsers with different process counts: RSS double
    /// counts shared pages once per process, so it is not comparable across
    /// process counts (`docs/performance-targets.md` §3.1).
    PssTotalBytes,
    /// How many processes contributed to [`MetricKey::PssTotalBytes`] in
    /// the same sample, out of [`MetricKey::RssProcessCount`] total —
    /// compare the two to tell a complete PSS total from a partial one.
    PssProcessCount,
    /// CPU utilization of the whole process tree between two consecutive
    /// RSS samples, as a percentage of one core (Issue #64): 100 is one
    /// core fully busy. The metric background-tab work is judged by — see
    /// `docs/decisions.md` D58.
    CpuPercent,
    /// How many `tab_suspend` events (Issue #63,
    /// `metrics::PerfRecord::TabSuspend`) fired during a trial — answers
    /// D97 Revisit condition (4) / Issue #197's latest comment: a figure
    /// like 616.8 MiB (D97 §28.5/§28.8) says nothing on its own about how
    /// many background tabs automatic suspension had to drop to get there.
    /// See `docs/decisions.md` D105 for the full design rationale.
    ///
    /// **Unlike every other key, this counts matching events instead of
    /// reading a numeric field out of them** — `tab_suspend` carries only
    /// `tab_id`/`reason` (dropping a webview is synchronous, so there is no
    /// duration to read; see `PerfRecord::TabSuspend`'s doc comment). See
    /// [`MetricKey::extract`] for how "1 trial -> 1 sample" is upheld for
    /// this key, and why a trial with zero suspensions is a real,
    /// meaningful `0` rather than an absent metric like
    /// [`MetricKey::PssTotalBytes`] (D105).
    ///
    /// **Counted over the whole trial, not the "measured phase" the rest of
    /// this project's memory metrics use** — see
    /// [`MetricKey::counts_whole_trial`] for why `tabs_hold_N`'s own design
    /// (D97) makes that necessary here specifically.
    ///
    /// This says nothing about the *cost* of a suspension: how long a
    /// resumed tab took to become usable again is
    /// [`MetricKey::TabResumeMs`] (a separate, already-existing metric,
    /// Issue #63), and the user-facing impact of losing a tab's in-memory
    /// state (scroll position, form input, unsaved JS state) is not
    /// quantified by either metric — see D105's "まだ分からないこと".
    SuspendedTabCount,
}

impl MetricKey {
    /// Every metric key, in a stable order — used to build a
    /// [`BenchmarkResult::metrics`] map deterministically and to drive
    /// [`aggregate_trials`].
    pub const ALL: [MetricKey; 21] = [
        MetricKey::StartupEventLoopMs,
        MetricKey::StartupPreWindowSetupMs,
        MetricKey::StartupNativeWindowMs,
        MetricKey::StartupToolbarWebviewMs,
        MetricKey::StartupWindowCreatedMs,
        MetricKey::StartupRustSetupDoneMs,
        MetricKey::StartupToolbarScriptStartedMs,
        MetricKey::StartupToolbarReadyMs,
        MetricKey::StartupFirstLoadMs,
        MetricKey::PageLoadMs,
        MetricKey::PageLoadEngineMs,
        MetricKey::PageLoadDispatchMs,
        MetricKey::TabCreateMs,
        MetricKey::TabSwitchMs,
        MetricKey::TabResumeMs,
        MetricKey::RssTotalBytes,
        MetricKey::RssProcessCount,
        MetricKey::PssTotalBytes,
        MetricKey::PssProcessCount,
        MetricKey::CpuPercent,
        MetricKey::SuspendedTabCount,
    ];

    /// The key's name as stored in [`BenchmarkResult::metrics`] and printed
    /// by `velox-bench compare`.
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKey::StartupEventLoopMs => "startup_event_loop_ms",
            MetricKey::StartupPreWindowSetupMs => "startup_pre_window_setup_ms",
            MetricKey::StartupNativeWindowMs => "startup_native_window_ms",
            MetricKey::StartupToolbarWebviewMs => "startup_toolbar_webview_ms",
            MetricKey::StartupWindowCreatedMs => "startup_window_created_ms",
            MetricKey::StartupRustSetupDoneMs => "startup_rust_setup_done_ms",
            MetricKey::StartupToolbarScriptStartedMs => "startup_toolbar_script_started_ms",
            MetricKey::StartupToolbarReadyMs => "startup_toolbar_ready_ms",
            MetricKey::StartupFirstLoadMs => "startup_first_load_ms",
            MetricKey::PageLoadMs => "page_load_ms",
            MetricKey::PageLoadEngineMs => "page_load_engine_ms",
            MetricKey::PageLoadDispatchMs => "page_load_dispatch_ms",
            MetricKey::TabCreateMs => "tab_create_ms",
            MetricKey::TabSwitchMs => "tab_switch_ms",
            MetricKey::TabResumeMs => "tab_resume_ms",
            MetricKey::RssTotalBytes => "rss_total_bytes",
            MetricKey::RssProcessCount => "rss_process_count",
            MetricKey::PssTotalBytes => "pss_total_bytes",
            MetricKey::PssProcessCount => "pss_process_count",
            MetricKey::CpuPercent => "cpu_percent",
            MetricKey::SuspendedTabCount => "suspended_tab_count",
        }
    }

    /// Reverse of [`MetricKey::as_str`] — looks a key up by the name it is
    /// stored under in [`BenchmarkResult::metrics`]. Used by
    /// [`evaluate_gate`] to find each metric's [`MetricKey::min_significant_delta`]
    /// from the string-keyed maps that [`compare`]/[`evaluate_gate`] both
    /// work over. `None` for any name not in [`MetricKey::ALL`] (e.g. a
    /// result file from a newer VeloX with metrics this build does not know
    /// about) — callers fall back to a conservative default in that case.
    pub fn from_metric_name(name: &str) -> Option<MetricKey> {
        MetricKey::ALL.into_iter().find(|key| key.as_str() == name)
    }

    /// The smallest absolute `|candidate_median - baseline_median|` worth
    /// treating as a real change at all, regardless of `pct_change`
    /// (Issue #72 / D46). This is a *different* safeguard than the
    /// warn/fail percentage thresholds in [`GateThresholds`]: it exists so
    /// that a metric whose baseline is naturally tiny (e.g. `page_load_ms`
    /// on a fast fixture, or a `pct_change` computed off a near-zero
    /// baseline) cannot swing its percentage into the hundreds from a
    /// change of a few milliseconds — measured directly: a same-binary
    /// `page_load_ms` comparison swung +78.9% off nothing more than a
    /// 16.2ms absolute change (D46). The percentage thresholds separately
    /// absorb this environment's *session-to-session* noise on
    /// larger-magnitude metrics; this floor absorbs noise at the
    /// *opposite* end of the scale (small metrics, small absolute deltas).
    pub fn min_significant_delta(self) -> f64 {
        match self {
            MetricKey::StartupEventLoopMs
            | MetricKey::StartupPreWindowSetupMs
            | MetricKey::StartupNativeWindowMs
            | MetricKey::StartupToolbarWebviewMs
            | MetricKey::StartupWindowCreatedMs
            | MetricKey::StartupRustSetupDoneMs
            | MetricKey::StartupToolbarScriptStartedMs
            | MetricKey::StartupToolbarReadyMs
            | MetricKey::StartupFirstLoadMs
            | MetricKey::PageLoadMs
            | MetricKey::PageLoadEngineMs
            | MetricKey::PageLoadDispatchMs
            | MetricKey::TabCreateMs
            | MetricKey::TabSwitchMs
            | MetricKey::TabResumeMs => 20.0, // milliseconds
            MetricKey::RssTotalBytes | MetricKey::PssTotalBytes => 5.0 * 1024.0 * 1024.0, // 5 MiB
            MetricKey::RssProcessCount | MetricKey::PssProcessCount => 1.0, // whole processes
            // 5 percentage points of one core. Below that, the difference
            // between two runs on a shared machine is scheduling noise.
            MetricKey::CpuPercent => 5.0,
            // 1 whole tab — but **not for the same reason as the timing
            // metrics above.** Those floors exist to absorb *measurement
            // noise* (a real quantity that jitters run to run even with no
            // code change). A suspended-tab count has no such noise: it is
            // an exact integer count of discrete events
            // ([`MetricKey::extract`]), not a sampled continuous quantity,
            // so there is no sub-1 "noise" to filter. The floor here exists
            // only so a change of *less than one whole tab* — which cannot
            // physically happen but could in principle appear from a
            // hand-edited/foreign result file — is never treated as
            // significant, same reasoning as the process-count keys just
            // above.
            MetricKey::SuspendedTabCount => 1.0,
        }
    }

    /// The `PerfRecord` `"event"` value this metric is read from.
    fn event_name(self) -> &'static str {
        match self {
            MetricKey::StartupEventLoopMs
            | MetricKey::StartupPreWindowSetupMs
            | MetricKey::StartupNativeWindowMs
            | MetricKey::StartupToolbarWebviewMs
            | MetricKey::StartupWindowCreatedMs
            | MetricKey::StartupRustSetupDoneMs
            | MetricKey::StartupToolbarScriptStartedMs
            | MetricKey::StartupToolbarReadyMs
            | MetricKey::StartupFirstLoadMs => "startup",
            MetricKey::PageLoadMs | MetricKey::PageLoadEngineMs | MetricKey::PageLoadDispatchMs => {
                "page_load"
            }
            MetricKey::TabCreateMs => "tab_create",
            MetricKey::TabSwitchMs => "tab_switch",
            MetricKey::TabResumeMs => "tab_resume",
            MetricKey::RssTotalBytes
            | MetricKey::RssProcessCount
            | MetricKey::PssTotalBytes
            | MetricKey::PssProcessCount => "rss",
            MetricKey::CpuPercent => "cpu",
            MetricKey::SuspendedTabCount => "tab_suspend",
        }
    }

    /// The JSON field this metric's numeric value lives in, within a
    /// matching event.
    fn field_name(self) -> &'static str {
        match self {
            MetricKey::StartupEventLoopMs => "event_loop_ms",
            MetricKey::StartupPreWindowSetupMs => "pre_window_setup_ms",
            MetricKey::StartupNativeWindowMs => "native_window_ms",
            MetricKey::StartupToolbarWebviewMs => "toolbar_webview_ms",
            MetricKey::StartupWindowCreatedMs => "window_created_ms",
            MetricKey::StartupRustSetupDoneMs => "rust_setup_done_ms",
            MetricKey::StartupToolbarScriptStartedMs => "toolbar_script_started_ms",
            MetricKey::StartupToolbarReadyMs => "toolbar_ready_ms",
            MetricKey::StartupFirstLoadMs => "first_load_ms",
            MetricKey::PageLoadMs => "duration_ms",
            MetricKey::PageLoadEngineMs => "engine_duration_ms",
            MetricKey::PageLoadDispatchMs => "dispatch_duration_ms",
            MetricKey::TabCreateMs => "duration_ms",
            MetricKey::TabSwitchMs => "duration_ms",
            MetricKey::TabResumeMs => "duration_ms",
            MetricKey::RssTotalBytes => "total_rss_bytes",
            MetricKey::RssProcessCount => "process_count",
            MetricKey::PssTotalBytes => "total_pss_bytes",
            MetricKey::PssProcessCount => "pss_process_count",
            // The `cpu` event names its own field simply `percent`; the
            // metric is called `cpu_percent` to stay unambiguous in a
            // result file that also carries byte and millisecond metrics.
            MetricKey::CpuPercent => "percent",
            // Never reached: `extract` special-cases `SuspendedTabCount`
            // before it gets here, because `tab_suspend` events carry no
            // numeric field worth reading — the value this key reports is
            // the *count* of matching events, not a field inside one. Kept
            // as a real (panicking) arm rather than folded into a
            // catch-all so this match stays exhaustive and self-documenting
            // if a future variant is added.
            MetricKey::SuspendedTabCount => unreachable!(
                "SuspendedTabCount is counted by MetricKey::extract's own \
                 branch, not read from a field — see MetricKey::extract"
            ),
        }
    }

    /// Pull every value for this metric out of `events` (one trial's worth
    /// of parsed [`parse_jsonl`] output). `tab_create`/`tab_switch` share
    /// the `"tab_create"`/`"tab_switch"` event names respectively (not both
    /// named `"tab_latency"`), so no extra disambiguation is needed beyond
    /// matching on `event_name()`.
    ///
    /// [`MetricKey::PssTotalBytes`] rides the same `.and_then(Value::as_f64)`
    /// as every other key: a JSON `null` `total_pss_bytes` (an `rss` sample
    /// where PSS could not be read at all) parses to `None` and is dropped
    /// here, same as a field that is simply absent — so a trial where PSS
    /// was never available ends up with zero samples for this key, and
    /// [`aggregate_trials`] omits it from the result entirely rather than
    /// reporting a misleading `0.0`.
    ///
    /// [`MetricKey::SuspendedTabCount`] does not fit that "read a field"
    /// shape at all (D105, Issue #197 revisit condition (4)) and gets its
    /// own branch:
    ///
    /// - **Why one sample per trial, not one per event.** Every other key's
    ///   natural multiplicity is "however many matching events the trial
    ///   produced" (e.g. several `rss` samples). Following that same
    ///   pattern here — one sample per `tab_suspend` event, all fixed at
    ///   the value `1.0` — would silently change what [`Stats::count`]
    ///   means for this key alone: a single 20-tab trial that suspends 16
    ///   tabs would contribute *16* samples to [`compute_stats`], making it
    ///   look like 16 trials' worth of data sat next to every other
    ///   metric's "N samples = N trials" count in the same aggregated
    ///   result. Counting once per trial keeps that invariant, and gives
    ///   the natural answer to the question this key exists to answer:
    ///   "how many tabs did *this trial* suspend".
    /// - **Why a trial with zero matches is a real `0`, not an absent
    ///   metric — the opposite of the [`MetricKey::PssTotalBytes`] rule
    ///   just above.** That rule protects a measurement that *could not be
    ///   taken* (unsupported platform, a field that failed to parse) from
    ///   being reported as a confirmed zero. Here the failure mode does not
    ///   exist the same way: `tab_suspend` is written by
    ///   `app::record_tab_suspend` only when perf metrics are on, and it is
    ///   a no-op — never a zero-valued record — otherwise. So a trial whose
    ///   events are non-empty (i.e. metrics genuinely ran) but contains no
    ///   `tab_suspend` at all did not "fail to measure" anything: it
    ///   measured, and the answer was zero (e.g. every tab stayed under the
    ///   memory budget). Reporting that as an *absent* key would make "this
    ///   config never suspends anything" indistinguishable from "this
    ///   build predates the feature" — exactly the ambiguity the "no
    ///   fabricated 0" rule exists to prevent for the other keys, just
    ///   pointed the other way.
    /// - **Why an entirely empty `events` slice is still treated as
    ///   absent**, matching every other key: a trial that produced zero
    ///   perf records of *any* kind (metrics were off, or the process
    ///   crashed before writing anything) did fail to measure, and must
    ///   not report a fabricated "0 tabs suspended" alongside a result set
    ///   where every other key is correctly missing.
    pub fn extract(self, events: &[Value]) -> Vec<f64> {
        if self == MetricKey::SuspendedTabCount {
            if events.is_empty() {
                return Vec::new();
            }
            let suspended = events
                .iter()
                .filter(|event| {
                    event.get("event").and_then(Value::as_str) == Some(self.event_name())
                })
                .count();
            return vec![suspended as f64];
        }
        events
            .iter()
            .filter(|event| event.get("event").and_then(Value::as_str) == Some(self.event_name()))
            .filter_map(|event| event.get(self.field_name()).and_then(Value::as_f64))
            .collect()
    }

    /// Whether [`MetricKey::extract`] should be given **the whole trial**
    /// rather than only its "measured phase" (the slice after the last
    /// `measure_start` marker that [`measured_phase`] cuts to, and that
    /// [`aggregate_trials`] passes to every other key). `true` only for
    /// [`MetricKey::SuspendedTabCount`] (D105).
    ///
    /// This is not an arbitrary exception: it is forced by how
    /// `tabs_hold_N` (`Scenario::TabCountMemoryHold`, D97) — the scenario
    /// this key exists to annotate — is deliberately shaped.
    /// `tabs_hold_N` waits `automation::MEMORY_HOLD_SETTLE_MS` (12s, twice
    /// the 5s default memory-check period) **before** marking, precisely so
    /// the memory-pressure sampler's suspension sweep(s) land before the
    /// marker rather than inside the measured window
    /// (`docs/performance-targets.md` §27.5). And a tab that is already
    /// suspended is never reconsidered (§25.3), so by the time
    /// `measure_start` fires, the very suspensions this key exists to
    /// count have almost always already happened. Cutting to the measured
    /// phase the same way every other key does would read back at or near
    /// zero for this key on nearly every trial — hiding the exact number
    /// Issue #197's latest comment asks for, on the one scenario built to
    /// answer it.
    ///
    /// Every duration/byte metric, by contrast, genuinely wants the
    /// "settled" window: `tab_create_ms` averaged over 20 warm-up creations
    /// plus the measured ones would blur two different tab counts together
    /// (the reason [`measured_phase`] exists at all, Issue #60). This key
    /// is cumulative rather than a settled-state snapshot, so it wants the
    /// opposite: everything the trial did, not just its final window.
    fn counts_whole_trial(self) -> bool {
        matches!(self, MetricKey::SuspendedTabCount)
    }
}

// ---------------------------------------------------------------------
// Summary statistics
// ---------------------------------------------------------------------

/// Summary statistics over one metric's samples. `median`/`p95` are the
/// representative values the acceptance criteria ask for; `mean`/`stddev`
/// and `min`/`max` are kept alongside for context when reviewing a result
/// file by hand.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub median: f64,
    pub p95: f64,
    pub stddev: f64,
}

/// Compute [`Stats`] over `values`. `None` for an empty slice — an absent
/// metric (e.g. a scenario that never triggers a `tab_switch`) should be
/// absent from the result, not a row of zeroes that would be
/// indistinguishable from a genuinely instantaneous operation.
pub fn compute_stats(values: &[f64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = sorted.len();
    let min = sorted[0];
    let max = sorted[count - 1];
    let mean = sorted.iter().sum::<f64>() / count as f64;
    let median = percentile(&sorted, 50.0);
    let p95 = percentile(&sorted, 95.0);
    let stddev = if count > 1 {
        let variance =
            sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (count as f64 - 1.0);
        variance.sqrt()
    } else {
        0.0
    };
    Some(Stats {
        count,
        min,
        max,
        mean,
        median,
        p95,
        stddev,
    })
}

/// Linear-interpolation percentile over an already-sorted, non-empty slice
/// (the "closest ranks, interpolated" method — same shape as numpy's
/// default `linear` interpolation, chosen so results are reproducible
/// against a well-known convention rather than an ad hoc one).
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (pct / 100.0) * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        let frac = rank - lower as f64;
        sorted[lower] + (sorted[upper] - sorted[lower]) * frac
    }
}

// ---------------------------------------------------------------------
// Multi-trial aggregation
// ---------------------------------------------------------------------

/// Merge `trials` (one `Vec<Value>` of parsed events per trial/run) into one
/// [`Stats`] per [`MetricKey`] that has at least one sample across all
/// trials. Values from every trial are pooled before computing statistics
/// (rather than averaging per-trial statistics), so `median`/`p95` reflect
/// the full sample distribution across repeated runs — the "同一条件で複数回
/// 実行できる" + "中央値またはp95等の代表値を算出できる" acceptance
/// criteria together.
pub fn aggregate_trials(trials: &[Vec<Value>]) -> BTreeMap<String, Stats> {
    let measured: Vec<&[Value]> = trials.iter().map(|trial| measured_phase(trial)).collect();
    let mut out = BTreeMap::new();
    for key in MetricKey::ALL {
        let mut values = Vec::new();
        // [`MetricKey::counts_whole_trial`] (D105): almost every key wants
        // the post-warm-up `measured` slice, but a cumulative
        // whole-trial-count key like `SuspendedTabCount` wants the
        // unmodified trial instead — see that method's doc comment.
        for (trial, trial_measured) in trials.iter().zip(&measured) {
            let source: &[Value] = if key.counts_whole_trial() {
                trial
            } else {
                trial_measured
            };
            values.extend(key.extract(source));
        }
        if let Some(stats) = compute_stats(&values) {
            out.insert(key.as_str().to_owned(), stats);
        }
    }
    out
}

/// The part of one trial's events that counts as measurement: everything
/// after the **last** `measure_start` marker (the `mark` automation
/// command, Issue #60), or the whole trial when there is none.
///
/// This is what lets a scenario have a warm-up phase. `tab_create_20`, for
/// instance, opens 20 tabs before it measures anything; without the cut,
/// those 20 setup `tab_create` events — taken at 1, 2, 3 … tabs — would be
/// pooled with the 8 samples actually taken at 20 tabs, and the median
/// would describe neither. Scenarios that emit no marker (every scenario
/// before this one) are unaffected, which is why the fallback is "keep
/// everything" rather than "keep nothing".
///
/// The *last* marker wins, so a script may mark more than once (each one
/// discarding what came before) without the aggregate silently keeping the
/// earliest phase.
fn measured_phase(trial: &[Value]) -> &[Value] {
    match trial
        .iter()
        .rposition(|event| event.get("event").and_then(Value::as_str) == Some("measure_start"))
    {
        Some(index) => &trial[index + 1..],
        None => trial,
    }
}

// ---------------------------------------------------------------------
// IPC traffic summary (Issue #66)
// ---------------------------------------------------------------------

/// One `(direction, name)` pair's aggregated `ipc` events (Issue #66,
/// `metrics::PerfRecord::Ipc`): how many messages, how many total raw JSON
/// bytes, and the distribution of `duration_ms` (the Rust-side
/// `parse_command`/`evaluate_script` cost only — never JS execution or DOM
/// work; see `PerfRecord::Ipc`'s doc comment). Built by [`summarize_ipc`],
/// printed as a table (or saved as JSON) by `velox-bench ipc-summary` — see
/// docs/benchmarking.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcSummary {
    /// `"in"` (JS → Rust, a `ToolbarCommand`) or `"out"` (Rust → JS, one
    /// `ui::window::BrowserWindow::eval_toolbar` call site).
    pub direction: String,
    /// The `ToolbarCommand`'s `cmd` tag (`In`) or the `eval_toolbar` call
    /// site's label, e.g. `"set_tabs"` (`Out`).
    pub name: String,
    pub count: usize,
    pub total_bytes: u64,
    pub duration_ms: Stats,
}

/// Aggregate every `ipc` event in `events` (already-flattened
/// [`parse_jsonl`] output — pass `.chain`ed/`.extend`ed events from as many
/// perf-log files as needed; unlike [`aggregate_trials`] there is no
/// per-trial warm-up cut here, since a message a real user's IPC traffic
/// includes is not "warm-up" the way a benchmark scenario's setup tabs
/// are) into one [`IpcSummary`] row per `(direction, name)` pair.
///
/// Sorted by `total_bytes` descending, then `count` descending: "what
/// dominates this channel's *traffic*" is the question this exists to
/// answer, ahead of a single message's worst-case size (already visible
/// per row as `duration_ms.max`) or an alphabetical listing that hides
/// which rows actually matter — the "高頻度イベントの特定" acceptance
/// criterion Issue #66 asks for.
///
/// Pure and unit-tested like the rest of this module — no process
/// spawning, no filesystem, no WebView. `velox-bench ipc-summary` is the IO
/// layer around this (reads `--input` files, prints/saves the table).
pub fn summarize_ipc(events: &[Value]) -> Vec<IpcSummary> {
    let mut by_key: BTreeMap<(String, String), (usize, u64, Vec<f64>)> = BTreeMap::new();
    for event in events {
        if event.get("event").and_then(Value::as_str) != Some("ipc") {
            continue;
        }
        let direction = event
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let name = event
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let bytes = event.get("bytes").and_then(Value::as_u64).unwrap_or(0);
        let duration_ms = event
            .get("duration_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let entry = by_key
            .entry((direction, name))
            .or_insert((0, 0, Vec::new()));
        entry.0 += 1;
        entry.1 += bytes;
        entry.2.push(duration_ms);
    }
    let mut rows: Vec<IpcSummary> = by_key
        .into_iter()
        .filter_map(|((direction, name), (count, total_bytes, durations))| {
            compute_stats(&durations).map(|duration_ms| IpcSummary {
                direction,
                name,
                count,
                total_bytes,
                duration_ms,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.total_bytes
            .cmp(&a.total_bytes)
            .then_with(|| b.count.cmp(&a.count))
    });
    rows
}

// ---------------------------------------------------------------------
// Memory-scenario sample-count confidence (Issue #119, D50)
// ---------------------------------------------------------------------

/// The minimum number of pooled `pss_total_bytes`/`rss_total_bytes` samples
/// [`memory_sample_confidence`] expects *per trial* for a
/// [`scenario::Scenario::TabCountMemory`] scenario's aggregated metric to be
/// trustworthy. [`crate::browser::automation::recommended_rss_interval_ms`]
/// paces the sampler to land about four samples inside the fixed settle
/// window alone, so two per trial is a conservative floor with real margin
/// for scheduling jitter — not a tight bound tuned to just barely pass. A trial that
/// contributed fewer than this (in the limit, the pre-fix bug: exactly one
/// sample, taken before any tab had opened) means the sampler most likely
/// missed the "all tabs open" state this scenario exists to measure.
pub const MIN_RSS_SAMPLES_PER_TRIAL: usize = 2;

/// Whether a scenario's aggregated PSS/RSS metrics were sampled densely
/// enough, across `trials` trials, to trust as "tabs finished opening"
/// figures — see [`MIN_RSS_SAMPLES_PER_TRIAL`] and docs/decisions.md D50.
///
/// Only [`scenario::Scenario::TabCountMemory`] is checked
/// ([`Self::NotApplicable`] for everything else, including the three
/// startup scenarios and `navigation`/`tab_create`/`tab_switch`, none of
/// which claim to measure a "settled" memory state): those scenarios pass
/// `VELOX_PERF_METRICS=1` too, so they always produce *some* `rss` samples,
/// but a low count there says nothing about their own (non-memory) metrics
/// being unreliable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySampleConfidence {
    /// `scenario` is not a memory scenario; this check does not apply.
    NotApplicable,
    /// At least `required` pooled PSS/RSS samples were observed.
    Sufficient { observed: usize, required: usize },
    /// Fewer than `required` pooled PSS/RSS samples were observed — the
    /// scenario's `pss_total_bytes`/`rss_total_bytes` figures may reflect a
    /// process that had not finished opening its tabs yet, not the
    /// "N tabs open" state the scenario name promises. Per D42's existing
    /// rule against ever turning "could not measure" into a silent zero,
    /// the caller must not present these numbers as reliable without
    /// surfacing this — see `docs/decisions.md` D50 and
    /// `docs/benchmarking.md` for how `velox-bench run`/`aggregate` do so
    /// (a stderr warning and a non-zero exit code, not a dropped metric —
    /// the samples that *were* taken are still real data, just too few to
    /// trust as this scenario's answer).
    Insufficient { observed: usize, required: usize },
}

impl MemorySampleConfidence {
    /// `true` for [`Self::Insufficient`] — the one variant a caller needs to
    /// act on.
    pub fn is_insufficient(self) -> bool {
        matches!(self, MemorySampleConfidence::Insufficient { .. })
    }
}

/// Evaluate [`MemorySampleConfidence`] for one aggregated result. `metrics`
/// is normally [`BenchmarkResult::metrics`] (or the map [`aggregate_trials`]
/// just built, before it is wrapped in one) — keyed exactly the way that map
/// is, so this can run against either a freshly aggregated result or one
/// just read back from disk.
///
/// Prefers [`MetricKey::PssTotalBytes`]'s sample count when present (the
/// metric this project recommends comparing across builds, D42) and falls
/// back to [`MetricKey::RssTotalBytes`]'s when PSS could not be read at all
/// in this environment (old kernel, permissions, non-Linux) — RSS is always
/// present alongside PSS in the same `rss` event
/// ([`crate::browser::metrics::RssSample`]'s field docs), so the two
/// metrics' sample counts are identical whenever both exist; this only
/// matters when PSS is entirely absent. A scenario with *neither* metric at
/// all (e.g. every trial failed to spawn) is `observed: 0`, which is always
/// [`MemorySampleConfidence::Insufficient`] for a non-zero `required`.
pub fn memory_sample_confidence(
    scenario: scenario::Scenario,
    trials: u32,
    metrics: &BTreeMap<String, Stats>,
) -> MemorySampleConfidence {
    if !matches!(scenario, scenario::Scenario::TabCountMemory(_)) {
        return MemorySampleConfidence::NotApplicable;
    }
    let required = trials as usize * MIN_RSS_SAMPLES_PER_TRIAL;
    let observed = metrics
        .get(MetricKey::PssTotalBytes.as_str())
        .or_else(|| metrics.get(MetricKey::RssTotalBytes.as_str()))
        .map(|stats| stats.count)
        .unwrap_or(0);
    if observed >= required {
        MemorySampleConfidence::Sufficient { observed, required }
    } else {
        MemorySampleConfidence::Insufficient { observed, required }
    }
}

// ---------------------------------------------------------------------
// Saved result shape
// ---------------------------------------------------------------------

/// Execution environment metadata saved alongside every [`BenchmarkResult`]
/// — required by the acceptance criteria ("実行環境情報 (OS、CPUコア数、
/// VeloXのgit commit、実行日時、試行回数)") and by the parent Epic's "OSごと
/// に結果を分ける" rule, since WebView backend and memory behavior differ by
/// platform (`docs/decisions.md` D1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEnvironment {
    /// `std::env::consts::OS` (`"linux"`, `"macos"`, `"windows"`, ...).
    pub os: String,
    /// Logical CPU count as seen by the runner process.
    pub cpu_count: usize,
    /// `git rev-parse HEAD` of the VeloX checkout the benchmarked binary
    /// was built from, when known. `None` when the runner could not
    /// determine it (e.g. running against a binary copied out of a git
    /// checkout, or `git` unavailable).
    pub git_commit: Option<String>,
    /// When this result was produced, as an RFC 3339 timestamp. A plain
    /// string (not a `SystemTime`) so the saved JSON is human-readable and
    /// has no timezone-handling surprises on deserialization.
    pub generated_at: String,
    /// Number of trials this result was aggregated from.
    pub trials: u32,
    /// CPU のモデル名（例: "AMD EPYC 9V74 80-Core Processor"）。Issue #211。
    /// `windows-latest` ランナーが run ごとに異なる機種を割り当てること
    /// (`docs/decisions.md` D96) が判明したため、`results/
    /// environment-info.md` に別立てで記録するだけでは結果 JSON と機械的に
    /// 突き合わせられない。`None`（未取得）かつ `#[serde(default)]` なのは、
    /// このフィールドが存在しない過去の結果ファイル（`results/baseline/`
    /// や `results/history/` の JSONL）のデシリアライズを壊さないため
    /// (`docs/decisions.md` D104)。
    #[serde(default)]
    pub cpu_model: Option<String>,
    /// 物理メモリの総量（バイト）。`None`/`#[serde(default)]` の理由は
    /// `cpu_model` と同じ (D104)。
    #[serde(default)]
    pub total_memory_bytes: Option<u64>,
    /// OS のバージョン/ビルド（例: Windows なら "10.0.26100"）。
    /// `None`/`#[serde(default)]` の理由は `cpu_model` と同じ (D104)。
    #[serde(default)]
    pub os_version: Option<String>,
    /// WebView ランタイムのバージョン（Windows なら WebView2 Runtime の
    /// `pv` 値）。`None`/`#[serde(default)]` の理由は `cpu_model` と同じ
    /// (D104)。
    #[serde(default)]
    pub webview_runtime: Option<String>,
}

/// One scenario's aggregated benchmark result — the unit this project saves
/// to disk (machine-readable JSON) and compares across versions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    /// Scenario identifier, e.g. `"cold_startup"`, `"tabs_5"` — see
    /// [`scenario::Scenario::id`].
    pub scenario: String,
    pub environment: RunEnvironment,
    /// One entry per [`MetricKey`] that had at least one sample, keyed by
    /// [`MetricKey::as_str`].
    pub metrics: BTreeMap<String, Stats>,
}

// ---------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------

/// One metric's baseline-vs-candidate comparison, using each side's
/// `median` as the representative value (p95 is more sensitive to the tail
/// noise a shared CI runner tends to add; median is kept as the primary
/// regression signal, with both full [`Stats`] still available in the
/// [`BenchmarkResult`]s themselves for anyone who wants to look closer).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricDiff {
    pub baseline_median: f64,
    pub candidate_median: f64,
    /// `candidate_median - baseline_median`.
    pub delta: f64,
    /// `delta / baseline_median * 100`. When `baseline_median` is `0.0`,
    /// defined as `0.0` if `candidate_median` is also `0.0` (no change),
    /// otherwise `f64::INFINITY` (any nonzero value is an infinite relative
    /// increase off a zero baseline) — never a division-by-zero `NaN`.
    pub pct_change: f64,
    /// `true` when `pct_change` exceeds the caller's threshold. Every
    /// metric this project currently measures (durations, RSS bytes) is
    /// "lower is better", so a regression is always an *increase*.
    pub regressed: bool,
}

fn diff_metric(baseline_median: f64, candidate_median: f64, threshold_pct: f64) -> MetricDiff {
    let delta = candidate_median - baseline_median;
    let pct_change = if baseline_median == 0.0 {
        if candidate_median == 0.0 {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        delta / baseline_median * 100.0
    };
    MetricDiff {
        baseline_median,
        candidate_median,
        delta,
        pct_change,
        regressed: pct_change > threshold_pct,
    }
}

/// Full comparison between two [`BenchmarkResult`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub baseline_scenario: String,
    pub candidate_scenario: String,
    pub threshold_pct: f64,
    /// Per-metric diffs, keyed the same as [`BenchmarkResult::metrics`].
    /// Only metrics present in *both* results are compared; a metric that
    /// only exists on one side (e.g. a newly added measurement) is omitted
    /// here rather than guessed at.
    pub diffs: BTreeMap<String, MetricDiff>,
    /// Metric names present in only one of the two results, for visibility
    /// (not treated as a regression either way).
    pub only_in_baseline: Vec<String>,
    pub only_in_candidate: Vec<String>,
    /// `true` if any [`MetricDiff::regressed`] is `true` — the single
    /// pass/fail signal Issue #36's CI regression check can key off of
    /// (`velox-bench compare` exits non-zero exactly when this is `true`).
    pub any_regressed: bool,
}

/// Compare `candidate` against `baseline`. `threshold_pct` is the maximum
/// allowed percentage increase (e.g. `5.0` for "flag anything more than 5%
/// slower/heavier"); use `0.0` to flag any increase at all, or a negative
/// number to also flag ties as regressions (rarely useful, but not
/// rejected — the arithmetic does not require a positive threshold).
pub fn compare(
    baseline: &BenchmarkResult,
    candidate: &BenchmarkResult,
    threshold_pct: f64,
) -> ComparisonReport {
    let mut diffs = BTreeMap::new();
    let mut only_in_baseline = Vec::new();
    let mut only_in_candidate = Vec::new();

    for (name, baseline_stats) in &baseline.metrics {
        match candidate.metrics.get(name) {
            Some(candidate_stats) => {
                diffs.insert(
                    name.clone(),
                    diff_metric(baseline_stats.median, candidate_stats.median, threshold_pct),
                );
            }
            None => only_in_baseline.push(name.clone()),
        }
    }
    for name in candidate.metrics.keys() {
        if !baseline.metrics.contains_key(name) {
            only_in_candidate.push(name.clone());
        }
    }
    only_in_baseline.sort();
    only_in_candidate.sort();

    let any_regressed = diffs.values().any(|diff| diff.regressed);

    ComparisonReport {
        baseline_scenario: baseline.scenario.clone(),
        candidate_scenario: candidate.scenario.clone(),
        threshold_pct,
        diffs,
        only_in_baseline,
        only_in_candidate,
        any_regressed,
    }
}

// ---------------------------------------------------------------------
// Regression gate (Issue #72 / D46)
// ---------------------------------------------------------------------
//
// `compare` above answers "did anything change between two saved results,
// by how much". The gate built on top of it answers the different,
// CI-shaped question Issue #72 asks: "should this PR be blocked". Those are
// not the same question in this environment, because a *single*
// baseline-vs-candidate `compare` is dominated by session-to-session noise,
// not code changes — see the module-level numbers below.
//
// **Measured noise this design is calibrated against** — two experiments,
// both on the identical `cold_startup` binary/commit with no code change
// in between (full data and the exact commands in `docs/decisions.md` D46):
//
// 1. *Adjacent-set noise*: six consecutive 10-trial runs, each compared
//    only to the one immediately before it (the gap a same-CI-job
//    baseline/candidate pair would realistically see):
//
//    | metric                     | max adjacent-pair swing | peak-to-peak across all 6 |
//    |-----------------------------|--------------------------|------------------------------|
//    | `startup_first_load_ms`     | 19.0%                     | 45.0%                         |
//    | `startup_toolbar_ready_ms`  | 16.0%                     | 42.5%                         |
//    | `startup_window_created_ms` | 17.4%                     | 31.9%                         |
//    | `pss_total_bytes`           | 28.8%                     | 43.4%                         |
//    | `rss_total_bytes`           | 19.6%                     | 23.5%                         |
//
// 2. *Wide-gap noise*: comparing the first and last of those same six runs
//    directly against each other (a ~9-minute gap, standing in for what a
//    stale committed baseline file looks like against a run measured much
//    later) surfaced far larger swings on the very same unchanged binary:
//    `page_load_ms` +77.0%/+78.9%, `startup_toolbar_ready_ms`
//    +52.2%/+53.4%, `startup_first_load_ms` +49.2%/+52.4%. This is *why*
//    Issue #72's CI gate must never block on a comparison against an old,
//    separately-captured baseline file (`docs/performance-targets.md`'s own
//    rule: never compare numbers from different sessions/machines) — the
//    blocking gate in this repo's CI workflow always measures its baseline
//    (the PR's merge-base commit) and its candidate (the PR head commit)
//    back-to-back in the same job, keeping the gap closer to case 1 above
//    than case 2.
//
// A single fixed threshold cannot both (a) sit below this noise floor and
// (b) catch a real regression, so `evaluate_gate` combines several
// independent mitigations instead of one bigger number:
//
// 1. **Two severity tiers** ([`Severity`]) with different thresholds: a low
//    `warn_pct` surfaces anything unusual for a human to glance at (this
//    environment's noise routinely reaches it), while a much higher
//    `fail_pct` — above every adjacent-pair swing measured above, with a
//    smaller but still positive margin over the worst *wide-gap* swings
//    seen on unrelated (non-floor-guarded) metrics — is reserved for
//    changes large enough that noise alone is an implausible explanation
//    for a same-job comparison.
// 2. **A minimum absolute delta** ([`MetricKey::min_significant_delta`]),
//    orthogonal to the percentage tiers: guards metrics with a naturally
//    tiny baseline (`page_load_ms`'s wide-gap 78.9% swing above was a
//    16.2ms absolute change) from a huge `pct_change` computed off noise.
// 3. **Majority vote across repeated candidate runs**: `evaluate_gate`
//    accepts more than one candidate [`BenchmarkResult`] (e.g. two runs of
//    the same PR commit) and only escalates a metric to [`Severity::Fail`]
//    when *more than half* of the candidates independently exceed
//    `fail_pct` against the same baseline — approximating "N consecutive
//    worse" within a single CI job rather than across PR history. A metric
//    that low-confidence data touched (see below) is never escalated past
//    [`Severity::Warn`], however many candidates agree.
//
// **Residual risk, stated plainly**: no fixed threshold fully absorbs the
// wide-gap noise this environment can produce (`startup_toolbar_ready_ms`'s
// measured 53.4% is only ~7 points under the default 60% `fail_pct`, and
// nothing in this module can distinguish that from a genuine 53%
// regression). The mitigation is architectural — always compare same-job,
// back-to-back measurements — not purely statistical; a `Fail` verdict with
// no plausible code cause should be treated as "re-run the gate" before
// "revert the PR", the same way a flaky test is handled.
//
// **Insufficient trial count**: a [`Stats::count`] below
// [`MIN_TRIALS_FOR_CONFIDENT_GATE`] on either side of a comparison marks
// that pairing `low_confidence` and caps its severity at
// [`Severity::Warn`] — a `Fail` verdict should never rest on a median of
// (say) one or two samples.
//
// **Baseline gaps**: a metric present in the candidate(s) but absent from
// the baseline (including a baseline with no metrics at all — e.g. a run
// that collected zero records) cannot be evaluated and is reported in
// [`GateReport::only_in_candidates`] rather than guessed at; the reverse
// (baseline-only) goes in [`GateReport::only_in_baseline`]. A
// candidate-only metric is a normal thing for a PR to introduce, so it
// does not contribute to [`GateReport::overall`]; a *baseline*-only one is
// not (see the input preconditions below).
//
// **Input preconditions** (Issue #196): every mitigation above assumes the
// two sides are actually comparable. `evaluate_gate` therefore checks that
// assumption itself rather than trusting its caller, and records each
// violation as a [`GateInputProblem`] that feeds [`GateReport::overall`]
// alongside the per-metric verdicts:
//
// - **Scenario mismatch / OS mismatch** ([`Severity::Fail`]): comparing
//   `cold_startup` against `tabs_20`, or a Linux result against a Windows
//   one, is meaningless — Epic #57's absolute rule 5 ("record results per
//   OS") and `docs/performance-targets.md` §10 both say so. Before this
//   check, `gate` happily compared whatever metric names the two files
//   happened to share, so a mistyped `--baseline` path produced a
//   confident-looking verdict off unrelated numbers.
// - **A candidate with no metrics at all** ([`Severity::Fail`]): an empty
//   candidate cannot regress against anything, so every baseline metric
//   fell into `only_in_baseline` and `overall` came out
//   [`Severity::Ok`] — a silent pass for a run that measured nothing.
// - **No comparable metrics at all** ([`Severity::Fail`]): same failure
//   mode one level up. A gate that compared zero metrics must not report
//   `Ok`; there is no evidence either way.
// - **Baseline metrics missing from every candidate**
//   ([`Severity::Warn`], i.e. non-blocking): a metric the baseline
//   measured and no candidate did is either an incomplete candidate run or
//   a deliberately removed metric. Both are worth a human's glance, but
//   neither is reliably a regression, so this stops at `Warn`.
//
// The CI workflow (`.github/workflows/perf-gate.yml`) already passed
// same-OS, same-scenario files, so none of these fire there today. They
// exist because `gate` is a general-purpose CLI that anyone can point at
// two arbitrary result files, and "OK" from a regression gate has to mean
// "measured and compared", not "found nothing to compare".

/// A regression gate's verdict for one metric, or for a whole
/// [`GateReport`] (as the worst of its metrics' verdicts).
///
/// Ordered `Ok < Warn < Fail` so `Iterator::max` over a set of per-metric
/// severities gives the correct overall verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// No metric change worth a human's attention.
    Ok,
    /// Non-blocking: worth surfacing in a PR summary, but within the range
    /// this environment's session-to-session noise alone can produce.
    Warn,
    /// Blocking: large enough, and (when multiple candidates were given)
    /// consistent enough, that noise is an implausible sole explanation.
    Fail,
}

/// A trial count below this, on either side of a comparison, is not enough
/// to trust a median at all — the comparison is still reported (never
/// silently dropped) but capped at [`Severity::Warn`] and flagged
/// `low_confidence`. `velox-bench run`/`aggregate` default to 10 trials
/// (`docs/benchmarking.md`); this is deliberately well below that default
/// so a slightly short run still gates normally, while a pathologically
/// small one (e.g. every trial but one failed to spawn) does not.
pub const MIN_TRIALS_FOR_CONFIDENT_GATE: usize = 5;

/// A violation of one of [`evaluate_gate`]'s input preconditions — see the
/// module-level "Input preconditions" notes (Issue #196).
///
/// These sit *beside* the per-metric verdicts rather than replacing them:
/// a report with a `ScenarioMismatch` still carries whatever metric
/// comparisons were computable, so a human reading the report can see both
/// the numbers and the reason they must not be trusted. Making
/// `evaluate_gate` return `Err` instead would have thrown that context
/// away, and would have made "the gate could not run" indistinguishable
/// from "the gate crashed" in `velox-bench`'s exit codes (`2` is reserved
/// for argument/IO errors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GateInputProblem {
    /// The baseline and a candidate measured different scenarios.
    ScenarioMismatch {
        /// Index into `evaluate_gate`'s `candidates` slice, so a caller
        /// with several `--candidate` files can tell which one is wrong.
        candidate_index: usize,
        baseline: String,
        candidate: String,
    },
    /// The baseline and a candidate were measured on different operating
    /// systems (Epic #57 absolute rule 5).
    OsMismatch {
        candidate_index: usize,
        baseline: String,
        candidate: String,
    },
    /// A candidate result carries no metrics at all — an empty or failed
    /// run, which must never read as "no regression".
    CandidateWithoutMetrics { candidate_index: usize },
    /// Not one metric could be compared (an empty baseline, empty
    /// candidates, or two results with no metric names in common).
    NoComparableMetrics,
    /// Metrics the baseline measured that no candidate measured. Same list
    /// as [`GateReport::only_in_baseline`], surfaced here so it reaches
    /// [`GateReport::overall`].
    MetricsMissingFromCandidates { metrics: Vec<String> },
}

impl GateInputProblem {
    /// How much this problem should weigh on [`GateReport::overall`]. See
    /// the module-level "Input preconditions" notes for why the missing
    /// metrics case stops at [`Severity::Warn`] while the rest are
    /// [`Severity::Fail`].
    pub fn severity(&self) -> Severity {
        match self {
            GateInputProblem::MetricsMissingFromCandidates { .. } => Severity::Warn,
            _ => Severity::Fail,
        }
    }

    /// A one-line human-readable description, in the same language as the
    /// rest of `velox-bench`'s output.
    pub fn describe(&self) -> String {
        match self {
            GateInputProblem::ScenarioMismatch {
                candidate_index,
                baseline,
                candidate,
            } => format!(
                "candidate[{candidate_index}] のシナリオが baseline と異なります \
                 (baseline={baseline} / candidate={candidate})"
            ),
            GateInputProblem::OsMismatch {
                candidate_index,
                baseline,
                candidate,
            } => format!(
                "candidate[{candidate_index}] の OS が baseline と異なります \
                 (baseline={baseline} / candidate={candidate}) — \
                 OS をまたぐ比較は成立しません"
            ),
            GateInputProblem::CandidateWithoutMetrics { candidate_index } => format!(
                "candidate[{candidate_index}] にメトリクスが 1 件もありません \
                 (計測に失敗した結果ファイルの可能性があります)"
            ),
            GateInputProblem::NoComparableMetrics => {
                "baseline と candidate に共通するメトリクスが 1 件もないため、\
                 比較は行われていません"
                    .to_owned()
            }
            GateInputProblem::MetricsMissingFromCandidates { metrics } => format!(
                "baseline にはあるが candidate に無いメトリクス: {}",
                metrics.join(", ")
            ),
        }
    }
}

/// Percentage thresholds for [`evaluate_gate`]. See the module-level "Regression
/// gate" section above for how these were chosen relative to this
/// environment's measured noise.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GateThresholds {
    /// A `pct_change` strictly greater than this is [`Severity::Warn`] (and
    /// candidate for [`Severity::Fail`] if it also clears `fail_pct` with
    /// majority agreement). Default 20.0: at or below every measured
    /// adjacent-pair swing for the three startup-timing metrics
    /// (16.0–19.0%) and RSS (19.6%), so it also fires — correctly, as a
    /// *non-blocking* notice — on ordinary noise in those metrics; PSS's
    /// adjacent-pair swing (28.8%) clears it too, which is exactly the
    /// point of a warn tier.
    pub warn_pct: f64,
    /// A `pct_change` strictly greater than this, in *more than half* of
    /// the supplied candidates, is [`Severity::Fail`]. Default 60.0: a
    /// ~31 percentage point margin above the largest adjacent-pair swing
    /// measured in this environment (28.8%, PSS), and a real but
    /// deliberately narrower ~7 point margin above the largest *wide-gap*
    /// swing measured on a metric the absolute-delta floor does not shield
    /// (`startup_toolbar_ready_ms` at 53.4% — see the module docs' "Residual
    /// risk" note). Wider-gap comparisons are exactly what this repo's CI
    /// gate is designed to avoid by measuring baseline and candidate in the
    /// same job.
    pub fail_pct: f64,
}

impl Default for GateThresholds {
    fn default() -> Self {
        GateThresholds {
            warn_pct: 20.0,
            fail_pct: 60.0,
        }
    }
}

/// One metric's regression-gate verdict: the baseline compared against
/// every supplied candidate, majority-voted into a single [`Severity`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateMetricVerdict {
    pub baseline_median: f64,
    pub baseline_count: usize,
    /// One entry per candidate that had this metric, in the order the
    /// candidates were supplied.
    pub candidate_medians: Vec<f64>,
    pub pct_changes: Vec<f64>,
    /// Per-candidate severity, before the majority vote and the
    /// `low_confidence` cap that produce [`Self::severity`].
    pub per_candidate_severity: Vec<Severity>,
    /// `true` if the baseline or *any* contributing candidate had fewer
    /// than [`MIN_TRIALS_FOR_CONFIDENT_GATE`] trials — see the module-level
    /// docs. When `true`, [`Self::severity`] is never [`Severity::Fail`].
    pub low_confidence: bool,
    /// The final, majority-voted, low-confidence-capped verdict for this
    /// metric.
    pub severity: Severity,
}

/// The full result of [`evaluate_gate`] — one [`GateMetricVerdict`] per
/// metric both the baseline and at least one candidate measured, plus the
/// overall verdict CI should act on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateReport {
    pub scenario: String,
    pub thresholds: GateThresholds,
    /// Number of candidate results the gate was evaluated against.
    pub candidate_count: usize,
    pub metrics: BTreeMap<String, GateMetricVerdict>,
    /// Metrics the baseline measured but no candidate did.
    pub only_in_baseline: Vec<String>,
    /// Metrics at least one candidate measured but the baseline did not.
    pub only_in_candidates: Vec<String>,
    /// Input preconditions this comparison violated (Issue #196). Empty
    /// for a well-formed baseline/candidate pairing; every entry
    /// contributes its [`GateInputProblem::severity`] to [`Self::overall`].
    #[serde(default)]
    pub problems: Vec<GateInputProblem>,
    /// The worst [`Severity`] across [`Self::metrics`] *and*
    /// [`Self::problems`]. This is the single value `velox-bench gate`'s
    /// exit code encodes.
    ///
    /// Note it is **not** [`Severity::Ok`] when `metrics` is empty: an
    /// empty comparison raises [`GateInputProblem::NoComparableMetrics`],
    /// so a gate that compared nothing fails rather than silently passing
    /// (Issue #196 — this used to be the `Ok` case).
    pub overall: Severity,
}

/// Classify one baseline/candidate median pair against `thresholds` and
/// `min_abs_delta`, ignoring trial counts (the caller applies the
/// `low_confidence` cap separately, since that is a property of the whole
/// metric across all candidates, not of one pair).
fn classify_pair(
    baseline_median: f64,
    candidate_median: f64,
    min_abs_delta: f64,
    thresholds: &GateThresholds,
) -> (f64, Severity) {
    let delta = candidate_median - baseline_median;
    let pct_change = if baseline_median == 0.0 {
        if candidate_median == 0.0 {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        delta / baseline_median * 100.0
    };
    if delta.abs() < min_abs_delta {
        return (pct_change, Severity::Ok);
    }
    let severity = if pct_change > thresholds.fail_pct {
        Severity::Fail
    } else if pct_change > thresholds.warn_pct {
        Severity::Warn
    } else {
        Severity::Ok
    };
    (pct_change, severity)
}

/// Evaluate a regression gate: `baseline` against one or more `candidates`
/// (measurements of the same scenario/OS to compare against it — see the
/// module-level "Regression gate" docs for why more than one is useful).
///
/// "Same scenario/OS" is **checked, not assumed** (Issue #196): a mismatch,
/// an empty candidate, or a pairing with no metrics in common is recorded
/// in [`GateReport::problems`] and reflected in [`GateReport::overall`],
/// so `gate` cannot report `Ok` for a comparison it never actually made.
///
/// `candidates` should be non-empty; an empty slice returns a report with
/// no metrics and a [`GateInputProblem::NoComparableMetrics`] (i.e.
/// [`Severity::Fail`]) rather than panicking, since "no candidates" is a
/// caller bug best surfaced by `velox-bench` requiring `--candidate` at
/// least once, not by a panic deep in pure logic.
pub fn evaluate_gate(
    baseline: &BenchmarkResult,
    candidates: &[&BenchmarkResult],
    thresholds: &GateThresholds,
) -> GateReport {
    let mut metrics = BTreeMap::new();
    let mut only_in_baseline = Vec::new();

    for (name, baseline_stats) in &baseline.metrics {
        let min_abs_delta = MetricKey::from_metric_name(name)
            .map(MetricKey::min_significant_delta)
            .unwrap_or(0.0);

        let mut candidate_medians = Vec::new();
        let mut pct_changes = Vec::new();
        let mut per_candidate_severity = Vec::new();
        let mut low_confidence = baseline_stats.count < MIN_TRIALS_FOR_CONFIDENT_GATE;

        for candidate in candidates {
            let Some(candidate_stats) = candidate.metrics.get(name) else {
                continue;
            };
            if candidate_stats.count < MIN_TRIALS_FOR_CONFIDENT_GATE {
                low_confidence = true;
            }
            let (pct_change, severity) = classify_pair(
                baseline_stats.median,
                candidate_stats.median,
                min_abs_delta,
                thresholds,
            );
            candidate_medians.push(candidate_stats.median);
            pct_changes.push(pct_change);
            per_candidate_severity.push(severity);
        }

        if candidate_medians.is_empty() {
            only_in_baseline.push(name.clone());
            continue;
        }

        let fail_votes = per_candidate_severity
            .iter()
            .filter(|s| **s == Severity::Fail)
            .count();
        // "More than half" — for 1 candidate that is 1/1, for 2 candidates
        // it is 2/2 (both must fail), for 3 it is 2/3. See module docs.
        let majority_fail = fail_votes * 2 > per_candidate_severity.len();
        let any_at_least_warn = per_candidate_severity.iter().any(|s| *s >= Severity::Warn);

        let severity = if low_confidence {
            if any_at_least_warn {
                Severity::Warn
            } else {
                Severity::Ok
            }
        } else if majority_fail {
            Severity::Fail
        } else if any_at_least_warn {
            Severity::Warn
        } else {
            Severity::Ok
        };

        metrics.insert(
            name.clone(),
            GateMetricVerdict {
                baseline_median: baseline_stats.median,
                baseline_count: baseline_stats.count,
                candidate_medians,
                pct_changes,
                per_candidate_severity,
                low_confidence,
                severity,
            },
        );
    }

    let mut only_in_candidates = Vec::new();
    for candidate in candidates {
        for name in candidate.metrics.keys() {
            if !baseline.metrics.contains_key(name) && !only_in_candidates.contains(name) {
                only_in_candidates.push(name.clone());
            }
        }
    }
    only_in_baseline.sort();
    only_in_candidates.sort();

    // Issue #196: the comparability checks. Deliberately run *after* the
    // metric loop above rather than short-circuiting it — a report that
    // says "these two files are not comparable" is more useful with the
    // numbers still attached than without them.
    let mut problems = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.scenario != baseline.scenario {
            problems.push(GateInputProblem::ScenarioMismatch {
                candidate_index: index,
                baseline: baseline.scenario.clone(),
                candidate: candidate.scenario.clone(),
            });
        }
        if candidate.environment.os != baseline.environment.os {
            problems.push(GateInputProblem::OsMismatch {
                candidate_index: index,
                baseline: baseline.environment.os.clone(),
                candidate: candidate.environment.os.clone(),
            });
        }
        if candidate.metrics.is_empty() {
            problems.push(GateInputProblem::CandidateWithoutMetrics {
                candidate_index: index,
            });
        }
    }
    if metrics.is_empty() {
        problems.push(GateInputProblem::NoComparableMetrics);
    } else if !only_in_baseline.is_empty() {
        // Only worth saying when *something* was comparable: when nothing
        // was, `NoComparableMetrics` above already covers it and this
        // would just restate the whole baseline metric list.
        problems.push(GateInputProblem::MetricsMissingFromCandidates {
            metrics: only_in_baseline.clone(),
        });
    }

    let overall = metrics
        .values()
        .map(|verdict| verdict.severity)
        .chain(problems.iter().map(GateInputProblem::severity))
        .max()
        .unwrap_or(Severity::Ok);

    GateReport {
        scenario: baseline.scenario.clone(),
        thresholds: *thresholds,
        candidate_count: candidates.len(),
        metrics,
        only_in_baseline,
        only_in_candidates,
        problems,
        overall,
    }
}

/// Render `report` as a Markdown table, suitable for a GitHub Actions job
/// summary (`$GITHUB_STEP_SUMMARY`) or a PR comment — the "PR
/// summary/comment" acceptance item in Issue #72.
pub fn render_gate_markdown(report: &GateReport) -> String {
    let severity_label = |s: Severity| match s {
        Severity::Ok => "OK",
        Severity::Warn => "WARN",
        Severity::Fail => "FAIL",
    };
    let mut out = String::new();
    out.push_str(&format!(
        "### 性能回帰ゲート: {} — 総合判定: **{}**\n\n",
        report.scenario,
        severity_label(report.overall)
    ));
    out.push_str(&format!(
        "候補測定 {} 件 / warn 閾値 {:.1}% / fail 閾値 {:.1}%\n\n",
        report.candidate_count, report.thresholds.warn_pct, report.thresholds.fail_pct
    ));
    if report.metrics.is_empty() {
        out.push_str("_比較可能なメトリクスがありません。_\n");
    } else {
        out.push_str("| metric | baseline | candidate (中央値) | 変化率 | 判定 |\n");
        out.push_str("|---|---:|---|---|---|\n");
        for (name, verdict) in &report.metrics {
            let candidates_display = verdict
                .candidate_medians
                .iter()
                .zip(&verdict.pct_changes)
                .map(|(median, pct)| {
                    if pct.is_infinite() {
                        format!("{median:.2} (inf)")
                    } else {
                        format!("{median:.2} ({pct:+.1}%)")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let confidence_note = if verdict.low_confidence {
                " ⚠️試行数不足"
            } else {
                ""
            };
            out.push_str(&format!(
                "| {} | {:.2} | {} | — | {}{} |\n",
                name,
                verdict.baseline_median,
                candidates_display,
                severity_label(verdict.severity),
                confidence_note
            ));
        }
    }
    if !report.only_in_baseline.is_empty() {
        out.push_str(&format!(
            "\nbaseline のみに存在: {}\n",
            report.only_in_baseline.join(", ")
        ));
    }
    if !report.only_in_candidates.is_empty() {
        out.push_str(&format!(
            "\ncandidate のみに存在: {}\n",
            report.only_in_candidates.join(", ")
        ));
    }
    // Issue #196: listed last and unconditionally, so a `Fail` overall that
    // came from a precondition (not from a metric) always has its reason
    // visible in the same Job Summary the verdict is read from.
    if !report.problems.is_empty() {
        out.push_str("\n**入力の前提を満たしていません:**\n\n");
        for problem in &report.problems {
            out.push_str(&format!(
                "- [{}] {}\n",
                severity_label(problem.severity()),
                problem.describe()
            ));
        }
    }
    out
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn stats_with(count: usize, median: f64) -> Stats {
        Stats {
            count,
            min: median,
            max: median,
            mean: median,
            median,
            p95: median,
            stddev: 0.0,
        }
    }

    fn result_with_stats(scenario: &str, metrics: &[(&str, Stats)]) -> BenchmarkResult {
        BenchmarkResult {
            scenario: scenario.to_owned(),
            environment: RunEnvironment {
                os: "linux".to_owned(),
                cpu_count: 4,
                git_commit: Some("abc123".to_owned()),
                generated_at: "2026-09-02T00:00:00Z".to_owned(),
                trials: 10,
                cpu_model: None,
                total_memory_bytes: None,
                os_version: None,
                webview_runtime: None,
            },
            metrics: metrics
                .iter()
                .map(|(name, stats)| ((*name).to_owned(), *stats))
                .collect(),
        }
    }

    const KEY: &str = "startup_first_load_ms";

    // -- basic pass/warn/fail --------------------------------------------

    #[test]
    fn ok_when_change_is_negligible() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 502.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Ok);
        assert_eq!(report.overall, Severity::Ok);
    }

    #[test]
    fn warn_when_change_exceeds_warn_pct_but_not_fail_pct() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        // +30%: above default warn_pct (20.0), below default fail_pct (60.0).
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 650.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Warn);
        assert_eq!(report.overall, Severity::Warn);
    }

    #[test]
    fn fail_when_single_candidate_exceeds_fail_pct() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        // +80%: above default fail_pct (60.0). One candidate is "more than
        // half of 1", so this alone is enough to fail.
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Fail);
        assert_eq!(report.overall, Severity::Fail);
    }

    #[test]
    fn improvement_is_ok_not_warn_or_fail() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 200.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Ok);
    }

    // -- boundary values ---------------------------------------------------

    #[test]
    fn exactly_at_warn_pct_is_still_ok() {
        // +20.0% exactly == warn_pct: strictly-greater-than means this is
        // still Ok, not Warn.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 600.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Ok);
    }

    #[test]
    fn just_above_warn_pct_is_warn() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 600.01))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Warn);
    }

    #[test]
    fn exactly_at_fail_pct_is_warn_not_fail() {
        // +60.0% exactly == fail_pct: strictly-greater-than means this is
        // Warn, not Fail.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 800.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Warn);
    }

    #[test]
    fn just_above_fail_pct_is_fail() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 800.01))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Fail);
    }

    // -- ties / no change ---------------------------------------------------

    #[test]
    fn identical_medians_are_ok() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let verdict = &report.metrics[KEY];
        assert_eq!(verdict.severity, Severity::Ok);
        assert_eq!(verdict.pct_changes, vec![0.0]);
    }

    #[test]
    fn zero_baseline_and_zero_candidate_is_ok_not_nan() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 0.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 0.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Ok);
        assert_eq!(report.metrics[KEY].pct_changes, vec![0.0]);
    }

    // -- minimum absolute delta floor ---------------------------------------

    #[test]
    fn tiny_absolute_change_stays_ok_despite_huge_pct_change() {
        // tab_switch_ms floor is 15.0ms; 0.1ms -> 5.0ms is a 4900% change
        // but only a 4.9ms absolute delta, below the floor.
        let baseline = result_with_stats("tab_switch", &[("tab_switch_ms", stats_with(10, 0.1))]);
        let candidate = result_with_stats("tab_switch", &[("tab_switch_ms", stats_with(10, 5.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics["tab_switch_ms"].severity, Severity::Ok);
    }

    #[test]
    fn zero_baseline_with_delta_above_floor_is_fail() {
        // Infinite pct_change, but the 20ms absolute delta clears
        // tab_switch_ms's 15ms floor, so it is evaluated normally.
        let baseline = result_with_stats("tab_switch", &[("tab_switch_ms", stats_with(10, 0.0))]);
        let candidate = result_with_stats("tab_switch", &[("tab_switch_ms", stats_with(10, 20.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let verdict = &report.metrics["tab_switch_ms"];
        assert_eq!(verdict.pct_changes, vec![f64::INFINITY]);
        assert_eq!(verdict.severity, Severity::Fail);
    }

    #[test]
    fn metric_with_no_known_min_delta_falls_back_to_zero_floor() {
        // A metric name the running build's MetricKey doesn't recognise
        // (e.g. saved by a newer velox-bench) has no floor to apply, so it
        // is evaluated on pct_change alone.
        let baseline =
            result_with_stats("cold_startup", &[("future_metric_ms", stats_with(10, 1.0))]);
        let candidate =
            result_with_stats("cold_startup", &[("future_metric_ms", stats_with(10, 2.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        // +100%, well above fail_pct, and the 1.0 absolute delta is not
        // filtered by any floor.
        assert_eq!(report.metrics["future_metric_ms"].severity, Severity::Fail);
    }

    // -- insufficient trial count --------------------------------------------

    #[test]
    fn low_baseline_trial_count_caps_severity_at_warn() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(2, 500.0))]);
        // +100%, which would otherwise be Fail.
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 1000.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let verdict = &report.metrics[KEY];
        assert!(verdict.low_confidence);
        assert_eq!(verdict.severity, Severity::Warn);
    }

    #[test]
    fn low_candidate_trial_count_caps_severity_at_warn() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(1, 1000.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let verdict = &report.metrics[KEY];
        assert!(verdict.low_confidence);
        assert_eq!(verdict.severity, Severity::Warn);
    }

    #[test]
    fn low_confidence_with_no_real_change_stays_ok() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(1, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(1, 502.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let verdict = &report.metrics[KEY];
        assert!(verdict.low_confidence);
        assert_eq!(verdict.severity, Severity::Ok);
    }

    #[test]
    fn exactly_at_min_trials_threshold_is_not_low_confidence() {
        let baseline = result_with_stats(
            "cold_startup",
            &[(KEY, stats_with(MIN_TRIALS_FOR_CONFIDENT_GATE, 500.0))],
        );
        let candidate = result_with_stats(
            "cold_startup",
            &[(KEY, stats_with(MIN_TRIALS_FOR_CONFIDENT_GATE, 900.0))],
        );
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let verdict = &report.metrics[KEY];
        assert!(!verdict.low_confidence);
        assert_eq!(verdict.severity, Severity::Fail);
    }

    // -- baseline / candidate gaps --------------------------------------------

    #[test]
    fn metric_missing_from_baseline_is_reported_not_guessed() {
        let baseline = result_with_stats("cold_startup", &[]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert!(report.metrics.is_empty());
        assert_eq!(report.only_in_candidates, vec![KEY.to_owned()]);
        // The candidate-only metric itself is never guessed at — but the
        // comparison as a whole measured nothing, which Issue #196 says
        // must not read as a pass.
        assert_eq!(report.problems, vec![GateInputProblem::NoComparableMetrics]);
        assert_eq!(report.overall, Severity::Fail);
    }

    #[test]
    fn empty_baseline_result_is_not_a_silent_pass() {
        // A baseline from a run that collected zero records (e.g. a
        // headless environment with no display — see
        // `docs/benchmarking.md` "実行環境要件") must not crash the gate,
        // and must not gate everything through as a pass either: there is
        // nothing to compare, so there is no evidence of "no regression".
        // Issue #196 — this used to assert `Severity::Ok`.
        let baseline = result_with_stats("cold_startup", &[]);
        let candidate = result_with_stats(
            "cold_startup",
            &[
                (KEY, stats_with(10, 500.0)),
                ("rss_total_bytes", stats_with(10, 1.0)),
            ],
        );
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.overall, Severity::Fail);
        assert_eq!(report.only_in_candidates.len(), 2);
        assert!(report
            .problems
            .contains(&GateInputProblem::NoComparableMetrics));
    }

    #[test]
    fn metric_missing_from_one_candidate_is_still_evaluated_from_the_other() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let with_metric = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]);
        let without_metric = result_with_stats("cold_startup", &[]);
        let report = evaluate_gate(
            &baseline,
            &[&with_metric, &without_metric],
            &GateThresholds::default(),
        );
        let verdict = &report.metrics[KEY];
        assert_eq!(verdict.candidate_medians, vec![900.0]);
        // 1 candidate measured it, and that 1 exceeded fail_pct: "more than
        // half of 1" is met.
        assert_eq!(verdict.severity, Severity::Fail);
    }

    #[test]
    fn no_candidates_at_all_fails_instead_of_panicking() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let report = evaluate_gate(&baseline, &[], &GateThresholds::default());
        // Nothing was actually compared (there is no candidate at all), so
        // no *metric* can be Warn/Fail — but the report as a whole fails
        // rather than passing (Issue #196), and the baseline's metric is
        // still visible via `only_in_baseline`. Still no panic: "no
        // candidates" stays a caller bug `velox-bench` rejects at argument
        // parsing, not a crash deep in pure logic.
        assert!(report.metrics.is_empty());
        assert_eq!(report.problems, vec![GateInputProblem::NoComparableMetrics]);
        assert_eq!(report.overall, Severity::Fail);
        assert_eq!(report.only_in_baseline, vec![KEY.to_owned()]);
        assert_eq!(report.candidate_count, 0);
    }

    // -- majority vote across multiple candidates ----------------------------

    #[test]
    fn two_candidates_both_failing_is_fail() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate_a = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]);
        let candidate_b = result_with_stats("cold_startup", &[(KEY, stats_with(10, 1000.0))]);
        let report = evaluate_gate(
            &baseline,
            &[&candidate_a, &candidate_b],
            &GateThresholds::default(),
        );
        assert_eq!(report.metrics[KEY].severity, Severity::Fail);
    }

    #[test]
    fn two_candidates_only_one_failing_is_warn_not_fail() {
        // Requires *more than half* to fail; 1 of 2 is not a majority, so
        // this is a noisy-looking single run, not a confirmed regression —
        // downgraded to Warn rather than dropped entirely.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate_a = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]); // fail
        let candidate_b = result_with_stats("cold_startup", &[(KEY, stats_with(10, 505.0))]); // ok
        let report = evaluate_gate(
            &baseline,
            &[&candidate_a, &candidate_b],
            &GateThresholds::default(),
        );
        assert_eq!(report.metrics[KEY].severity, Severity::Warn);
    }

    #[test]
    fn three_candidates_two_of_three_failing_is_fail() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let a = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]); // fail
        let b = result_with_stats("cold_startup", &[(KEY, stats_with(10, 1000.0))]); // fail
        let c = result_with_stats("cold_startup", &[(KEY, stats_with(10, 502.0))]); // ok
        let report = evaluate_gate(&baseline, &[&a, &b, &c], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Fail);
    }

    #[test]
    fn three_candidates_one_of_three_failing_is_warn() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let a = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]); // fail
        let b = result_with_stats("cold_startup", &[(KEY, stats_with(10, 502.0))]); // ok
        let c = result_with_stats("cold_startup", &[(KEY, stats_with(10, 503.0))]); // ok
        let report = evaluate_gate(&baseline, &[&a, &b, &c], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Warn);
    }

    // -- overall / multi-metric ------------------------------------------

    #[test]
    fn overall_is_the_worst_of_all_metrics() {
        // rss_total_bytes doubles (300MB -> 600MB): +100%, and the 300MB
        // absolute delta clears its 5MiB min_significant_delta floor.
        let baseline = result_with_stats(
            "cold_startup",
            &[
                (KEY, stats_with(10, 500.0)),
                ("rss_total_bytes", stats_with(10, 300_000_000.0)),
            ],
        );
        let candidate = result_with_stats(
            "cold_startup",
            &[
                (KEY, stats_with(10, 502.0)),                       // ok
                ("rss_total_bytes", stats_with(10, 600_000_000.0)), // +100% -> fail
            ],
        );
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Ok);
        assert_eq!(report.metrics["rss_total_bytes"].severity, Severity::Fail);
        assert_eq!(report.overall, Severity::Fail);
    }

    // -- markdown rendering --------------------------------------------------

    #[test]
    fn markdown_report_mentions_overall_severity_and_metric_rows() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let markdown = render_gate_markdown(&report);
        assert!(markdown.contains("FAIL"));
        assert!(markdown.contains(KEY));
        assert!(markdown.contains("cold_startup"));
    }

    #[test]
    fn markdown_report_of_empty_metrics_does_not_panic() {
        let baseline = result_with_stats("cold_startup", &[]);
        let candidate = result_with_stats("cold_startup", &[]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let markdown = render_gate_markdown(&report);
        assert!(markdown.contains("比較可能なメトリクスがありません"));
        // Issue #196: the verdict is FAIL, and the reason for it has to be
        // in the same rendered summary the verdict is read from.
        assert!(markdown.contains("FAIL"));
        assert!(markdown.contains("入力の前提を満たしていません"));
    }

    // -- input preconditions (Issue #196) ------------------------------------

    #[test]
    fn scenario_mismatch_fails_even_when_the_numbers_look_fine() {
        // Identical medians: without the precondition check this is a
        // confident-looking `Ok` computed from two unrelated scenarios.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("tabs_20", &[(KEY, stats_with(10, 500.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Ok);
        assert_eq!(
            report.problems,
            vec![GateInputProblem::ScenarioMismatch {
                candidate_index: 0,
                baseline: "cold_startup".to_owned(),
                candidate: "tabs_20".to_owned(),
            }]
        );
        assert_eq!(report.overall, Severity::Fail);
    }

    #[test]
    fn os_mismatch_fails() {
        // Epic #57 absolute rule 5: results are recorded per OS, and a
        // Linux number is never a stand-in for a Windows one.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let mut candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 505.0))]);
        candidate.environment.os = "windows".to_owned();
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(
            report.problems,
            vec![GateInputProblem::OsMismatch {
                candidate_index: 0,
                baseline: "linux".to_owned(),
                candidate: "windows".to_owned(),
            }]
        );
        assert_eq!(report.overall, Severity::Fail);
    }

    #[test]
    fn mismatch_names_the_offending_candidate_by_index() {
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let good = result_with_stats("cold_startup", &[(KEY, stats_with(10, 505.0))]);
        let bad = result_with_stats("tabs_5", &[(KEY, stats_with(10, 505.0))]);
        let report = evaluate_gate(&baseline, &[&good, &bad], &GateThresholds::default());
        assert_eq!(
            report.problems,
            vec![GateInputProblem::ScenarioMismatch {
                candidate_index: 1,
                baseline: "cold_startup".to_owned(),
                candidate: "tabs_5".to_owned(),
            }]
        );
        assert!(report.problems[0].describe().contains("candidate[1]"));
    }

    #[test]
    fn candidate_with_no_metrics_at_all_fails() {
        // The exact silent pass Issue #196 describes: every baseline
        // metric lands in `only_in_baseline`, no metric verdict exists, and
        // the old `overall` was `Ok`.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("cold_startup", &[]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert!(report.metrics.is_empty());
        assert_eq!(report.only_in_baseline, vec![KEY.to_owned()]);
        assert!(report
            .problems
            .contains(&GateInputProblem::CandidateWithoutMetrics { candidate_index: 0 }));
        assert_eq!(report.overall, Severity::Fail);
    }

    #[test]
    fn baseline_metric_missing_from_every_candidate_warns_but_does_not_block() {
        // A partial gap (one metric of two) is either an incomplete
        // candidate run or a deliberately removed metric — worth a look,
        // not worth blocking a PR, so it stops at Warn.
        let baseline = result_with_stats(
            "cold_startup",
            &[
                (KEY, stats_with(10, 500.0)),
                ("rss_total_bytes", stats_with(10, 100.0)),
            ],
        );
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 505.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Ok);
        assert_eq!(
            report.problems,
            vec![GateInputProblem::MetricsMissingFromCandidates {
                metrics: vec!["rss_total_bytes".to_owned()],
            }]
        );
        assert_eq!(report.overall, Severity::Warn);
    }

    #[test]
    fn a_metric_regression_still_outranks_a_warn_level_problem() {
        let baseline = result_with_stats(
            "cold_startup",
            &[
                (KEY, stats_with(10, 500.0)),
                ("rss_total_bytes", stats_with(10, 100.0)),
            ],
        );
        let candidate = result_with_stats("cold_startup", &[(KEY, stats_with(10, 900.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        assert_eq!(report.metrics[KEY].severity, Severity::Fail);
        assert_eq!(report.overall, Severity::Fail);
    }

    #[test]
    fn a_well_formed_comparison_reports_no_problems() {
        // The shape `perf-gate.yml` actually passes: same scenario, same
        // OS, same metric set, two candidate runs. Nothing here may change
        // its verdict just because the checks exist.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let first = result_with_stats("cold_startup", &[(KEY, stats_with(10, 505.0))]);
        let second = result_with_stats("cold_startup", &[(KEY, stats_with(10, 498.0))]);
        let report = evaluate_gate(&baseline, &[&first, &second], &GateThresholds::default());
        assert!(report.problems.is_empty());
        assert_eq!(report.overall, Severity::Ok);
    }

    #[test]
    fn gate_report_round_trips_through_json_with_problems() {
        // `velox-bench gate --output` writes this, and the dashboard reads
        // it back — the new field must survive the round trip.
        let baseline = result_with_stats("cold_startup", &[(KEY, stats_with(10, 500.0))]);
        let candidate = result_with_stats("tabs_20", &[(KEY, stats_with(10, 500.0))]);
        let report = evaluate_gate(&baseline, &[&candidate], &GateThresholds::default());
        let json = serde_json::to_string(&report).expect("serialize");
        let parsed: GateReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, report);
    }
}

// ---------------------------------------------------------------------
// Timestamp formatting
// ---------------------------------------------------------------------

/// Format `epoch_seconds` (seconds since the Unix epoch, UTC) as an RFC
/// 3339 timestamp with a trailing `Z`, e.g. `"2026-09-01T00:00:00Z"` — used
/// by `src/bin/velox-bench.rs` to fill [`RunEnvironment::generated_at`]. No
/// time/calendar crate is introduced for this (see docs/decisions.md D6):
/// the civil-date conversion is Howard Hinnant's well-known
/// `civil_from_days` algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html>), a few lines of
/// pure integer arithmetic. Takes a plain `i64` rather than a
/// [`std::time::SystemTime`] so it stays a pure, deterministically testable
/// function; the runner is the one place that reads the real clock.
pub fn format_unix_time_utc(epoch_seconds: i64) -> String {
    let days = epoch_seconds.div_euclid(86_400);
    let secs_of_day = epoch_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: converts a day count since the Unix
/// epoch (1970-01-01) into a proleptic-Gregorian `(year, month, day)`.
/// Correct for all `i64` inputs (proleptic, so also defined for dates
/// before 1970).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

// ---------------------------------------------------------------------
// Scenario catalog
// ---------------------------------------------------------------------

/// Canonical scenario identifiers, shared by `src/bin/velox-bench.rs` and
/// `docs/benchmarking.md` so there is exactly one place that spells out
/// "what a benchmark run of VeloX covers" (Issue #14's list: cold/warm
/// startup, first page load, navigation latency, tab creation/switching,
/// and 1/5/10/20/50-tab memory/CPU).
pub mod scenario {
    /// One benchmark scenario.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Scenario {
        /// First launch of a freshly started process (no warm OS file-cache
        /// assumption beyond whatever the environment already has).
        ColdStartup,
        /// A subsequent launch, immediately after a prior one — OS
        /// file/page cache primed by the cold run.
        WarmStartup,
        /// Time from process start to the first page finishing load. Read
        /// from the same `startup` event as cold/warm startup
        /// (`first_load_ms`); kept as its own scenario id because it is
        /// listed as its own acceptance item ("first page load").
        FirstPageLoad,
        /// Duration of a navigation away from the initial page (`page_load`
        /// events after the first one).
        Navigation,
        TabCreate,
        TabSwitch,
        /// CPU used by a *background* tab that is trying hard to be busy
        /// (Issue #64). Loads `scripts/bench/pages/busy.html` (a page with
        /// a `requestAnimationFrame` loop, a 10ms timer and a CSS
        /// animation, all burning real CPU) as the first tab, then opens
        /// the same page with `?idle=1` — which turns every loop off — as
        /// a second tab, so the busy one is backgrounded and the visible
        /// one costs nothing. What is left in `cpu_percent` is what the
        /// hidden tab still consumes.
        BackgroundCpu,
        /// Cost of creating one more tab with `tab_count` tabs already
        /// open (Issue #60). Every sample is taken at that exact tab
        /// count: the scenario opens `tab_count` tabs as setup, `mark`s
        /// the measured phase, then repeatedly opens one tab and closes it
        /// again. `tab_count` is one of [`Scenario::TAB_COUNTS`].
        TabCreateAt(u32),
        /// Cost of switching between `tab_count` already-open tabs (Issue
        /// #60), measured the same way: setup, `mark`, then switches.
        TabSwitchAt(u32),
        /// Restore cost of tab suspension (Issue #63): open a few tabs,
        /// then repeatedly suspend one (`suspend <index>`) and switch back
        /// to it, one `tab_resume` latency sample (plus the page reload's
        /// `page_load`) per round.
        TabResume,
        /// Memory (`rss_total_bytes`/`rss_process_count`) and CPU usage
        /// with exactly `tab_count` tabs open. `tab_count` is one of
        /// [`Scenario::TAB_COUNTS`].
        TabCountMemory(u32),
        /// Like [`Scenario::TabCountMemory`], but measured **after the
        /// browser has had time to settle** (Issue #197,
        /// `docs/performance-targets.md` §27.5).
        ///
        /// `tabs_N` opens every tab back to back and quits about 6 seconds
        /// later, which is shorter than the default 5-second period of the
        /// tab-suspension memory sampler (`app::
        /// spawn_memory_pressure_sampler` sleeps *before* its first
        /// sample). Two things follow: the memory budget barely gets a
        /// chance to fire at all, and whatever reclaim does happen is not
        /// reflected in the numbers. §27.4 could only measure the budget by
        /// shortening that period with `VELOX_MEMORY_CHECK_INTERVAL_MS`,
        /// which is not what a real user runs.
        ///
        /// This variant instead opens the tabs with a pause between them,
        /// waits long enough for suspension to happen **at the default
        /// period**, and only then emits the `mark` that starts the
        /// measured window — so the samples describe the *settled* state
        /// rather than the moments while tabs are still opening.
        /// `tab_count` is one of [`Scenario::TAB_COUNTS`].
        TabCountMemoryHold(u32),
    }

    impl Scenario {
        /// The 1/5/10/20/50-tab points the acceptance criteria name
        /// explicitly.
        pub const TAB_COUNTS: [u32; 5] = [1, 5, 10, 20, 50];

        /// All non-parameterized scenarios, plus one [`Scenario::TabCountMemory`]
        /// per [`Self::TAB_COUNTS`] entry — the full catalog `velox-bench
        /// list-scenarios` prints.
        pub fn all() -> Vec<Scenario> {
            let mut scenarios = vec![
                Scenario::ColdStartup,
                Scenario::WarmStartup,
                Scenario::FirstPageLoad,
                Scenario::Navigation,
                Scenario::TabCreate,
                Scenario::TabSwitch,
                Scenario::TabResume,
                Scenario::BackgroundCpu,
            ];
            scenarios.extend(
                Self::TAB_COUNTS
                    .iter()
                    .map(|&n| Scenario::TabCountMemory(n)),
            );
            scenarios.extend(
                Self::TAB_COUNTS
                    .iter()
                    .map(|&n| Scenario::TabCountMemoryHold(n)),
            );
            scenarios.extend(Self::TAB_COUNTS.iter().map(|&n| Scenario::TabCreateAt(n)));
            scenarios.extend(Self::TAB_COUNTS.iter().map(|&n| Scenario::TabSwitchAt(n)));
            scenarios
        }

        /// The stable string identifier used in file names, CLI
        /// `--scenario` values, and [`super::BenchmarkResult::scenario`].
        pub fn id(self) -> String {
            match self {
                Scenario::ColdStartup => "cold_startup".to_owned(),
                Scenario::WarmStartup => "warm_startup".to_owned(),
                Scenario::FirstPageLoad => "first_page_load".to_owned(),
                Scenario::Navigation => "navigation".to_owned(),
                Scenario::TabCreate => "tab_create".to_owned(),
                Scenario::TabSwitch => "tab_switch".to_owned(),
                Scenario::TabResume => "tab_resume".to_owned(),
                Scenario::BackgroundCpu => "background_cpu".to_owned(),
                Scenario::TabCreateAt(n) => format!("tab_create_{n}"),
                Scenario::TabSwitchAt(n) => format!("tab_switch_{n}"),
                Scenario::TabCountMemory(n) => format!("tabs_{n}"),
                Scenario::TabCountMemoryHold(n) => format!("tabs_hold_{n}"),
            }
        }

        /// Parse an `id()` string back into a [`Scenario`]. Used by the
        /// runner's `--scenario` flag.
        pub fn parse(id: &str) -> Option<Scenario> {
            match id {
                "cold_startup" => Some(Scenario::ColdStartup),
                "warm_startup" => Some(Scenario::WarmStartup),
                "first_page_load" => Some(Scenario::FirstPageLoad),
                "navigation" => Some(Scenario::Navigation),
                "tab_create" => Some(Scenario::TabCreate),
                "tab_switch" => Some(Scenario::TabSwitch),
                "tab_resume" => Some(Scenario::TabResume),
                "background_cpu" => Some(Scenario::BackgroundCpu),
                // Parameterized ids, checked after the exact matches above
                // so `tab_create`/`tab_switch` keep their own meaning.
                other => Self::parse_parameterized(other),
            }
        }

        /// The `<prefix>_<tab count>` half of [`Self::parse`], split out so
        /// the three parameterized families read as one table instead of a
        /// chain of `or_else`s. `None` for an unknown prefix or a tab count
        /// outside [`Self::TAB_COUNTS`].
        fn parse_parameterized(id: &str) -> Option<Scenario> {
            // ⚠️ **順序が意味を持つ。** `strip_prefix` が最初に一致した
            // ところで `return` するため、`tabs_hold_` は `tabs_` より
            // **前**に置かなければならない。逆にすると "tabs_hold_10" が
            // `tabs_` に食われ、残り "hold_10" の数値パースに失敗して
            // `None` になる (`tabs_hold_is_not_swallowed_by_tabs` が
            // これを守っている)。
            for (prefix, build) in [
                (
                    "tabs_hold_",
                    Scenario::TabCountMemoryHold as fn(u32) -> Scenario,
                ),
                ("tabs_", Scenario::TabCountMemory as fn(u32) -> Scenario),
                ("tab_create_", Scenario::TabCreateAt as fn(u32) -> Scenario),
                ("tab_switch_", Scenario::TabSwitchAt as fn(u32) -> Scenario),
            ] {
                if let Some(rest) = id.strip_prefix(prefix) {
                    return rest
                        .parse::<u32>()
                        .ok()
                        .filter(|n| Self::TAB_COUNTS.contains(n))
                        .map(build);
                }
            }
            None
        }

        /// Whether `velox-bench run` can drive this scenario unattended
        /// (launch the binary, wait, collect) versus needing a real display
        /// plus manual interaction before its log can be fed to
        /// `velox-bench aggregate`. See `docs/benchmarking.md`, "自動実行".
        ///
        /// Before Issue #112 this was only true for the three startup
        /// scenarios, since nothing could drive tab creation/switching or
        /// navigation without a human at the keyboard. #112's
        /// `VELOX_AUTOMATION_SCRIPT` hook (`browser::automation`) plus
        /// `browser::automation::generate_bench_script` cover the rest:
        /// `velox-bench run` now generates and feeds a script for every
        /// scenario, so every variant is unattended. The method (and its
        /// `match`, rather than a bare `true`) is kept so a future scenario
        /// that genuinely needs a human has a single, obvious place to say
        /// so.
        pub fn is_unattended(self) -> bool {
            match self {
                Scenario::ColdStartup
                | Scenario::WarmStartup
                | Scenario::FirstPageLoad
                | Scenario::Navigation
                | Scenario::TabCreate
                | Scenario::TabSwitch
                | Scenario::TabResume
                | Scenario::BackgroundCpu
                | Scenario::TabCreateAt(_)
                | Scenario::TabSwitchAt(_)
                | Scenario::TabCountMemory(_)
                | Scenario::TabCountMemoryHold(_) => true,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_scenario_round_trips_through_its_id() {
            for scenario in Scenario::all() {
                let id = scenario.id();
                assert_eq!(Scenario::parse(&id), Some(scenario), "id was {id:?}");
            }
        }

        #[test]
        fn tabs_hold_is_not_swallowed_by_tabs() {
            // `parse_parameterized` は最初に一致したプレフィックスで
            // return するので、`tabs_hold_` が `tabs_` より後ろにあると
            // "tabs_hold_10" が None になる。順序を入れ替えたときに
            // 気付けるようにここで固定する。
            assert_eq!(
                Scenario::parse("tabs_hold_10"),
                Some(Scenario::TabCountMemoryHold(10))
            );
            assert_eq!(
                Scenario::parse("tabs_10"),
                Some(Scenario::TabCountMemory(10))
            );
            assert_eq!(Scenario::parse("tabs_hold_7"), None);
            assert_eq!(Scenario::parse("tabs_hold_"), None);
        }

        #[test]
        fn parse_rejects_unknown_tab_counts() {
            assert_eq!(Scenario::parse("tabs_7"), None);
            assert_eq!(Scenario::parse("tabs_"), None);
            assert_eq!(Scenario::parse("not_a_scenario"), None);
            assert_eq!(Scenario::parse("tab_create_7"), None);
            assert_eq!(Scenario::parse("tab_switch_"), None);
        }

        #[test]
        fn unparameterized_and_parameterized_ids_stay_distinct() {
            // `tab_create` must not be read as a `tab_create_<n>` with an
            // empty count, and the parameterized ids must not collide with
            // the fixed ones they are named after.
            assert_eq!(Scenario::parse("tab_create"), Some(Scenario::TabCreate));
            assert_eq!(Scenario::parse("tab_switch"), Some(Scenario::TabSwitch));
            assert_eq!(
                Scenario::parse("tab_create_5"),
                Some(Scenario::TabCreateAt(5))
            );
            assert_eq!(
                Scenario::parse("tab_switch_20"),
                Some(Scenario::TabSwitchAt(20))
            );
        }

        #[test]
        fn all_covers_eight_fixed_plus_four_parameterized_families() {
            // 固定 8 + タブ数でパラメータ化された 4 系統
            // (`tabs_N` / `tabs_hold_N` / `tab_create_N` / `tab_switch_N`)。
            // `tabs_hold_N` は Issue #197 で追加。
            assert_eq!(Scenario::all().len(), 8 + 4 * Scenario::TAB_COUNTS.len());
        }

        #[test]
        fn every_scenario_is_unattended_since_112() {
            // #112 gave `velox-bench run` a `VELOX_AUTOMATION_SCRIPT` hook
            // (`browser::automation`) covering tab create/switch/close and
            // navigation, so nothing is left that needs a human anymore —
            // see `is_unattended`'s doc comment.
            assert!(Scenario::all().into_iter().all(Scenario::is_unattended));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    // -- parse_jsonl ------------------------------------------------------

    #[test]
    fn parse_jsonl_skips_blank_and_non_json_lines() {
        let text = "\n{\"event\":\"startup\",\"ts_ms\":1.0}\n\nnot json at all\nvelox: some other diagnostic\n";
        let events = parse_jsonl(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "startup");
    }

    #[test]
    fn parse_jsonl_skips_json_without_an_event_field() {
        let text = "{\"foo\":1}\n{\"event\":\"page_load\",\"duration_ms\":5.0}\n";
        let events = parse_jsonl(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "page_load");
    }

    #[test]
    fn parse_jsonl_skips_a_truncated_final_line() {
        let text =
            "{\"event\":\"tab_create\",\"tab_id\":1,\"duration_ms\":2.0}\n{\"event\":\"tab_cre";
        let events = parse_jsonl(text);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn parse_jsonl_of_empty_input_is_empty() {
        assert!(parse_jsonl("").is_empty());
        assert!(parse_jsonl("\n\n\n").is_empty());
    }

    // -- MetricKey::extract -------------------------------------------------

    // -- measured_phase / warm-up cut (Issue #60) --------------------------

    #[test]
    fn aggregate_ignores_events_before_the_last_measure_start() {
        let trial = vec![
            // Warm-up: two creations at low tab counts.
            event(r#"{"event":"tab_create","tab_id":1,"duration_ms":50.0}"#),
            event(r#"{"event":"tab_create","tab_id":2,"duration_ms":60.0}"#),
            event(r#"{"event":"measure_start","ts_ms":100.0}"#),
            // Measured phase.
            event(r#"{"event":"tab_create","tab_id":3,"duration_ms":10.0}"#),
            event(r#"{"event":"tab_create","tab_id":4,"duration_ms":12.0}"#),
        ];
        let aggregated = aggregate_trials(&[trial]);
        let stats = aggregated.get("tab_create_ms").unwrap();
        assert_eq!(stats.count, 2, "warm-up samples must not be pooled in");
        assert_eq!(stats.median, 11.0);
    }

    #[test]
    fn the_last_measure_start_is_the_one_that_cuts() {
        let trial = vec![
            event(r#"{"event":"measure_start","ts_ms":1.0}"#),
            event(r#"{"event":"tab_switch","tab_id":1,"duration_ms":99.0}"#),
            event(r#"{"event":"measure_start","ts_ms":2.0}"#),
            event(r#"{"event":"tab_switch","tab_id":2,"duration_ms":3.0}"#),
        ];
        let aggregated = aggregate_trials(&[trial]);
        let stats = aggregated.get("tab_switch_ms").unwrap();
        assert_eq!(stats.count, 1);
        assert_eq!(stats.median, 3.0);
    }

    #[test]
    fn a_trial_without_a_marker_keeps_every_event() {
        // Every pre-#60 scenario emits no marker; their aggregation must
        // be byte-for-byte what it always was.
        let trial = vec![
            event(r#"{"event":"tab_create","tab_id":1,"duration_ms":10.0}"#),
            event(r#"{"event":"tab_create","tab_id":2,"duration_ms":20.0}"#),
        ];
        let stats = aggregate_trials(&[trial]);
        assert_eq!(stats.get("tab_create_ms").unwrap().count, 2);
    }

    #[test]
    fn a_marker_with_nothing_after_it_yields_no_samples() {
        let trial = vec![
            event(r#"{"event":"tab_create","tab_id":1,"duration_ms":10.0}"#),
            event(r#"{"event":"measure_start","ts_ms":5.0}"#),
        ];
        // Not a panic and not a silent fallback to the warm-up numbers:
        // the metric is simply absent.
        assert!(!aggregate_trials(&[trial]).contains_key("tab_create_ms"));
    }

    // -- summarize_ipc (Issue #66) ------------------------------------------

    #[test]
    fn summarize_ipc_groups_by_direction_and_name_and_sums_bytes_and_count() {
        let events = vec![
            event(
                r#"{"event":"ipc","direction":"in","name":"navigate","bytes":40,"duration_ms":0.5}"#,
            ),
            event(
                r#"{"event":"ipc","direction":"in","name":"navigate","bytes":42,"duration_ms":0.7}"#,
            ),
            event(
                r#"{"event":"ipc","direction":"out","name":"set_tabs","bytes":500,"duration_ms":1.0}"#,
            ),
        ];
        let rows = summarize_ipc(&events);
        assert_eq!(rows.len(), 2);

        let navigate = rows.iter().find(|r| r.name == "navigate").unwrap();
        assert_eq!(navigate.direction, "in");
        assert_eq!(navigate.count, 2);
        assert_eq!(navigate.total_bytes, 82);
        assert_eq!(navigate.duration_ms.count, 2);
        assert_eq!(navigate.duration_ms.median, 0.6);

        let set_tabs = rows.iter().find(|r| r.name == "set_tabs").unwrap();
        assert_eq!(set_tabs.direction, "out");
        assert_eq!(set_tabs.count, 1);
        assert_eq!(set_tabs.total_bytes, 500);
    }

    #[test]
    fn summarize_ipc_ignores_non_ipc_events() {
        let events = vec![
            event(r#"{"event":"tab_create","tab_id":1,"duration_ms":2.0}"#),
            event(r#"{"event":"ipc","direction":"in","name":"back","bytes":12,"duration_ms":0.1}"#),
        ];
        let rows = summarize_ipc(&events);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "back");
    }

    #[test]
    fn summarize_ipc_of_no_ipc_events_is_empty() {
        let events = vec![event(r#"{"event":"startup","ts_ms":1.0}"#)];
        assert!(summarize_ipc(&events).is_empty());
    }

    #[test]
    fn summarize_ipc_sorts_by_total_bytes_descending() {
        let events = vec![
            event(
                r#"{"event":"ipc","direction":"in","name":"small","bytes":10,"duration_ms":0.1}"#,
            ),
            event(
                r#"{"event":"ipc","direction":"out","name":"big","bytes":9000,"duration_ms":0.1}"#,
            ),
            event(
                r#"{"event":"ipc","direction":"in","name":"medium","bytes":500,"duration_ms":0.1}"#,
            ),
        ];
        let rows = summarize_ipc(&events);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["big", "medium", "small"]);
    }

    #[test]
    fn cpu_percent_is_read_from_the_cpu_event() {
        let events = vec![
            event(r#"{"event":"cpu","percent":97.5,"ts_ms":10.0}"#),
            event(r#"{"event":"cpu","percent":0.6,"ts_ms":20.0}"#),
            // An `rss` event must not contribute to it, and vice versa.
            event(
                r#"{"event":"rss","process_count":4,"total_rss_bytes":10,"total_cpu_seconds":3.0,"ts_ms":20.0}"#,
            ),
        ];
        assert_eq!(MetricKey::CpuPercent.extract(&events), vec![97.5, 0.6]);
        assert_eq!(MetricKey::RssProcessCount.extract(&events), vec![4.0]);
    }

    #[test]
    fn extract_reads_matching_events_only() {
        let events = vec![
            event(r#"{"event":"tab_create","tab_id":1,"duration_ms":10.0}"#),
            event(r#"{"event":"tab_switch","tab_id":1,"duration_ms":2.0}"#),
            event(r#"{"event":"tab_create","tab_id":2,"duration_ms":12.0}"#),
        ];
        assert_eq!(MetricKey::TabCreateMs.extract(&events), vec![10.0, 12.0]);
        assert_eq!(MetricKey::TabSwitchMs.extract(&events), vec![2.0]);
    }

    #[test]
    fn extract_startup_reads_all_three_fields_from_one_event() {
        let events = vec![event(
            r#"{"event":"startup","ts_ms":30.0,"window_created_ms":10.0,"toolbar_ready_ms":20.0,"first_load_ms":30.0}"#,
        )];
        assert_eq!(
            MetricKey::StartupWindowCreatedMs.extract(&events),
            vec![10.0]
        );
        assert_eq!(
            MetricKey::StartupToolbarReadyMs.extract(&events),
            vec![20.0]
        );
        assert_eq!(MetricKey::StartupFirstLoadMs.extract(&events), vec![30.0]);
    }

    #[test]
    fn extract_rss_reads_bytes_and_process_count_separately() {
        let events = vec![event(
            r#"{"event":"rss","ts_ms":1.0,"pid":42,"process_count":5,"total_rss_bytes":1048576}"#,
        )];
        assert_eq!(MetricKey::RssTotalBytes.extract(&events), vec![1048576.0]);
        assert_eq!(MetricKey::RssProcessCount.extract(&events), vec![5.0]);
    }

    #[test]
    fn extract_pss_reads_bytes_and_process_count_separately() {
        let events = vec![event(
            r#"{"event":"rss","ts_ms":1.0,"pid":42,"process_count":5,"total_rss_bytes":1048576,"total_pss_bytes":524288,"pss_process_count":4}"#,
        )];
        assert_eq!(MetricKey::PssTotalBytes.extract(&events), vec![524288.0]);
        assert_eq!(MetricKey::PssProcessCount.extract(&events), vec![4.0]);
    }

    #[test]
    fn extract_pss_total_bytes_is_empty_when_json_null() {
        // matches an `rss` event where PSS could not be read for any
        // process in the tree (see `metrics::RssSample::total_pss_bytes`).
        let events = vec![event(
            r#"{"event":"rss","ts_ms":1.0,"pid":42,"process_count":5,"total_rss_bytes":1048576,"total_pss_bytes":null,"pss_process_count":0}"#,
        )];
        assert!(MetricKey::PssTotalBytes.extract(&events).is_empty());
        // pss_process_count is still a real number (0), unlike the null
        // total — it is extracted normally.
        assert_eq!(MetricKey::PssProcessCount.extract(&events), vec![0.0]);
    }

    #[test]
    fn extract_pss_total_bytes_is_empty_when_field_absent() {
        // An `rss` event from before Issue #108 (or any producer that never
        // added the field) has no `total_pss_bytes` key at all — same
        // "absent metric" outcome as a JSON `null`.
        let events = vec![event(
            r#"{"event":"rss","ts_ms":1.0,"pid":42,"process_count":5,"total_rss_bytes":1048576}"#,
        )];
        assert!(MetricKey::PssTotalBytes.extract(&events).is_empty());
        assert!(MetricKey::PssProcessCount.extract(&events).is_empty());
    }

    #[test]
    fn extract_returns_empty_for_absent_metric() {
        let events = vec![event(
            r#"{"event":"page_load","url":"x","duration_ms":1.0}"#,
        )];
        assert!(MetricKey::TabCreateMs.extract(&events).is_empty());
    }

    // -- MetricKey::SuspendedTabCount (Issue #197 revisit condition (4), D105) --

    #[test]
    fn suspended_tab_count_counts_every_tab_suspend_event_as_one_sample() {
        let events = vec![
            event(r#"{"event":"tab_suspend","tab_id":1,"reason":"memory","ts_ms":1.0}"#),
            event(r#"{"event":"tab_suspend","tab_id":2,"reason":"memory","ts_ms":2.0}"#),
            event(r#"{"event":"tab_suspend","tab_id":3,"reason":"idle","ts_ms":3.0}"#),
        ];
        // 3 matching events fold into exactly 1 sample — the trial's total
        // — not 3 samples, unlike every field-based key.
        assert_eq!(MetricKey::SuspendedTabCount.extract(&events), vec![3.0]);
    }

    #[test]
    fn suspended_tab_count_is_a_real_zero_when_the_event_never_fires_but_others_did() {
        // Metrics genuinely ran (the trial is non-empty) but nothing was
        // ever suspended (e.g. every tab stayed under the memory budget) —
        // this must be a measured `0.0`, not an absent key.
        let events = vec![
            event(r#"{"event":"tab_create","tab_id":1,"duration_ms":10.0}"#),
            event(r#"{"event":"rss","process_count":1,"total_rss_bytes":100,"ts_ms":1.0}"#),
        ];
        assert_eq!(MetricKey::SuspendedTabCount.extract(&events), vec![0.0]);
    }

    #[test]
    fn suspended_tab_count_is_absent_for_a_wholly_empty_trial() {
        // No perf records at all (metrics off, or a crash before anything
        // was written) is a measurement *failure*, not "zero suspended" —
        // same absence rule every other key follows for its own failure
        // mode.
        assert!(MetricKey::SuspendedTabCount.extract(&[]).is_empty());
    }

    #[test]
    fn suspended_tab_count_ignores_other_event_kinds() {
        let events = vec![
            event(r#"{"event":"tab_resume","tab_id":1,"duration_ms":5.0}"#),
            event(r#"{"event":"tab_suspend","tab_id":1,"reason":"tab_count","ts_ms":1.0}"#),
            event(r#"{"event":"tab_create","tab_id":2,"duration_ms":10.0}"#),
        ];
        assert_eq!(MetricKey::SuspendedTabCount.extract(&events), vec![1.0]);
    }

    #[test]
    fn suspended_tab_count_round_trips_through_from_metric_name() {
        assert_eq!(
            MetricKey::from_metric_name("suspended_tab_count"),
            Some(MetricKey::SuspendedTabCount)
        );
    }

    #[test]
    fn aggregate_trials_counts_suspensions_from_before_the_measure_start_marker() {
        // Unlike every other key, SuspendedTabCount must see suspensions
        // that happened during the tabs_hold_N warm-up wait (before
        // `measure_start`) — see MetricKey::counts_whole_trial. A
        // tab_create in the same warm-up window must still be excluded
        // from tab_create_ms, proving the whole-trial exception is scoped
        // to this one key.
        let trial = vec![
            event(r#"{"event":"tab_create","tab_id":1,"duration_ms":50.0}"#),
            event(r#"{"event":"tab_suspend","tab_id":1,"reason":"memory","ts_ms":60.0}"#),
            event(r#"{"event":"tab_suspend","tab_id":2,"reason":"memory","ts_ms":61.0}"#),
            event(r#"{"event":"measure_start","ts_ms":100.0}"#),
            event(r#"{"event":"tab_create","tab_id":3,"duration_ms":10.0}"#),
        ];
        let aggregated = aggregate_trials(&[trial]);
        assert_eq!(
            aggregated.get("suspended_tab_count").unwrap().median,
            2.0,
            "suspensions before measure_start must still be counted"
        );
        assert_eq!(
            aggregated.get("tab_create_ms").unwrap().count,
            1,
            "the warm-up tab_create must still be cut, unlike suspended_tab_count"
        );
    }

    // -- compute_stats / percentile ------------------------------------

    #[test]
    fn compute_stats_of_empty_input_is_none() {
        assert!(compute_stats(&[]).is_none());
    }

    #[test]
    fn compute_stats_of_a_single_value() {
        let stats = compute_stats(&[42.0]).unwrap();
        assert_eq!(stats.count, 1);
        assert_eq!(stats.min, 42.0);
        assert_eq!(stats.max, 42.0);
        assert_eq!(stats.mean, 42.0);
        assert_eq!(stats.median, 42.0);
        assert_eq!(stats.p95, 42.0);
        assert_eq!(stats.stddev, 0.0);
    }

    #[test]
    fn compute_stats_median_of_even_count_interpolates() {
        let stats = compute_stats(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(stats.median, 2.5);
    }

    #[test]
    fn compute_stats_p95_over_1_to_100() {
        let values: Vec<f64> = (1..=100).map(|n| n as f64).collect();
        let stats = compute_stats(&values).unwrap();
        // Linear-interpolation p95 over 1..=100 (rank = 0.95 * 99 = 94.05):
        // 95th sample (index 94, value 95) plus 5% of the step to 96.
        assert!((stats.p95 - 95.05).abs() < 1e-9);
        assert_eq!(stats.median, 50.5);
        assert_eq!(stats.min, 1.0);
        assert_eq!(stats.max, 100.0);
        assert_eq!(stats.mean, 50.5);
    }

    #[test]
    fn compute_stats_is_order_independent() {
        let ascending = compute_stats(&[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
        let shuffled = compute_stats(&[3.0, 1.0, 5.0, 2.0, 4.0]).unwrap();
        assert_eq!(ascending, shuffled);
    }

    #[test]
    fn compute_stats_stddev_of_uniform_values_is_zero() {
        let stats = compute_stats(&[7.0, 7.0, 7.0]).unwrap();
        assert_eq!(stats.stddev, 0.0);
    }

    #[test]
    fn compute_stats_handles_an_outlier_without_panicking() {
        // Median/p95 stay well below the outlier; mean and stddev do not.
        let mut values = vec![10.0; 9];
        values.push(10_000.0);
        let stats = compute_stats(&values).unwrap();
        assert_eq!(stats.median, 10.0);
        assert!(stats.mean > 10.0);
        assert!(stats.stddev > 0.0);
    }

    // -- aggregate_trials --------------------------------------------------

    #[test]
    fn aggregate_trials_pools_samples_across_trials() {
        let trial_a = vec![event(
            r#"{"event":"tab_create","tab_id":1,"duration_ms":10.0}"#,
        )];
        let trial_b = vec![event(
            r#"{"event":"tab_create","tab_id":1,"duration_ms":20.0}"#,
        )];
        let aggregated = aggregate_trials(&[trial_a, trial_b]);
        let stats = aggregated.get("tab_create_ms").unwrap();
        assert_eq!(stats.count, 2);
        assert_eq!(stats.median, 15.0);
    }

    #[test]
    fn aggregate_trials_of_a_single_trial_works() {
        let trial = vec![event(
            r#"{"event":"page_load","url":"https://example.com/","duration_ms":250.0}"#,
        )];
        let aggregated = aggregate_trials(std::slice::from_ref(&trial));
        assert_eq!(aggregated["page_load_ms"].count, 1);
        assert_eq!(aggregated["page_load_ms"].median, 250.0);
    }

    #[test]
    fn aggregate_trials_of_no_trials_is_empty() {
        assert!(aggregate_trials(&[]).is_empty());
    }

    #[test]
    fn aggregate_trials_omits_metrics_with_no_samples() {
        let trial = vec![event(
            r#"{"event":"startup","window_created_ms":1.0,"toolbar_ready_ms":2.0,"first_load_ms":3.0}"#,
        )];
        let aggregated = aggregate_trials(&[trial]);
        assert!(!aggregated.contains_key("tab_create_ms"));
        assert!(!aggregated.contains_key("rss_total_bytes"));
        // 3 startup_* fields, plus `suspended_tab_count`: unlike the two
        // metrics just asserted absent above (whose *event kind* never
        // fired at all), this trial's events are non-empty, so
        // `SuspendedTabCount` reports a real, measured `0` rather than
        // being omitted — see `MetricKey::extract`'s doc comment (D105).
        assert_eq!(aggregated.len(), 4);
        assert_eq!(aggregated["suspended_tab_count"].median, 0.0);
    }

    #[test]
    fn aggregate_trials_includes_pss_when_present() {
        let trial = vec![event(
            r#"{"event":"rss","process_count":5,"total_rss_bytes":1000,"total_pss_bytes":600,"pss_process_count":5}"#,
        )];
        let aggregated = aggregate_trials(&[trial]);
        assert_eq!(aggregated["pss_total_bytes"].median, 600.0);
        assert_eq!(aggregated["pss_process_count"].median, 5.0);
    }

    #[test]
    fn aggregate_trials_omits_pss_total_bytes_when_unreadable_but_keeps_rss() {
        let trial = vec![event(
            r#"{"event":"rss","process_count":5,"total_rss_bytes":1000,"total_pss_bytes":null,"pss_process_count":0}"#,
        )];
        let aggregated = aggregate_trials(&[trial]);
        assert!(!aggregated.contains_key("pss_total_bytes"));
        assert_eq!(aggregated["rss_total_bytes"].median, 1000.0);
        // pss_process_count(0) is a real, present sample — it says "zero of
        // process_count were readable", which is different information from
        // the metric being entirely absent.
        assert_eq!(aggregated["pss_process_count"].median, 0.0);
    }

    #[test]
    fn aggregate_trials_ignores_a_trial_with_broken_lines_via_parse_jsonl() {
        let good = parse_jsonl("{\"event\":\"tab_switch\",\"tab_id\":1,\"duration_ms\":5.0}\n");
        let broken = parse_jsonl("not json\n\n");
        let aggregated = aggregate_trials(&[good, broken]);
        assert_eq!(aggregated["tab_switch_ms"].count, 1);
    }

    // -- memory_sample_confidence (Issue #119, D50) -----------------------

    fn rss_stats(count: usize) -> Stats {
        // The specific values don't matter to `memory_sample_confidence` —
        // only `Stats::count` — but `compute_stats` is used anyway so this
        // stays a realistic `Stats`, not a hand-built one that could drift
        // from what `compute_stats` actually produces.
        compute_stats(&vec![100.0; count]).expect("non-empty sample vec")
    }

    #[test]
    fn memory_confidence_is_not_applicable_to_non_memory_scenarios() {
        let metrics = BTreeMap::new();
        for scenario in [
            scenario::Scenario::ColdStartup,
            scenario::Scenario::WarmStartup,
            scenario::Scenario::FirstPageLoad,
            scenario::Scenario::Navigation,
            scenario::Scenario::TabCreate,
            scenario::Scenario::TabSwitch,
            scenario::Scenario::TabResume,
            scenario::Scenario::BackgroundCpu,
        ] {
            assert_eq!(
                memory_sample_confidence(scenario, 3, &metrics),
                MemorySampleConfidence::NotApplicable,
                "{scenario:?} should not be checked at all"
            );
        }
    }

    #[test]
    fn memory_confidence_insufficient_with_zero_samples_reproduces_the_bug() {
        // The pre-fix failure mode this issue is about: the sampler took
        // exactly its startup sample and nothing else, so the metric never
        // even makes it into the aggregated map.
        let metrics = BTreeMap::new();
        let confidence =
            memory_sample_confidence(scenario::Scenario::TabCountMemory(20), 3, &metrics);
        assert_eq!(
            confidence,
            MemorySampleConfidence::Insufficient {
                observed: 0,
                required: 3 * MIN_RSS_SAMPLES_PER_TRIAL,
            }
        );
        assert!(confidence.is_insufficient());
    }

    #[test]
    fn memory_confidence_insufficient_one_sample_below_the_floor() {
        let mut metrics = BTreeMap::new();
        let required = 4 * MIN_RSS_SAMPLES_PER_TRIAL;
        metrics.insert(
            MetricKey::PssTotalBytes.as_str().to_owned(),
            rss_stats(required - 1),
        );
        let confidence =
            memory_sample_confidence(scenario::Scenario::TabCountMemory(5), 4, &metrics);
        assert!(confidence.is_insufficient());
        assert_eq!(
            confidence,
            MemorySampleConfidence::Insufficient {
                observed: required - 1,
                required,
            }
        );
    }

    #[test]
    fn memory_confidence_sufficient_exactly_at_the_boundary() {
        let mut metrics = BTreeMap::new();
        let required = 4 * MIN_RSS_SAMPLES_PER_TRIAL;
        metrics.insert(
            MetricKey::PssTotalBytes.as_str().to_owned(),
            rss_stats(required),
        );
        let confidence =
            memory_sample_confidence(scenario::Scenario::TabCountMemory(5), 4, &metrics);
        assert!(!confidence.is_insufficient());
        assert_eq!(
            confidence,
            MemorySampleConfidence::Sufficient {
                observed: required,
                required
            }
        );
    }

    #[test]
    fn memory_confidence_sufficient_comfortably_above_the_boundary() {
        let mut metrics = BTreeMap::new();
        metrics.insert(MetricKey::PssTotalBytes.as_str().to_owned(), rss_stats(50));
        let confidence =
            memory_sample_confidence(scenario::Scenario::TabCountMemory(1), 3, &metrics);
        assert!(!confidence.is_insufficient());
    }

    #[test]
    fn memory_confidence_falls_back_to_rss_when_pss_is_absent() {
        // A build/environment where PSS could never be read at all (old
        // kernel, permissions, non-Linux) still has RSS samples — the check
        // must not treat that as automatically insufficient just because
        // the preferred metric is missing.
        let mut metrics = BTreeMap::new();
        let required = 2 * MIN_RSS_SAMPLES_PER_TRIAL;
        metrics.insert(
            MetricKey::RssTotalBytes.as_str().to_owned(),
            rss_stats(required),
        );
        let confidence =
            memory_sample_confidence(scenario::Scenario::TabCountMemory(10), 2, &metrics);
        assert_eq!(
            confidence,
            MemorySampleConfidence::Sufficient {
                observed: required,
                required
            }
        );
    }

    #[test]
    fn memory_confidence_prefers_pss_count_over_rss_when_both_present() {
        // PSS and RSS come from the same `rss` event, so their sample
        // counts are normally identical — but the function must read PSS's
        // `count`, not RSS's, to honor D42's "PSS is the metric to compare"
        // guidance even in a contrived case where they'd disagree.
        let mut metrics = BTreeMap::new();
        metrics.insert(MetricKey::PssTotalBytes.as_str().to_owned(), rss_stats(2));
        metrics.insert(MetricKey::RssTotalBytes.as_str().to_owned(), rss_stats(99));
        // 2 trials => required = 4: PSS's count (2) alone is insufficient,
        // while RSS's count (99) alone would easily pass — this only proves
        // PSS's count is the one actually consulted when the two disagree.
        let confidence =
            memory_sample_confidence(scenario::Scenario::TabCountMemory(1), 2, &metrics);
        assert_eq!(
            confidence,
            MemorySampleConfidence::Insufficient {
                observed: 2,
                required: 2 * MIN_RSS_SAMPLES_PER_TRIAL,
            }
        );
    }

    #[test]
    fn memory_confidence_required_scales_with_trial_count() {
        let metrics = BTreeMap::new();
        for trials in [1u32, 5, 10] {
            let confidence =
                memory_sample_confidence(scenario::Scenario::TabCountMemory(1), trials, &metrics);
            assert_eq!(
                confidence,
                MemorySampleConfidence::Insufficient {
                    observed: 0,
                    required: trials as usize * MIN_RSS_SAMPLES_PER_TRIAL,
                }
            );
        }
    }

    // -- format_unix_time_utc -------------------------------------------

    #[test]
    fn format_unix_time_utc_of_epoch_zero() {
        assert_eq!(format_unix_time_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_unix_time_utc_of_a_known_new_year() {
        assert_eq!(format_unix_time_utc(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn format_unix_time_utc_of_a_recent_timestamp() {
        assert_eq!(format_unix_time_utc(1_756_684_800), "2025-09-01T00:00:00Z");
    }

    #[test]
    fn format_unix_time_utc_end_of_first_day() {
        assert_eq!(format_unix_time_utc(86_399), "1970-01-01T23:59:59Z");
    }

    #[test]
    fn format_unix_time_utc_one_second_after_epoch() {
        assert_eq!(format_unix_time_utc(1), "1970-01-01T00:00:01Z");
    }

    // -- RunEnvironment: 機種情報フィールドの後方互換 (Issue #211/D104) ----

    /// `cpu_model`/`total_memory_bytes`/`os_version`/`webview_runtime` を
    /// 追加する前に保存された結果ファイル (`results/baseline/` や
    /// `results/history/` の JSONL) はこれらのキーを一切持たない。
    /// `#[serde(default)]` により `None` へフォールバックし、デシリアライズ
    /// が失敗しないことを確認する。
    #[test]
    fn run_environment_deserializes_without_machine_fields() {
        let json = r#"{
            "os": "linux",
            "cpu_count": 4,
            "git_commit": "abc123",
            "generated_at": "2026-09-01T00:00:00Z",
            "trials": 10
        }"#;
        let env: RunEnvironment = serde_json::from_str(json).unwrap();
        assert_eq!(env.os, "linux");
        assert_eq!(env.cpu_count, 4);
        assert_eq!(env.cpu_model, None);
        assert_eq!(env.total_memory_bytes, None);
        assert_eq!(env.os_version, None);
        assert_eq!(env.webview_runtime, None);
    }

    /// 新フィールドを含む結果のシリアライズ→デシリアライズが値を保つこと
    /// (ラウンドトリップ)。
    #[test]
    fn run_environment_roundtrips_with_machine_fields() {
        let env = RunEnvironment {
            os: "windows".to_owned(),
            cpu_count: 4,
            git_commit: Some("abc123".to_owned()),
            generated_at: "2026-09-01T00:00:00Z".to_owned(),
            trials: 10,
            cpu_model: Some("AMD EPYC 9V74 80-Core Processor".to_owned()),
            total_memory_bytes: Some(8_589_934_592),
            os_version: Some("10.0.26100".to_owned()),
            webview_runtime: Some("128.0.2739.79".to_owned()),
        };
        let json = serde_json::to_string(&env).unwrap();
        let restored: RunEnvironment = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, env);
    }

    // -- compare -------------------------------------------------------

    fn result_with(scenario: &str, metrics: &[(&str, f64)]) -> BenchmarkResult {
        BenchmarkResult {
            scenario: scenario.to_owned(),
            environment: RunEnvironment {
                os: "linux".to_owned(),
                cpu_count: 8,
                git_commit: Some("abc123".to_owned()),
                generated_at: "2026-09-01T00:00:00Z".to_owned(),
                trials: 10,
                cpu_model: None,
                total_memory_bytes: None,
                os_version: None,
                webview_runtime: None,
            },
            metrics: metrics
                .iter()
                .map(|(name, median)| {
                    (
                        (*name).to_owned(),
                        Stats {
                            count: 10,
                            min: *median,
                            max: *median,
                            mean: *median,
                            median: *median,
                            p95: *median,
                            stddev: 0.0,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn compare_flags_a_regression_beyond_threshold() {
        let baseline = result_with("cold_startup", &[("startup_first_load_ms", 100.0)]);
        let candidate = result_with("cold_startup", &[("startup_first_load_ms", 120.0)]);
        let report = compare(&baseline, &candidate, 10.0);
        let diff = &report.diffs["startup_first_load_ms"];
        assert_eq!(diff.pct_change, 20.0);
        assert!(diff.regressed);
        assert!(report.any_regressed);
    }

    #[test]
    fn compare_does_not_flag_an_improvement() {
        let baseline = result_with("cold_startup", &[("startup_first_load_ms", 100.0)]);
        let candidate = result_with("cold_startup", &[("startup_first_load_ms", 80.0)]);
        let report = compare(&baseline, &candidate, 10.0);
        let diff = &report.diffs["startup_first_load_ms"];
        assert_eq!(diff.pct_change, -20.0);
        assert!(!diff.regressed);
        assert!(!report.any_regressed);
    }

    #[test]
    fn compare_within_threshold_is_not_a_regression() {
        let baseline = result_with("cold_startup", &[("startup_first_load_ms", 100.0)]);
        let candidate = result_with("cold_startup", &[("startup_first_load_ms", 105.0)]);
        let report = compare(&baseline, &candidate, 10.0);
        assert!(!report.diffs["startup_first_load_ms"].regressed);
        assert!(!report.any_regressed);
    }

    #[test]
    fn compare_zero_baseline_with_nonzero_candidate_is_infinite_and_regressed() {
        let baseline = result_with("tab_create", &[("tab_create_ms", 0.0)]);
        let candidate = result_with("tab_create", &[("tab_create_ms", 5.0)]);
        let report = compare(&baseline, &candidate, 10.0);
        let diff = &report.diffs["tab_create_ms"];
        assert_eq!(diff.pct_change, f64::INFINITY);
        assert!(diff.regressed);
    }

    #[test]
    fn compare_zero_baseline_and_zero_candidate_is_unchanged() {
        let baseline = result_with("tab_create", &[("tab_create_ms", 0.0)]);
        let candidate = result_with("tab_create", &[("tab_create_ms", 0.0)]);
        let report = compare(&baseline, &candidate, 10.0);
        let diff = &report.diffs["tab_create_ms"];
        assert_eq!(diff.pct_change, 0.0);
        assert!(!diff.regressed);
    }

    #[test]
    fn compare_lists_metrics_present_on_only_one_side() {
        let baseline = result_with(
            "cold_startup",
            &[("startup_first_load_ms", 100.0), ("only_baseline", 1.0)],
        );
        let candidate = result_with(
            "cold_startup",
            &[("startup_first_load_ms", 100.0), ("only_candidate", 1.0)],
        );
        let report = compare(&baseline, &candidate, 10.0);
        assert_eq!(report.only_in_baseline, vec!["only_baseline".to_owned()]);
        assert_eq!(report.only_in_candidate, vec!["only_candidate".to_owned()]);
        assert_eq!(report.diffs.len(), 1);
    }

    #[test]
    fn compare_round_trips_through_json() {
        let baseline = result_with("cold_startup", &[("startup_first_load_ms", 100.0)]);
        let candidate = result_with("cold_startup", &[("startup_first_load_ms", 100.0)]);
        let report = compare(&baseline, &candidate, 5.0);
        let json = serde_json::to_string(&report).expect("serialize");
        let parsed: ComparisonReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, report);
    }

    #[test]
    fn benchmark_result_round_trips_through_json() {
        let result = result_with(
            "tabs_5",
            &[("rss_total_bytes", 123_456.0), ("rss_process_count", 6.0)],
        );
        let json = serde_json::to_string_pretty(&result).expect("serialize");
        let parsed: BenchmarkResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, result);
    }
}
