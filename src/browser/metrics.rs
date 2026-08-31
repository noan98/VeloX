//! Performance measurement primitives: independent of any UI toolkit or web
//! engine, so the arithmetic/formatting/process-tree-walking logic here is
//! unit-testable without a window (see `docs/architecture.md`, "Performance
//! extension points").
//!
//! Three independent pieces, matching Issue #3:
//!
//! - [`StartupTimestamps`] — the four startup checkpoints (process start,
//!   window created, toolbar ready, first page loaded).
//! - [`PageLoadTimer`] — brackets one `NavigationStarted` .. `LoadFinished`
//!   pair into a [`Duration`].
//! - [`sample_process_tree_rss`] — a standalone, public function that
//!   samples the RSS of a process and all of its descendants. It does not
//!   depend on `Config` or the running app, so it can be called from
//!   anywhere (e.g. from a future tab-suspension feature verifying that
//!   suspending a tab actually shrinks the process tree).
//!
//! Enabling/disabling instrumentation is the caller's job (see
//! `Config::perf_metrics` and `app::run`): this module never reads env vars
//! or a `Config` itself, so callers can skip it entirely when metrics are
//! off, which keeps the off-path free of timestamp calls and thread spawns.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------
// Startup timestamps
// ---------------------------------------------------------------------

/// The four startup checkpoints from Issue #3: process start, window
/// creation, toolbar `ready`, and the first `LoadFinished` (≈
/// time-to-first-page).
#[derive(Debug, Clone, Copy)]
pub struct StartupTimestamps {
    process_start: Instant,
    window_created: Option<Instant>,
    toolbar_ready: Option<Instant>,
    first_load_finished: Option<Instant>,
}

impl StartupTimestamps {
    /// Start tracking from `process_start` (the earliest timestamp the
    /// caller could capture, ideally at the top of `main`).
    pub fn new(process_start: Instant) -> Self {
        Self {
            process_start,
            window_created: None,
            toolbar_ready: None,
            first_load_finished: None,
        }
    }

    /// Record the window-creation checkpoint. Only the first call counts.
    pub fn mark_window_created(&mut self, now: Instant) {
        self.window_created.get_or_insert(now);
    }

    /// Record the toolbar's first `ready` handshake. Only the first call
    /// counts.
    pub fn mark_toolbar_ready(&mut self, now: Instant) {
        self.toolbar_ready.get_or_insert(now);
    }

    /// Record the first `LoadFinished`. Only the first call counts.
    pub fn mark_first_load_finished(&mut self, now: Instant) {
        self.first_load_finished.get_or_insert(now);
    }

    /// Build a report of elapsed time from process start to each
    /// checkpoint. Returns `None` until all three post-start checkpoints
    /// have been recorded (order does not matter).
    pub fn report(&self) -> Option<StartupReport> {
        Some(StartupReport {
            to_window_created: self.window_created?.duration_since(self.process_start),
            to_toolbar_ready: self.toolbar_ready?.duration_since(self.process_start),
            to_first_load_finished: self.first_load_finished?.duration_since(self.process_start),
        })
    }
}

/// Elapsed time from process start to each startup checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupReport {
    pub to_window_created: Duration,
    pub to_toolbar_ready: Duration,
    pub to_first_load_finished: Duration,
}

impl fmt::Display for StartupReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "startup window_created={} toolbar_ready={} first_page={}",
            format_duration(self.to_window_created),
            format_duration(self.to_toolbar_ready),
            format_duration(self.to_first_load_finished),
        )
    }
}

/// Format a duration as fractional milliseconds, e.g. `"12.3ms"`.
pub fn format_duration(duration: Duration) -> String {
    format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
}

// ---------------------------------------------------------------------
// Page load timing
// ---------------------------------------------------------------------

/// Brackets one page load: `start()` on `NavigationStarted`, `finish()` on
/// `LoadFinished`. A fresh timer (or one that was never started) reports no
/// duration on `finish`.
#[derive(Debug, Default, Clone, Copy)]
pub struct PageLoadTimer {
    started_at: Option<Instant>,
}

impl PageLoadTimer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the start of a page load. A load already in flight (e.g. a
    /// redirect re-triggering navigation) is simply restarted from `now`.
    pub fn start(&mut self, now: Instant) {
        self.started_at = Some(now);
    }

    /// Mark the end of a page load, returning its duration if `start` was
    /// called first. Consumes the start mark, so a stray `finish` without a
    /// matching `start` (or a repeated `finish`) returns `None`.
    pub fn finish(&mut self, now: Instant) -> Option<Duration> {
        self.started_at
            .take()
            .map(|start| now.saturating_duration_since(start))
    }
}

/// Format a page-load log line for `url`.
pub fn format_page_load(url: &str, duration: Duration) -> String {
    format!("page_load url={url} duration={}", format_duration(duration))
}

// ---------------------------------------------------------------------
// Process-tree RSS sampling
// ---------------------------------------------------------------------

/// One RSS sample of a process and all of its descendants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RssSample {
    pub root_pid: u32,
    pub process_count: usize,
    pub total_rss_bytes: u64,
}

impl fmt::Display for RssSample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rss pid={} processes={} total_mib={:.1}",
            self.root_pid,
            self.process_count,
            self.total_rss_bytes as f64 / (1024.0 * 1024.0)
        )
    }
}

/// Failure modes for [`sample_process_tree_rss`].
#[derive(Debug)]
pub enum RssError {
    /// `root_pid` is not a currently running process.
    ProcessNotFound(u32),
    /// Reading process information failed.
    Io(io::Error),
    /// No RSS sampling strategy is implemented for this platform yet.
    Unsupported,
}

impl fmt::Display for RssError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RssError::ProcessNotFound(pid) => write!(f, "process {pid} not found"),
            RssError::Io(err) => write!(f, "I/O error reading process info: {err}"),
            RssError::Unsupported => {
                write!(f, "RSS sampling is not implemented on this platform")
            }
        }
    }
}

impl std::error::Error for RssError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RssError::Io(err) => Some(err),
            RssError::ProcessNotFound(_) | RssError::Unsupported => None,
        }
    }
}

/// Sample the resident set size of `root_pid` and every process descended
/// from it (WebKit splits into network/render/GPU helper processes, so a
/// meaningful memory measurement needs the whole tree, not just one PID).
///
/// This is a plain function with no dependency on [`crate::config::Config`]
/// or the running app: it can be called on demand from anywhere, e.g. a
/// benchmark, a test, or (per Issue #5) to compare RSS before/after
/// suspending a tab.
///
/// Platform support:
/// - Linux: reads `/proc` directly, no extra dependency.
/// - Other Unix (macOS, *BSD): shells out to `ps`, best-effort/untested by
///   this project's CI (Linux-only).
/// - Windows: not implemented yet; returns [`RssError::Unsupported`].
pub fn sample_process_tree_rss(root_pid: u32) -> Result<RssSample, RssError> {
    let processes = imp::process_map()?;
    build_sample(root_pid, &processes)
}

/// Minimal per-process info needed to walk the process tree and sum RSS.
#[derive(Debug, Clone, Copy)]
struct ProcInfo {
    ppid: u32,
    rss_bytes: u64,
}

/// Pure tree-walk + summation, independent of how `processes` was obtained
/// (real `/proc`, `ps` output, or synthetic data in tests).
fn build_sample(root_pid: u32, processes: &HashMap<u32, ProcInfo>) -> Result<RssSample, RssError> {
    if !processes.contains_key(&root_pid) {
        return Err(RssError::ProcessNotFound(root_pid));
    }
    let tree = collect_descendants(root_pid, processes);
    let total_rss_bytes = tree
        .iter()
        .filter_map(|pid| processes.get(pid))
        .map(|info| info.rss_bytes)
        .sum();
    Ok(RssSample {
        root_pid,
        process_count: tree.len(),
        total_rss_bytes,
    })
}

/// Breadth/depth-first collection of `root` and every transitive child, per
/// the `ppid` links in `processes`. Order is unspecified; `root` is always
/// first.
fn collect_descendants(root: u32, processes: &HashMap<u32, ProcInfo>) -> Vec<u32> {
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, info) in processes {
        children_of.entry(info.ppid).or_default().push(pid);
    }

    let mut result = vec![root];
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if let Some(children) = children_of.get(&pid) {
            for &child in children {
                result.push(child);
                stack.push(child);
            }
        }
    }
    result
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{HashMap, ProcInfo};
    use std::fs;

    /// Build a `pid -> (ppid, rss_bytes)` map for every process currently
    /// visible under `/proc`. Processes that exit mid-scan are silently
    /// skipped rather than treated as an error — RSS sampling is inherently
    /// a snapshot of a moving target.
    pub(super) fn process_map() -> Result<HashMap<u32, ProcInfo>, super::RssError> {
        let mut map = HashMap::new();
        for entry in fs::read_dir("/proc").map_err(super::RssError::Io)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue; // not a PID directory (e.g. "self", "net")
            };
            let Ok(contents) = fs::read_to_string(entry.path().join("status")) else {
                continue; // process exited between listing and reading
            };
            if let Some(info) = parse_status(&contents) {
                map.insert(pid, info);
            }
        }
        Ok(map)
    }

    /// Parse the `PPid:` and `VmRSS:` fields out of `/proc/<pid>/status`
    /// text. Missing `PPid:` means we cannot place this process in the
    /// tree, so it is skipped entirely; missing `VmRSS:` (e.g. a zombie)
    /// defaults to zero bytes rather than dropping the process.
    fn parse_status(contents: &str) -> Option<ProcInfo> {
        let mut ppid = None;
        let mut rss_kb = None;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                ppid = rest.trim().parse::<u32>().ok();
            } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                // e.g. "   1234 kB"
                rss_kb = rest
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok());
            }
        }
        Some(ProcInfo {
            ppid: ppid?,
            rss_bytes: rss_kb.unwrap_or(0) * 1024,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_ppid_and_rss_from_status_text() {
            let text = "Name:\tbash\nState:\tS (sleeping)\nPPid:\t1234\nVmRSS:\t   4567 kB\n";
            let info = parse_status(text).unwrap();
            assert_eq!(info.ppid, 1234);
            assert_eq!(info.rss_bytes, 4567 * 1024);
        }

        #[test]
        fn missing_ppid_yields_none() {
            let text = "Name:\tbash\nVmRSS:\t100 kB\n";
            assert!(parse_status(text).is_none());
        }

        #[test]
        fn missing_vmrss_defaults_to_zero() {
            let text = "Name:\tbash\nPPid:\t1\n";
            let info = parse_status(text).unwrap();
            assert_eq!(info.rss_bytes, 0);
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod imp {
    use super::{HashMap, ProcInfo};
    use std::process::Command;

    /// Best-effort fallback for non-Linux Unix (macOS, *BSD): parse `ps`
    /// output instead of `/proc`. Untested by this project's CI, which
    /// runs on Linux only; kept intentionally simple.
    pub(super) fn process_map() -> Result<HashMap<u32, ProcInfo>, super::RssError> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,ppid=,rss="])
            .output()
            .map_err(super::RssError::Io)?;
        if !output.status.success() {
            return Err(super::RssError::Unsupported);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_ps_output(&text))
    }

    fn parse_ps_output(text: &str) -> HashMap<u32, ProcInfo> {
        let mut map = HashMap::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(ppid), Some(rss_kb)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Ok(pid), Ok(ppid), Ok(rss_kb)) = (
                pid.parse::<u32>(),
                ppid.parse::<u32>(),
                rss_kb.parse::<u64>(),
            ) else {
                continue;
            };
            map.insert(
                pid,
                ProcInfo {
                    ppid,
                    rss_bytes: rss_kb * 1024,
                },
            );
        }
        map
    }
}

#[cfg(not(any(target_os = "linux", unix)))]
mod imp {
    use super::{HashMap, ProcInfo};

    /// No sampling strategy implemented yet (e.g. Windows). Callers get a
    /// clean [`super::RssError::Unsupported`] instead of a panic.
    pub(super) fn process_map() -> Result<HashMap<u32, ProcInfo>, super::RssError> {
        Err(super::RssError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- StartupTimestamps ------------------------------------------------

    #[test]
    fn report_is_none_until_all_checkpoints_recorded() {
        let start = Instant::now();
        let mut timestamps = StartupTimestamps::new(start);
        assert!(timestamps.report().is_none());

        timestamps.mark_window_created(Instant::now());
        assert!(timestamps.report().is_none());

        timestamps.mark_toolbar_ready(Instant::now());
        assert!(timestamps.report().is_none());

        timestamps.mark_first_load_finished(Instant::now());
        assert!(timestamps.report().is_some());
    }

    #[test]
    fn report_order_of_checkpoints_does_not_matter() {
        let start = Instant::now();
        let mut timestamps = StartupTimestamps::new(start);
        timestamps.mark_first_load_finished(Instant::now());
        timestamps.mark_toolbar_ready(Instant::now());
        timestamps.mark_window_created(Instant::now());
        assert!(timestamps.report().is_some());
    }

    #[test]
    fn only_the_first_mark_counts() {
        let start = Instant::now();
        let mut timestamps = StartupTimestamps::new(start);
        timestamps.mark_window_created(start);
        let later = start + Duration::from_secs(10);
        timestamps.mark_window_created(later); // ignored, already set
        timestamps.mark_toolbar_ready(start);
        timestamps.mark_first_load_finished(start);
        let report = timestamps.report().unwrap();
        assert_eq!(report.to_window_created, Duration::ZERO);
    }

    #[test]
    fn format_duration_renders_fractional_milliseconds() {
        assert_eq!(format_duration(Duration::from_micros(1500)), "1.5ms");
        assert_eq!(format_duration(Duration::ZERO), "0.0ms");
    }

    // -- PageLoadTimer ------------------------------------------------

    #[test]
    fn finish_without_start_is_none() {
        let mut timer = PageLoadTimer::new();
        assert_eq!(timer.finish(Instant::now()), None);
    }

    #[test]
    fn start_then_finish_reports_a_duration() {
        let mut timer = PageLoadTimer::new();
        let start = Instant::now();
        timer.start(start);
        let end = start + Duration::from_millis(42);
        assert_eq!(timer.finish(end), Some(Duration::from_millis(42)));
    }

    #[test]
    fn finish_consumes_the_start_mark() {
        let mut timer = PageLoadTimer::new();
        let start = Instant::now();
        timer.start(start);
        assert!(timer.finish(start).is_some());
        assert_eq!(timer.finish(start), None);
    }

    #[test]
    fn restarting_before_finish_discards_the_earlier_start() {
        let mut timer = PageLoadTimer::new();
        let first_start = Instant::now();
        timer.start(first_start);
        let second_start = first_start + Duration::from_millis(100);
        timer.start(second_start); // e.g. a redirect re-triggered navigation
        let end = second_start + Duration::from_millis(10);
        assert_eq!(timer.finish(end), Some(Duration::from_millis(10)));
    }

    #[test]
    fn page_load_line_includes_url_and_duration() {
        let line = format_page_load("https://example.com/", Duration::from_millis(250));
        assert_eq!(line, "page_load url=https://example.com/ duration=250.0ms");
    }

    // -- RSS: pure tree-walk / summation ------------------------------

    #[test]
    fn build_sample_sums_rss_over_whole_subtree() {
        let mut processes = HashMap::new();
        processes.insert(
            1,
            ProcInfo {
                ppid: 0,
                rss_bytes: 1000,
            },
        );
        processes.insert(
            2,
            ProcInfo {
                ppid: 1,
                rss_bytes: 2000,
            },
        ); // child of 1
        processes.insert(
            3,
            ProcInfo {
                ppid: 2,
                rss_bytes: 3000,
            },
        ); // grandchild
        processes.insert(
            4,
            ProcInfo {
                ppid: 0,
                rss_bytes: 4000,
            },
        ); // unrelated

        let sample = build_sample(1, &processes).unwrap();
        assert_eq!(sample.root_pid, 1);
        assert_eq!(sample.process_count, 3);
        assert_eq!(sample.total_rss_bytes, 6000);
    }

    #[test]
    fn build_sample_root_with_no_children_counts_only_itself() {
        let mut processes = HashMap::new();
        processes.insert(
            5,
            ProcInfo {
                ppid: 0,
                rss_bytes: 999,
            },
        );
        let sample = build_sample(5, &processes).unwrap();
        assert_eq!(sample.process_count, 1);
        assert_eq!(sample.total_rss_bytes, 999);
    }

    #[test]
    fn build_sample_errors_for_unknown_root() {
        let processes = HashMap::new();
        assert!(matches!(
            build_sample(99, &processes),
            Err(RssError::ProcessNotFound(99))
        ));
    }

    #[test]
    fn rss_sample_display_reports_mib() {
        let sample = RssSample {
            root_pid: 42,
            process_count: 3,
            total_rss_bytes: 2 * 1024 * 1024,
        };
        assert_eq!(sample.to_string(), "rss pid=42 processes=3 total_mib=2.0");
    }

    // -- RSS: real /proc integration (Linux only, matches this project's CI) --

    #[cfg(target_os = "linux")]
    #[test]
    fn samples_own_process_from_real_proc() {
        let pid = std::process::id();
        let sample = sample_process_tree_rss(pid).expect("sampling the current process");
        assert_eq!(sample.root_pid, pid);
        assert!(sample.process_count >= 1);
        assert!(sample.total_rss_bytes > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn samples_pick_up_a_freshly_spawned_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("spawn a child process");
        // Give /proc a moment to expose the new entry.
        std::thread::sleep(Duration::from_millis(100));

        let pid = std::process::id();
        let sample = sample_process_tree_rss(pid).expect("sampling self + child");
        assert!(
            sample.process_count >= 2,
            "expected self + at least one child, got {}",
            sample.process_count
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_pid_is_reported_as_not_found() {
        let result = sample_process_tree_rss(u32::MAX);
        assert!(matches!(result, Err(RssError::ProcessNotFound(pid)) if pid == u32::MAX));
    }
}
