//! `velox-bench` — the runner half of Issue #14's benchmark suite.
//!
//! All aggregation/comparison arithmetic lives in `velox::browser::benchmark`
//! (pure Rust, fully covered by `cargo test`, no process or WebView
//! dependency). This binary is the deliberately "dumb" IO layer around it:
//! it spawns the compiled `velox` binary, waits, reads back whatever
//! `VELOX_PERF_OUTPUT` JSON Lines it produced, and hands that to
//! `benchmark::aggregate_trials`/`benchmark::compare`.
//!
//! **This binary needs a real display to do anything useful for the
//! `run` subcommand** — VeloX creates a WebKitGTK/WKWebView/WebView2 window,
//! which fails immediately in a headless environment (no `DISPLAY`, no
//! Xvfb). That is expected here and is exactly why the arithmetic above
//! lives in a separately-tested pure module instead of being folded into
//! this file: `cargo test` never touches this binary's `main`, so a
//! headless CI/dev container never fails over it. See
//! `docs/benchmarking.md` for the full methodology, environment
//! requirements, and worked examples of every subcommand below.
//!
//! Subcommands:
//! - `list-scenarios` — print the fixed scenario catalog
//!   (`benchmark::scenario::Scenario::all`).
//! - `run` — spawn the `velox` binary N times and aggregate the results.
//!   Every scenario is unattended (`Scenario::is_unattended`): the three
//!   startup scenarios need no interaction after launch, and — since
//!   Issue #112 — `navigation`/`tab_create`/`tab_switch`/`tabs_N` are
//!   driven by a generated `VELOX_AUTOMATION_SCRIPT`
//!   (`velox::browser::automation::generate_bench_script`) instead of a
//!   human at the keyboard. See `docs/decisions.md` D44.
//! - `aggregate` — build the same machine-readable result file from
//!   already-collected `VELOX_PERF_OUTPUT` log files (one per trial) — the
//!   path for a manually-driven trial, or one collected some other way
//!   outside `run` entirely.
//! - `compare` — diff two saved result files and exit non-zero on a
//!   regression beyond `--threshold-pct`, the original two-file diff Issue
//!   #36 asked for.
//! - `gate` (Issue #72) — the CI-shaped regression gate: one baseline
//!   result against one or more candidate results, majority-voted into a
//!   warn/fail verdict via `benchmark::evaluate_gate`. See that function's
//!   module docs (`src/browser/benchmark.rs`) and `docs/decisions.md` D46
//!   for why this is not just `compare` with a stricter threshold — a
//!   single median comparison in this environment cannot distinguish a
//!   real regression from session-to-session noise.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use velox::browser::automation;
use velox::browser::benchmark::scenario::Scenario;
use velox::browser::benchmark::{
    self, BenchmarkResult, ComparisonReport, GateThresholds, IpcSummary, MemorySampleConfidence,
    MetricDiff, RunEnvironment, Severity,
};

fn main() {
    let mut args = env::args().skip(1);
    let subcommand = args.next();
    let rest: Vec<String> = args.collect();

    let result = match subcommand.as_deref() {
        Some("list-scenarios") => cmd_list_scenarios(),
        Some("run") => cmd_run(&rest),
        Some("aggregate") => cmd_aggregate(&rest),
        Some("compare") => cmd_compare(&rest),
        Some("gate") => cmd_gate(&rest),
        Some("ipc-summary") => cmd_ipc_summary(&rest),
        Some(other) => Err(format!("未知のサブコマンドです: {other}\n\n{USAGE}")),
        None => Err(USAGE.to_owned()),
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("velox-bench: {message}");
            std::process::exit(2);
        }
    }
}

const USAGE: &str = "使い方:\n\
  velox-bench list-scenarios\n\
  velox-bench run --scenario <id> --trials <N> --output <path> [--url <URL>] [--velox-bin <path>] [--warmup-secs <secs>] [--rss-interval-ms <ms>] [--git-commit <sha>]\n\
  velox-bench aggregate --scenario <id> --output <path> --input <path> [--input <path> ...] [--git-commit <sha>]\n\
  velox-bench compare --baseline <path> --candidate <path> [--threshold-pct <pct>] [--output <path>]\n\
  velox-bench gate --baseline <path> --candidate <path> [--candidate <path> ...] \\\n\
      [--warn-pct <pct>] [--fail-pct <pct>] [--output <path>] [--markdown-output <path>]\n\
  velox-bench ipc-summary --input <path> [--input <path> ...] [--output <path>]\n\n\
gate の終了コード: 0=OK, 1=FAIL (CIをブロックすべき), 3=WARN (非ブロッキング、要確認)。\n\
それ以外の引数エラー等は 2。詳細は docs/benchmarking.md を参照してください。";

// ---------------------------------------------------------------------
// list-scenarios
// ---------------------------------------------------------------------

fn cmd_list_scenarios() -> Result<i32, String> {
    let scenarios = Scenario::all();
    // 列幅は実際の ID から決める。固定値 (以前は 16) だと、それより
    // 長い ID を持つシナリオが増えた時点で列が崩れる —
    // `tabs_hold_resume_50` (19 文字、Issue #176) で実際に崩れた。
    let width = scenarios
        .iter()
        .map(|scenario| scenario.id().chars().count())
        .chain(std::iter::once("scenario".chars().count()))
        .max()
        .unwrap_or(16);
    println!("{:<width$} 自動実行 (run)", "scenario");
    for scenario in scenarios {
        // Every scenario is unattended as of Issue #112 (see
        // `Scenario::is_unattended`'s doc comment); what differs is
        // whether `run` needs `--url` to build a
        // `VELOX_AUTOMATION_SCRIPT` for it.
        let note = if automation::needs_automation_script(scenario) {
            "可 (run で自動実行可能、--url 必須)"
        } else {
            "可 (run で自動実行可能)"
        };
        println!("{:<width$} {}", scenario.id(), note);
    }
    Ok(0)
}

// ---------------------------------------------------------------------
// Shared flag parsing
// ---------------------------------------------------------------------

/// Minimal `--flag value` parser (no CLI-parsing crate, matching
/// `Config::from_env_and_args`'s policy — see docs/decisions.md D6).
/// Repeatable flags (like `aggregate`'s `--input`) accumulate; the rest
/// keep only their last occurrence.
struct Flags {
    values: std::collections::HashMap<String, Vec<String>>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut values: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("不正な引数です: {arg}"));
            };
            let value = iter
                .next()
                .ok_or_else(|| format!("--{name} には値が必要です"))?;
            values
                .entry(name.to_owned())
                .or_default()
                .push(value.clone());
        }
        Ok(Self { values })
    }

    fn one(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.last())
            .map(|s| s.as_str())
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.one(name).ok_or_else(|| format!("--{name} は必須です"))
    }

    fn many(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------
// run
// ---------------------------------------------------------------------

fn cmd_run(args: &[String]) -> Result<i32, String> {
    let flags = Flags::parse(args)?;
    let scenario_id = flags.required("scenario")?;
    let scenario = Scenario::parse(scenario_id)
        .ok_or_else(|| format!("未知のシナリオです: {scenario_id} (list-scenarios を参照)"))?;
    // As of Issue #112 every scenario is unattended (`is_unattended` always
    // returns `true` now — see its doc comment); this guard is kept so a
    // future scenario that genuinely needs a human still fails loudly here
    // instead of silently trying to run.
    if !scenario.is_unattended() {
        return Err(format!(
            "シナリオ {scenario_id} は run では自動実行できません。実際に VeloX を \
             手動で操作し、VELOX_PERF_OUTPUT に書き出したログファイルを \
             `velox-bench aggregate --scenario {scenario_id} --input <path> ...` \
             に渡してください。docs/benchmarking.md 参照。"
        ));
    }

    let trials: u32 = flags
        .required("trials")?
        .parse()
        .map_err(|_| "--trials は正の整数で指定してください".to_owned())?;
    if trials == 0 {
        return Err("--trials は 1 以上を指定してください".to_owned());
    }
    let output_path = flags.required("output")?;
    let velox_bin = match flags.one("velox-bin") {
        Some(path) => PathBuf::from(path),
        None => default_velox_bin_path()?,
    };
    // `--rss-interval-ms` always wins when given explicitly. Otherwise, for
    // `tabs_N` scenarios only, fall back to
    // `automation::recommended_rss_interval_ms` rather than leaving this
    // `None` (which would leave `Config`'s 5000ms default in effect) — see
    // that function's doc comment and docs/decisions.md D50 for why the
    // 5000ms default undersamples a `tabs_N` run badly enough that its
    // `pss_total_bytes`/`rss_total_bytes` stop tracking tab count at all.
    // Every other scenario keeps getting `None` here exactly as before, so
    // this cannot change `cold_startup`/`warm_startup`/`first_page_load`'s
    // (or `navigation`/`tab_create`/`tab_switch`'s) behavior.
    let rss_interval_ms: Option<String> = flags
        .one("rss-interval-ms")
        .map(str::to_owned)
        .or_else(|| automation::recommended_rss_interval_ms(scenario).map(|ms| ms.to_string()));
    // The page every trial loads. Handed to VeloX as `VELOX_HOMEPAGE`
    // (Issue #106): without it every trial would measure whatever the
    // compiled-in default homepage is, which is network-dependent and
    // therefore not reproducible. Point this at a `scripts/bench/pages/`
    // fixture served over loopback for comparable numbers.
    let url = flags.one("url");

    // Issue #112: `navigation`/`tab_create`/`tab_switch`/`tabs_N` need an
    // automation script (`browser::automation::generate_bench_script`) to
    // open/switch/navigate tabs, and that script needs a concrete page to
    // point at — unlike the three startup scenarios, there is no sensible
    // "measure whatever the default homepage is" fallback here, since the
    // whole point is comparable, network-independent pages.
    if automation::needs_automation_script(scenario) && url.is_none() {
        return Err(format!(
            "シナリオ {scenario_id} の自動実行には --url が必要です \
             (scripts/bench/pages/ の固定ページを指定してください。docs/benchmarking.md 参照)。"
        ));
    }
    let automation_script = url.and_then(|url| automation::generate_bench_script(scenario, url));

    // A script ends with `quit`, so a trial normally exits on its own well
    // before this — see `wait_for_exit_or_timeout`. `--warmup-secs`
    // overrides the default either way (e.g. to force a longer wait on a
    // slower machine).
    let default_warmup_secs = automation::recommended_timeout_secs(scenario);
    let warmup_secs: u64 = flags
        .one("warmup-secs")
        .map(|v| v.parse().unwrap_or(default_warmup_secs))
        .unwrap_or(default_warmup_secs);

    match url {
        Some(url) => println!(
            "velox-bench: {scenario_id} を {trials} 回実行します (velox バイナリ: {}, URL: {url}{})",
            velox_bin.display(),
            if automation_script.is_some() {
                "、自動操作スクリプトあり"
            } else {
                ""
            }
        ),
        None => println!(
            "velox-bench: {scenario_id} を {trials} 回実行します (velox バイナリ: {})\n\
             velox-bench: 警告: --url が未指定のため VeloX の既定ホームページを計測します。\n\
             velox-bench: 再現性のある結果には scripts/bench/pages/ の固定ページを \
             --url で指定してください (docs/benchmarking.md 参照)。",
            velox_bin.display()
        ),
    }
    if flags.one("rss-interval-ms").is_none() {
        if let Some(interval) = &rss_interval_ms {
            println!(
                "velox-bench: {scenario_id} は RSS/PSS サンプリング間隔を \
                 {interval}ms に自動調整します (タブを開き終えた後の状態を確実に \
                 サンプリングするため、既定の間隔のままだと使えません。\
                 docs/decisions.md D50 参照。--rss-interval-ms で上書き可能)。",
            );
        }
    }

    // Written once (its content only depends on `scenario`/`url`, not on
    // the trial number) and removed again once every trial has run.
    let script_path = match &automation_script {
        Some(text) => {
            let path = env::temp_dir().join(format!(
                "velox-bench-{}-{scenario_id}-script.txt",
                std::process::id()
            ));
            fs::write(&path, text)
                .map_err(|err| format!("自動操作スクリプトを書き出せません: {err}"))?;
            Some(path)
        }
        None => None,
    };

    let mut trial_events = Vec::with_capacity(trials as usize);
    let mut spawn_failures = 0u32;
    for trial in 1..=trials {
        let log_path = env::temp_dir().join(format!(
            "velox-bench-{}-{scenario_id}-{trial}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_file(&log_path);

        let mut command = Command::new(&velox_bin);
        command
            .env("VELOX_PERF_METRICS", "1")
            .env("VELOX_PERF_FORMAT", "json")
            .env("VELOX_PERF_OUTPUT", &log_path);
        if let Some(interval) = &rss_interval_ms {
            command.env("VELOX_PERF_RSS_INTERVAL_MS", interval);
        }
        if let Some(url) = url {
            command.env("VELOX_HOMEPAGE", url);
        }
        if let Some(script_path) = &script_path {
            command.env("VELOX_AUTOMATION_SCRIPT", script_path);
        }

        match command.spawn() {
            Ok(mut child) => {
                wait_for_exit_or_timeout(&mut child, Duration::from_secs(warmup_secs));
                terminate(child);
            }
            Err(err) => {
                spawn_failures += 1;
                eprintln!(
                    "velox-bench: 試行 {trial}/{trials}: {} の起動に失敗しました: {err} \
                     (ヘッドレス環境では想定内です。docs/benchmarking.md 参照)",
                    velox_bin.display()
                );
            }
        }

        let text = fs::read_to_string(&log_path).unwrap_or_default();
        let events = benchmark::parse_jsonl(&text);
        println!(
            "  試行 {trial}/{trials}: {} 件のレコードを取得",
            events.len()
        );
        trial_events.push(events);
        let _ = fs::remove_file(&log_path);
    }

    if let Some(script_path) = &script_path {
        let _ = fs::remove_file(script_path);
    }

    let metrics = benchmark::aggregate_trials(&trial_events);
    let total_events: usize = trial_events.iter().map(Vec::len).sum();

    let environment = collect_environment(trials, flags.one("git-commit").map(str::to_owned));
    let result = BenchmarkResult {
        scenario: scenario_id.to_owned(),
        environment,
        metrics,
    };
    write_result(output_path, &result)?;
    print_result_summary(&result);

    if total_events == 0 {
        eprintln!(
            "velox-bench: 警告: どの試行からもレコードを取得できませんでした \
             (spawn 失敗 {spawn_failures}/{trials})。この環境では GUI を起動できない \
             可能性があります — docs/benchmarking.md の実行環境要件を確認してください。\
             結果ファイルは書き出しましたが、metrics は空です。"
        );
        return Ok(1);
    }

    // Issue #119 / D50: a `tabs_N` scenario can produce *some* records
    // (total_events > 0 above) while still not having sampled memory densely
    // enough to trust — the original bug. Never let that pass silently: the
    // saved result already carries whatever `pss_total_bytes`/
    // `rss_total_bytes` samples were taken (nothing is dropped, matching
    // D42's "partial data is still real data" rule), but a caller must be
    // told loudly that this scenario's memory figures may not reflect "all
    // tabs open" before trusting them.
    if warn_on_insufficient_memory_samples(scenario_id, scenario, trials, &result.metrics) {
        return Ok(1);
    }
    Ok(0)
}

/// Check `metrics` via [`benchmark::memory_sample_confidence`] and, if
/// insufficient, print a warning explaining why. Returns `true` when a
/// warning was printed, so callers can fold it into their exit code exactly
/// like the existing "zero records" check above. A no-op (returns `false`)
/// for every scenario `memory_sample_confidence` does not apply to.
fn warn_on_insufficient_memory_samples(
    scenario_id: &str,
    scenario: Scenario,
    trials: u32,
    metrics: &std::collections::BTreeMap<String, benchmark::Stats>,
) -> bool {
    let MemorySampleConfidence::Insufficient { observed, required } =
        benchmark::memory_sample_confidence(scenario, trials, metrics)
    else {
        return false;
    };
    eprintln!(
        "velox-bench: 警告: {scenario_id} の PSS/RSS サンプル数が不足しています \
         ({observed} 件 / 最低 {required} 件必要、trials={trials})。タブを開き \
         終える前 (またはごく初期) の状態しかサンプリングできていない可能性が \
         あり、pss_total_bytes/rss_total_bytes をタブ数比較の根拠にしないで \
         ください。結果ファイルには採取できたサンプルをそのまま書き出しました \
         — docs/benchmarking.md と docs/decisions.md の D50 を参照してください。"
    );
    true
}

/// Poll `child` until it exits on its own or `timeout` elapses, whichever
/// comes first, then return either way — `terminate` (called right after)
/// is always the one that actually reaps it. A `VELOX_AUTOMATION_SCRIPT`
/// (Issue #112) ends with `quit`, so a scripted trial's process usually
/// exits well before `timeout`; this lets that trial move on immediately
/// instead of always waiting out the full timeout, while a scenario with
/// no script (or a script that never reaches `quit`) simply waits out
/// `timeout` exactly like the pre-#112 fixed `sleep`.
fn wait_for_exit_or_timeout(child: &mut Child, timeout: Duration) {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            // Exited on its own (normally or otherwise) — nothing left to
            // wait for.
            Ok(Some(_status)) => return,
            // Still running.
            Ok(None) => {}
            // Can no longer observe this child's state; give up polling
            // rather than looping forever.
            Err(_) => return,
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Send a graceful terminate-then-kill to `child` and reap it. Perf log
/// lines are flushed as each event happens (`PerfLog::write`), so even an
/// abrupt kill loses nothing already written to `VELOX_PERF_OUTPUT`.
fn terminate(mut child: Child) {
    // `std::process::Child` has no portable "SIGTERM" on all targets via
    // std alone; `kill()` (SIGKILL on Unix, TerminateProcess on Windows) is
    // sufficient here since we only need the process gone, not a chance to
    // clean up — the perf log file is already durable.
    let _ = child.kill();
    let _ = child.wait();
}

fn default_velox_bin_path() -> Result<PathBuf, String> {
    let mut path =
        env::current_exe().map_err(|err| format!("自分自身の実行パスを取得できません: {err}"))?;
    path.pop();
    path.push(if cfg!(windows) { "velox.exe" } else { "velox" });
    Ok(path)
}

// ---------------------------------------------------------------------
// aggregate
// ---------------------------------------------------------------------

fn cmd_aggregate(args: &[String]) -> Result<i32, String> {
    let flags = Flags::parse(args)?;
    let scenario_id = flags.required("scenario")?;
    let scenario = Scenario::parse(scenario_id)
        .ok_or_else(|| format!("未知のシナリオです: {scenario_id} (list-scenarios を参照)"))?;
    let output_path = flags.required("output")?;
    let inputs = flags.many("input");
    if inputs.is_empty() {
        return Err("--input を少なくとも 1 つ指定してください".to_owned());
    }

    let mut trial_events = Vec::with_capacity(inputs.len());
    let mut read_failures = 0usize;
    for input in &inputs {
        match fs::read_to_string(input) {
            Ok(text) => {
                let events = benchmark::parse_jsonl(&text);
                println!("{input}: {} 件のレコード", events.len());
                trial_events.push(events);
            }
            Err(err) => {
                read_failures += 1;
                eprintln!("velox-bench: {input} を読み込めませんでした: {err}");
            }
        }
    }
    if trial_events.is_empty() {
        return Err("読み込めた --input が 1 つもありません".to_owned());
    }

    let metrics = benchmark::aggregate_trials(&trial_events);
    let total_events: usize = trial_events.iter().map(Vec::len).sum();
    let trials = trial_events.len() as u32;
    let environment = collect_environment(trials, flags.one("git-commit").map(str::to_owned));
    let result = BenchmarkResult {
        scenario: scenario_id.to_owned(),
        environment,
        metrics,
    };
    write_result(output_path, &result)?;
    print_result_summary(&result);

    if read_failures > 0 {
        eprintln!("velox-bench: 警告: {read_failures} 件の --input を読み込めませんでした");
    }
    if total_events == 0 {
        eprintln!("velox-bench: 警告: どの入力からもレコードを取得できませんでした");
        return Ok(1);
    }

    // Same D50 safety net as `cmd_run` — `aggregate` builds a
    // `BenchmarkResult` from externally-supplied logs, which can just as
    // easily under-sample a `tabs_N` run (e.g. logs collected by hand, or
    // from an older `velox-bench run` that predates this scenario's
    // auto-tuned interval).
    if warn_on_insufficient_memory_samples(scenario_id, scenario, trials, &result.metrics) {
        return Ok(1);
    }
    Ok(0)
}

// ---------------------------------------------------------------------
// ipc-summary (Issue #66)
// ---------------------------------------------------------------------

/// Aggregate `ipc` events (`metrics::PerfRecord::Ipc`) out of one or more
/// `VELOX_PERF_OUTPUT` JSON Lines files and print a per-`(direction, name)`
/// traffic table — count, total bytes, and the `duration_ms` distribution
/// (`benchmark::summarize_ipc`, the pure half this is the IO layer around).
/// Unlike `aggregate`, this is not scenario-shaped and does not build a
/// `BenchmarkResult`/run a regression gate: it is the diagnostic step Issue
/// #66 asks for ("IPCの回数・サイズ・時間を計測", "高頻度イベントを特定"),
/// meant to be re-run against any real perf log — a manual session, a
/// `velox-bench run` trial, or a future scenario built for this — to keep
/// answering "which toolbar IPC messages dominate, by count and by bytes"
/// as the codebase changes. See docs/benchmarking.md for a worked example.
fn cmd_ipc_summary(args: &[String]) -> Result<i32, String> {
    let flags = Flags::parse(args)?;
    let inputs = flags.many("input");
    if inputs.is_empty() {
        return Err("--input を少なくとも 1 つ指定してください".to_owned());
    }

    let mut events = Vec::new();
    let mut read_failures = 0usize;
    for input in &inputs {
        match fs::read_to_string(input) {
            Ok(text) => events.extend(benchmark::parse_jsonl(&text)),
            Err(err) => {
                read_failures += 1;
                eprintln!("velox-bench: {input} を読み込めませんでした: {err}");
            }
        }
    }
    if read_failures == inputs.len() {
        return Err("読み込めた --input が 1 つもありません".to_owned());
    }

    let rows = benchmark::summarize_ipc(&events);
    print_ipc_summary(&rows);

    if let Some(output_path) = flags.one("output") {
        let json = serde_json::to_string_pretty(&rows)
            .map_err(|err| format!("結果のシリアライズに失敗しました: {err}"))?;
        if let Some(parent) = Path::new(output_path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        fs::write(output_path, json)
            .map_err(|err| format!("{output_path} へ書き込めませんでした: {err}"))?;
        println!("velox-bench: {output_path} に書き込みました");
    }

    if read_failures > 0 {
        eprintln!("velox-bench: 警告: {read_failures} 件の --input を読み込めませんでした");
    }
    if rows.is_empty() {
        eprintln!(
            "velox-bench: 警告: ipc イベントが見つかりませんでした \
             (VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json で採取したログか確認してください)"
        );
        return Ok(1);
    }
    Ok(0)
}

fn print_ipc_summary(rows: &[IpcSummary]) {
    if rows.is_empty() {
        println!("(ipc イベントはありません)");
        return;
    }
    println!(
        "{:<4} {:<24} {:>8} {:>12} {:>10} {:>10}",
        "dir", "name", "count", "total_bytes", "median_ms", "p95_ms"
    );
    for row in rows {
        println!(
            "{:<4} {:<24} {:>8} {:>12} {:>10.3} {:>10.3}",
            row.direction,
            row.name,
            row.count,
            row.total_bytes,
            row.duration_ms.median,
            row.duration_ms.p95,
        );
    }
    let total_count: usize = rows.iter().map(|row| row.count).sum();
    let total_bytes: u64 = rows.iter().map(|row| row.total_bytes).sum();
    println!("\n合計: {total_count} 件 / {total_bytes} bytes");
}

// ---------------------------------------------------------------------
// compare
// ---------------------------------------------------------------------

fn cmd_compare(args: &[String]) -> Result<i32, String> {
    let flags = Flags::parse(args)?;
    let baseline_path = flags.required("baseline")?;
    let candidate_path = flags.required("candidate")?;
    let threshold_pct: f64 = flags
        .one("threshold-pct")
        .map(|v| {
            v.parse()
                .map_err(|_| "--threshold-pct は数値で指定してください".to_owned())
        })
        .transpose()?
        .unwrap_or(10.0);

    let baseline = read_result(baseline_path)?;
    let candidate = read_result(candidate_path)?;
    let report = benchmark::compare(&baseline, &candidate, threshold_pct);

    print_comparison(&report);

    if let Some(output_path) = flags.one("output") {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|err| format!("比較結果のシリアライズに失敗しました: {err}"))?;
        fs::write(output_path, json)
            .map_err(|err| format!("{output_path} へ書き込めませんでした: {err}"))?;
    }

    Ok(if report.any_regressed { 1 } else { 0 })
}

fn read_result(path: &str) -> Result<BenchmarkResult, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{path} を読み込めません: {err}"))?;
    serde_json::from_str(&text).map_err(|err| format!("{path} の解析に失敗しました: {err}"))
}

fn print_comparison(report: &ComparisonReport) {
    println!(
        "比較: baseline={} candidate={} (閾値 {:.1}%)",
        report.baseline_scenario, report.candidate_scenario, report.threshold_pct
    );
    println!(
        "{:<28} {:>14} {:>14} {:>10} {:>8}",
        "metric", "baseline", "candidate", "変化率", "判定"
    );
    for (name, diff) in &report.diffs {
        print_diff_row(name, diff);
    }
    if !report.only_in_baseline.is_empty() {
        println!(
            "baseline のみに存在: {}",
            report.only_in_baseline.join(", ")
        );
    }
    if !report.only_in_candidate.is_empty() {
        println!(
            "candidate のみに存在: {}",
            report.only_in_candidate.join(", ")
        );
    }
    println!(
        "\n結果: {}",
        if report.any_regressed {
            "回帰あり (regressed)"
        } else {
            "回帰なし (ok)"
        }
    );
}

fn print_diff_row(name: &str, diff: &MetricDiff) {
    let pct_display = if diff.pct_change.is_infinite() {
        "inf".to_owned()
    } else {
        format!("{:+.1}%", diff.pct_change)
    };
    println!(
        "{:<28} {:>14.2} {:>14.2} {:>10} {:>8}",
        name,
        diff.baseline_median,
        diff.candidate_median,
        pct_display,
        if diff.regressed { "NG" } else { "OK" }
    );
}

// ---------------------------------------------------------------------
// gate (Issue #72 / D46)
// ---------------------------------------------------------------------

/// `gate`'s exit codes are its CI contract, so they get names rather than
/// bare literals scattered through `cmd_gate`. `Ok`/`Warn` are both
/// non-blocking (`0`/`3`); only `Fail` (`1`) should stop a CI job — see
/// `docs/benchmarking.md` "回帰ゲート (gate)".
const EXIT_GATE_OK: i32 = 0;
const EXIT_GATE_FAIL: i32 = 1;
const EXIT_GATE_WARN: i32 = 3;

fn cmd_gate(args: &[String]) -> Result<i32, String> {
    let flags = Flags::parse(args)?;
    let baseline_path = flags.required("baseline")?;
    let candidate_paths = flags.many("candidate");
    if candidate_paths.is_empty() {
        return Err("--candidate を少なくとも 1 つ指定してください".to_owned());
    }
    let warn_pct: f64 = flags
        .one("warn-pct")
        .map(|v| {
            v.parse()
                .map_err(|_| "--warn-pct は数値で指定してください".to_owned())
        })
        .transpose()?
        .unwrap_or(GateThresholds::default().warn_pct);
    let fail_pct: f64 = flags
        .one("fail-pct")
        .map(|v| {
            v.parse()
                .map_err(|_| "--fail-pct は数値で指定してください".to_owned())
        })
        .transpose()?
        .unwrap_or(GateThresholds::default().fail_pct);
    if fail_pct <= warn_pct {
        return Err(format!(
            "--fail-pct ({fail_pct}) は --warn-pct ({warn_pct}) より大きい必要があります"
        ));
    }
    let thresholds = GateThresholds { warn_pct, fail_pct };

    let baseline = read_result(baseline_path)?;
    let candidates = candidate_paths
        .iter()
        .map(|path| read_result(path))
        .collect::<Result<Vec<_>, _>>()?;
    let candidate_refs: Vec<&BenchmarkResult> = candidates.iter().collect();

    let report = benchmark::evaluate_gate(&baseline, &candidate_refs, &thresholds);
    print_gate_report(&report);

    if let Some(output_path) = flags.one("output") {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|err| format!("ゲート結果のシリアライズに失敗しました: {err}"))?;
        fs::write(output_path, json)
            .map_err(|err| format!("{output_path} へ書き込めませんでした: {err}"))?;
    }
    if let Some(markdown_path) = flags.one("markdown-output") {
        let markdown = benchmark::render_gate_markdown(&report);
        fs::write(markdown_path, markdown)
            .map_err(|err| format!("{markdown_path} へ書き込めませんでした: {err}"))?;
    }

    Ok(match report.overall {
        Severity::Ok => EXIT_GATE_OK,
        Severity::Warn => EXIT_GATE_WARN,
        Severity::Fail => EXIT_GATE_FAIL,
    })
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Ok => "OK",
        Severity::Warn => "WARN",
        Severity::Fail => "FAIL",
    }
}

fn print_gate_report(report: &benchmark::GateReport) {
    println!(
        "regression gate: scenario={} candidates={} (warn>{:.1}% fail>{:.1}%)",
        report.scenario,
        report.candidate_count,
        report.thresholds.warn_pct,
        report.thresholds.fail_pct
    );
    println!(
        "{:<28} {:>14} {:>26} {:>8} {:>8}",
        "metric", "baseline", "candidates (中央値/変化率)", "判定", "備考"
    );
    for (name, verdict) in &report.metrics {
        let candidates_display = verdict
            .candidate_medians
            .iter()
            .zip(&verdict.pct_changes)
            .map(|(median, pct)| {
                if pct.is_infinite() {
                    format!("{median:.1}(inf)")
                } else {
                    format!("{median:.1}({pct:+.1}%)")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{:<28} {:>14.2} {:>26} {:>8} {:>8}",
            name,
            verdict.baseline_median,
            candidates_display,
            severity_label(verdict.severity),
            if verdict.low_confidence {
                "試行数不足"
            } else {
                ""
            }
        );
    }
    if !report.only_in_baseline.is_empty() {
        println!(
            "baseline のみに存在: {}",
            report.only_in_baseline.join(", ")
        );
    }
    if !report.only_in_candidates.is_empty() {
        println!(
            "candidate のみに存在: {}",
            report.only_in_candidates.join(", ")
        );
    }
    // Issue #196: a precondition violation is the reason for the verdict
    // below, so print it right above that verdict — on stderr, since a
    // caller piping stdout into a report file still needs to see it.
    if !report.problems.is_empty() {
        eprintln!("\n入力の前提を満たしていません:");
        for problem in &report.problems {
            eprintln!(
                "  [{}] {}",
                severity_label(problem.severity()),
                problem.describe()
            );
        }
    }
    println!("\n総合判定: {}", severity_label(report.overall));
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

fn collect_environment(trials: u32, git_commit_override: Option<String>) -> RunEnvironment {
    let os = env::consts::OS.to_owned();
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let git_commit = git_commit_override.or_else(detect_git_commit);
    let epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let generated_at = benchmark::format_unix_time_utc(epoch_seconds);

    // Issue #211 (docs/decisions.md D104)。CPU モデル・搭載メモリ・OS
    // バージョン・WebView ランタイムは、まずネイティブに収集を試み
    // (`collect_native_environment_info`)、`VELOX_BENCH_*` 環境変数が
    // 設定されていればそちらを優先する (`apply_environment_overrides`)。
    // CI (`perf-windows.yml`) が PowerShell で既に採った値をそのまま渡せる
    // ようにするため、また実機を持たない開発環境で検証できるようにする
    // ためのオーバーライドである。
    let native = collect_native_environment_info();
    let cpu_model_env = env::var("VELOX_BENCH_CPU_MODEL").ok();
    let total_memory_bytes_env = env::var("VELOX_BENCH_TOTAL_MEMORY_BYTES").ok();
    let os_version_env = env::var("VELOX_BENCH_OS_VERSION").ok();
    let webview_runtime_env = env::var("VELOX_BENCH_WEBVIEW_RUNTIME").ok();
    let machine_info = apply_environment_overrides(
        native,
        cpu_model_env.as_deref(),
        total_memory_bytes_env.as_deref(),
        os_version_env.as_deref(),
        webview_runtime_env.as_deref(),
    );

    RunEnvironment {
        os,
        cpu_count,
        git_commit,
        generated_at,
        trials,
        cpu_model: machine_info.cpu_model,
        total_memory_bytes: machine_info.total_memory_bytes,
        os_version: machine_info.os_version,
        webview_runtime: machine_info.webview_runtime,
    }
}

// ---------------------------------------------------------------------
// 機種情報の収集 (Issue #211, docs/decisions.md D104)
// ---------------------------------------------------------------------

/// `RunEnvironment` の機種情報 4 フィールド分。ネイティブ収集
/// (`collect_native_environment_info`) と `VELOX_BENCH_*` 上書き
/// (`apply_environment_overrides`) の両方が共通で組み立てる中間表現。
#[derive(Debug, Default, Clone, PartialEq)]
struct MachineInfo {
    cpu_model: Option<String>,
    total_memory_bytes: Option<u64>,
    os_version: Option<String>,
    webview_runtime: Option<String>,
}

/// 現在実行中の OS のネイティブ収集を、実行時に `env::consts::OS` で
/// 振り分けて行う。`#[cfg(target_os = ...)]` ではなくランタイム分岐に
/// しているのは、CLAUDE.md が求める
/// `cargo check --target x86_64-pc-windows-msvc` 型チェックを含め、
/// どのターゲットでも全分岐がコンパイル・単体テストされるようにするため
/// (Windows 固有のロジックを Linux の `cargo test` でも検証できる)。
///
/// 取得に失敗しても本関数は絶対にエラーを返さない — 各収集関数は内部で
/// `Result`/`Option` を握りつぶし、取得できなかったフィールドは `None`
/// のまま返す (ベンチマーク実行そのものを失敗させない、という CLAUDE.md
/// の方針に従う)。
fn collect_native_environment_info() -> MachineInfo {
    match env::consts::OS {
        "linux" => collect_linux_environment_info(),
        "windows" => collect_windows_environment_info(),
        // macOS / その他: CLAUDE.md「対応 OS の優先度」により Windows を
        // 最優先し、macOS/Linux は「最低限の整備」に留める方針
        // (docs/decisions.md D104)。ネイティブ収集は用意せず、必要なら
        // `VELOX_BENCH_*` の上書きで対応する。
        _ => MachineInfo::default(),
    }
}

/// Linux: `/proc` から CPU モデル・物理メモリ・カーネルバージョンを読む。
/// `webview_runtime` は WebKitGTK のバージョンを機械的に取得する手段が
/// 無いため常に `None`。
fn collect_linux_environment_info() -> MachineInfo {
    let cpu_model = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|contents| parse_cpu_model_from_proc_cpuinfo(&contents));
    let total_memory_bytes = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| parse_total_memory_bytes_from_proc_meminfo(&contents));
    let os_version = fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .and_then(|s| non_empty_machine_field(&s));
    MachineInfo {
        cpu_model,
        total_memory_bytes,
        os_version,
        webview_runtime: None,
    }
}

/// 機種情報の文字列項目を「値として使えるもの」だけに絞る。前後の空白を
/// 落とし、空になったものは `None` にする。
///
/// **取得経路をまたいで同じ規則を使うためのもの。** Linux の
/// `/proc` パースはもともと空値を `None` に落としていたが、Windows の
/// PowerShell 出力 (`$cpu.Name` が空文字列で返る場合) と `VELOX_BENCH_*`
/// による上書き (壊れた設定や、値を取れなかった CI が空文字列を渡す場合)
/// にはその扱いが無く、**空文字列がネイティブ収集済みの値を押しのけて
/// 採用されてしまう**非対称があった。空文字列は「値がある」ではなく
/// 「取れなかった」なので、どの経路でも `None` に揃える。
fn non_empty_machine_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// `/proc/cpuinfo` の `model name` 行から CPU モデル名を取り出す。
/// 複数コア分同じ行が繰り返されるので最初の 1 件だけを使う。
fn parse_cpu_model_from_proc_cpuinfo(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "model name" {
            return None;
        }
        non_empty_machine_field(value)
    })
}

/// `/proc/meminfo` の `MemTotal:` 行 (kB 単位) からバイト単位の総メモリ量
/// を取り出す。
fn parse_total_memory_bytes_from_proc_meminfo(contents: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "MemTotal" {
            return None;
        }
        let kb: u64 = value.split_whitespace().next()?.parse().ok()?;
        Some(kb.saturating_mul(1024))
    })
}

/// Windows: `.github/workflows/perf-windows.yml` の "Record environment
/// info" ステップと同じ情報源 (`Get-CimInstance Win32_OperatingSystem` /
/// `Win32_Processor`、WebView2 Runtime の `pv` レジストリ値) を PowerShell
/// 経由で読む。
///
/// `windows` クレート (このプロジェクトは D59/D76/D88 で既に依存済み) の
/// 生の Win32 レジストリ/API バインディングを直接叩く実装ではなく、
/// `std::process::Command` で PowerShell を呼ぶ実装を選んだ — CLAUDE.md
/// が `unsafe` を原則禁止しているところ、レジストリ/`GlobalMemoryStatusEx`
/// 等の Win32 API バインディングは `unsafe fn` であり、これを避けられる。
/// `collect_environment` はベンチマーク 1 回の実行につき 1 度しか呼ばれず、
/// 計測対象そのものではないため、プロセス起動のコストは無視できる。
/// **呼ぶのは `pwsh` (PowerShell 7) ではなく `powershell` (Windows
/// PowerShell 5.1) である点に注意。** `perf-windows.yml` の各ステップは
/// `shell: pwsh` を使っており、そちらとは別のバイナリになる。ここで 5.1 を
/// 選ぶのは、**5.1 は Windows に標準で入っているが `pwsh` は入っていない**
/// ため — `velox-bench` は CI 専用のツールではなく、開発者が自分の Windows
/// 機で走らせるものでもあり、`pwsh` を前提にすると素の Windows で機種情報が
/// 丸ごと欠ける。上のスクリプトが使っているのは `Get-CimInstance` /
/// `Get-ItemProperty` / `ConvertTo-Json` だけで、いずれも 5.1 と 7 の
/// 双方にあるため、情報源が `perf-windows.yml` と一致することは変わらない。
///
/// なお本実装は Windows 実機で検証していない (Linux 開発環境からは
/// `powershell` コマンド自体が見つからず `None` にフォールバックするため)。
/// PR レビューではここを重点的に見てほしい。
fn collect_windows_environment_info() -> MachineInfo {
    // here-string の中身は perf-windows.yml のステップと同じ発想 (Get-
    // CimInstance + EdgeUpdate クライアントのレジストリ pv 値) で、結果を
    // 1 行の JSON として出力する。`ConvertTo-Json -Compress` を使うのは、
    // Rust 側の `serde_json` でそのままパースできる形にするため。
    const SCRIPT: &str = r#"
$ErrorActionPreference = "SilentlyContinue"
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$webview2 = $null
$regPaths = @(
  "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
  "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
)
foreach ($p in $regPaths) {
  if (Test-Path $p) {
    $pv = (Get-ItemProperty -Path $p -Name "pv" -ErrorAction SilentlyContinue).pv
    if ($pv) { $webview2 = $pv; break }
  }
}
[PSCustomObject]@{
  cpu_model = $cpu.Name
  total_memory_bytes = [uint64]$os.TotalVisibleMemorySize * 1024
  os_version = $os.Version
  webview_runtime = $webview2
} | ConvertTo-Json -Compress
"#;

    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            parse_windows_hardware_info_json(&String::from_utf8_lossy(&out.stdout))
        }
        _ => MachineInfo::default(),
    }
}

/// [`collect_windows_environment_info`] の PowerShell 出力 (JSON 1 行) を
/// パースする純粋関数。プロセス起動を伴わないため Linux 上の `cargo test`
/// でも検証できる。
fn parse_windows_hardware_info_json(json: &str) -> MachineInfo {
    let value: Value = match serde_json::from_str(json.trim()) {
        Ok(v) => v,
        Err(_) => return MachineInfo::default(),
    };
    MachineInfo {
        cpu_model: value
            .get("cpu_model")
            .and_then(Value::as_str)
            .and_then(non_empty_machine_field),
        total_memory_bytes: value.get("total_memory_bytes").and_then(Value::as_u64),
        os_version: value
            .get("os_version")
            .and_then(Value::as_str)
            .and_then(non_empty_machine_field),
        webview_runtime: value
            .get("webview_runtime")
            .and_then(Value::as_str)
            .and_then(non_empty_machine_field),
    }
}

/// `VELOX_BENCH_CPU_MODEL` / `VELOX_BENCH_TOTAL_MEMORY_BYTES` /
/// `VELOX_BENCH_OS_VERSION` / `VELOX_BENCH_WEBVIEW_RUNTIME` の値
/// (未設定なら `None`) を、ネイティブ収集結果の上に重ねる。
///
/// 環境変数の読み取り自体を行わない純粋関数にしてあるのは、`std::env` を
/// 直接読むテストは並列実行されるテスト間でプロセス環境を取り合って干渉
/// するため (docs/decisions.md D104) — 値の受け渡し部分だけを切り出せば
/// 環境変数を一切触らずにテストできる。
///
/// `total_memory_bytes` のオーバーライドが数値としてパースできない場合は
/// 無視してネイティブ収集側の値にフォールバックする (壊れた環境変数で
/// ベンチマークを失敗させない)。文字列項目も同じ考え方で、空文字列や
/// 空白だけの値は [`non_empty_machine_field`] が `None` に落とすため、
/// **ネイティブ収集済みの値を空文字列で押しのけることはない。**
fn apply_environment_overrides(
    native: MachineInfo,
    cpu_model_override: Option<&str>,
    total_memory_bytes_override: Option<&str>,
    os_version_override: Option<&str>,
    webview_runtime_override: Option<&str>,
) -> MachineInfo {
    MachineInfo {
        cpu_model: cpu_model_override
            .and_then(non_empty_machine_field)
            .or(native.cpu_model),
        total_memory_bytes: total_memory_bytes_override
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or(native.total_memory_bytes),
        os_version: os_version_override
            .and_then(non_empty_machine_field)
            .or(native.os_version),
        webview_runtime: webview_runtime_override
            .and_then(non_empty_machine_field)
            .or(native.webview_runtime),
    }
}

/// Best-effort `git rev-parse HEAD` in the current working directory.
/// `None` (never an error) when `git` is unavailable or the working
/// directory is not a checkout — this is metadata, not something worth
/// failing a benchmark run over.
fn detect_git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.is_empty() {
        None
    } else {
        Some(commit)
    }
}

fn write_result(path: &str, result: &BenchmarkResult) -> Result<(), String> {
    let json = serde_json::to_string_pretty(result)
        .map_err(|err| format!("結果のシリアライズに失敗しました: {err}"))?;
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    fs::write(path, json).map_err(|err| format!("{path} へ書き込めませんでした: {err}"))
}

fn print_result_summary(result: &BenchmarkResult) {
    println!(
        "\nscenario={} os={} cpu={} trials={} commit={}",
        result.scenario,
        result.environment.os,
        result.environment.cpu_count,
        result.environment.trials,
        result
            .environment
            .git_commit
            .as_deref()
            .unwrap_or("unknown")
    );
    if result.metrics.is_empty() {
        println!("(記録されたメトリクスはありません)");
        return;
    }
    println!("{:<28} {:>8} {:>12} {:>12}", "metric", "n", "median", "p95");
    for (name, stats) in &result.metrics {
        println!(
            "{:<28} {:>8} {:>12.2} {:>12.2}",
            name, stats.count, stats.median, stats.p95
        );
    }
}

#[cfg(test)]
mod environment_info_tests {
    use super::*;

    // -- /proc/cpuinfo / /proc/meminfo のパース (Linux) ---------------------

    #[test]
    fn parses_cpu_model_from_typical_proc_cpuinfo() {
        let contents = "\
processor\t: 0
vendor_id\t: AuthenticAMD
model name\t: AMD EPYC 9V74 80-Core Processor
cache size\t: 512 KB

processor\t: 1
model name\t: AMD EPYC 9V74 80-Core Processor
";
        assert_eq!(
            parse_cpu_model_from_proc_cpuinfo(contents),
            Some("AMD EPYC 9V74 80-Core Processor".to_owned())
        );
    }

    #[test]
    fn cpu_model_is_none_when_field_absent() {
        assert_eq!(parse_cpu_model_from_proc_cpuinfo("processor\t: 0\n"), None);
    }

    #[test]
    fn parses_total_memory_bytes_from_typical_proc_meminfo() {
        let contents = "\
MemTotal:       16336864 kB
MemFree:         1234567 kB
";
        // 16336864 kB * 1024 = 16728948736 bytes
        assert_eq!(
            parse_total_memory_bytes_from_proc_meminfo(contents),
            Some(16_728_948_736)
        );
    }

    #[test]
    fn total_memory_bytes_is_none_when_field_absent() {
        assert_eq!(
            parse_total_memory_bytes_from_proc_meminfo("MemFree: 1234 kB\n"),
            None
        );
    }

    // -- Windows PowerShell JSON のパース ------------------------------------

    #[test]
    fn parses_windows_hardware_info_json_with_all_fields() {
        let json = r#"{"cpu_model":"AMD EPYC 9V74 80-Core Processor","total_memory_bytes":8589934592,"os_version":"10.0.26100","webview_runtime":"128.0.2739.79"}"#;
        let info = parse_windows_hardware_info_json(json);
        assert_eq!(
            info.cpu_model,
            Some("AMD EPYC 9V74 80-Core Processor".to_owned())
        );
        assert_eq!(info.total_memory_bytes, Some(8_589_934_592));
        assert_eq!(info.os_version, Some("10.0.26100".to_owned()));
        assert_eq!(info.webview_runtime, Some("128.0.2739.79".to_owned()));
    }

    #[test]
    fn parses_windows_hardware_info_json_with_null_webview_runtime() {
        // WebView2 Runtime が見つからなかった場合、PowerShell 側は
        // `$webview2 = $null` のまま JSON 化する。
        let json = r#"{"cpu_model":"Intel(R) Xeon(R) Platinum 8573C","total_memory_bytes":17179869184,"os_version":"10.0.26100","webview_runtime":null}"#;
        let info = parse_windows_hardware_info_json(json);
        assert_eq!(info.webview_runtime, None);
    }

    #[test]
    fn windows_hardware_info_json_falls_back_to_default_on_garbage_input() {
        // PowerShell 自体が使えない・出力が壊れている場合でもパニックせず
        // 全フィールド `None` を返す (ベンチマークを失敗させない)。
        assert_eq!(
            parse_windows_hardware_info_json("not json at all"),
            MachineInfo::default()
        );
        assert_eq!(parse_windows_hardware_info_json(""), MachineInfo::default());
    }

    // -- VELOX_BENCH_* による上書き (Issue #211) -----------------------------
    //
    // `std::env` を直接読まない純粋関数として `apply_environment_overrides`
    // を切り出してあるので、プロセス環境変数を触らずにテストできる
    // (環境変数を触るテストは並列実行で干渉するため)。

    fn sample_native() -> MachineInfo {
        MachineInfo {
            cpu_model: Some("native-cpu".to_owned()),
            total_memory_bytes: Some(1_024),
            os_version: Some("native-os".to_owned()),
            webview_runtime: Some("native-webview".to_owned()),
        }
    }

    #[test]
    fn overrides_are_none_by_default_keep_native_values() {
        let result = apply_environment_overrides(sample_native(), None, None, None, None);
        assert_eq!(result, sample_native());
    }

    #[test]
    fn overrides_replace_native_values_when_set() {
        let result = apply_environment_overrides(
            sample_native(),
            Some("overridden-cpu"),
            Some("2048"),
            Some("overridden-os"),
            Some("overridden-webview"),
        );
        assert_eq!(
            result,
            MachineInfo {
                cpu_model: Some("overridden-cpu".to_owned()),
                total_memory_bytes: Some(2_048),
                os_version: Some("overridden-os".to_owned()),
                webview_runtime: Some("overridden-webview".to_owned()),
            }
        );
    }

    #[test]
    fn unparseable_total_memory_bytes_override_falls_back_to_native() {
        // 壊れた環境変数 (数値でない) でベンチマークを失敗させず、
        // ネイティブ収集側の値にフォールバックする。
        let result =
            apply_environment_overrides(sample_native(), None, Some("not-a-number"), None, None);
        assert_eq!(result.total_memory_bytes, Some(1_024));
    }

    #[test]
    fn empty_native_with_no_overrides_yields_default() {
        let result = apply_environment_overrides(MachineInfo::default(), None, None, None, None);
        assert_eq!(result, MachineInfo::default());
    }

    #[test]
    fn blank_string_overrides_fall_back_to_native() {
        // 空文字列や空白だけの `VELOX_BENCH_*` は「値がある」ではなく
        // 「取れなかった」なので、ネイティブ収集済みの値を押しのけては
        // ならない。値を取れなかった CI がそのまま空文字列を渡す経路が
        // 現実にありうる。
        let result = apply_environment_overrides(
            sample_native(),
            Some(""),
            Some("   "),
            Some("   "),
            Some(""),
        );
        assert_eq!(result, sample_native());
    }

    #[test]
    fn whitespace_padded_overrides_are_trimmed() {
        let result = apply_environment_overrides(
            sample_native(),
            Some("  Override CPU  "),
            Some(" 2048 "),
            Some(" 10.0.26100 "),
            Some("  151.0.4129.101 "),
        );
        assert_eq!(result.cpu_model.as_deref(), Some("Override CPU"));
        assert_eq!(result.total_memory_bytes, Some(2_048));
        assert_eq!(result.os_version.as_deref(), Some("10.0.26100"));
        assert_eq!(result.webview_runtime.as_deref(), Some("151.0.4129.101"));
    }

    #[test]
    fn windows_json_blank_strings_become_none() {
        // PowerShell 側で値が取れなかったとき、`$cpu.Name` が `null` では
        // なく空文字列で返ることがある。`None` と同じ扱いにしないと、
        // 「機種不明」を「機種名が空文字列の機種」として記録してしまう。
        let result = parse_windows_hardware_info_json(
            r#"{"cpu_model":"","total_memory_bytes":2048,"os_version":"   ","webview_runtime":""}"#,
        );
        assert_eq!(result.cpu_model, None);
        assert_eq!(result.total_memory_bytes, Some(2_048));
        assert_eq!(result.os_version, None);
        assert_eq!(result.webview_runtime, None);
    }
}
