//! Performance measurement primitives: independent of any UI toolkit or web
//! engine, so the arithmetic/formatting/process-tree-walking logic here is
//! unit-testable without a window (see `docs/architecture.md`, "Performance
//! extension points").
//!
//! Pieces, matching Issue #3 (the four independent ones) and Issue #13 (the
//! two tab-latency additions, plus [`PerfRecord`]/[`PerfFormat`] unifying
//! all five into one loggable shape):
//!
//! - [`StartupTimestamps`] — the four startup checkpoints (process start,
//!   window created, toolbar ready, first page loaded).
//! - [`PageLoadTimer`] — brackets one `NavigationStarted` .. `LoadFinished`
//!   pair into a [`Duration`].
//! - [`TabLatencyKind`] — tags a tab-create or tab-switch [`Duration`],
//!   measured by the caller bracketing `Instant::now()` around the
//!   synchronous webview call in `app.rs` (no timer type needed here, unlike
//!   `PageLoadTimer`: start and finish happen in the same call stack).
//! - [`sample_process_tree_rss`] — a standalone, public function that
//!   samples the RSS of a process and all of its descendants. It does not
//!   depend on `Config` or the running app, so it can be called from
//!   anywhere (e.g. from a future tab-suspension feature verifying that
//!   suspending a tab actually shrinks the process tree).
//! - [`PerfRecord`] — wraps any of the above into one event with a name and
//!   a monotonic timestamp, rendered as either the original plain-text line
//!   ([`PerfRecord::to_text`]) or a JSON Lines record
//!   ([`PerfRecord::to_json_line`]) per [`PerfFormat`]. See
//!   `docs/architecture.md`, "Performance extension points" for the schema
//!   Issue #14's benchmark runner is expected to parse.
//!
//! Enabling/disabling instrumentation is the caller's job (see
//! `Config::perf_metrics` and `app::run`): this module never reads env vars
//! or a `Config` itself, so callers can skip it entirely when metrics are
//! off, which keeps the off-path free of timestamp calls and thread spawns.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use serde_json::json;

// ---------------------------------------------------------------------
// Startup timestamps
// ---------------------------------------------------------------------

/// The startup checkpoints: process start, four sub-checkpoints splitting
/// the `process_start` → `window_created` gap (Issue #182 — see
/// docs/decisions.md D92), window creation, two sub-checkpoints splitting
/// the `window_created` → `toolbar_ready` gap (Issue #59 — see
/// docs/decisions.md D43), the toolbar's `ready` handshake, and the first
/// `LoadFinished` (≈ time-to-first-page).
///
/// The `window_created` → `toolbar_ready` pair (`rust_setup_done`,
/// `toolbar_script_started`) exists to answer one question: is that gap
/// spent in VeloX's own Rust-side setup (persistence I/O, building
/// `AppState`) before the event loop even starts pumping the webview, or
/// inside the toolbar webview itself (HTML/CSS parse, JS execution)? See
/// D43 for the measurement this was built to answer.
///
/// The `process_start` → `window_created` quartet (`event_loop_built`,
/// `pre_window_setup_done`, `native_window_built`, `toolbar_webview_built`)
/// answers the equivalent question for the *other*, much larger half of
/// startup, which Windows' first real measurement (Issue #136/#180,
/// `docs/performance-targets.md` §21) showed to be 644ms of a 717ms
/// `startup_first_load_ms`: how much of it is VeloX's own Rust work
/// (improvable here) versus toolkit/engine initialization that Epic #57
/// rule 3 treats as a black box (tao's event loop and native window,
/// WebView2/WebKitGTK's first webview)? See D92.
///
/// **Only the original five checkpoints are required for [`report`] to
/// produce a record.** The four Issue #182 additions are optional in
/// [`StartupReport`] precisely so that a future refactor that forgets to
/// mark one can never suppress the whole `startup` perf record — and with
/// it every startup metric `velox-bench` reads.
///
/// [`report`]: StartupTimestamps::report
#[derive(Debug, Clone, Copy)]
pub struct StartupTimestamps {
    process_start: Instant,
    /// `EventLoopBuilder::with_user_event().build()` has returned — i.e.
    /// tao has finished whatever the platform needs before any window can
    /// exist (`gtk_init` on Linux; window-class registration, OLE/COM and
    /// DPI-awareness setup on Windows). Nothing of VeloX's own runs before
    /// this, so it is a floor on startup that VeloX cannot move without
    /// changing or patching tao.
    event_loop_built: Option<Instant>,
    /// Every Rust-side prerequisite of the first window is ready and
    /// `BrowserWindow::new` is about to be called: `settings.json` loaded
    /// and applied, blocklist and site-exception lists built, site
    /// permissions and the restored session read from disk, `Windows`/`Tabs`
    /// constructed. This span (`event_loop_built` → here) is the one part of
    /// `process_start` → `window_created` that is entirely VeloX's own code.
    pre_window_setup_done: Option<Instant>,
    /// tao's `WindowBuilder::build()` has returned — the native window
    /// (HWND / GtkWindow) exists, icon loaded, but no webview is attached to
    /// it yet.
    native_window_built: Option<Instant>,
    /// The toolbar webview — the *first* webview of the process — has been
    /// attached. This span (`native_window_built` → here) is where the web
    /// engine pays its one-time initialization cost (on Windows: creating
    /// the WebView2 environment and spawning `msedgewebview2.exe`; on Linux:
    /// WebKitGTK's first `build_gtk`). Black box per Epic #57 rule 3.
    toolbar_webview_built: Option<Instant>,
    window_created: Option<Instant>,
    /// Right before `app::run` calls `event_loop.run(...)` — after
    /// history/bookmarks/input-history have been loaded from disk and
    /// `AppState` is built. Everything between this and `window_created` is
    /// synchronous Rust code that runs before the GTK/webview event loop
    /// even starts pumping.
    rust_setup_done: Option<Instant>,
    /// The toolbar webview's inline `<script>` has started executing (sent
    /// as the very first statement, see `ui/toolbar.html`) — i.e. the
    /// document's HTML markup and `<style>` block have already been parsed
    /// by the engine. Everything between this and `toolbar_ready` is the
    /// toolbar's own JS running (DOM lookups, initial render calls) plus
    /// the IPC round-trip back to Rust.
    toolbar_script_started: Option<Instant>,
    toolbar_ready: Option<Instant>,
    first_load_finished: Option<Instant>,
}

impl StartupTimestamps {
    /// Start tracking from `process_start` (the earliest timestamp the
    /// caller could capture, ideally at the top of `main`).
    pub fn new(process_start: Instant) -> Self {
        Self {
            process_start,
            event_loop_built: None,
            pre_window_setup_done: None,
            native_window_built: None,
            toolbar_webview_built: None,
            window_created: None,
            rust_setup_done: None,
            toolbar_script_started: None,
            toolbar_ready: None,
            first_load_finished: None,
        }
    }

    /// Record the "tao event loop built" checkpoint (Issue #182). Only the
    /// first call counts.
    ///
    /// Unlike every other `mark_*` here, the caller has to capture this
    /// `Instant` *before* it can know whether metrics are enabled at all —
    /// `app::run` builds the event loop before `settings.json` (which can
    /// flip `perf_metrics`) has been read. See its call site.
    pub fn mark_event_loop_built(&mut self, now: Instant) {
        self.event_loop_built.get_or_insert(now);
    }

    /// Record the "Rust-side prerequisites of the first window are ready"
    /// checkpoint (Issue #182). Only the first call counts.
    pub fn mark_pre_window_setup_done(&mut self, now: Instant) {
        self.pre_window_setup_done.get_or_insert(now);
    }

    /// Record the "tao native window built" checkpoint (Issue #182). Only
    /// the first call counts.
    pub fn mark_native_window_built(&mut self, now: Instant) {
        self.native_window_built.get_or_insert(now);
    }

    /// Record the "first (toolbar) webview attached" checkpoint (Issue
    /// #182). Only the first call counts.
    pub fn mark_toolbar_webview_built(&mut self, now: Instant) {
        self.toolbar_webview_built.get_or_insert(now);
    }

    /// Record the window-creation checkpoint. Only the first call counts.
    pub fn mark_window_created(&mut self, now: Instant) {
        self.window_created.get_or_insert(now);
    }

    /// Record the "Rust-side setup finished, about to enter the event loop"
    /// checkpoint. Only the first call counts.
    pub fn mark_rust_setup_done(&mut self, now: Instant) {
        self.rust_setup_done.get_or_insert(now);
    }

    /// Record the toolbar's inline script starting to execute. Only the
    /// first call counts.
    pub fn mark_toolbar_script_started(&mut self, now: Instant) {
        self.toolbar_script_started.get_or_insert(now);
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
    /// checkpoint. Returns `None` until every *required* post-start
    /// checkpoint has been recorded (order does not matter).
    ///
    /// "Required" means the original five (Issue #3/#59). The four Issue
    /// #182 sub-checkpoints of `process_start` → `window_created` are
    /// reported as `Option<Duration>` and a missing one is simply absent
    /// from the record: they are diagnostics layered onto an existing
    /// measurement, and must never be able to withhold the record that
    /// `velox-bench` reads every startup metric from.
    pub fn report(&self) -> Option<StartupReport> {
        let elapsed = |at: Option<Instant>| at.map(|at| at.duration_since(self.process_start));
        Some(StartupReport {
            to_event_loop_built: elapsed(self.event_loop_built),
            to_pre_window_setup_done: elapsed(self.pre_window_setup_done),
            to_native_window_built: elapsed(self.native_window_built),
            to_toolbar_webview_built: elapsed(self.toolbar_webview_built),
            to_window_created: self.window_created?.duration_since(self.process_start),
            to_rust_setup_done: self.rust_setup_done?.duration_since(self.process_start),
            to_toolbar_script_started: self
                .toolbar_script_started?
                .duration_since(self.process_start),
            to_toolbar_ready: self.toolbar_ready?.duration_since(self.process_start),
            to_first_load_finished: self.first_load_finished?.duration_since(self.process_start),
        })
    }
}

/// Elapsed time from process start to each startup checkpoint.
///
/// The four `Option` fields are Issue #182's decomposition of
/// `process_start` → `to_window_created`; see [`StartupTimestamps`] for what
/// each one brackets and why they are optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupReport {
    pub to_event_loop_built: Option<Duration>,
    pub to_pre_window_setup_done: Option<Duration>,
    pub to_native_window_built: Option<Duration>,
    pub to_toolbar_webview_built: Option<Duration>,
    pub to_window_created: Duration,
    pub to_rust_setup_done: Duration,
    pub to_toolbar_script_started: Duration,
    pub to_toolbar_ready: Duration,
    pub to_first_load_finished: Duration,
}

impl fmt::Display for StartupReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Issue #182's four sub-checkpoints render as `-` when absent
        // rather than being dropped, so the field list of this line stays
        // the same shape every time — a human diffing two `VELOX_PERF_OUTPUT`
        // files (the `text` format's only consumer) can line them up.
        let optional = |at: Option<Duration>| match at {
            Some(at) => format_duration(at),
            None => "-".to_owned(),
        };
        write!(
            f,
            "startup event_loop={} pre_window_setup={} native_window={} toolbar_webview={} ",
            optional(self.to_event_loop_built),
            optional(self.to_pre_window_setup_done),
            optional(self.to_native_window_built),
            optional(self.to_toolbar_webview_built),
        )?;
        write!(
            f,
            "window_created={} rust_setup_done={} toolbar_script_started={} \
             toolbar_ready={} first_page={}",
            format_duration(self.to_window_created),
            format_duration(self.to_rust_setup_done),
            format_duration(self.to_toolbar_script_started),
            format_duration(self.to_toolbar_ready),
            format_duration(self.to_first_load_finished),
        )
    }
}

/// The two checkpoints Issue #182 needs from *inside* window construction,
/// filled in by `ui::BrowserWindow::new` and folded into
/// [`StartupTimestamps`] by its caller.
///
/// A plain out-parameter rather than passing `&mut StartupTimestamps` down
/// into `ui::` for two reasons. First, layering: `ui::BrowserWindow` is the
/// one type that owns `tao`/`wry` handles, and this module is deliberately
/// UI-toolkit-independent — a `Default`-constructed pair of `Option<Instant>`
/// keeps the dependency pointing one way. Second, lifetime: `BrowserWindow`
/// is built for *every* window (`app::open_new_window`, Ctrl/Cmd+N), but
/// only the process's first one is part of startup; `open_new_window` passes
/// `None` and pays nothing, exactly like the `ipc_log: Option<IpcLog>`
/// parameter next to it.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowBuildTimings {
    /// tao's `WindowBuilder::build()` returned.
    pub native_window_built: Option<Instant>,
    /// The toolbar (first) webview was attached.
    pub toolbar_webview_built: Option<Instant>,
}

/// Format a duration as fractional milliseconds, e.g. `"12.3ms"`.
pub fn format_duration(duration: Duration) -> String {
    format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
}

// ---------------------------------------------------------------------
// Page load timing
// ---------------------------------------------------------------------

/// Brackets one page load: `start()` on `NavigationStarted`, an optional
/// mid-checkpoint `mark_load_started()` on `LoadStarted`
/// (`PageLoadEvent::Started` — WebKitGTK's `LoadEvent::Committed`, see
/// `docs/decisions.md` D87), `finish()` on `LoadFinished`. A fresh timer (or
/// one that was never started) reports no outcome on `finish`.
///
/// The mid-checkpoint exists to answer Issue #69's "段階的な計測" acceptance
/// criterion: split the one `NavigationStarted` → `LoadFinished` span this
/// project measured through Issue #13 into `NavigationStarted` →
/// `LoadStarted` ([`PageLoadOutcome::dispatch`]) and `LoadStarted` →
/// `LoadFinished` ([`PageLoadOutcome::engine`] — resource
/// loading/parsing/rendering after the load is committed, black-box per
/// Epic #57 rule 3). `LoadStarted` is optional and best-effort: some loads
/// never reach it (e.g. a load cancelled after `NavigationStarted`), so
/// `engine` is `None` in that case rather than a misleading `0`.
///
/// **`dispatch` is *not* "VeloX's own cost".** `NavigationStarted` fires
/// from `with_navigation_handler`, before the engine has done any network
/// work for this load; `LoadStarted` (WebKitGTK's `LoadEvent::Committed`)
/// fires only once the engine has connected, sent the request, and started
/// receiving the response — so `dispatch` bundles VeloX's own event
/// handling (`app::record_perf_event`'s `NavigationStarted`/`LoadStarted`
/// arms, `sync_tab_strip`) together with the engine's connect/request/
/// response-head time, and this project measured `dispatch` as the
/// *larger* of the two halves even against a loopback HTTP server with no
/// real DNS/TLS cost (`docs/decisions.md` D87). Cross-reference Issue #66's
/// `ipc` timing (sub-millisecond even in the worst case measured) to see
/// that VeloX's own Rust-side share of `dispatch` is small — the rest is
/// engine/network time this project cannot attribute away from the engine.
#[derive(Debug, Default, Clone, Copy)]
pub struct PageLoadTimer {
    started_at: Option<Instant>,
    load_started_at: Option<Instant>,
}

impl PageLoadTimer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the start of a page load. A load already in flight (e.g. a
    /// redirect re-triggering navigation) is simply restarted from `now`,
    /// discarding any `load_started` mark it had picked up so far.
    pub fn start(&mut self, now: Instant) {
        self.started_at = Some(now);
        self.load_started_at = None;
    }

    /// Mark the `LoadStarted` mid-checkpoint. A no-op if `start` was never
    /// called (this timer is not tracking a load right now) or if this
    /// checkpoint already fired since the last `start` — only the first
    /// call per load counts, matching [`StartupTimestamps`]'s
    /// `get_or_insert` pattern.
    pub fn mark_load_started(&mut self, now: Instant) {
        if self.started_at.is_some() {
            self.load_started_at.get_or_insert(now);
        }
    }

    /// Mark the end of a page load, returning its outcome if `start` was
    /// called first. Consumes both marks, so a stray `finish` without a
    /// matching `start` (or a repeated `finish`) returns `None`.
    pub fn finish(&mut self, now: Instant) -> Option<PageLoadOutcome> {
        let start = self.started_at.take()?;
        let load_started_at = self.load_started_at.take();
        Some(PageLoadOutcome {
            total: now.saturating_duration_since(start),
            engine: load_started_at.map(|load_started| now.saturating_duration_since(load_started)),
        })
    }
}

/// One page load's timing, as reported by [`PageLoadTimer::finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLoadOutcome {
    /// `NavigationStarted` → `LoadFinished`, unchanged from what this
    /// project has always reported as `page_load_ms`.
    pub total: Duration,
    /// `LoadStarted` → `LoadFinished`, when `LoadStarted` fired for this
    /// load. This is the portion Epic #57 rule 3 puts off-limits (engine
    /// resource loading/parsing/rendering).
    pub engine: Option<Duration>,
}

impl PageLoadOutcome {
    /// `total` minus `engine`, when `engine` is known — **not** a
    /// VeloX-attributable duration on its own, see [`PageLoadTimer`]'s doc
    /// comment. Not simply "the rest" when `engine` is `None` — there is
    /// nothing to subtract from, so this is `None` too rather than silently
    /// reporting the full `total` as `dispatch` time.
    pub fn dispatch(&self) -> Option<Duration> {
        self.engine.map(|engine| self.total.saturating_sub(engine))
    }
}

/// Format a page-load log line for `url`.
pub fn format_page_load(url: &str, duration: Duration) -> String {
    format!("page_load url={url} duration={}", format_duration(duration))
}

// ---------------------------------------------------------------------
// Process-tree RSS sampling
// ---------------------------------------------------------------------

/// One RSS/PSS sample of a process and all of its descendants.
///
/// The RSS side (`total_rss_bytes`) always succeeds when the tree itself
/// could be walked at all (see [`RssError`]) — it comes from the same
/// `/proc/<pid>/status` read that finds `PPid:`. The PSS side
/// (`total_pss_bytes`/`pss_process_count`) is best-effort on top of that: a
/// process's proportional share of shared memory
/// (`/proc/<pid>/smaps_rollup`'s `Pss:` line) requires a newer kernel and
/// enough privilege to read it, neither of which is guaranteed. See D42 in
/// `docs/decisions.md` for why PSS — not summed RSS — is the right total
/// when comparing browsers with different process counts.
// No `Eq`: `total_cpu_seconds` is an `f64` (Issue #64). `PartialEq` is
// kept for the tests that compare whole samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RssSample {
    pub root_pid: u32,
    /// Total number of processes in the tree (root plus every descendant),
    /// all of which contributed to `total_rss_bytes`.
    pub process_count: usize,
    pub total_rss_bytes: u64,
    /// Sum of PSS over every process in the tree whose `smaps_rollup` could
    /// be read. `None` means *none* of the `process_count` processes could
    /// be read (no Linux `smaps_rollup` support, a permissions failure, or a
    /// non-Linux platform) — callers must not treat `None` as "zero PSS".
    /// When `Some` but `pss_process_count < process_count`, the sum is a
    /// genuine but incomplete total: some processes' shared-memory share is
    /// simply missing from it rather than being counted as zero.
    pub total_pss_bytes: Option<u64>,
    /// How many of `process_count` processes contributed to
    /// `total_pss_bytes`. Compare against `process_count` to tell a
    /// complete PSS total from a partial one.
    pub pss_process_count: usize,
    /// Total CPU time (user + system) consumed by the whole tree since each
    /// process started, in seconds (Issue #64). Cumulative, not a rate:
    /// subtract two samples and divide by the wall time between them to get
    /// a percentage — `spawn_rss_sampler` does exactly that and logs
    /// `cpu_percent` alongside.
    ///
    /// `None` when no process's CPU time could be read (a non-Linux
    /// platform, where the `ps` fallback does not provide it). A process
    /// that exits between two samples takes its accumulated time with it,
    /// which can make a delta *negative*; callers must clamp rather than
    /// assume monotonicity.
    pub total_cpu_seconds: Option<f64>,
}

impl fmt::Display for RssSample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rss pid={} processes={} total_mib={:.1} pss_processes={}/{}",
            self.root_pid,
            self.process_count,
            self.total_rss_bytes as f64 / (1024.0 * 1024.0),
            self.pss_process_count,
            self.process_count,
        )?;
        match self.total_pss_bytes {
            Some(bytes) => write!(f, " pss_mib={:.1}", bytes as f64 / (1024.0 * 1024.0))?,
            None => write!(f, " pss_mib=n/a")?,
        }
        // Appended, never inserted — same rule D42 followed for the PSS
        // fields, so a scraper matching the original prefix keeps working.
        match self.total_cpu_seconds {
            Some(secs) => write!(f, " cpu_s={secs:.2}"),
            None => write!(f, " cpu_s=n/a"),
        }
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

/// Sample the resident set size *and* proportional set size of `root_pid`
/// and every process descended from it (WebKit splits into network/render/
/// GPU helper processes, so a meaningful memory measurement needs the whole
/// tree, not just one PID).
///
/// RSS sums each process's resident pages, counting shared pages (e.g. a
/// shared library mapped into every helper process) once *per process* — a
/// browser with more helper processes looks heavier than one with fewer,
/// even at equal real memory use. PSS divides each shared page by its
/// sharer count before summing, so it does not inflate with process count;
/// see D42 in `docs/decisions.md` for a measured case where this reverses
/// which of two browsers looks lighter. RSS is kept alongside PSS in
/// [`RssSample`] anyway: it is always available (see below), and dropping
/// it would leave no memory number at all on a platform/kernel where PSS
/// cannot be read.
///
/// This is a plain function with no dependency on [`crate::config::Config`]
/// or the running app: it can be called on demand from anywhere, e.g. a
/// benchmark, a test, or (per Issue #5) to compare RSS before/after
/// suspending a tab.
///
/// Platform support:
/// - Linux: reads `/proc` directly, no extra dependency. RSS comes from
///   `/proc/<pid>/status` (`VmRSS:`) and is always available for any
///   process this can see at all. PSS comes from
///   `/proc/<pid>/smaps_rollup` (`Pss:`), which needs a kernel new enough
///   to have it (Linux ≥ 4.14) and permission to read it; when a given
///   process's `smaps_rollup` cannot be read, that process is simply
///   excluded from [`RssSample::total_pss_bytes`] (see its field docs) —
///   sampling never fails or falls back to treating it as zero.
/// - Other Unix (macOS, *BSD): shells out to `ps` for RSS,
///   best-effort/untested by this project's CI (Linux-only). PSS has no
///   equivalent here, so `total_pss_bytes` is always `None`.
/// - Windows (Issue #136, D88 in `docs/decisions.md`): `CreateToolhelp32Snapshot`
///   walks the process tree (PID/PPID), `GetProcessMemoryInfo`'s
///   `WorkingSetSize` gives RSS, and `GetProcessTimes` gives CPU time — all
///   best-effort per process (a process this project's own, non-elevated
///   process cannot `OpenProcess` simply contributes no RSS/CPU, mirroring
///   the Linux/`ps` fallbacks' "exclude, don't fail the whole sample"
///   policy). **PSS has no Windows equivalent and is not attempted**:
///   `total_pss_bytes` is always `None` here, exactly as on non-Linux Unix.
///   See D88 for why (a self-computed `QueryWorkingSetEx` approximation was
///   investigated and rejected for this project, not attempted here) — and
///   note its warning that a Windows PSS-shaped number must never be
///   compared against a Linux PSS number even if one existed, since the two
///   would be computed by entirely different methods.
pub fn sample_process_tree_rss(root_pid: u32) -> Result<RssSample, RssError> {
    let processes = imp::process_map(root_pid)?;
    build_sample(root_pid, &processes)
}

/// Minimal per-process info needed to walk the process tree and sum
/// RSS/PSS. `pss_bytes` is `None` when this process's `smaps_rollup`
/// couldn't be read (unsupported platform, old kernel, permissions) — RSS
/// has no such gap since it comes from `status`, which every visible `/proc`
/// entry has.
#[derive(Debug, Clone, Copy)]
struct ProcInfo {
    ppid: u32,
    rss_bytes: u64,
    pss_bytes: Option<u64>,
    /// User + system CPU time this process has used since it started, in
    /// seconds. `None` where the platform does not provide it (see
    /// [`RssSample::total_cpu_seconds`]).
    cpu_seconds: Option<f64>,
}

/// Pure tree-walk + summation, independent of how `processes` was obtained
/// (real `/proc`, `ps` output, or synthetic data in tests).
fn build_sample(root_pid: u32, processes: &HashMap<u32, ProcInfo>) -> Result<RssSample, RssError> {
    if !processes.contains_key(&root_pid) {
        return Err(RssError::ProcessNotFound(root_pid));
    }
    let tree = collect_descendants(root_pid, processes);
    let mut total_rss_bytes = 0u64;
    let mut total_pss_bytes = 0u64;
    let mut pss_process_count = 0usize;
    let mut total_cpu_seconds = 0.0f64;
    let mut cpu_process_count = 0usize;
    for info in tree.iter().filter_map(|pid| processes.get(pid)) {
        total_rss_bytes += info.rss_bytes;
        if let Some(pss) = info.pss_bytes {
            total_pss_bytes += pss;
            pss_process_count += 1;
        }
        if let Some(cpu) = info.cpu_seconds {
            total_cpu_seconds += cpu;
            cpu_process_count += 1;
        }
    }
    Ok(RssSample {
        root_pid,
        process_count: tree.len(),
        total_rss_bytes,
        total_pss_bytes: (pss_process_count > 0).then_some(total_pss_bytes),
        pss_process_count,
        total_cpu_seconds: (cpu_process_count > 0).then_some(total_cpu_seconds),
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

// ---------------------------------------------------------------------
// Tab-latency events (Issue #13)
// ---------------------------------------------------------------------

/// Which tab operation a [`PerfRecord::TabLatency`] measures.
///
/// Unlike [`PageLoadTimer`], no timer type is needed: `NewTab`/`ActivateTab`
/// are handled synchronously in `app.rs` (the new webview is usable, or the
/// switch is visible, by the time the handler returns), so the caller just
/// brackets `Instant::now()` around the existing call and hands the
/// resulting [`Duration`] straight to [`PerfRecord::tab_latency`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabLatencyKind {
    /// `ToolbarCommand::NewTab` to the new tab's webview being usable.
    Create,
    /// `ToolbarCommand::ActivateTab` to the switch being visible, for a
    /// tab that already had a live webview (`ActivationEffect::Switch`).
    Switch,
    /// `ToolbarCommand::ActivateTab` to a *suspended* tab's rebuilt webview
    /// being visible (`ActivationEffect::Resume`, see
    /// `app::activate_and_refresh`) — the restore cost of tab suspension
    /// (Issue #63). Split from [`Self::Switch`] because the two are an
    /// order of magnitude apart (a `set_visible` vs. building a webview),
    /// and mixing them would make `tab_switch_ms` unreadable the moment
    /// suspension is on. The page itself reloading afterwards is reported
    /// separately as the usual `page_load` event for that tab.
    Resume,
}

impl TabLatencyKind {
    fn event_name(self) -> &'static str {
        match self {
            TabLatencyKind::Create => "tab_create",
            TabLatencyKind::Switch => "tab_switch",
            TabLatencyKind::Resume => "tab_resume",
        }
    }
}

// ---------------------------------------------------------------------
// IPC traffic events (Issue #66)
// ---------------------------------------------------------------------

/// Which side of the WebView boundary sent an IPC message: the toolbar's
/// `window.ipc.postMessage` (JS → Rust, a [`ToolbarCommand`]) or one of
/// `ui::window::BrowserWindow`'s `evaluate_script` calls that push updated
/// state into the toolbar webview (Rust → JS). See
/// `docs/architecture.md`, "IPC traffic metrics" for how these two
/// directions are measured and what a caller can and cannot conclude from
/// them (in particular: `Out`'s `duration` covers only the Rust-side
/// `evaluate_script` call, never the JS execution or DOM work it triggers —
/// the WebView stays a black box per Epic #57's rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcDirection {
    /// `window.ipc.postMessage` → `ui::window`'s `with_ipc_handler` →
    /// `UserEvent::ToolbarMessage`.
    In,
    /// `ui::window::BrowserWindow`'s `evaluate_script` calls that push
    /// updated toolbar state (tab strip, address bar, panels, ...).
    Out,
}

impl IpcDirection {
    fn as_str(self) -> &'static str {
        match self {
            IpcDirection::In => "in",
            IpcDirection::Out => "out",
        }
    }
}

// ---------------------------------------------------------------------
// State-file write events (Issue #67)
// ---------------------------------------------------------------------

/// Which persisted-state file a [`PerfRecord::StateWrite`] measures —
/// matches `persistence.rs`'s file name without its `.json` extension.
/// A fixed set (not a free `&str`) because every call site is one of
/// `app::persist_session`/`persist_history`/`persist_bookmarks`/
/// `persist_input_history`, known at compile time; keeping this an enum
/// (like [`TabLatencyKind`]) rather than a `String` avoids allocating on
/// every persisted write, which is the exact kind of per-event cost this
/// Issue is checking for in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateWriteKind {
    /// `app::persist_session` → `persistence::save_session`. Called from
    /// `app::sync_tab_strip` after nearly every tab-affecting event (Issue
    /// #67's starting hypothesis: this is the one `persist_*` call that
    /// runs *unconditionally* on a hot path, not just when its own state
    /// actually changed — see docs/decisions.md D86).
    Session,
    /// `app::persist_history` → `persistence::save_history`. Only called
    /// from actual history mutations (a visit, title/favicon resolution,
    /// delete, clear) — once per real change, not per UI-sync event.
    History,
    /// `app::persist_bookmarks` → `persistence::save_bookmarks`. Only
    /// called from actual bookmark mutations.
    Bookmarks,
    /// `app::persist_input_history` → `persistence::save_input_history`.
    /// Only called when a search/navigation query is actually recorded.
    InputHistory,
}

impl StateWriteKind {
    fn as_str(self) -> &'static str {
        match self {
            StateWriteKind::Session => "session",
            StateWriteKind::History => "history",
            StateWriteKind::Bookmarks => "bookmarks",
            StateWriteKind::InputHistory => "input_history",
        }
    }
}

// ---------------------------------------------------------------------
// Unified event record + output format (Issue #13)
// ---------------------------------------------------------------------

/// Which shape [`PerfRecord::write`]-style callers should render lines in.
/// `Text` (the default) reproduces the exact `velox[perf] ...` lines this
/// project has always logged (Issue #3 / D16) so enabling metrics never
/// changes anyone's existing log-scraping. `Json` emits one JSON object per
/// line (JSON Lines) instead — no `velox[perf] ` prefix, since a future
/// consumer (Issue #14's benchmark runner, Issue #36's CI regression check)
/// needs every line to parse as JSON on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PerfFormat {
    #[default]
    Text,
    Json,
}

impl PerfFormat {
    /// Parse `VELOX_PERF_FORMAT`'s raw value (`"text"`/`"json"`,
    /// case-insensitive, surrounding whitespace ignored). Anything else —
    /// unset, empty, or unrecognized — falls back to `Text`, matching this
    /// project's "never fail a run over a bad env var" convention (see
    /// `Config::resolve_perf_env`).
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("json") => PerfFormat::Json,
            _ => PerfFormat::Text,
        }
    }
}

/// One structured performance-log event: a name, a monotonic timestamp
/// (elapsed time since process start), and event-specific fields — the
/// common shape [`StartupReport`], page-load, tab-latency and
/// [`RssSample`] events are all rendered through, so a future consumer only
/// has to understand one record shape instead of five ad hoc log lines. See
/// `docs/architecture.md`, "Performance extension points" for the exact
/// JSON schema.
#[derive(Debug, Clone)]
pub enum PerfRecord {
    Startup(StartupReport),
    PageLoad {
        url: String,
        duration: Duration,
        /// `LoadStarted` → `LoadFinished` (Issue #69), when `LoadStarted`
        /// fired for this load — see [`PageLoadOutcome`]. `None` keeps this
        /// record identical to what this project has always logged (Issue
        /// #13 through #66).
        engine: Option<Duration>,
    },
    TabLatency {
        kind: TabLatencyKind,
        tab_id: u64,
        duration: Duration,
    },
    /// The benchmark script reached its measured phase (Issue #60): the
    /// `mark` automation command. Everything logged before it is warm-up —
    /// `benchmark::aggregate_trials` drops it, so a scenario can set up a
    /// given number of tabs without those setup operations polluting the
    /// numbers the scenario is actually about. Carries nothing but its
    /// position in the log.
    MeasureStart,
    /// A background tab was suspended automatically (Issue #63,
    /// `browser::suspension`), and why. Carries no duration — dropping a
    /// webview is synchronous and cheap; what a reader wants to know is
    /// *when* and *why* a tab went dormant, to line up against the `rss`
    /// samples that follow.
    TabSuspend {
        tab_id: u64,
        reason: crate::browser::suspension::SuspendReason,
    },
    Rss(RssSample),
    /// CPU utilization of the whole process tree over the interval between
    /// the two most recent [`PerfRecord::Rss`] samples (Issue #64), as a
    /// percentage of one core. A separate record rather than a field on
    /// `Rss` because it describes an *interval*, not the instant the sample
    /// was taken: the first sample of a run has no predecessor and so emits
    /// no `Cpu` record at all, which a field would have had to fake as 0.
    Cpu {
        percent: f64,
    },
    /// One WebView ↔ Rust IPC message (Issue #66): `direction` says which
    /// side sent it, `name` is the command/update name (a `ToolbarCommand`
    /// variant's `cmd` tag for [`IpcDirection::In`], or the `evaluate_script`
    /// call site's name — e.g. `"set_tabs"` — for [`IpcDirection::Out`]),
    /// `bytes` is the raw JSON payload size, and `duration` is how long the
    /// Rust-side call took (`parse_command` for `In`, the `evaluate_script`
    /// FFI call for `Out` — never JS execution time; see [`IpcDirection`]'s
    /// doc comment). `name` is an owned `String` rather than `&'static str`
    /// because `In` names come from parsing untrusted, dynamic JSON.
    Ipc {
        direction: IpcDirection,
        name: String,
        bytes: u64,
        duration: Duration,
    },
    /// One `persistence::save_*` disk write of a persisted-state file
    /// (Issue #67). `duration` covers `write_json`'s full cost —
    /// `fs::create_dir_all` + `serde_json::to_string_pretty` + `fs::write`
    /// — real synchronous I/O, unlike [`Self::Ipc`]'s in-process
    /// `evaluate_script` call. See [`StateWriteKind`]'s doc comment for
    /// what each call site is and how often it should fire.
    StateWrite {
        kind: StateWriteKind,
        duration: Duration,
    },
}

impl PerfRecord {
    pub fn startup(report: StartupReport) -> Self {
        PerfRecord::Startup(report)
    }

    /// `engine` is [`PageLoadOutcome::engine`] — pass `None` for a plain
    /// total-only record (matching every `page_load` record before Issue
    /// #69).
    pub fn page_load(url: impl Into<String>, duration: Duration, engine: Option<Duration>) -> Self {
        PerfRecord::PageLoad {
            url: url.into(),
            duration,
            engine,
        }
    }

    pub fn tab_latency(kind: TabLatencyKind, tab_id: u64, duration: Duration) -> Self {
        PerfRecord::TabLatency {
            kind,
            tab_id,
            duration,
        }
    }

    pub fn rss(sample: RssSample) -> Self {
        PerfRecord::Rss(sample)
    }

    /// Average CPU utilization of the whole process tree between two
    /// samples, as a percentage of one core (Issue #64): 100 means one core
    /// fully busy, 400 means four. `None` when either sample lacks CPU
    /// times, when no wall time passed, or when the delta came out negative
    /// (a process in the tree exited between the samples and took its
    /// accumulated time with it — see [`RssSample::total_cpu_seconds`]).
    ///
    /// A free function rather than a method so the caller keeps ownership
    /// of both samples and of the clock: the sampler already knows the wall
    /// time between its own two reads, and nothing here should read a clock
    /// of its own.
    pub fn cpu(percent: f64) -> Self {
        PerfRecord::Cpu { percent }
    }

    pub fn cpu_percent_between(
        previous: &RssSample,
        current: &RssSample,
        wall: Duration,
    ) -> Option<f64> {
        let before = previous.total_cpu_seconds?;
        let after = current.total_cpu_seconds?;
        let elapsed = wall.as_secs_f64();
        if elapsed <= 0.0 || after < before {
            return None;
        }
        Some((after - before) / elapsed * 100.0)
    }

    pub fn tab_suspend(tab_id: u64, reason: crate::browser::suspension::SuspendReason) -> Self {
        PerfRecord::TabSuspend { tab_id, reason }
    }

    pub fn measure_start() -> Self {
        PerfRecord::MeasureStart
    }

    /// Build an [`PerfRecord::Ipc`] event. `started` is when the caller
    /// began the Rust-side work being measured (`Instant::now()` right
    /// before `parse_command`/`evaluate_script`); this computes the
    /// duration itself, matching [`Self::tab_latency`]'s "caller only
    /// brackets a clock read" contract.
    pub fn ipc(
        direction: IpcDirection,
        name: impl Into<String>,
        bytes: usize,
        started: Instant,
    ) -> Self {
        PerfRecord::Ipc {
            direction,
            name: name.into(),
            bytes: bytes as u64,
            duration: started.elapsed(),
        }
    }

    /// Build a [`PerfRecord::StateWrite`] event. `duration` is the caller's
    /// own `Instant::now().saturating_duration_since(started)` around the
    /// `persistence::save_*` call — the same "caller measures, this just
    /// wraps the value" shape [`Self::tab_latency`] uses, matching
    /// `app::record_tab_latency`/`app::record_state_write`'s pairing.
    pub fn state_write(kind: StateWriteKind, duration: Duration) -> Self {
        PerfRecord::StateWrite { kind, duration }
    }

    /// The event name used by both output formats (`"startup"`,
    /// `"page_load"`, `"tab_create"`, `"tab_switch"`, `"tab_resume"`,
    /// `"tab_suspend"`, `"measure_start"`, `"cpu"`, `"rss"`, `"ipc"`,
    /// `"state_write"`).
    pub fn event_name(&self) -> &'static str {
        match self {
            PerfRecord::Startup(_) => "startup",
            PerfRecord::PageLoad { .. } => "page_load",
            PerfRecord::TabLatency { kind, .. } => kind.event_name(),
            PerfRecord::TabSuspend { .. } => "tab_suspend",
            PerfRecord::MeasureStart => "measure_start",
            PerfRecord::Cpu { .. } => "cpu",
            PerfRecord::Rss(_) => "rss",
            PerfRecord::Ipc { .. } => "ipc",
            PerfRecord::StateWrite { .. } => "state_write",
        }
    }

    /// The original plain-text line for this record (no `velox[perf] `
    /// prefix — callers have always added that themselves at the log call
    /// site; see `app::record_perf_event`), byte-for-byte identical to what
    /// this project logged before Issue #13 for the three pre-existing
    /// event kinds.
    pub fn to_text(&self) -> String {
        match self {
            PerfRecord::Startup(report) => report.to_string(),
            // Issue #69: `engine`/`dispatch` are appended, never inserted —
            // an existing scraper matching the original `format_page_load`
            // prefix still works, same convention as the `rss` line's
            // `pss_mib` addition (Issue #108/D42) and the `startup` line's
            // sub-checkpoints (Issue #59/D43). Absent (`None`) when
            // `LoadStarted` never fired for this load.
            PerfRecord::PageLoad {
                url,
                duration,
                engine,
            } => {
                let base = format_page_load(url, *duration);
                match (engine, engine.map(|e| duration.saturating_sub(e))) {
                    (Some(engine), Some(dispatch)) => format!(
                        "{base} engine_duration={} dispatch_duration={}",
                        format_duration(*engine),
                        format_duration(dispatch)
                    ),
                    _ => base,
                }
            }
            PerfRecord::TabLatency {
                kind,
                tab_id,
                duration,
            } => format!(
                "{} id={tab_id} duration={}",
                kind.event_name(),
                format_duration(*duration)
            ),
            PerfRecord::TabSuspend { tab_id, reason } => {
                format!("tab_suspend id={tab_id} reason={}", reason.as_str())
            }
            PerfRecord::MeasureStart => "measure_start".to_owned(),
            PerfRecord::Cpu { percent } => format!("cpu percent={percent:.1}"),
            PerfRecord::Rss(sample) => sample.to_string(),
            PerfRecord::Ipc {
                direction,
                name,
                bytes,
                duration,
            } => format!(
                "ipc dir={} name={name} bytes={bytes} duration={}",
                direction.as_str(),
                format_duration(*duration)
            ),
            PerfRecord::StateWrite { kind, duration } => format!(
                "state_write name={} duration={}",
                kind.as_str(),
                format_duration(*duration)
            ),
        }
    }

    /// This record as a `serde_json::Value` object: always `"event"` and
    /// `"ts_ms"` (`elapsed` since process start, in fractional
    /// milliseconds — monotonic across one run because it derives from
    /// [`Instant`]), plus event-specific numeric/string fields.
    pub fn to_json(&self, elapsed: Duration) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        fields.insert("event".to_owned(), json!(self.event_name()));
        fields.insert("ts_ms".to_owned(), json!(ms(elapsed)));
        match self {
            PerfRecord::Startup(report) => {
                // Issue #182: like `engine_duration_ms`/`dispatch_duration_ms`
                // on `page_load` (Issue #69/D87) and `total_pss_bytes` on
                // `rss` (Issue #108/D42), these four keys are *always*
                // present and serialize to JSON `null` when the checkpoint
                // was never marked — never an absent key. A consumer can
                // then tell "this build does not report it" (key missing
                // entirely, i.e. a pre-#182 result file) apart from "this
                // run did not reach it" (key present, `null`).
                // `MetricKey::extract`'s `as_f64` drops both alike.
                let optional_ms = |at: Option<Duration>| match at {
                    Some(at) => json!(ms(at)),
                    None => serde_json::Value::Null,
                };
                fields.insert(
                    "event_loop_ms".to_owned(),
                    optional_ms(report.to_event_loop_built),
                );
                fields.insert(
                    "pre_window_setup_ms".to_owned(),
                    optional_ms(report.to_pre_window_setup_done),
                );
                fields.insert(
                    "native_window_ms".to_owned(),
                    optional_ms(report.to_native_window_built),
                );
                fields.insert(
                    "toolbar_webview_ms".to_owned(),
                    optional_ms(report.to_toolbar_webview_built),
                );
                fields.insert(
                    "window_created_ms".to_owned(),
                    json!(ms(report.to_window_created)),
                );
                fields.insert(
                    "rust_setup_done_ms".to_owned(),
                    json!(ms(report.to_rust_setup_done)),
                );
                fields.insert(
                    "toolbar_script_started_ms".to_owned(),
                    json!(ms(report.to_toolbar_script_started)),
                );
                fields.insert(
                    "toolbar_ready_ms".to_owned(),
                    json!(ms(report.to_toolbar_ready)),
                );
                fields.insert(
                    "first_load_ms".to_owned(),
                    json!(ms(report.to_first_load_finished)),
                );
            }
            PerfRecord::PageLoad {
                url,
                duration,
                engine,
            } => {
                fields.insert("url".to_owned(), json!(url));
                fields.insert("duration_ms".to_owned(), json!(ms(*duration)));
                // Issue #69: `engine_duration_ms`/`dispatch_duration_ms`
                // always appear (never an absent key), serializing to JSON
                // `null` when `LoadStarted` never fired — same convention
                // as `total_pss_bytes` (Issue #108/D42), so a consumer
                // always sees the key and cannot mistake "unmeasured" for
                // "0ms".
                fields.insert("engine_duration_ms".to_owned(), json!(engine.map(ms)));
                fields.insert(
                    "dispatch_duration_ms".to_owned(),
                    json!(engine.map(|e| ms(duration.saturating_sub(e)))),
                );
            }
            PerfRecord::TabLatency {
                tab_id, duration, ..
            } => {
                fields.insert("tab_id".to_owned(), json!(tab_id));
                fields.insert("duration_ms".to_owned(), json!(ms(*duration)));
            }
            PerfRecord::TabSuspend { tab_id, reason } => {
                fields.insert("tab_id".to_owned(), json!(tab_id));
                fields.insert("reason".to_owned(), json!(reason.as_str()));
            }
            // Only `event`/`ts_ms` — the marker's whole content is where it
            // sits in the log.
            PerfRecord::MeasureStart => {}
            PerfRecord::Cpu { percent } => {
                fields.insert("percent".to_owned(), json!(percent));
            }
            PerfRecord::Rss(sample) => {
                fields.insert("pid".to_owned(), json!(sample.root_pid));
                fields.insert("process_count".to_owned(), json!(sample.process_count));
                fields.insert("total_rss_bytes".to_owned(), json!(sample.total_rss_bytes));
                // `total_pss_bytes` serializes to JSON `null` (not an
                // absent key) when `None`, so a consumer parsing this field
                // always sees it and cannot mistake "unmeasured" for "0
                // bytes". `pss_process_count` lets it tell a full PSS total
                // from a partial one even when `total_pss_bytes` is `Some`.
                fields.insert("total_pss_bytes".to_owned(), json!(sample.total_pss_bytes));
                fields.insert(
                    "pss_process_count".to_owned(),
                    json!(sample.pss_process_count),
                );
                // Cumulative CPU time; `cpu_percent` (the rate a benchmark
                // actually compares) is added by the sampler, which is the
                // only caller that knows the interval between two samples.
                fields.insert(
                    "total_cpu_seconds".to_owned(),
                    json!(sample.total_cpu_seconds),
                );
            }
            PerfRecord::Ipc {
                direction,
                name,
                bytes,
                duration,
            } => {
                fields.insert("direction".to_owned(), json!(direction.as_str()));
                fields.insert("name".to_owned(), json!(name));
                fields.insert("bytes".to_owned(), json!(bytes));
                fields.insert("duration_ms".to_owned(), json!(ms(*duration)));
            }
            PerfRecord::StateWrite { kind, duration } => {
                fields.insert("name".to_owned(), json!(kind.as_str()));
                fields.insert("duration_ms".to_owned(), json!(ms(*duration)));
            }
        }
        serde_json::Value::Object(fields)
    }

    /// [`Self::to_json`] serialized to one line, no trailing newline (JSON
    /// Lines). Serializing a `Value` built entirely from finite numbers and
    /// UTF-8 strings, as `to_json` always builds it, cannot realistically
    /// fail; the fallback below only guards against that assumption ever
    /// becoming false, so a perf-log write can never panic the caller.
    pub fn to_json_line(&self, elapsed: Duration) -> String {
        serde_json::to_string(&self.to_json(elapsed))
            .unwrap_or_else(|_| format!("{{\"event\":{:?}}}", self.event_name()))
    }
}

/// Fractional milliseconds, rounded to one decimal place — matches the
/// precision [`format_duration`] already prints in text mode, so a reader
/// comparing the two formats side by side sees the same numbers.
fn ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

/// Convert a Windows `FILETIME`'s low/high 32-bit halves — 100ns ticks
/// since 1601-01-01, per `GetProcessTimes`'s `lpKernelTime`/`lpUserTime` —
/// into seconds (Issue #136). Plain arithmetic on two `u32`s rather than
/// the `windows::Win32::Foundation::FILETIME` type itself, so this stays
/// unit-testable on every platform this project's CI runs on (Linux) even
/// though `FILETIME` is only available when building for Windows (`windows`
/// is a `cfg(windows)` dependency, see `Cargo.toml`) — the Windows `imp`
/// module below is the only caller, and it does the
/// `FILETIME` → `(u32, u32)` unpacking right at the FFI boundary.
///
/// `#[cfg(any(test, target_os = "windows"))]`: its only non-test caller is
/// the Windows `imp` module below, so on every other platform this would
/// otherwise be dead code (`cargo clippy --all-targets -D warnings`'s
/// `dead_code` lint) outside of `cfg(test)` builds, where the tests at the
/// bottom of this file call it directly to verify the arithmetic without
/// needing the Windows-only `imp` module at all.
#[cfg(any(test, target_os = "windows"))]
fn filetime_ticks_to_seconds(low: u32, high: u32) -> f64 {
    let ticks = ((high as u64) << 32) | low as u64;
    ticks as f64 * 1e-7
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{HashMap, ProcInfo};
    use std::fs;
    use std::path::Path;

    /// Build a `pid -> ProcInfo` map covering `root_pid` and every
    /// transitive child of it (Issue #189 follow-up to #187: two passes,
    /// not one).
    ///
    /// **Pass 1** lists every process visible under `/proc` and reads only
    /// `status` (`PPid:`/`VmRSS:`) for each — cheap, a few fields per file
    /// (measured ~1.2ms for ~90 processes in #187's reference container).
    /// This is enough to compute the tree shape
    /// (`super::collect_descendants`), so `root_pid`'s descendant set is
    /// now known.
    ///
    /// **Pass 2** reads `smaps_rollup` (PSS) and `stat` (CPU time) — the
    /// two per-process reads that actually cost anything (a PSS read alone
    /// measured ~85% of the whole scan's time in #187, because the kernel
    /// has to walk that process's page table to answer it) — **only for
    /// processes in that descendant set**, not for every process on the
    /// machine. `build_sample` only ever looks at tree members anyway, so
    /// every non-descendant process's `smaps_rollup`/`stat` read was pure
    /// waste before this change — in this project's own reference
    /// container, 21 processes were readable but only ~9 of them were ever
    /// VeloX's own tree.
    ///
    /// Processes that exit mid-scan are silently skipped rather than
    /// treated as an error — RSS/PSS sampling is inherently a snapshot of
    /// a moving target. Every entry not in the descendant set keeps
    /// `pss_bytes: None`/`cpu_seconds: None` (never even attempted) —
    /// indistinguishable from D42/#63's existing "could not be read"
    /// case, which `build_sample` already treats as "excluded", not "0".
    pub(super) fn process_map(root_pid: u32) -> Result<HashMap<u32, ProcInfo>, super::RssError> {
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
            let Some(info) = parse_status(&contents) else {
                continue;
            };
            map.insert(pid, info);
        }
        // Pass 2: only the tree `sample_process_tree_rss` will actually sum
        // pays for `smaps_rollup`/`stat`. `collect_descendants` tolerates
        // `root_pid` being absent (e.g. it exited between listing `/proc`
        // and getting here) by simply returning `[root_pid]` with nothing
        // to look up — `build_sample`'s own `ProcessNotFound` check (using
        // this same map) is what actually reports that case.
        for pid in super::collect_descendants(root_pid, &map) {
            let Some(info) = map.get_mut(&pid) else {
                continue;
            };
            let pid_path = Path::new("/proc").join(pid.to_string());
            info.pss_bytes = read_pss_bytes(&pid_path);
            info.cpu_seconds = read_cpu_seconds(&pid_path);
        }
        Ok(map)
    }

    /// Parse the `PPid:` and `VmRSS:` fields out of `/proc/<pid>/status`
    /// text. Missing `PPid:` means we cannot place this process in the
    /// tree, so it is skipped entirely; missing `VmRSS:` (e.g. a zombie)
    /// defaults to zero bytes rather than dropping the process. `pss_bytes`
    /// always starts `None` here — `process_map` fills it in separately
    /// from `smaps_rollup`, a different file with its own failure mode.
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
            pss_bytes: None,
            cpu_seconds: None,
        })
    }

    /// User + system CPU time from `/proc/<pid>/stat`, in seconds (Issue
    /// #64). Fields 14 and 15 (1-based, `utime`/`stime`) counted in clock
    /// ticks; the divisor is `sysconf(_SC_CLK_TCK)`, which is 100 on every
    /// Linux this project targets and is hardcoded rather than pulling in
    /// a libc dependency for one constant (docs/decisions.md D6).
    ///
    /// The `comm` field can itself contain spaces and parentheses, so the
    /// line is split at the **last** `)` before the fields are counted —
    /// splitting on whitespace from the start would misalign for a process
    /// named e.g. `WebKitWebProcess (1)`.
    fn read_cpu_seconds(pid_path: &Path) -> Option<f64> {
        const CLOCK_TICKS_PER_SEC: f64 = 100.0;
        let raw = fs::read_to_string(pid_path.join("stat")).ok()?;
        let after_comm = raw.rsplit_once(')')?.1;
        let mut fields = after_comm.split_whitespace();
        // After `)` the next field is `state` (field 3), so `utime`
        // (field 14) is 11 positions further on, and `stime` right after.
        let utime: u64 = fields.nth(11)?.parse().ok()?;
        let stime: u64 = fields.next()?.parse().ok()?;
        Some((utime + stime) as f64 / CLOCK_TICKS_PER_SEC)
    }

    /// Read the `Pss:` line (kB) out of `<pid_path>/smaps_rollup`, VeloX's
    /// PSS source (see D42 in `docs/decisions.md`) — a kernel-computed
    /// rollup of the same per-mapping PSS `/proc/<pid>/smaps` exposes,
    /// without the cost of parsing every mapping individually. `None` for
    /// any failure (file absent on kernels older than 4.14, no permission,
    /// the process having exited, or unexpected content) — this mirrors
    /// `scripts/bench/compare_browsers.py`'s `_pss_bytes`, the reference
    /// implementation this logic follows, so the two measurements agree.
    fn read_pss_bytes(pid_path: &std::path::Path) -> Option<u64> {
        let contents = fs::read_to_string(pid_path.join("smaps_rollup")).ok()?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("Pss:") {
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                return Some(kb * 1024);
            }
        }
        None
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
            assert_eq!(info.pss_bytes, None);
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

        #[test]
        fn read_pss_bytes_parses_the_pss_line() {
            let dir = std::env::temp_dir().join(format!(
                "velox-pss-test-{}-{:?}-ok",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            fs::write(
                dir.join("smaps_rollup"),
                "00400000-00452000 r-xp 00000000 00:00 0\nRss:            1234 kB\nPss:             567 kB\nShared_Clean:      0 kB\n",
            )
            .expect("write fake smaps_rollup");

            assert_eq!(read_pss_bytes(&dir), Some(567 * 1024));

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn read_pss_bytes_missing_file_is_none() {
            let dir = std::env::temp_dir().join(format!(
                "velox-pss-test-{}-{:?}-missing",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir); // ensure it does not exist
            assert_eq!(read_pss_bytes(&dir), None);
        }

        #[test]
        fn read_pss_bytes_without_pss_line_is_none() {
            let dir = std::env::temp_dir().join(format!(
                "velox-pss-test-{}-{:?}-nopss",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            fs::write(dir.join("smaps_rollup"), "Rss:            1234 kB\n")
                .expect("write fake smaps_rollup");

            assert_eq!(read_pss_bytes(&dir), None);

            let _ = fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod imp {
    use super::{HashMap, ProcInfo};
    use std::process::Command;

    /// Best-effort fallback for non-Linux Unix (macOS, *BSD): parse `ps`
    /// output instead of `/proc`. Untested by this project's CI, which
    /// runs on Linux only; kept intentionally simple. `root_pid` is unused
    /// here (kept only so every platform's `process_map` shares one
    /// signature, Issue #189) — this already does one cheap, whole-machine
    /// `ps` call with no per-process PSS/CPU reads to narrow down, unlike
    /// the Linux and Windows two-pass implementations.
    pub(super) fn process_map(_root_pid: u32) -> Result<HashMap<u32, ProcInfo>, super::RssError> {
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
                    // `ps` has no PSS equivalent on these platforms, so
                    // `total_pss_bytes` stays `None` for every sample taken
                    // here (see `sample_process_tree_rss`'s platform docs).
                    pss_bytes: None,
                    // Likewise no CPU time: the `ps` call above asks for
                    // pid/ppid/rss only, and adding a second invocation for
                    // a platform this project's CI never exercises is not
                    // worth it (Issue #64).
                    cpu_seconds: None,
                },
            );
        }
        map
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{filetime_ticks_to_seconds, HashMap, ProcInfo};
    use std::io;
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    /// Closes a Win32 `HANDLE` on drop. `CreateToolhelp32Snapshot` and
    /// `OpenProcess` (the only two handle sources in this module) both
    /// return an *owning* handle on success — nothing else holds or closes
    /// it — so a thin RAII wrapper is enough to guarantee it is closed
    /// exactly once, on every exit path (including the early `?`/`return`s
    /// below), without a manual `CloseHandle` at each one.
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` was obtained from `CreateToolhelp32Snapshot`
            // or `OpenProcess` (see the two call sites below) and has not
            // been closed anywhere else — this `Drop` impl is the only
            // place that closes it, and it runs at most once per
            // `OwnedHandle` value.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Windows process-tree walk (Issue #136, two-pass since #189's follow-
    /// up to #187). `CreateToolhelp32Snapshot` + `Process32First/NextW` is
    /// the Windows equivalent of iterating `/proc` on Linux (see that
    /// `imp::process_map` above): the snapshot's `PROCESSENTRY32W` entries
    /// already carry each process's PID and parent PID
    /// (`th32ProcessID`/`th32ParentProcessID`) for free, with no extra API
    /// call — so **pass 1** below only builds the pid->ppid tree shape from
    /// the snapshot, exactly like Linux's pass 1 reads only `status`.
    ///
    /// RSS and CPU time each need one further per-process API
    /// (`GetProcessMemoryInfo`, `GetProcessTimes`, both behind an
    /// `OpenProcess` handle — see [`query_process`]) — on Windows these are
    /// full kernel round-trips, at least as expensive as Linux's
    /// `smaps_rollup` read per #187's measurements, and Windows has no
    /// snapshot-wide equivalent that gives them for free the way `status`
    /// gives Linux's RSS. **Pass 2** below therefore calls `query_process`
    /// only for `root_pid` and its descendants (`super::
    /// collect_descendants`, computed from pass 1's tree), exactly
    /// mirroring the Linux fix — this halves the surface `build_sample`
    /// never even looks at. Unverified on real Windows hardware/CI (this
    /// project's sandboxed session cannot run Windows); only `cargo check
    /// --target x86_64-pc-windows-msvc` confirms it type-checks. See
    /// docs/decisions.md D90 for the Linux-side measurement this mirrors
    /// and the explicit "not measured on Windows" caveat.
    ///
    /// PSS has no Windows equivalent and is not attempted here; every
    /// [`ProcInfo::pss_bytes`] this returns is `None` — see the module docs
    /// on [`super::sample_process_tree_rss`] and D88 in `docs/decisions.md`
    /// for the investigation and why.
    pub(super) fn process_map(root_pid: u32) -> Result<HashMap<u32, ProcInfo>, super::RssError> {
        // SAFETY: a plain FFI call into kernel32 with no caller-provided
        // buffers — it takes a system-wide snapshot and returns a handle to
        // it (or an error, via `windows_core::Result`, mapped below). The
        // returned handle is immediately wrapped in `OwnedHandle` so it is
        // closed on every return path out of this function.
        let snapshot = OwnedHandle(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(to_io_error)?,
        );

        // Pass 1: pid -> ppid only, no `OpenProcess`/`GetProcessMemoryInfo`/
        // `GetProcessTimes` yet. `rss_bytes` starts at 0 and `cpu_seconds`
        // at `None`; pass 2 below fills both in for tree members only —
        // every other entry keeps these placeholder values, which is fine
        // since `build_sample` (via `super::collect_descendants`) never
        // looks at a non-tree entry's `rss_bytes`/`cpu_seconds` at all.
        let mut map = HashMap::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        // SAFETY: `snapshot.0` is the valid `TH32CS_SNAPPROCESS` handle
        // just created above (still open — `OwnedHandle` has not been
        // dropped yet); `entry` is a stack-allocated `PROCESSENTRY32W` with
        // `dwSize` set as `Process32FirstW` requires, and we pass a valid
        // `*mut` to it that the call fills in on success.
        let mut has_entry = unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok();

        while has_entry {
            let pid = entry.th32ProcessID;
            map.insert(
                pid,
                ProcInfo {
                    ppid: entry.th32ParentProcessID,
                    rss_bytes: 0,
                    pss_bytes: None,
                    cpu_seconds: None,
                },
            );

            // SAFETY: same still-open snapshot handle and the same `entry`
            // buffer as above, reused across iterations per the
            // Toolhelp32 API's contract.
            has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) }.is_ok();
        }
        // The snapshot itself is done with — pass 2 below queries live
        // processes directly via `OpenProcess`, not the snapshot.
        drop(snapshot);

        // Pass 2: RSS + CPU time, only for root_pid and its descendants.
        for pid in super::collect_descendants(root_pid, &map) {
            let Some(info) = map.get_mut(&pid) else {
                continue;
            };
            let (rss_bytes, cpu_seconds) = query_process(pid);
            info.rss_bytes = rss_bytes.unwrap_or(0);
            info.cpu_seconds = cpu_seconds;
        }

        Ok(map)
    }

    /// Best-effort RSS + CPU time for one process: `OpenProcess` with the
    /// minimal access rights `GetProcessMemoryInfo`/`GetProcessTimes`
    /// need (`PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`, never
    /// `PROCESS_ALL_ACCESS`). A process VeloX cannot open — a
    /// higher-privileged or another user's process, since this project does
    /// not expect to run elevated — simply contributes no RSS/CPU rather
    /// than failing the whole sample, mirroring how the Linux
    /// implementation treats an unreadable `/proc/<pid>/status`
    /// (`build_sample`'s summation already treats a missing map entry —
    /// which never happens here since a `ProcInfo` is inserted regardless —
    /// and a `None` field the same permissive way).
    fn query_process(pid: u32) -> (Option<u64>, Option<f64>) {
        // SAFETY: a plain FFI call; the access mask requested is the
        // minimal one documented above, `bInheritHandle = false` so this
        // handle is not inherited by any child process VeloX later spawns,
        // and the returned handle (on success) is immediately wrapped in
        // `OwnedHandle` so it is always closed before this function
        // returns.
        let handle = match unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                false,
                pid,
            )
        } {
            Ok(handle) => OwnedHandle(handle),
            Err(_) => return (None, None),
        };

        let rss_bytes = {
            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            // SAFETY: `handle.0` was just opened above and is still valid
            // (not dropped until this block's `counters` has been read);
            // `counters` is a stack buffer sized exactly as `cb` states,
            // matching what `GetProcessMemoryInfo` requires.
            unsafe { GetProcessMemoryInfo(handle.0, &mut counters, counters.cb) }
                .ok()
                .map(|()| counters.WorkingSetSize as u64)
        };

        let cpu_seconds = {
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            // SAFETY: `handle.0` is still valid, as above; the four
            // `FILETIME` out-parameters are stack-allocated and correctly
            // sized for what `GetProcessTimes` writes into them.
            unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) }
                .ok()
                .map(|()| {
                    filetime_ticks_to_seconds(kernel.dwLowDateTime, kernel.dwHighDateTime)
                        + filetime_ticks_to_seconds(user.dwLowDateTime, user.dwHighDateTime)
                })
        };

        (rss_bytes, cpu_seconds)
    }

    /// A `windows::core::Error` (an `HRESULT` plus an optional message) as
    /// [`super::RssError::Io`]. `.message()` renders a human-readable
    /// description (falling back to a generic one for the `HRESULT` alone
    /// when no extended `IErrorInfo` is attached) — preserved as the
    /// `io::Error`'s text via `io::Error::other`, since `io::Error::
    /// from_raw_os_error` expects a raw Win32 error code, not the `HRESULT`
    /// this crate's errors carry, and would misdecode it.
    fn to_io_error(err: windows::core::Error) -> super::RssError {
        super::RssError::Io(io::Error::other(err.message()))
    }
}

#[cfg(not(any(target_os = "linux", unix, target_os = "windows")))]
mod imp {
    use super::{HashMap, ProcInfo};

    /// No sampling strategy implemented yet (an OS other than Linux, other
    /// Unix, or Windows). Callers get a clean [`super::RssError::Unsupported`]
    /// instead of a panic. `root_pid` is unused (kept only for signature
    /// parity with every other platform's `process_map`, Issue #189).
    pub(super) fn process_map(_root_pid: u32) -> Result<HashMap<u32, ProcInfo>, super::RssError> {
        Err(super::RssError::Unsupported)
    }
}

// ---------------------------------------------------------------------
// Installed RAM (Issue #176 / D93 子 Issue 案 B)
// ---------------------------------------------------------------------

/// How much physical RAM the machine has, in bytes. `None` when this
/// platform has no implementation yet, or when the OS would not say.
///
/// **なぜ必要か** (D93): メモリ予算の既定は現在 700 MiB という**絶対値**
/// で、搭載 RAM を一切見ていない (D90)。4 GiB 機でも 64 GiB 機でも同じ
/// 値になるため、前者では予算として機能せず、後者では実質 OFF になる
/// (D93「なぜ裸の比率では成立しないか」)。式を RAM 相対にするには、
/// まず搭載 RAM が分かる必要がある — **本関数はその材料を用意するだけ
/// で、予算の式は一切変えない** (D93 子 Issue 案 B が案 C と分けられて
/// いるのはこのため)。
///
/// **失敗はエラーにしない。** 呼び出し側にとって「RAM が分からない」は
/// 異常ではなく、**絶対値の既定にフォールバックすればよいだけ**である
/// (macOS が今まさにその状態)。`RssError` のような型を返すと、呼び出し
/// 側に「どう失敗したか」を判断させることになるが、判断の余地は無い。
///
/// | OS | 実装 |
/// | --- | --- |
/// | Linux | `/proc/meminfo` の `MemTotal:` 行 (kB) |
/// | Windows | `GlobalMemoryStatusEx` の `ullTotalPhys` |
/// | macOS / その他 | 未実装 (`None`) — CLAUDE.md の OS 優先度に従う |
pub fn installed_ram_bytes() -> Option<u64> {
    ram_imp::installed_ram_bytes()
}

/// `/proc/meminfo` の `MemTotal:` 行からバイト単位の総メモリ量を取り出す。
///
/// 行の形は `MemTotal:       16307556 kB` で、**単位は kB 固定**
/// (kernel の `meminfo` は常に kB で出す)。それでも単位を読み飛ばさずに
/// 検査するのは、**将来 kernel が単位を変えたときに黙って 1024 倍ずれた
/// 値を返すより、`None` を返す方が安全**だからである。
///
/// `velox-bench` の機種情報収集 (D104) も同じ行を読む必要があるため、
/// パーサはここに一本化してあちらから呼ぶ (同じファイルを 2 か所で
/// 解釈すると、片方だけ直したときに値が食い違う)。
pub fn parse_mem_total_bytes(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "MemTotal" {
            return None;
        }
        let mut parts = value.split_whitespace();
        let amount: u64 = parts.next()?.parse().ok()?;
        // 単位が無い/kB 以外なら読まない (上記の理由)。
        match parts.next() {
            Some(unit) if unit.eq_ignore_ascii_case("kb") => amount.checked_mul(1024),
            _ => None,
        }
    })
}

#[cfg(target_os = "linux")]
mod ram_imp {
    pub(super) fn installed_ram_bytes() -> Option<u64> {
        let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
        super::parse_mem_total_bytes(&contents)
    }
}

#[cfg(target_os = "windows")]
mod ram_imp {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    pub(super) fn installed_ram_bytes() -> Option<u64> {
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?,
            ..Default::default()
        };
        // SAFETY: `GlobalMemoryStatusEx` は呼び出し側が用意した
        // `MEMORYSTATUSEX` を埋めるだけで、`dwLength` に構造体の大きさが
        // 入っていることだけを要求する (上で設定済み)。ポインタは生存中の
        // ローカル変数への排他参照であり、関数は同期的に返る。失敗時は
        // 構造体を触らずエラーを返すので、未初期化の値を読むことも無い。
        // CLAUDE.md の「unsafe は理由をコメントで明記」に従う。
        unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
        // 0 は「取れなかった」と区別がつかないので None にする
        // (D105 の「観測できたものだけを書く」と同じ方向)。
        (status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod ram_imp {
    /// macOS などは未実装。`sysctl hw.memsize` で取れるが、CLAUDE.md の
    /// OS 優先度 (Windows 最優先、macOS/Linux は最低限) に従い、**必要に
    /// なるまで足さない。** 呼び出し側は絶対値の既定にフォールバックする
    /// だけなので、`None` でも壊れない。
    pub(super) fn installed_ram_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- installed RAM (Issue #176 / D93 案 B) --------------------------

    #[test]
    fn mem_total_is_read_as_kilobytes() {
        // 実際の /proc/meminfo の先頭数行。MemTotal は MemFree より前に
        // あるが、前後の行に引きずられないことも同時に確かめる。
        let meminfo = "\
MemTotal:       16307556 kB
MemFree:         1234567 kB
MemAvailable:    8901234 kB
";
        assert_eq!(
            parse_mem_total_bytes(meminfo),
            Some(16_307_556 * 1024),
            "kB は 1024 倍でバイトにする"
        );
    }

    #[test]
    fn mem_total_ignores_similar_keys() {
        // `MemTotalFoo` や `SwapTotal` を拾わないこと。前方一致ではなく
        // キー完全一致で見ている。
        let meminfo = "SwapTotal:  1000 kB\nMemTotalish: 2000 kB\nMemTotal: 4 kB\n";
        assert_eq!(parse_mem_total_bytes(meminfo), Some(4096));
    }

    #[test]
    fn mem_total_without_a_unit_is_not_guessed() {
        // **単位が無い/違う行は読まない。** kernel が単位を変えたときに
        // 黙って 1024 倍ずれた値を返すより、None の方が安全である
        // (呼び出し側は絶対値の既定へフォールバックするだけ)。
        assert_eq!(parse_mem_total_bytes("MemTotal: 16307556\n"), None);
        assert_eq!(parse_mem_total_bytes("MemTotal: 16307556 MB\n"), None);
        assert_eq!(parse_mem_total_bytes("MemTotal: kB\n"), None);
    }

    #[test]
    fn mem_total_accepts_the_unit_case_insensitively() {
        assert_eq!(parse_mem_total_bytes("MemTotal: 4 KB\n"), Some(4096));
    }

    #[test]
    fn mem_total_absent_or_unparsable_is_none() {
        assert_eq!(parse_mem_total_bytes(""), None);
        assert_eq!(parse_mem_total_bytes("MemFree: 100 kB\n"), None);
        assert_eq!(parse_mem_total_bytes("MemTotal: not-a-number kB\n"), None);
        assert_eq!(parse_mem_total_bytes("no colon here\n"), None);
    }

    #[test]
    fn mem_total_does_not_overflow_on_an_absurd_value() {
        // u64::MAX kB は 1024 倍でオーバーフローする。パニックさせない。
        let meminfo = format!("MemTotal: {} kB\n", u64::MAX);
        assert_eq!(parse_mem_total_bytes(&meminfo), None);
    }

    /// 実際にこの環境で呼んでみる。**値そのものは環境依存なので検査
    /// できない** — 検査するのは「呼んでもパニックせず、値が返るなら
    /// 常識的な範囲にある」ことだけ。
    #[test]
    fn installed_ram_is_plausible_when_available() {
        // `None` は異常ではない (Linux/Windows 以外、または /proc が
        // 読めない環境) のでそのまま通す。
        if let Some(bytes) = installed_ram_bytes() {
            // 64 MiB 未満や 1 PiB 超はこの関数の読み違いを疑う値。
            const PLAUSIBLE: std::ops::Range<u64> =
                64 * 1024 * 1024..1024 * 1024 * 1024 * 1024 * 1024;
            assert!(
                PLAUSIBLE.contains(&bytes),
                "搭載 RAM が {bytes} バイトはあり得ない"
            );
        }
    }

    /// A [`StartupReport`] with all nine checkpoints set to distinct,
    /// chronologically ordered values. Shared by the `PerfRecord` tests so
    /// each one asserts on the fields it cares about instead of restating
    /// the whole struct.
    fn full_startup_report() -> StartupReport {
        StartupReport {
            to_event_loop_built: Some(Duration::from_millis(2)),
            to_pre_window_setup_done: Some(Duration::from_millis(4)),
            to_native_window_built: Some(Duration::from_millis(5)),
            to_toolbar_webview_built: Some(Duration::from_millis(8)),
            to_window_created: Duration::from_millis(10),
            to_rust_setup_done: Duration::from_millis(12),
            to_toolbar_script_started: Duration::from_millis(15),
            to_toolbar_ready: Duration::from_millis(20),
            to_first_load_finished: Duration::from_millis(30),
        }
    }

    // -- StartupTimestamps ------------------------------------------------

    #[test]
    fn report_is_none_until_all_checkpoints_recorded() {
        let start = Instant::now();
        let mut timestamps = StartupTimestamps::new(start);
        assert!(timestamps.report().is_none());

        timestamps.mark_window_created(Instant::now());
        assert!(timestamps.report().is_none());

        timestamps.mark_rust_setup_done(Instant::now());
        assert!(timestamps.report().is_none());

        timestamps.mark_toolbar_script_started(Instant::now());
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
        timestamps.mark_toolbar_script_started(Instant::now());
        timestamps.mark_rust_setup_done(Instant::now());
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
        timestamps.mark_rust_setup_done(start);
        timestamps.mark_toolbar_script_started(start);
        timestamps.mark_toolbar_ready(start);
        timestamps.mark_first_load_finished(start);
        let report = timestamps.report().unwrap();
        assert_eq!(report.to_window_created, Duration::ZERO);
    }

    /// Issue #182: the four sub-checkpoints of `process_start` ->
    /// `window_created` are diagnostics layered onto an existing
    /// measurement. They must never be able to withhold the `startup`
    /// record — every startup metric `velox-bench` reads comes from it.
    #[test]
    fn report_does_not_require_the_issue_182_sub_checkpoints() {
        let start = Instant::now();
        let mut timestamps = StartupTimestamps::new(start);
        timestamps.mark_window_created(start);
        timestamps.mark_rust_setup_done(start);
        timestamps.mark_toolbar_script_started(start);
        timestamps.mark_toolbar_ready(start);
        timestamps.mark_first_load_finished(start);

        let report = timestamps.report().expect("the original five suffice");
        assert_eq!(report.to_event_loop_built, None);
        assert_eq!(report.to_pre_window_setup_done, None);
        assert_eq!(report.to_native_window_built, None);
        assert_eq!(report.to_toolbar_webview_built, None);
    }

    #[test]
    fn report_measures_the_issue_182_sub_checkpoints_from_process_start() {
        let start = Instant::now();
        let mut timestamps = StartupTimestamps::new(start);
        timestamps.mark_event_loop_built(start + Duration::from_millis(100));
        timestamps.mark_pre_window_setup_done(start + Duration::from_millis(110));
        timestamps.mark_native_window_built(start + Duration::from_millis(120));
        timestamps.mark_toolbar_webview_built(start + Duration::from_millis(600));
        timestamps.mark_window_created(start + Duration::from_millis(644));
        timestamps.mark_rust_setup_done(start + Duration::from_millis(645));
        timestamps.mark_toolbar_script_started(start + Duration::from_millis(646));
        timestamps.mark_toolbar_ready(start + Duration::from_millis(646));
        timestamps.mark_first_load_finished(start + Duration::from_millis(717));

        let report = timestamps.report().expect("every checkpoint recorded");
        // Cumulative from process start, like every other checkpoint —
        // *not* per-segment durations. Consumers subtract adjacent pairs.
        assert_eq!(report.to_event_loop_built, Some(Duration::from_millis(100)));
        assert_eq!(
            report.to_pre_window_setup_done,
            Some(Duration::from_millis(110))
        );
        assert_eq!(
            report.to_native_window_built,
            Some(Duration::from_millis(120))
        );
        assert_eq!(
            report.to_toolbar_webview_built,
            Some(Duration::from_millis(600))
        );
        assert_eq!(report.to_window_created, Duration::from_millis(644));
    }

    #[test]
    fn only_the_first_mark_counts_for_the_issue_182_sub_checkpoints() {
        let start = Instant::now();
        let mut timestamps = StartupTimestamps::new(start);
        timestamps.mark_event_loop_built(start);
        timestamps.mark_event_loop_built(start + Duration::from_secs(10)); // ignored
        timestamps.mark_pre_window_setup_done(start);
        timestamps.mark_pre_window_setup_done(start + Duration::from_secs(10)); // ignored
        timestamps.mark_native_window_built(start);
        timestamps.mark_native_window_built(start + Duration::from_secs(10)); // ignored
        timestamps.mark_toolbar_webview_built(start);
        timestamps.mark_toolbar_webview_built(start + Duration::from_secs(10)); // ignored
        timestamps.mark_window_created(start);
        timestamps.mark_rust_setup_done(start);
        timestamps.mark_toolbar_script_started(start);
        timestamps.mark_toolbar_ready(start);
        timestamps.mark_first_load_finished(start);

        let report = timestamps.report().unwrap();
        assert_eq!(report.to_event_loop_built, Some(Duration::ZERO));
        assert_eq!(report.to_pre_window_setup_done, Some(Duration::ZERO));
        assert_eq!(report.to_native_window_built, Some(Duration::ZERO));
        assert_eq!(report.to_toolbar_webview_built, Some(Duration::ZERO));
    }

    /// The `text` perf format keeps one field per checkpoint even when a
    /// sub-checkpoint is missing, so two logs stay diffable line-by-line.
    #[test]
    fn startup_display_renders_absent_sub_checkpoints_as_a_dash() {
        let full = full_startup_report().to_string();
        assert!(full.contains("event_loop=2.0ms"), "{full}");
        assert!(full.contains("toolbar_webview=8.0ms"), "{full}");
        assert!(full.contains("window_created=10.0ms"), "{full}");

        let partial = StartupReport {
            to_event_loop_built: None,
            to_pre_window_setup_done: None,
            to_native_window_built: None,
            to_toolbar_webview_built: None,
            ..full_startup_report()
        }
        .to_string();
        assert!(partial.contains("event_loop=- "), "{partial}");
        assert!(partial.contains("toolbar_webview=- "), "{partial}");
        assert!(partial.contains("window_created=10.0ms"), "{partial}");
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
        let outcome = timer.finish(end).expect("start was called");
        assert_eq!(outcome.total, Duration::from_millis(42));
        assert_eq!(outcome.engine, None);
        assert_eq!(outcome.dispatch(), None);
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
        assert_eq!(
            timer.finish(end).map(|o| o.total),
            Some(Duration::from_millis(10))
        );
    }

    #[test]
    fn mark_load_started_without_start_is_ignored() {
        // A `LoadStarted` for a tab this timer never `start`ed (e.g. it
        // arrived after a stray/duplicate event) must not fabricate an
        // `engine` duration out of nothing.
        let mut timer = PageLoadTimer::new();
        let now = Instant::now();
        timer.mark_load_started(now);
        assert_eq!(timer.finish(now), None);
    }

    #[test]
    fn load_started_between_start_and_finish_splits_into_dispatch_and_engine() {
        let mut timer = PageLoadTimer::new();
        let start = Instant::now();
        timer.start(start);
        let load_started = start + Duration::from_millis(5);
        timer.mark_load_started(load_started);
        let end = start + Duration::from_millis(50);
        let outcome = timer.finish(end).expect("start was called");
        assert_eq!(outcome.total, Duration::from_millis(50));
        assert_eq!(outcome.engine, Some(Duration::from_millis(45)));
        assert_eq!(outcome.dispatch(), Some(Duration::from_millis(5)));
    }

    #[test]
    fn only_the_first_load_started_call_counts() {
        let mut timer = PageLoadTimer::new();
        let start = Instant::now();
        timer.start(start);
        timer.mark_load_started(start + Duration::from_millis(5));
        timer.mark_load_started(start + Duration::from_millis(20)); // ignored
        let outcome = timer
            .finish(start + Duration::from_millis(50))
            .expect("start was called");
        assert_eq!(outcome.engine, Some(Duration::from_millis(45)));
    }

    #[test]
    fn restarting_clears_a_pending_load_started_mark() {
        // A redirect that re-triggers `NavigationStarted` must not let a
        // `LoadStarted` mark from the *previous* (abandoned) load leak into
        // the new one's `engine` duration.
        let mut timer = PageLoadTimer::new();
        let first_start = Instant::now();
        timer.start(first_start);
        timer.mark_load_started(first_start + Duration::from_millis(5));
        let second_start = first_start + Duration::from_millis(100);
        timer.start(second_start);
        let end = second_start + Duration::from_millis(10);
        let outcome = timer.finish(end).expect("start was called");
        assert_eq!(outcome.total, Duration::from_millis(10));
        assert_eq!(outcome.engine, None);
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
                pss_bytes: None,
                cpu_seconds: None,
            },
        );
        processes.insert(
            2,
            ProcInfo {
                ppid: 1,
                rss_bytes: 2000,
                pss_bytes: None,
                cpu_seconds: None,
            },
        ); // child of 1
        processes.insert(
            3,
            ProcInfo {
                ppid: 2,
                rss_bytes: 3000,
                pss_bytes: None,
                cpu_seconds: None,
            },
        ); // grandchild
        processes.insert(
            4,
            ProcInfo {
                ppid: 0,
                rss_bytes: 4000,
                pss_bytes: None,
                cpu_seconds: None,
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
                pss_bytes: None,
                cpu_seconds: None,
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

    // -- RSS: pure tree-walk / summation, PSS side ----------------------

    #[test]
    fn build_sample_sums_pss_when_every_process_has_it() {
        let mut processes = HashMap::new();
        processes.insert(
            1,
            ProcInfo {
                ppid: 0,
                rss_bytes: 1000,
                pss_bytes: Some(400),
                cpu_seconds: None,
            },
        );
        processes.insert(
            2,
            ProcInfo {
                ppid: 1,
                rss_bytes: 2000,
                pss_bytes: Some(600),
                cpu_seconds: None,
            },
        );

        let sample = build_sample(1, &processes).unwrap();
        assert_eq!(sample.process_count, 2);
        assert_eq!(sample.total_pss_bytes, Some(1000));
        assert_eq!(sample.pss_process_count, 2);
    }

    #[test]
    fn build_sample_pss_is_none_when_no_process_has_it() {
        let mut processes = HashMap::new();
        processes.insert(
            1,
            ProcInfo {
                ppid: 0,
                rss_bytes: 1000,
                pss_bytes: None,
                cpu_seconds: None,
            },
        );
        processes.insert(
            2,
            ProcInfo {
                ppid: 1,
                rss_bytes: 2000,
                pss_bytes: None,
                cpu_seconds: None,
            },
        );

        let sample = build_sample(1, &processes).unwrap();
        // RSS is unaffected — it never depends on PSS being readable.
        assert_eq!(sample.total_rss_bytes, 3000);
        assert_eq!(sample.total_pss_bytes, None);
        assert_eq!(sample.pss_process_count, 0);
    }

    #[test]
    fn build_sample_pss_partial_when_only_some_processes_have_it() {
        // e.g. a helper process's smaps_rollup could not be read, while the
        // root's could — the sum must still reflect what *was* readable
        // instead of silently dropping to `None`, and `pss_process_count`
        // must say the total is incomplete (2 readable out of 3 processes).
        let mut processes = HashMap::new();
        processes.insert(
            1,
            ProcInfo {
                ppid: 0,
                rss_bytes: 1000,
                pss_bytes: Some(300),
                cpu_seconds: None,
            },
        );
        processes.insert(
            2,
            ProcInfo {
                ppid: 1,
                rss_bytes: 2000,
                pss_bytes: None,
                cpu_seconds: None,
            },
        );
        processes.insert(
            3,
            ProcInfo {
                ppid: 1,
                rss_bytes: 500,
                pss_bytes: Some(150),
                cpu_seconds: None,
            },
        );

        let sample = build_sample(1, &processes).unwrap();
        assert_eq!(sample.process_count, 3);
        assert_eq!(sample.total_pss_bytes, Some(450));
        assert_eq!(sample.pss_process_count, 2);
        assert!(
            sample.pss_process_count < sample.process_count,
            "a partial PSS total must be distinguishable from a complete one"
        );
    }

    #[test]
    fn rss_sample_display_reports_mib() {
        let sample = RssSample {
            root_pid: 42,
            process_count: 3,
            total_rss_bytes: 2 * 1024 * 1024,
            total_pss_bytes: Some(1024 * 1024),
            pss_process_count: 3,
            total_cpu_seconds: None,
        };
        assert_eq!(
            sample.to_string(),
            "rss pid=42 processes=3 total_mib=2.0 pss_processes=3/3 pss_mib=1.0 cpu_s=n/a"
        );
    }

    #[test]
    fn rss_sample_display_reports_n_a_when_pss_unavailable() {
        let sample = RssSample {
            root_pid: 42,
            process_count: 3,
            total_rss_bytes: 2 * 1024 * 1024,
            total_pss_bytes: None,
            pss_process_count: 0,
            total_cpu_seconds: None,
        };
        assert_eq!(
            sample.to_string(),
            "rss pid=42 processes=3 total_mib=2.0 pss_processes=0/3 pss_mib=n/a cpu_s=n/a"
        );
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
    fn samples_own_process_reports_consistent_pss_invariants() {
        // Whether `smaps_rollup` is actually readable depends on the
        // sandbox this test runs in (kernel version, permissions), so this
        // does not assert PSS is present — only that the two PSS fields
        // never contradict each other, on the real `/proc` this project
        // ships against.
        let pid = std::process::id();
        let sample = sample_process_tree_rss(pid).expect("sampling the current process");
        assert!(sample.pss_process_count <= sample.process_count);
        match sample.total_pss_bytes {
            Some(_) => assert!(sample.pss_process_count > 0),
            None => assert_eq!(sample.pss_process_count, 0),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_pid_is_reported_as_not_found() {
        let result = sample_process_tree_rss(u32::MAX);
        assert!(matches!(result, Err(RssError::ProcessNotFound(pid)) if pid == u32::MAX));
    }

    // -- PerfFormat -----------------------------------------------------

    #[test]
    fn perf_format_parses_json_case_insensitively() {
        assert_eq!(PerfFormat::parse(Some("json")), PerfFormat::Json);
        assert_eq!(PerfFormat::parse(Some("JSON")), PerfFormat::Json);
        assert_eq!(PerfFormat::parse(Some("  Json  ")), PerfFormat::Json);
    }

    #[test]
    fn perf_format_defaults_to_text_for_anything_else() {
        assert_eq!(PerfFormat::parse(None), PerfFormat::Text);
        assert_eq!(PerfFormat::parse(Some("")), PerfFormat::Text);
        assert_eq!(PerfFormat::parse(Some("text")), PerfFormat::Text);
        assert_eq!(PerfFormat::parse(Some("xml")), PerfFormat::Text);
    }

    // -- PerfRecord: text output matches the pre-Issue-#13 lines ---------

    #[test]
    fn perf_record_startup_text_matches_legacy_display() {
        let report = full_startup_report();
        let expected = report.to_string();
        assert_eq!(PerfRecord::startup(report).to_text(), expected);
        assert_eq!(PerfRecord::startup(report).event_name(), "startup");
    }

    #[test]
    fn perf_record_page_load_text_matches_legacy_format_function() {
        let record =
            PerfRecord::page_load("https://example.com/", Duration::from_millis(250), None);
        assert_eq!(
            record.to_text(),
            format_page_load("https://example.com/", Duration::from_millis(250))
        );
        assert_eq!(record.event_name(), "page_load");
    }

    #[test]
    fn perf_record_page_load_text_appends_engine_and_dispatch_when_present() {
        // Issue #69: appended, not inserted — the base line stays exactly
        // what `format_page_load` produces (previous test), so an existing
        // scraper matching that prefix keeps working.
        let record = PerfRecord::page_load(
            "https://example.com/",
            Duration::from_millis(250),
            Some(Duration::from_millis(230)),
        );
        assert_eq!(
            record.to_text(),
            format!(
                "{} engine_duration=230.0ms dispatch_duration=20.0ms",
                format_page_load("https://example.com/", Duration::from_millis(250))
            )
        );
    }

    #[test]
    fn perf_record_rss_text_matches_legacy_display() {
        let sample = RssSample {
            root_pid: 42,
            process_count: 3,
            total_rss_bytes: 2 * 1024 * 1024,
            total_pss_bytes: Some(1024 * 1024),
            pss_process_count: 3,
            total_cpu_seconds: None,
        };
        let expected = sample.to_string();
        assert_eq!(PerfRecord::rss(sample).to_text(), expected);
        assert_eq!(PerfRecord::rss(sample).event_name(), "rss");
    }

    #[test]
    fn perf_record_tab_latency_text_is_readable() {
        let record = PerfRecord::tab_latency(TabLatencyKind::Create, 7, Duration::from_millis(15));
        assert_eq!(record.to_text(), "tab_create id=7 duration=15.0ms");
        assert_eq!(record.event_name(), "tab_create");

        let record =
            PerfRecord::tab_latency(TabLatencyKind::Switch, 7, Duration::from_micros(3100));
        assert_eq!(record.to_text(), "tab_switch id=7 duration=3.1ms");
        assert_eq!(record.event_name(), "tab_switch");

        let record = PerfRecord::tab_latency(TabLatencyKind::Resume, 7, Duration::from_millis(40));
        assert_eq!(record.to_text(), "tab_resume id=7 duration=40.0ms");
        assert_eq!(record.event_name(), "tab_resume");
    }

    #[test]
    fn cpu_percent_between_is_a_rate_over_the_interval() {
        let sample = |cpu: Option<f64>| RssSample {
            root_pid: 1,
            process_count: 2,
            total_rss_bytes: 0,
            total_pss_bytes: None,
            pss_process_count: 0,
            total_cpu_seconds: cpu,
        };
        // 2 CPU-seconds over 1 second of wall time = two cores busy.
        let percent = PerfRecord::cpu_percent_between(
            &sample(Some(1.0)),
            &sample(Some(3.0)),
            Duration::from_secs(1),
        );
        assert_eq!(percent, Some(200.0));
        // Half a core over 4 seconds.
        let percent = PerfRecord::cpu_percent_between(
            &sample(Some(10.0)),
            &sample(Some(12.0)),
            Duration::from_secs(4),
        );
        assert_eq!(percent, Some(50.0));

        // A process exiting between samples can make the total go *down*;
        // that is not a negative CPU rate, it is an unusable interval.
        assert_eq!(
            PerfRecord::cpu_percent_between(
                &sample(Some(5.0)),
                &sample(Some(4.0)),
                Duration::from_secs(1)
            ),
            None
        );
        // No wall time, or no CPU reading at either end: no rate.
        assert_eq!(
            PerfRecord::cpu_percent_between(&sample(Some(1.0)), &sample(Some(2.0)), Duration::ZERO),
            None
        );
        assert_eq!(
            PerfRecord::cpu_percent_between(
                &sample(None),
                &sample(Some(2.0)),
                Duration::from_secs(1)
            ),
            None
        );
        assert_eq!(
            PerfRecord::cpu_percent_between(
                &sample(Some(1.0)),
                &sample(None),
                Duration::from_secs(1)
            ),
            None
        );
    }

    #[test]
    fn perf_record_cpu_renders_in_both_formats() {
        let record = PerfRecord::cpu(97.5);
        assert_eq!(record.event_name(), "cpu");
        assert_eq!(record.to_text(), "cpu percent=97.5");
        let value = record.to_json(Duration::from_millis(20));
        assert_eq!(value["event"], "cpu");
        assert_eq!(value["percent"], 97.5);
        assert_eq!(value["ts_ms"], 20.0);
    }

    #[test]
    fn perf_record_measure_start_is_just_the_marker() {
        let record = PerfRecord::measure_start();
        assert_eq!(record.event_name(), "measure_start");
        assert_eq!(record.to_text(), "measure_start");
        let value = record.to_json(Duration::from_millis(12));
        assert_eq!(value["event"], "measure_start");
        assert_eq!(value["ts_ms"], 12.0);
        // Nothing else: the marker carries no payload of its own.
        assert_eq!(value.as_object().unwrap().len(), 2);
    }

    #[test]
    fn perf_record_tab_suspend_carries_the_reason_in_both_formats() {
        use crate::browser::suspension::SuspendReason;
        let record = PerfRecord::tab_suspend(9, SuspendReason::Memory);
        assert_eq!(record.event_name(), "tab_suspend");
        assert_eq!(record.to_text(), "tab_suspend id=9 reason=memory");
        let value = record.to_json(Duration::from_millis(5));
        assert_eq!(value["event"], "tab_suspend");
        assert_eq!(value["ts_ms"], 5.0);
        assert_eq!(value["tab_id"], 9);
        assert_eq!(value["reason"], "memory");
        assert_eq!(
            PerfRecord::tab_suspend(1, SuspendReason::Idle).to_json(Duration::ZERO)["reason"],
            "idle"
        );
        assert_eq!(
            PerfRecord::tab_suspend(1, SuspendReason::TabCount).to_json(Duration::ZERO)["reason"],
            "tab_count"
        );
    }

    // -- PerfRecord: JSON Lines output ------------------------------------

    #[test]
    fn perf_record_json_always_has_event_and_ts_ms() {
        let record =
            PerfRecord::page_load("https://example.com/", Duration::from_millis(250), None);
        let value = record.to_json(Duration::from_millis(1234));
        assert_eq!(value["event"], "page_load");
        assert_eq!(value["ts_ms"], 1234.0);
        assert_eq!(value["url"], "https://example.com/");
        assert_eq!(value["duration_ms"], 250.0);
        // Issue #69: the key is always present, `null` (not absent) when
        // `LoadStarted` never fired — same convention as `total_pss_bytes`.
        assert!(value["engine_duration_ms"].is_null());
        assert!(value["dispatch_duration_ms"].is_null());
    }

    #[test]
    fn perf_record_json_reports_engine_and_dispatch_when_present() {
        let record = PerfRecord::page_load(
            "https://example.com/",
            Duration::from_millis(250),
            Some(Duration::from_millis(230)),
        );
        let value = record.to_json(Duration::ZERO);
        assert_eq!(value["engine_duration_ms"], 230.0);
        assert_eq!(value["dispatch_duration_ms"], 20.0);
    }

    #[test]
    fn perf_record_json_line_is_valid_single_line_json() {
        let record = PerfRecord::tab_latency(TabLatencyKind::Switch, 3, Duration::from_millis(5));
        let line = record.to_json_line(Duration::ZERO);
        assert!(!line.contains('\n'));
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(parsed["event"], "tab_switch");
        assert_eq!(parsed["tab_id"], 3);
        assert_eq!(parsed["duration_ms"], 5.0);
    }

    #[test]
    fn perf_record_startup_json_has_all_nine_checkpoints() {
        let value = PerfRecord::startup(full_startup_report()).to_json(Duration::from_millis(30));
        // Issue #182's four sub-checkpoints of `process_start` ->
        // `window_created`, in chronological order ...
        assert_eq!(value["event_loop_ms"], 2.0);
        assert_eq!(value["pre_window_setup_ms"], 4.0);
        assert_eq!(value["native_window_ms"], 5.0);
        assert_eq!(value["toolbar_webview_ms"], 8.0);
        // ... then the five that predate it.
        assert_eq!(value["window_created_ms"], 10.0);
        assert_eq!(value["rust_setup_done_ms"], 12.0);
        assert_eq!(value["toolbar_script_started_ms"], 15.0);
        assert_eq!(value["toolbar_ready_ms"], 20.0);
        assert_eq!(value["first_load_ms"], 30.0);
    }

    /// Issue #182: a build that never marked the four new checkpoints (or a
    /// run that somehow skipped them) must still emit the keys, as JSON
    /// `null` — never an absent key and never a fabricated `0`, matching
    /// `total_pss_bytes` (D42) and `engine_duration_ms` (D87).
    #[test]
    fn perf_record_startup_json_nulls_absent_sub_checkpoints() {
        let report = StartupReport {
            to_event_loop_built: None,
            to_pre_window_setup_done: None,
            to_native_window_built: None,
            to_toolbar_webview_built: None,
            ..full_startup_report()
        };
        let value = PerfRecord::startup(report).to_json(Duration::from_millis(30));
        for key in [
            "event_loop_ms",
            "pre_window_setup_ms",
            "native_window_ms",
            "toolbar_webview_ms",
        ] {
            assert!(value.get(key).is_some(), "{key} must still be present");
            assert!(value[key].is_null(), "{key} must be null, not 0");
        }
        // The five original checkpoints are unaffected.
        assert_eq!(value["window_created_ms"], 10.0);
        assert_eq!(value["first_load_ms"], 30.0);
    }

    #[test]
    fn perf_record_rss_json_has_pid_process_count_and_bytes() {
        let sample = RssSample {
            root_pid: 42,
            process_count: 3,
            total_rss_bytes: 2 * 1024 * 1024,
            total_pss_bytes: Some(1024 * 1024),
            pss_process_count: 2,
            total_cpu_seconds: None,
        };
        let value = PerfRecord::rss(sample).to_json(Duration::ZERO);
        assert_eq!(value["pid"], 42);
        assert_eq!(value["process_count"], 3);
        assert_eq!(value["total_rss_bytes"], 2 * 1024 * 1024);
        assert_eq!(value["total_pss_bytes"], 1024 * 1024);
        assert_eq!(value["pss_process_count"], 2);
    }

    #[test]
    fn perf_record_rss_json_total_pss_bytes_is_null_when_unavailable() {
        let sample = RssSample {
            root_pid: 42,
            process_count: 3,
            total_rss_bytes: 2 * 1024 * 1024,
            total_pss_bytes: None,
            pss_process_count: 0,
            total_cpu_seconds: None,
        };
        let value = PerfRecord::rss(sample).to_json(Duration::ZERO);
        // `null`, not an absent key — a consumer must be able to tell
        // "unmeasured" from "field not implemented yet" by parsing this
        // value, per the JSON schema in docs/architecture.md.
        assert!(value["total_pss_bytes"].is_null());
        assert_eq!(value["pss_process_count"], 0);
    }

    #[test]
    fn perf_record_ipc_renders_in_both_formats() {
        let started = Instant::now();
        let record = PerfRecord::ipc(IpcDirection::In, "navigate", 42, started);
        assert_eq!(record.event_name(), "ipc");
        let text = record.to_text();
        assert!(text.starts_with("ipc dir=in name=navigate bytes=42 duration="));
        let value = record.to_json(Duration::from_millis(7));
        assert_eq!(value["event"], "ipc");
        assert_eq!(value["ts_ms"], 7.0);
        assert_eq!(value["direction"], "in");
        assert_eq!(value["name"], "navigate");
        assert_eq!(value["bytes"], 42);
        assert!(value["duration_ms"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn perf_record_ipc_out_direction_renders_as_out() {
        let record = PerfRecord::ipc(IpcDirection::Out, "set_tabs", 128, Instant::now());
        assert_eq!(record.to_json(Duration::ZERO)["direction"], "out");
        assert!(record.to_text().contains("dir=out"));
        assert!(record.to_text().contains("name=set_tabs"));
        assert!(record.to_text().contains("bytes=128"));
    }

    #[test]
    fn perf_record_state_write_renders_in_both_formats() {
        let record = PerfRecord::state_write(StateWriteKind::Session, Duration::from_millis(3));
        assert_eq!(record.event_name(), "state_write");
        let text = record.to_text();
        assert_eq!(text, "state_write name=session duration=3.0ms");
        let value = record.to_json(Duration::from_millis(7));
        assert_eq!(value["event"], "state_write");
        assert_eq!(value["ts_ms"], 7.0);
        assert_eq!(value["name"], "session");
        assert_eq!(value["duration_ms"], 3.0);
    }

    #[test]
    fn state_write_kind_as_str_matches_persistence_file_stems() {
        assert_eq!(StateWriteKind::Session.as_str(), "session");
        assert_eq!(StateWriteKind::History.as_str(), "history");
        assert_eq!(StateWriteKind::Bookmarks.as_str(), "bookmarks");
        assert_eq!(StateWriteKind::InputHistory.as_str(), "input_history");
    }

    #[test]
    fn ms_rounds_to_one_decimal_place() {
        assert_eq!(ms(Duration::from_micros(1_549)), 1.5);
        assert_eq!(ms(Duration::ZERO), 0.0);
    }

    // -- filetime_ticks_to_seconds (Issue #136, Windows CPU time) --------
    //
    // Pure arithmetic, so this runs on every platform this project's CI
    // exercises (Linux) even though the only caller (`imp::query_process`)
    // is `#[cfg(target_os = "windows")]` — see the function's doc comment.

    #[test]
    fn filetime_ticks_to_seconds_converts_low_half_only() {
        // 10_000_000 ticks * 100ns/tick = 1.0 second exactly.
        assert_eq!(filetime_ticks_to_seconds(10_000_000, 0), 1.0);
        assert_eq!(filetime_ticks_to_seconds(0, 0), 0.0);
    }

    #[test]
    fn filetime_ticks_to_seconds_combines_high_and_low_halves() {
        // high=1 contributes 2^32 ticks = 429.4967296 seconds; low=0 adds
        // nothing on top.
        let expected = (1u64 << 32) as f64 * 1e-7;
        assert_eq!(filetime_ticks_to_seconds(0, 1), expected);
    }

    #[test]
    fn filetime_ticks_to_seconds_matches_manual_tick_math_for_arbitrary_input() {
        let low = 123_456_789u32;
        let high = 7u32;
        let ticks = ((high as u64) << 32) | low as u64;
        assert_eq!(filetime_ticks_to_seconds(low, high), ticks as f64 * 1e-7);
    }
}
