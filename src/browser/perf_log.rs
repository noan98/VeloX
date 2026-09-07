//! Thin IO layer that writes [`crate::browser::metrics::PerfRecord`] lines
//! somewhere — stderr by default, or an append-mode file when
//! `Config::perf_output_path` is set (so Issue #14's benchmark runner can
//! point `VELOX_PERF_OUTPUT` at a file instead of scraping stderr).
//!
//! Mirrors `persistence.rs`'s role for this module: `browser::metrics` stays
//! pure/unit-testable logic (see its module doc comment), and this file is
//! the one deliberately "dumb" exception that actually touches the
//! filesystem/stderr.
//!
//! Shared across threads — the RSS sampler thread (`app::spawn_rss_sampler`)
//! and the main event loop both write through the same [`PerfLog`], so
//! writes are serialized behind a `Mutex`: two records landing at the same
//! instant must not interleave into one garbled, unparsable line.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::metrics::{IpcDirection, PerfFormat, PerfRecord};

/// Where perf lines actually go.
enum Sink {
    Stderr,
    File(std::fs::File),
}

/// Writes [`PerfRecord`]s in a configured [`PerfFormat`] to a configured
/// [`Sink`]. Construct once per run (see `app::run`) and share via `Arc`
/// with any background sampler thread — never fatal to construct or write
/// to, matching this project's `log_failure`/`log_io_failure` philosophy
/// (`app.rs`): a write failure is logged to stderr and otherwise dropped.
pub struct PerfLog {
    format: PerfFormat,
    sink: Mutex<Sink>,
}

impl PerfLog {
    /// Write to stderr — the original (pre-Issue #13) behavior, and the
    /// fallback used when no output file was requested or opening a
    /// requested one failed (see `app::build_perf_log`).
    pub fn stderr(format: PerfFormat) -> Self {
        Self {
            format,
            sink: Mutex::new(Sink::Stderr),
        }
    }

    /// Open `path` in append mode and write there instead. Returns an error
    /// rather than falling back itself, so the caller can log which path
    /// failed before falling back to [`Self::stderr`].
    pub fn to_file(format: PerfFormat, path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            format,
            sink: Mutex::new(Sink::File(file)),
        })
    }

    /// Render `record` per this log's [`PerfFormat`] and write it as one
    /// line. `elapsed` is the monotonic time since process start, used only
    /// by the JSON format's `ts_ms` field (the text format matches this
    /// project's original lines exactly, which never had a timestamp).
    pub fn write(&self, record: &PerfRecord, elapsed: Duration) {
        let line = match self.format {
            PerfFormat::Text => format!("velox[perf] {}", record.to_text()),
            PerfFormat::Json => record.to_json_line(elapsed),
        };
        let Ok(mut sink) = self.sink.lock() else {
            // A poisoned mutex means some earlier writer panicked mid-write;
            // metrics are best-effort, so drop this line rather than
            // panicking every subsequent caller too.
            return;
        };
        let result = match &mut *sink {
            Sink::Stderr => writeln!(io::stderr(), "{line}"),
            Sink::File(file) => writeln!(file, "{line}"),
        };
        if let Err(err) = result {
            eprintln!("velox: failed to write perf log line: {err}");
        }
    }
}

/// Where to send IPC-traffic records (Issue #66): a [`PerfLog`] sink plus
/// the epoch its `ts_ms` timestamps are relative to, bundled so a caller
/// only has to hold and clone one `Option<IpcLog>` field. `Clone`-able
/// (`Arc`-backed `PerfLog`, `Copy` `Instant`) so both `app::AppState` (via
/// `PerfContext::to_ipc_log`) and every open `ui::window::BrowserWindow`
/// can hold their own copy pointing at the same underlying log.
///
/// Structurally identical to `app::PerfContext` (also a `Arc<PerfLog>` +
/// `Instant` pair, for tab-latency logging) — kept as a separate `pub` type
/// rather than reusing that one because `PerfContext`'s fields are private
/// to `app.rs` and `ui::window` cannot depend on `app` (docs/architecture.md
/// layers UI toolkit code below `app.rs`'s event dispatch, not above it).
#[derive(Clone)]
pub struct IpcLog {
    log: Arc<PerfLog>,
    process_start: Instant,
}

impl IpcLog {
    pub fn new(log: Arc<PerfLog>, process_start: Instant) -> Self {
        Self { log, process_start }
    }

    /// Record one IPC message. `started` is when the caller began the
    /// Rust-side work being measured (`Instant::now()` right before
    /// `parse_command`/`evaluate_script`) — mirrors
    /// `PerfRecord::ipc`/`PerfRecord::tab_latency`'s "caller only brackets a
    /// clock read" contract.
    pub fn record(
        &self,
        direction: IpcDirection,
        name: impl Into<String>,
        bytes: usize,
        started: Instant,
    ) {
        self.log.write(
            &PerfRecord::ipc(direction, name, bytes, started),
            self.process_start.elapsed(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::metrics::{RssSample, TabLatencyKind};
    use std::fs;

    /// A path under the OS temp dir unique to this test process + thread,
    /// so parallel `cargo test` runs never collide.
    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "velox-perf-log-test-{}-{:?}-{name}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn stderr_write_does_not_panic_for_either_format() {
        let log = PerfLog::stderr(PerfFormat::Text);
        log.write(
            &PerfRecord::tab_latency(TabLatencyKind::Create, 1, Duration::from_millis(5)),
            Duration::ZERO,
        );

        let log = PerfLog::stderr(PerfFormat::Json);
        log.write(
            &PerfRecord::rss(RssSample {
                root_pid: 1,
                process_count: 1,
                total_rss_bytes: 1024,
                total_pss_bytes: None,
                pss_process_count: 0,
                total_cpu_seconds: None,
            }),
            Duration::from_millis(10),
        );
    }

    #[test]
    fn to_file_appends_text_lines() {
        let path = temp_path("text.log");
        let _ = fs::remove_file(&path);

        let log = PerfLog::to_file(PerfFormat::Text, &path).expect("open perf log file");
        log.write(
            &PerfRecord::page_load("https://example.com/", Duration::from_millis(250), None),
            Duration::ZERO,
        );
        log.write(
            &PerfRecord::tab_latency(TabLatencyKind::Switch, 2, Duration::from_millis(3)),
            Duration::ZERO,
        );
        drop(log);

        let contents = fs::read_to_string(&path).expect("read perf log file");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "velox[perf] page_load url=https://example.com/ duration=250.0ms"
        );
        assert_eq!(lines[1], "velox[perf] tab_switch id=2 duration=3.0ms");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn to_file_appends_json_lines_across_reopens() {
        let path = temp_path("json.log");
        let _ = fs::remove_file(&path);

        {
            let log = PerfLog::to_file(PerfFormat::Json, &path).expect("open perf log file");
            log.write(
                &PerfRecord::tab_latency(TabLatencyKind::Create, 9, Duration::from_millis(1)),
                Duration::from_millis(100),
            );
        }
        {
            // A second `PerfLog` over the same path (e.g. a fresh process
            // run) must append, not truncate — one JSON object per line is
            // only valid if earlier lines survive.
            let log = PerfLog::to_file(PerfFormat::Json, &path).expect("reopen perf log file");
            log.write(
                &PerfRecord::tab_latency(TabLatencyKind::Switch, 9, Duration::from_millis(2)),
                Duration::from_millis(200),
            );
        }

        let contents = fs::read_to_string(&path).expect("read perf log file");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).expect("each line is valid JSON");
        }
        assert!(lines[0].contains("\"tab_create\""));
        assert!(lines[1].contains("\"tab_switch\""));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn to_file_fails_for_an_unwritable_path() {
        // A path inside a nonexistent directory can never be opened for
        // append; the caller (`app::build_perf_log`) falls back to stderr.
        let path = temp_path("does-not-exist-dir")
            .join("nested")
            .join("log.jsonl");
        assert!(PerfLog::to_file(PerfFormat::Text, &path).is_err());
    }

    #[test]
    fn ipc_log_writes_an_ipc_record() {
        use crate::browser::metrics::IpcDirection;
        use std::time::Instant;

        let path = temp_path("ipc.jsonl");
        let _ = fs::remove_file(&path);

        let log = Arc::new(PerfLog::to_file(PerfFormat::Json, &path).expect("open perf log file"));
        let ipc = IpcLog::new(Arc::clone(&log), Instant::now());
        ipc.record(IpcDirection::In, "navigate", 17, Instant::now());
        drop(log);

        let contents = fs::read_to_string(&path).expect("read perf log file");
        let line = contents.lines().next().expect("one ipc line written");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(value["event"], "ipc");
        assert_eq!(value["direction"], "in");
        assert_eq!(value["name"], "navigate");
        assert_eq!(value["bytes"], 17);

        let _ = fs::remove_file(&path);
    }
}
