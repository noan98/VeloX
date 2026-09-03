//! File-driven benchmark automation (Issue #112).
//!
//! **Design constraint — no listening socket, no RPC server.** VeloX's IPC
//! trust boundary (docs/decisions.md D18/D23) exists precisely so that
//! nothing outside the trusted toolbar webview can drive structured browser
//! commands; a socket or RPC endpoint that accepted tab-management commands
//! from an external process would be a second, much wider hole in that same
//! boundary. Instead: if the environment variable `VELOX_AUTOMATION_SCRIPT`
//! names a file, `app::run` reads it exactly once at startup, parses it with
//! [`parse_script`], and feeds the resulting commands into the existing
//! `UserEvent` dispatch on the main thread (see docs/decisions.md D44 for
//! the full reasoning). When the variable is unset — the default, for every
//! normal launch — this module's parser never runs at all: zero added
//! attack surface, zero added cost.
//!
//! This module is pure, UI/engine-independent Rust (no `wry`/`tao`/`gtk`
//! dependency — see docs/decisions.md D20's layering rule), so it is fully
//! covered by `cargo test` without a display. Everything that actually
//! drives a webview (spawning the automation thread, converting a
//! [`AutomationCommand`] into calls against `browser::tabs::Tabs`/
//! `ui::window::BrowserWindow`) lives in `src/app.rs`.
//!
//! ## Script format
//!
//! One command per line, in execution order:
//!
//! ```text
//! open <url>        # open a new tab and make it active
//! switch <index>    # activate the tab at position <index> (0-based)
//! close <index>     # close the tab at position <index> (0-based)
//! navigate <url>    # navigate the active tab
//! wait <ms>         # sleep before the next command (<= MAX_WAIT_MS)
//! quit              # exit the application
//! ```
//!
//! A line that is empty (after trimming) or starts with `#` is ignored.
//! Anything else that fails to parse is a hard error carrying the 1-based
//! line number it came from — [`parse_script`] never panics on malformed
//! input, and no command runs until the whole script has parsed
//! successfully.

use std::fmt;

use crate::browser::navigation;

/// Hard cap on a single `wait <ms>` command, so a typo (or a hostile script,
/// if one ever reached this parser some other way) cannot stall the browser
/// indefinitely. 120 seconds comfortably covers the slowest scenario this
/// issue generates (`tabs_50`, see [`generate_bench_script`]) with headroom.
pub const MAX_WAIT_MS: u64 = 120_000;

/// One parsed automation command. Carries no line number itself —
/// [`parse_script`] reports that separately via [`AutomationError`] — since
/// by the time a script has parsed successfully there is nothing left that
/// needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationCommand {
    /// `open <url>` — open a new tab at `url` (already normalized) and make
    /// it active.
    Open { url: String },
    /// `switch <index>` — activate the tab currently at position `index`
    /// (0-based, in on-screen tab-strip order). A no-op at runtime if
    /// `index` is out of range by the time this command runs (tabs may
    /// have since closed) — never a hard failure.
    Switch { index: usize },
    /// `close <index>` — close the tab currently at position `index`. Same
    /// out-of-range handling as `Switch`.
    Close { index: usize },
    /// `navigate <url>` — navigate the active tab to `url` (already
    /// normalized).
    Navigate { url: String },
    /// `wait <ms>` — sleep for `ms` milliseconds before the next command.
    /// Never exceeds [`MAX_WAIT_MS`] (enforced at parse time).
    Wait { ms: u64 },
    /// `quit` — exit the application.
    Quit,
}

/// A malformed automation script line: unknown command, missing/invalid
/// argument, or a `wait` beyond [`MAX_WAIT_MS`]. `line` is 1-based, matching
/// what a human editing the script file would count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for AutomationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}行目: {}", self.line, self.message)
    }
}

impl std::error::Error for AutomationError {}

/// Parse a whole automation script. `#`-prefixed and blank (after trimming)
/// lines are ignored; every other line must be one of the commands listed
/// in the module doc comment. Returns every parsed command in file order,
/// or the first [`AutomationError`] encountered (line numbers are 1-based).
///
/// URLs (`open`/`navigate`) are normalized here via
/// [`navigation::normalize_input`] — the same function address-bar input
/// goes through — so a rejected scheme or unparseable URL is caught at
/// parse time, before anything runs, rather than failing silently mid-run.
pub fn parse_script(text: &str) -> Result<Vec<AutomationCommand>, AutomationError> {
    let mut commands = Vec::new();
    for (offset, raw_line) in text.lines().enumerate() {
        let line = offset + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        commands.push(parse_line(line, trimmed)?);
    }
    Ok(commands)
}

fn parse_line(line: usize, text: &str) -> Result<AutomationCommand, AutomationError> {
    let mut parts = text.splitn(2, char::is_whitespace);
    let keyword = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    match keyword {
        "open" => Ok(AutomationCommand::Open {
            url: parse_url(line, rest)?,
        }),
        "navigate" => Ok(AutomationCommand::Navigate {
            url: parse_url(line, rest)?,
        }),
        "switch" => Ok(AutomationCommand::Switch {
            index: parse_index(line, "switch", rest)?,
        }),
        "close" => Ok(AutomationCommand::Close {
            index: parse_index(line, "close", rest)?,
        }),
        "wait" => Ok(AutomationCommand::Wait {
            ms: parse_wait(line, rest)?,
        }),
        "quit" => {
            if rest.is_empty() {
                Ok(AutomationCommand::Quit)
            } else {
                Err(err(line, format!("quit は引数を取りません: {rest:?}")))
            }
        }
        "" => Err(err(line, "空のコマンドです".to_owned())),
        other => Err(err(line, format!("未知のコマンドです: {other:?}"))),
    }
}

fn parse_url(line: usize, rest: &str) -> Result<String, AutomationError> {
    if rest.is_empty() {
        return Err(err(line, "URL が指定されていません".to_owned()));
    }
    navigation::normalize_input(rest)
        .ok_or_else(|| err(line, format!("URL を解釈できません: {rest:?}")))
}

fn parse_index(line: usize, keyword: &str, rest: &str) -> Result<usize, AutomationError> {
    if rest.is_empty() {
        return Err(err(line, format!("{keyword} には index 引数が必要です")));
    }
    rest.parse::<usize>().map_err(|_| {
        err(
            line,
            format!("index は非負整数で指定してください: {rest:?}"),
        )
    })
}

fn parse_wait(line: usize, rest: &str) -> Result<u64, AutomationError> {
    if rest.is_empty() {
        return Err(err(line, "wait には ms 引数が必要です".to_owned()));
    }
    let ms: u64 = rest.parse().map_err(|_| {
        err(
            line,
            format!("wait の ms は非負整数で指定してください: {rest:?}"),
        )
    })?;
    if ms > MAX_WAIT_MS {
        return Err(err(
            line,
            format!("wait は最大 {MAX_WAIT_MS}ms までです (指定値: {ms}ms)"),
        ));
    }
    Ok(ms)
}

fn err(line: usize, message: String) -> AutomationError {
    AutomationError { line, message }
}

// ---------------------------------------------------------------------
// Script generation for `velox-bench run` (Issue #112, requirement 4)
// ---------------------------------------------------------------------
//
// Pure functions that turn a benchmark scenario into an automation script
// `velox-bench` can hand to VeloX via `VELOX_AUTOMATION_SCRIPT`. Kept here
// (rather than in `src/bin/velox-bench.rs`) so it is covered by `cargo
// test` like the rest of this module.

/// How many extra pages `Scenario::Navigation` navigates through, beyond
/// the page already loaded at startup.
const NAVIGATION_STEPS: usize = 5;
/// How many tabs `Scenario::TabCreate` opens (beyond the initial tab), one
/// `tab_create` latency sample each.
const TAB_CREATE_COUNT: usize = 5;
/// How many extra tabs `Scenario::TabSwitch` opens before switching between
/// them.
const TAB_SWITCH_EXTRA_TABS: usize = 4;
/// How many `switch` commands `Scenario::TabSwitch` issues, one
/// `tab_switch` latency sample each.
const TAB_SWITCH_REPEATS: usize = 8;
/// Pause after each `open`/`navigate` step, so the webview has a moment to
/// actually start the load before the next command fires.
const STEP_SETTLE_MS: u64 = 300;
/// Pause between successive `switch` commands.
const SWITCH_SETTLE_MS: u64 = 200;
/// Pause after opening every tab for a `tabs_N` run, before `quit` — gives
/// the RSS sampler (`VELOX_PERF_RSS_INTERVAL_MS`) time to take at least a
/// couple of samples with all tabs present.
const MEMORY_STABILIZE_MS: u64 = 3_000;
/// How many RSS/PSS samples [`recommended_rss_interval_ms`] aims to land
/// inside the fixed [`MEMORY_STABILIZE_MS`] settle window at the end of a
/// generated `tabs_N` script — see that function's doc comment and
/// docs/decisions.md D50 for why this window, not the scenario's total
/// duration, is what the interval is derived from.
const TARGET_STABILIZED_RSS_SAMPLES: u64 = 4;

/// Build the automation script text for `scenario`, given the fixed page
/// `url` every trial should use (the same `--url` `velox-bench run` already
/// requires for reproducibility — see docs/benchmarking.md). Returns `None`
/// for the three scenarios that need no interaction at all (`ColdStartup`/
/// `WarmStartup`/`FirstPageLoad` — "launch and wait" is already fully
/// automated without this module).
///
/// The returned text always ends with `quit`, so a caller that spawns
/// VeloX with `VELOX_AUTOMATION_SCRIPT` set to this script's contents can
/// wait for the child to exit on its own instead of killing it after a
/// fixed timeout.
pub fn generate_bench_script(
    scenario: crate::browser::benchmark::scenario::Scenario,
    url: &str,
) -> Option<String> {
    use crate::browser::benchmark::scenario::Scenario;

    let mut lines: Vec<String> = match scenario {
        Scenario::ColdStartup | Scenario::WarmStartup | Scenario::FirstPageLoad => return None,
        Scenario::Navigation => (1..=NAVIGATION_STEPS)
            .flat_map(|step| {
                [
                    format!("navigate {url}?velox-bench-step={step}"),
                    format!("wait {STEP_SETTLE_MS}"),
                ]
            })
            .collect(),
        Scenario::TabCreate => (0..TAB_CREATE_COUNT)
            .flat_map(|_| [format!("open {url}"), format!("wait {STEP_SETTLE_MS}")])
            .collect(),
        Scenario::TabSwitch => {
            let mut lines: Vec<String> = (0..TAB_SWITCH_EXTRA_TABS)
                .map(|_| format!("open {url}"))
                .collect();
            lines.push(format!("wait {STEP_SETTLE_MS}"));
            let total_tabs = TAB_SWITCH_EXTRA_TABS + 1;
            for i in 0..TAB_SWITCH_REPEATS {
                let index = i % total_tabs;
                lines.push(format!("switch {index}"));
                lines.push(format!("wait {SWITCH_SETTLE_MS}"));
            }
            lines
        }
        Scenario::TabCountMemory(tab_count) => {
            let extra_tabs = tab_count.saturating_sub(1);
            let mut lines: Vec<String> = (0..extra_tabs).map(|_| format!("open {url}")).collect();
            lines.push(format!("wait {MEMORY_STABILIZE_MS}"));
            lines
        }
    };
    lines.push("quit".to_owned());
    Some(lines.join("\n") + "\n")
}

/// Whether `scenario` needs a `VELOX_AUTOMATION_SCRIPT` at all — the same
/// split [`generate_bench_script`] makes internally (`None` for the three
/// startup scenarios, `Some` for everything else), exposed separately so a
/// caller (`velox-bench run`) can decide *before* it has a URL in hand
/// whether `--url` is required. See
/// `tests::needs_automation_script_agrees_with_generate_bench_script` for
/// the consistency check between the two.
pub fn needs_automation_script(scenario: crate::browser::benchmark::scenario::Scenario) -> bool {
    use crate::browser::benchmark::scenario::Scenario;
    !matches!(
        scenario,
        Scenario::ColdStartup | Scenario::WarmStartup | Scenario::FirstPageLoad
    )
}

/// A reasonable `velox-bench run` per-trial timeout (in seconds) for
/// `scenario` — used as the default `--warmup-secs` when the caller does
/// not pass one explicitly. Pure and independent of the actual generated
/// script text/URL: it only needs the scenario shape (how many
/// `open`/`switch`/`navigate` steps and explicit `wait`s
/// [`generate_bench_script`] would produce), which is fixed by the
/// constants above regardless of which page is loaded.
///
/// The three startup scenarios keep the pre-#112 default of 5 seconds
/// unchanged; the rest add enough margin for their own generated script's
/// steps and waits to comfortably finish (and hit `quit`) before
/// `velox-bench run` would otherwise kill the process.
pub fn recommended_timeout_secs(scenario: crate::browser::benchmark::scenario::Scenario) -> u64 {
    use crate::browser::benchmark::scenario::Scenario;

    /// Rough wall-clock cost of one `open`/`switch`/`close`/`navigate`
    /// step beyond its own explicit `wait`, covering the webview call
    /// itself plus IPC round-trip slack.
    const PER_STEP_OVERHEAD_MS: u64 = 250;
    const STARTUP_DEFAULT_SECS: u64 = 5;
    const TEARDOWN_BUFFER_SECS: u64 = 3;

    let script_ms: u64 = match scenario {
        Scenario::ColdStartup | Scenario::WarmStartup | Scenario::FirstPageLoad => {
            return STARTUP_DEFAULT_SECS;
        }
        Scenario::Navigation => NAVIGATION_STEPS as u64 * (PER_STEP_OVERHEAD_MS + STEP_SETTLE_MS),
        Scenario::TabCreate => TAB_CREATE_COUNT as u64 * (PER_STEP_OVERHEAD_MS + STEP_SETTLE_MS),
        Scenario::TabSwitch => {
            let open_ms = TAB_SWITCH_EXTRA_TABS as u64 * PER_STEP_OVERHEAD_MS + STEP_SETTLE_MS;
            let switch_ms = TAB_SWITCH_REPEATS as u64 * (PER_STEP_OVERHEAD_MS + SWITCH_SETTLE_MS);
            open_ms + switch_ms
        }
        Scenario::TabCountMemory(tab_count) => {
            let open_ms = u64::from(tab_count.saturating_sub(1)) * PER_STEP_OVERHEAD_MS;
            open_ms + MEMORY_STABILIZE_MS
        }
    };
    script_ms / 1000 + STARTUP_DEFAULT_SECS + TEARDOWN_BUFFER_SECS
}

/// A `VELOX_PERF_RSS_INTERVAL_MS` override for `velox-bench run`, when
/// `scenario`'s default (`config::DEFAULT_PERF_RSS_INTERVAL`, 5000ms) would
/// undersample it. `None` for every scenario but [`Scenario::TabCountMemory`]
/// — the default interval is left alone everywhere else, including the three
/// startup scenarios, so this function cannot change their behavior (see
/// docs/decisions.md D50).
///
/// **Why `tabs_N` needs an override at all**: `generate_bench_script`'s
/// `TabCountMemory` branch opens every extra tab back to back with no
/// `wait` between them, then waits [`MEMORY_STABILIZE_MS`] once at the end
/// before `quit`. A scenario's whole run can finish in a few seconds
/// (`docs/memory-analysis.md` §4.1 measured `tabs_1`/`tabs_5` at 3-4s), well
/// under the periodic RSS sampler's default 5000ms period
/// (`spawn_rss_sampler`, `src/app.rs`) — so the sampler's loop, which takes
/// its first sample immediately at startup and then sleeps `interval`
/// before the next one, can easily take *only* that first sample (all tabs
/// still closed, or only partway open) before the process exits. Every
/// `tabs_N` PSS/RSS figure this project has recorded ends up reporting
/// close to the same "just launched" number regardless of `N`, which is
/// this issue's bug.
///
/// **Why the interval is derived from [`MEMORY_STABILIZE_MS`] and not from
/// the scenario's total estimated duration** (unlike
/// [`recommended_timeout_secs`], which does use a per-step time estimate):
/// [`MEMORY_STABILIZE_MS`] is the one part of a `tabs_N` script whose
/// duration does not depend on how long the real `open` calls actually take
/// — it is a fixed `wait` the automation thread sleeps regardless. Pacing
/// the sampler off it means the interval keeps working whether the real
/// per-tab open cost this session happens to see is faster or slower than
/// any estimate: `MEMORY_STABILIZE_MS / TARGET_STABILIZED_RSS_SAMPLES`
/// (750ms today) fits comfortably more than [`TARGET_STABILIZED_RSS_SAMPLES`]
/// sampler ticks inside that fixed window no matter when the window starts,
/// and the sampler thread runs independently of the main thread's tab-open
/// work, so tab count does not slow it down either. `benchmark::
/// memory_sample_confidence` is the safety net for the case where the
/// real environment is slow enough that even this still undersamples.
pub fn recommended_rss_interval_ms(
    scenario: crate::browser::benchmark::scenario::Scenario,
) -> Option<u64> {
    use crate::browser::benchmark::scenario::Scenario;
    match scenario {
        Scenario::TabCountMemory(_) => Some(MEMORY_STABILIZE_MS / TARGET_STABILIZED_RSS_SAMPLES),
        Scenario::ColdStartup
        | Scenario::WarmStartup
        | Scenario::FirstPageLoad
        | Scenario::Navigation
        | Scenario::TabCreate
        | Scenario::TabSwitch => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_script: happy paths ---------------------------------------

    #[test]
    fn parses_every_command_kind() {
        let script = "\
            open https://example.com/\n\
            switch 2\n\
            close 1\n\
            navigate https://example.com/other\n\
            wait 500\n\
            quit\n";
        let commands = parse_script(script).unwrap();
        assert_eq!(
            commands,
            vec![
                AutomationCommand::Open {
                    url: "https://example.com/".to_owned()
                },
                AutomationCommand::Switch { index: 2 },
                AutomationCommand::Close { index: 1 },
                AutomationCommand::Navigate {
                    url: "https://example.com/other".to_owned()
                },
                AutomationCommand::Wait { ms: 500 },
                AutomationCommand::Quit,
            ]
        );
    }

    #[test]
    fn ignores_blank_lines_and_comments() {
        let script = "\n  \n# a comment\nquit\n   # trailing comment\n";
        assert_eq!(parse_script(script).unwrap(), vec![AutomationCommand::Quit]);
    }

    #[test]
    fn empty_script_parses_to_no_commands() {
        assert_eq!(parse_script("").unwrap(), vec![]);
        assert_eq!(parse_script("\n\n\n").unwrap(), vec![]);
    }

    #[test]
    fn schemeless_open_url_is_normalized_like_the_address_bar() {
        let commands = parse_script("open example.com\n").unwrap();
        assert_eq!(
            commands,
            vec![AutomationCommand::Open {
                url: "https://example.com/".to_owned()
            }]
        );
    }

    #[test]
    fn loopback_open_url_gets_http_not_https() {
        let commands = parse_script("open 127.0.0.1:8080/page.html\n").unwrap();
        assert_eq!(
            commands,
            vec![AutomationCommand::Open {
                url: "http://127.0.0.1:8080/page.html".to_owned()
            }]
        );
    }

    // -- parse_script: error paths, with line numbers --------------------

    #[test]
    fn unknown_command_is_rejected_with_its_line_number() {
        let err = parse_script("open https://example.com/\nfrobnicate 1\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("frobnicate"));
    }

    #[test]
    fn open_without_a_url_is_rejected() {
        let err = parse_script("open\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn open_with_an_unparseable_url_is_rejected() {
        let err = parse_script("open javascript:alert(1)\n").unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.message.contains("javascript:alert(1)"));
    }

    #[test]
    fn navigate_without_a_url_is_rejected() {
        let err = parse_script("navigate\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn switch_without_an_index_is_rejected() {
        let err = parse_script("switch\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn switch_with_a_non_numeric_index_is_rejected() {
        let err = parse_script("switch abc\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn switch_with_a_negative_index_is_rejected() {
        let err = parse_script("switch -1\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn close_without_an_index_is_rejected() {
        let err = parse_script("close\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn wait_without_ms_is_rejected() {
        let err = parse_script("wait\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn wait_beyond_the_cap_is_rejected() {
        let err = parse_script(&format!("wait {}\n", MAX_WAIT_MS + 1)).unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.message.contains(&MAX_WAIT_MS.to_string()));
    }

    #[test]
    fn wait_exactly_at_the_cap_is_accepted() {
        let commands = parse_script(&format!("wait {MAX_WAIT_MS}\n")).unwrap();
        assert_eq!(commands, vec![AutomationCommand::Wait { ms: MAX_WAIT_MS }]);
    }

    #[test]
    fn quit_with_extra_arguments_is_rejected() {
        let err = parse_script("quit now\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn a_later_line_reports_its_own_number_not_the_first() {
        let err = parse_script("quit\nquit\nbogus\n").unwrap_err();
        assert_eq!(err.line, 3);
    }

    #[test]
    fn does_not_panic_on_arbitrary_malformed_input() {
        for line in ["", "   ", "###", "open   ", "switch 99999999999999999999"] {
            let _ = parse_script(line);
        }
    }

    // -- generate_bench_script --------------------------------------------

    use crate::browser::benchmark::scenario::Scenario;

    #[test]
    fn startup_scenarios_need_no_automation_script() {
        assert_eq!(
            generate_bench_script(Scenario::ColdStartup, "http://127.0.0.1:8731/minimal.html"),
            None
        );
        assert_eq!(
            generate_bench_script(Scenario::WarmStartup, "http://127.0.0.1:8731/minimal.html"),
            None
        );
        assert_eq!(
            generate_bench_script(
                Scenario::FirstPageLoad,
                "http://127.0.0.1:8731/minimal.html"
            ),
            None
        );
    }

    #[test]
    fn every_generated_script_parses_and_ends_with_quit() {
        let url = "http://127.0.0.1:8731/minimal.html";
        for scenario in Scenario::all() {
            let Some(script) = generate_bench_script(scenario, url) else {
                continue;
            };
            let commands = parse_script(&script).unwrap_or_else(|err| {
                panic!(
                    "scenario {:?} generated an unparseable script: {err}",
                    scenario.id()
                )
            });
            assert_eq!(
                commands.last(),
                Some(&AutomationCommand::Quit),
                "scenario {:?} script did not end with quit",
                scenario.id()
            );
        }
    }

    #[test]
    fn tab_count_memory_script_opens_exactly_tab_count_minus_one_tabs() {
        let url = "http://127.0.0.1:8731/minimal.html";
        let script = generate_bench_script(Scenario::TabCountMemory(5), url).unwrap();
        let commands = parse_script(&script).unwrap();
        let open_count = commands
            .iter()
            .filter(|c| matches!(c, AutomationCommand::Open { .. }))
            .count();
        assert_eq!(open_count, 4);
    }

    #[test]
    fn tab_count_memory_of_one_opens_no_extra_tabs() {
        let url = "http://127.0.0.1:8731/minimal.html";
        let script = generate_bench_script(Scenario::TabCountMemory(1), url).unwrap();
        let commands = parse_script(&script).unwrap();
        assert!(!commands
            .iter()
            .any(|c| matches!(c, AutomationCommand::Open { .. })));
        assert_eq!(commands.last(), Some(&AutomationCommand::Quit));
    }

    #[test]
    fn tab_switch_script_opens_extra_tabs_and_switches_repeatedly() {
        let url = "http://127.0.0.1:8731/minimal.html";
        let script = generate_bench_script(Scenario::TabSwitch, url).unwrap();
        let commands = parse_script(&script).unwrap();
        let open_count = commands
            .iter()
            .filter(|c| matches!(c, AutomationCommand::Open { .. }))
            .count();
        let switch_count = commands
            .iter()
            .filter(|c| matches!(c, AutomationCommand::Switch { .. }))
            .count();
        assert_eq!(open_count, TAB_SWITCH_EXTRA_TABS);
        assert_eq!(switch_count, TAB_SWITCH_REPEATS);
    }

    #[test]
    fn navigation_script_navigates_to_distinct_urls() {
        let url = "http://127.0.0.1:8731/minimal.html";
        let script = generate_bench_script(Scenario::Navigation, url).unwrap();
        let commands = parse_script(&script).unwrap();
        let targets: Vec<&str> = commands
            .iter()
            .filter_map(|c| match c {
                AutomationCommand::Navigate { url } => Some(url.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(targets.len(), NAVIGATION_STEPS);
        let unique: std::collections::HashSet<_> = targets.iter().collect();
        assert_eq!(
            unique.len(),
            NAVIGATION_STEPS,
            "navigation targets must be distinct"
        );
    }

    #[test]
    fn tab_create_script_opens_tab_create_count_tabs() {
        let url = "http://127.0.0.1:8731/minimal.html";
        let script = generate_bench_script(Scenario::TabCreate, url).unwrap();
        let commands = parse_script(&script).unwrap();
        let open_count = commands
            .iter()
            .filter(|c| matches!(c, AutomationCommand::Open { .. }))
            .count();
        assert_eq!(open_count, TAB_CREATE_COUNT);
    }

    // -- needs_automation_script / recommended_timeout_secs ---------------

    #[test]
    fn needs_automation_script_agrees_with_generate_bench_script() {
        let url = "http://127.0.0.1:8731/minimal.html";
        for scenario in Scenario::all() {
            assert_eq!(
                needs_automation_script(scenario),
                generate_bench_script(scenario, url).is_some(),
                "mismatch for {:?}",
                scenario.id()
            );
        }
    }

    #[test]
    fn recommended_timeout_is_five_seconds_for_startup_scenarios() {
        assert_eq!(recommended_timeout_secs(Scenario::ColdStartup), 5);
        assert_eq!(recommended_timeout_secs(Scenario::WarmStartup), 5);
        assert_eq!(recommended_timeout_secs(Scenario::FirstPageLoad), 5);
    }

    #[test]
    fn recommended_timeout_grows_with_tab_count() {
        let small = recommended_timeout_secs(Scenario::TabCountMemory(1));
        let large = recommended_timeout_secs(Scenario::TabCountMemory(50));
        assert!(
            large > small,
            "expected tabs_50 timeout ({large}s) > tabs_1 timeout ({small}s)"
        );
    }

    #[test]
    fn recommended_timeout_is_always_positive() {
        for scenario in Scenario::all() {
            let secs = recommended_timeout_secs(scenario);
            assert!(secs > 0, "scenario {:?} had a zero timeout", scenario.id());
        }
    }

    // -- recommended_rss_interval_ms (Issue #119, D50) --------------------

    #[test]
    fn startup_scenarios_keep_the_default_rss_interval() {
        // `None` here means "`velox-bench run` does not pass
        // `VELOX_PERF_RSS_INTERVAL_MS` at all", i.e. `Config`'s existing
        // 5000ms default applies unchanged — this is the behavior the task
        // explicitly must not disturb.
        assert_eq!(recommended_rss_interval_ms(Scenario::ColdStartup), None);
        assert_eq!(recommended_rss_interval_ms(Scenario::WarmStartup), None);
        assert_eq!(recommended_rss_interval_ms(Scenario::FirstPageLoad), None);
    }

    #[test]
    fn non_memory_tab_scenarios_keep_the_default_rss_interval() {
        assert_eq!(recommended_rss_interval_ms(Scenario::Navigation), None);
        assert_eq!(recommended_rss_interval_ms(Scenario::TabCreate), None);
        assert_eq!(recommended_rss_interval_ms(Scenario::TabSwitch), None);
    }

    #[test]
    fn tab_count_memory_scenarios_get_an_explicit_short_interval() {
        for &n in &Scenario::TAB_COUNTS {
            let interval = recommended_rss_interval_ms(Scenario::TabCountMemory(n))
                .unwrap_or_else(|| panic!("tabs_{n} should override the RSS interval"));
            assert!(
                interval > 0,
                "tabs_{n} interval must be positive, got {interval}"
            );
            // The interval must be short enough that more than one sample
            // can land inside the fixed MEMORY_STABILIZE_MS settle window,
            // regardless of tab count (see the function's doc comment for
            // why this is independent of N).
            assert!(
                interval * 2 <= MEMORY_STABILIZE_MS,
                "tabs_{n} interval {interval}ms leaves room for fewer than 2 \
                 samples in a {MEMORY_STABILIZE_MS}ms settle window"
            );
        }
    }

    #[test]
    fn tab_count_memory_interval_is_independent_of_tab_count() {
        // Deliberately identical across every tab count: the settle window
        // (MEMORY_STABILIZE_MS) that it is derived from does not grow with
        // N, and the sampler thread runs independently of how many `open`
        // commands the main thread is still working through.
        let intervals: Vec<u64> = Scenario::TAB_COUNTS
            .iter()
            .map(|&n| recommended_rss_interval_ms(Scenario::TabCountMemory(n)).unwrap())
            .collect();
        assert!(
            intervals.windows(2).all(|pair| pair[0] == pair[1]),
            "expected the same interval for every tab count, got {intervals:?}"
        );
    }

    #[test]
    fn tab_count_memory_interval_matches_the_documented_formula() {
        assert_eq!(
            recommended_rss_interval_ms(Scenario::TabCountMemory(1)),
            Some(MEMORY_STABILIZE_MS / TARGET_STABILIZED_RSS_SAMPLES)
        );
    }
}
