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
//!   regression beyond `--threshold-pct`, the hook Issue #36's CI check is
//!   expected to call.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use velox::browser::automation;
use velox::browser::benchmark::scenario::Scenario;
use velox::browser::benchmark::{
    self, BenchmarkResult, ComparisonReport, MetricDiff, RunEnvironment,
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
  velox-bench compare --baseline <path> --candidate <path> [--threshold-pct <pct>] [--output <path>]\n\n\
詳細は docs/benchmarking.md を参照してください。";

// ---------------------------------------------------------------------
// list-scenarios
// ---------------------------------------------------------------------

fn cmd_list_scenarios() -> Result<i32, String> {
    println!("{:<16} 自動実行 (run)", "scenario");
    for scenario in Scenario::all() {
        // Every scenario is unattended as of Issue #112 (see
        // `Scenario::is_unattended`'s doc comment); what differs is
        // whether `run` needs `--url` to build a
        // `VELOX_AUTOMATION_SCRIPT` for it.
        let note = if automation::needs_automation_script(scenario) {
            "可 (run で自動実行可能、--url 必須)"
        } else {
            "可 (run で自動実行可能)"
        };
        println!("{:<16} {}", scenario.id(), note);
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
    let rss_interval_ms = flags.one("rss-interval-ms");
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
        if let Some(interval) = rss_interval_ms {
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
    Ok(0)
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
    if Scenario::parse(scenario_id).is_none() {
        return Err(format!(
            "未知のシナリオです: {scenario_id} (list-scenarios を参照)"
        ));
    }
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
    let environment = collect_environment(
        trial_events.len() as u32,
        flags.one("git-commit").map(str::to_owned),
    );
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
    Ok(0)
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
    RunEnvironment {
        os,
        cpu_count,
        git_commit,
        generated_at,
        trials,
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
