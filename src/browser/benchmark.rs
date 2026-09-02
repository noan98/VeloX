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
    StartupWindowCreatedMs,
    /// Right before `app::run` enters the event loop — see
    /// `metrics::StartupTimestamps::mark_rust_setup_done` and
    /// docs/decisions.md D42 (Issue #59). Splits `StartupWindowCreatedMs` →
    /// `StartupToolbarReadyMs` into "VeloX's own synchronous setup" vs.
    /// "inside the toolbar webview".
    StartupRustSetupDoneMs,
    /// The toolbar webview's inline script started executing — see
    /// `metrics::StartupTimestamps::mark_toolbar_script_started` and D42.
    StartupToolbarScriptStartedMs,
    StartupToolbarReadyMs,
    StartupFirstLoadMs,
    PageLoadMs,
    TabCreateMs,
    TabSwitchMs,
    RssTotalBytes,
    RssProcessCount,
}

impl MetricKey {
    /// Every metric key, in a stable order — used to build a
    /// [`BenchmarkResult::metrics`] map deterministically and to drive
    /// [`aggregate_trials`].
    pub const ALL: [MetricKey; 10] = [
        MetricKey::StartupWindowCreatedMs,
        MetricKey::StartupRustSetupDoneMs,
        MetricKey::StartupToolbarScriptStartedMs,
        MetricKey::StartupToolbarReadyMs,
        MetricKey::StartupFirstLoadMs,
        MetricKey::PageLoadMs,
        MetricKey::TabCreateMs,
        MetricKey::TabSwitchMs,
        MetricKey::RssTotalBytes,
        MetricKey::RssProcessCount,
    ];

    /// The key's name as stored in [`BenchmarkResult::metrics`] and printed
    /// by `velox-bench compare`.
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKey::StartupWindowCreatedMs => "startup_window_created_ms",
            MetricKey::StartupRustSetupDoneMs => "startup_rust_setup_done_ms",
            MetricKey::StartupToolbarScriptStartedMs => "startup_toolbar_script_started_ms",
            MetricKey::StartupToolbarReadyMs => "startup_toolbar_ready_ms",
            MetricKey::StartupFirstLoadMs => "startup_first_load_ms",
            MetricKey::PageLoadMs => "page_load_ms",
            MetricKey::TabCreateMs => "tab_create_ms",
            MetricKey::TabSwitchMs => "tab_switch_ms",
            MetricKey::RssTotalBytes => "rss_total_bytes",
            MetricKey::RssProcessCount => "rss_process_count",
        }
    }

    /// The `PerfRecord` `"event"` value this metric is read from.
    fn event_name(self) -> &'static str {
        match self {
            MetricKey::StartupWindowCreatedMs
            | MetricKey::StartupRustSetupDoneMs
            | MetricKey::StartupToolbarScriptStartedMs
            | MetricKey::StartupToolbarReadyMs
            | MetricKey::StartupFirstLoadMs => "startup",
            MetricKey::PageLoadMs => "page_load",
            MetricKey::TabCreateMs => "tab_create",
            MetricKey::TabSwitchMs => "tab_switch",
            MetricKey::RssTotalBytes | MetricKey::RssProcessCount => "rss",
        }
    }

    /// The JSON field this metric's numeric value lives in, within a
    /// matching event.
    fn field_name(self) -> &'static str {
        match self {
            MetricKey::StartupWindowCreatedMs => "window_created_ms",
            MetricKey::StartupRustSetupDoneMs => "rust_setup_done_ms",
            MetricKey::StartupToolbarScriptStartedMs => "toolbar_script_started_ms",
            MetricKey::StartupToolbarReadyMs => "toolbar_ready_ms",
            MetricKey::StartupFirstLoadMs => "first_load_ms",
            MetricKey::PageLoadMs => "duration_ms",
            MetricKey::TabCreateMs => "duration_ms",
            MetricKey::TabSwitchMs => "duration_ms",
            MetricKey::RssTotalBytes => "total_rss_bytes",
            MetricKey::RssProcessCount => "process_count",
        }
    }

    /// Pull every value for this metric out of `events` (one trial's worth
    /// of parsed [`parse_jsonl`] output). `tab_create`/`tab_switch` share
    /// the `"tab_create"`/`"tab_switch"` event names respectively (not both
    /// named `"tab_latency"`), so no extra disambiguation is needed beyond
    /// matching on `event_name()`.
    pub fn extract(self, events: &[Value]) -> Vec<f64> {
        events
            .iter()
            .filter(|event| event.get("event").and_then(Value::as_str) == Some(self.event_name()))
            .filter_map(|event| event.get(self.field_name()).and_then(Value::as_f64))
            .collect()
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
    let mut out = BTreeMap::new();
    for key in MetricKey::ALL {
        let mut values = Vec::new();
        for trial in trials {
            values.extend(key.extract(trial));
        }
        if let Some(stats) = compute_stats(&values) {
            out.insert(key.as_str().to_owned(), stats);
        }
    }
    out
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
        /// Memory (`rss_total_bytes`/`rss_process_count`) and CPU usage
        /// with exactly `tab_count` tabs open. `tab_count` is one of
        /// [`Scenario::TAB_COUNTS`].
        TabCountMemory(u32),
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
            ];
            scenarios.extend(
                Self::TAB_COUNTS
                    .iter()
                    .map(|&n| Scenario::TabCountMemory(n)),
            );
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
                Scenario::TabCountMemory(n) => format!("tabs_{n}"),
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
                other => other
                    .strip_prefix("tabs_")
                    .and_then(|rest| rest.parse::<u32>().ok())
                    .filter(|n| Self::TAB_COUNTS.contains(n))
                    .map(Scenario::TabCountMemory),
            }
        }

        /// Whether `velox-bench run` can drive this scenario unattended
        /// (launch the binary, wait, collect) versus needing a real display
        /// plus manual or externally-scripted interaction (opening tabs,
        /// navigating) before its log can be fed to `velox-bench
        /// aggregate`. See `docs/benchmarking.md`, "自動化できる範囲".
        pub fn is_unattended(self) -> bool {
            matches!(
                self,
                Scenario::ColdStartup | Scenario::WarmStartup | Scenario::FirstPageLoad
            )
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
        fn parse_rejects_unknown_tab_counts() {
            assert_eq!(Scenario::parse("tabs_7"), None);
            assert_eq!(Scenario::parse("tabs_"), None);
            assert_eq!(Scenario::parse("not_a_scenario"), None);
        }

        #[test]
        fn all_covers_six_fixed_plus_five_tab_count_scenarios() {
            assert_eq!(Scenario::all().len(), 6 + Scenario::TAB_COUNTS.len());
        }

        #[test]
        fn only_startup_and_first_page_load_are_unattended() {
            let unattended: Vec<_> = Scenario::all()
                .into_iter()
                .filter(|s| s.is_unattended())
                .map(Scenario::id)
                .collect();
            assert_eq!(
                unattended,
                vec!["cold_startup", "warm_startup", "first_page_load"]
            );
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
    fn extract_returns_empty_for_absent_metric() {
        let events = vec![event(
            r#"{"event":"page_load","url":"x","duration_ms":1.0}"#,
        )];
        assert!(MetricKey::TabCreateMs.extract(&events).is_empty());
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
        assert_eq!(aggregated.len(), 3);
    }

    #[test]
    fn aggregate_trials_ignores_a_trial_with_broken_lines_via_parse_jsonl() {
        let good = parse_jsonl("{\"event\":\"tab_switch\",\"tab_id\":1,\"duration_ms\":5.0}\n");
        let broken = parse_jsonl("not json\n\n");
        let aggregated = aggregate_trials(&[good, broken]);
        assert_eq!(aggregated["tab_switch_ms"].count, 1);
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
