//! Startup configuration for VeloX.
//!
//! Kept as a plain struct so that a config file / CLI flags can be layered on
//! later without touching the rest of the code.

use std::time::Duration;

use crate::browser::metrics::PerfFormat;
use crate::browser::navigation;
use crate::browser::suspension::SuspensionPolicy;

/// Default interval between process-tree RSS samples when performance
/// metrics are enabled but no explicit interval was requested.
const DEFAULT_PERF_RSS_INTERVAL: Duration = Duration::from_millis(5000);

/// Default for [`Config::max_tabs_per_web_process`] — D54's original
/// compile-time constant, kept as the default so behavior is unchanged
/// unless someone sets `VELOX_MAX_TABS_PER_PROCESS`.
const DEFAULT_MAX_TABS_PER_WEB_PROCESS: usize = 4;

/// One selectable search engine: a display name plus the query template URL
/// the omnibox builds a search request from (see
/// `browser::navigation::build_search_url`). `query_template` must contain
/// the literal placeholder `{}`, replaced with the percent-encoded query
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchEngine {
    pub name: String,
    pub query_template: String,
}

impl SearchEngine {
    fn new(name: &str, query_template: &str) -> Self {
        Self {
            name: name.to_owned(),
            query_template: query_template.to_owned(),
        }
    }

    /// VeloX's default — see docs/decisions.md D26 for why DuckDuckGo was
    /// chosen over Google/Bing/etc.
    pub fn duckduckgo() -> Self {
        Self::new("DuckDuckGo", "https://duckduckgo.com/?q={}")
    }

    pub fn google() -> Self {
        Self::new("Google", "https://www.google.com/search?q={}")
    }

    pub fn bing() -> Self {
        Self::new("Bing", "https://www.bing.com/search?q={}")
    }

    pub fn startpage() -> Self {
        Self::new("Startpage", "https://www.startpage.com/sp/search?query={}")
    }

    pub fn ecosia() -> Self {
        Self::new("Ecosia", "https://www.ecosia.org/search?q={}")
    }

    /// Look up one of the built-in presets by name (case-insensitive; a
    /// couple of common short aliases are accepted alongside the full
    /// name). `None` for anything unrecognized.
    fn preset(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => Some(Self::duckduckgo()),
            "google" => Some(Self::google()),
            "bing" => Some(Self::bing()),
            "startpage" => Some(Self::startpage()),
            "ecosia" => Some(Self::ecosia()),
            _ => None,
        }
    }
}

impl Default for SearchEngine {
    fn default() -> Self {
        Self::duckduckgo()
    }
}

/// Application configuration, currently compile-time defaults plus a handful
/// of environment-variable overrides (see [`Config::from_env_and_args`]).
#[derive(Debug, Clone)]
pub struct Config {
    /// Page loaded when the browser starts.
    pub homepage: String,
    /// Title of the browser window.
    pub window_title: String,
    /// Initial window size (logical pixels).
    pub window_width: u32,
    pub window_height: u32,
    /// Height of the toolbar strip (logical pixels), tab strip included.
    pub toolbar_height: u32,
    /// Whether ad/tracker content blocking is active. Applies to main-frame
    /// navigation today; see docs/decisions.md D17 for why subresource
    /// blocking is not implemented on top of wry 0.56.
    pub content_blocking_enabled: bool,
    /// Optional path to an extra EasyList-style filter list (see
    /// `browser::FilterList`), merged on top of VeloX's built-in list.
    /// `None` uses only the built-in list.
    pub extra_blocklist_path: Option<String>,
    /// Enable performance metrics logging to stderr: the four startup
    /// checkpoints, per-page-load duration, and (if
    /// [`Config::perf_rss_interval`] is set) periodic process-tree RSS
    /// sampling. Off by default so a normal run pays no timestamp or
    /// thread-spawn overhead (see `docs/architecture.md`, "Performance
    /// extension points"). Enable via `VELOX_PERF_METRICS=1`
    /// ([`Config::from_env_and_args`]), following the same opt-in pattern as the
    /// existing `VELOX_DEBUG` flag in `app.rs`.
    pub perf_metrics: bool,
    /// Interval between process-tree RSS samples while `perf_metrics` is
    /// on. `None` disables the periodic sampling thread. This only gates
    /// the *periodic* logger in `app::run`; on-demand sampling via
    /// [`crate::browser::metrics::sample_process_tree_rss`] is always
    /// available regardless of this setting.
    pub perf_rss_interval: Option<Duration>,
    /// Output format for perf log lines (Issue #13): [`PerfFormat::Text`],
    /// the original `velox[perf] ...` lines, or [`PerfFormat::Json`], one
    /// JSON object per line (JSON Lines) for a future CI benchmark runner
    /// to parse — see `docs/architecture.md`, "Performance extension
    /// points" for the schema. Only consulted when `perf_metrics` is on.
    /// Selected via `VELOX_PERF_FORMAT=json|text`; unset or unrecognized
    /// falls back to `Text`.
    pub perf_format: PerfFormat,
    /// Optional file to append perf log lines to instead of stderr. `None`
    /// (the default) keeps writing to stderr — pre-Issue #13 behavior.
    /// Set via `VELOX_PERF_OUTPUT=<path>`; only consulted when
    /// `perf_metrics` is on. `app::run` attempts to open it once; if that
    /// fails, perf logging falls back to stderr rather than losing metrics
    /// or crashing (see `browser::perf_log::PerfLog`).
    pub perf_output_path: Option<String>,
    /// Height of the history/bookmarks dropdown panel (logical pixels) when
    /// open; added to `toolbar_height` while a panel is showing.
    pub panel_height: u32,
    /// Height of the always-visible bookmark bar (logical pixels) when
    /// showing; added to `toolbar_height` independently of `panel_height`
    /// (see docs/decisions.md D35 — the bar and a panel can both be
    /// showing at once, their heights stack rather than replacing each
    /// other).
    pub bookmark_bar_height: u32,
    /// Hard cap on the number of entries kept in the history store. `0`
    /// means unlimited.
    pub history_max_entries: usize,
    /// Maximum number of entries sent to the history panel at once (the
    /// store itself may hold more, up to `history_max_entries`).
    pub history_panel_limit: usize,
    /// How many tabs at most share one `WebKitWebProcess` on Linux/BSD
    /// (docs/decisions.md D54, `ui::window::pick_process_group`). `1`
    /// disables sharing entirely (one renderer process per tab, the
    /// pre-D54 behavior). Ignored on macOS/Windows, where wry has no
    /// equivalent knob.
    ///
    /// D54 shipped this as a compile-time constant and said so plainly:
    /// "a tuning knob, not a measured optimum", matching the benchmark
    /// machine's core count. Issue #60 made it settable
    /// (`VELOX_MAX_TABS_PER_PROCESS`) so the trade-off can be measured
    /// against tab-creation and switching latency on one binary instead of
    /// five — see docs/decisions.md D57.
    pub max_tabs_per_web_process: usize,
    /// Automatic tab suspension policy (Issue #63, see
    /// `browser::suspension`): idle time, live-tab cap and memory budget,
    /// each individually optional. Defaults to every signal off
    /// ([`SuspensionPolicy::default`]) so a fresh checkout never suspends a
    /// tab the user did not ask to suspend — see docs/decisions.md D9 for
    /// why automatic suspension is opt-in, and D56 for the policy. Manual
    /// suspension (the tab strip's suspend button,
    /// `ui::toolbar::ToolbarCommand::SuspendTab`) is always available
    /// regardless. Configured via `VELOX_AUTO_SUSPEND_AFTER_MS`,
    /// `VELOX_MAX_LIVE_TABS`, `VELOX_MEMORY_BUDGET_MB` and
    /// `VELOX_MEMORY_CHECK_INTERVAL_MS` — see [`Config::from_env_and_args`].
    pub suspension: SuspensionPolicy,
    /// Whole-app private browsing mode (see docs/decisions.md D14). When
    /// `true`, every content webview runs with an ephemeral (non-persistent)
    /// data store and page visits are not recorded to `HistoryStore`.
    pub private: bool,
    /// The search engine the omnibox sends non-URL input to (see
    /// docs/decisions.md D26 and `browser::navigation::classify_input`).
    /// Selectable via `VELOX_SEARCH_ENGINE` (a preset name) or
    /// `VELOX_SEARCH_ENGINE_NAME`/`VELOX_SEARCH_ENGINE_URL` (a fully custom
    /// engine) — see [`Config::from_env_and_args`].
    pub search_engine: SearchEngine,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            homepage: "https://www.google.com/".to_owned(),
            window_title: "VeloX".to_owned(),
            window_width: 1024,
            window_height: 768,
            // A 34px tab strip row on top of the 48px address bar row (see
            // ui/toolbar.html).
            toolbar_height: 82,
            content_blocking_enabled: true,
            extra_blocklist_path: None,
            panel_height: 320,
            // A single row, roughly the height of a tab-strip row (see
            // ui/toolbar.html's #bookmark-bar rule) — enough for one line of
            // bookmark buttons.
            bookmark_bar_height: 30,
            history_max_entries: 5000,
            history_panel_limit: 200,
            max_tabs_per_web_process: DEFAULT_MAX_TABS_PER_WEB_PROCESS,
            suspension: SuspensionPolicy::default(),
            private: false,
            search_engine: SearchEngine::default(),
            perf_metrics: false,
            perf_rss_interval: None,
            perf_format: PerfFormat::Text,
            perf_output_path: None,
        }
    }
}

impl Config {
    /// Build a config from compiled defaults, overridden by the
    /// environment and command line:
    ///
    /// - `VELOX_HOMEPAGE`, or a `--homepage <URL>` / `--homepage=<URL>` flag
    ///   in `args`, overrides the page loaded at startup. The flag wins over
    ///   the environment variable. The value goes through
    ///   [`navigation::normalize_input`], exactly like address-bar input, so
    ///   a rejected scheme (`javascript:` and friends) or unparseable URL
    ///   falls back to the compiled-in default rather than starting a
    ///   browser that cannot navigate. See docs/decisions.md D40.
    /// - `VELOX_PRIVATE` — presence (like `VELOX_DEBUG`; see `app.rs`), or a
    ///   `--private` flag in `args`, turns on whole-app private browsing.
    /// - `VELOX_PERF_METRICS` — any value (including empty) turns on
    ///   `perf_metrics`; unset means off.
    /// - `VELOX_PERF_RSS_INTERVAL_MS` — only consulted when
    ///   `VELOX_PERF_METRICS` is set; overrides the periodic RSS sampling
    ///   interval in milliseconds. `0` disables periodic sampling while
    ///   still logging startup/page-load metrics. Not a valid number falls
    ///   back to the default interval.
    /// - `VELOX_PERF_FORMAT` — only consulted when `VELOX_PERF_METRICS` is
    ///   set; `json` selects [`PerfFormat::Json`] (JSON Lines), anything
    ///   else (including unset) keeps the default [`PerfFormat::Text`].
    /// - `VELOX_PERF_OUTPUT` — only consulted when `VELOX_PERF_METRICS` is
    ///   set; a file path to append perf lines to instead of stderr. Unset
    ///   or empty keeps stderr.
    /// - `VELOX_MAX_TABS_PER_PROCESS` — how many tabs may share one
    ///   `WebKitWebProcess` (Linux/BSD only, docs/decisions.md D54/D57).
    ///   `1` turns sharing off. Unset, `0` or not a number keeps the
    ///   default (4).
    /// - `VELOX_AUTO_SUSPEND_AFTER_MS` — suspend a background tab once it
    ///   has been idle this many milliseconds (Issue #63,
    ///   `browser::suspension`). Unset, `0` or not a number leaves the
    ///   idle signal off.
    /// - `VELOX_MAX_LIVE_TABS` — keep at most this many tabs alive at once
    ///   (the active tab included); the least recently used background
    ///   tabs beyond it are suspended. Unset, `0` or not a number leaves
    ///   the tab-count signal off.
    /// - `VELOX_MEMORY_BUDGET_MB` — suspend least recently used background
    ///   tabs whenever the whole process tree's memory (PSS on Linux)
    ///   exceeds this many MiB. Unset, `0` or not a number leaves the
    ///   memory signal off (and no memory sampling runs).
    /// - `VELOX_MEMORY_CHECK_INTERVAL_MS` — only consulted when
    ///   `VELOX_MEMORY_BUDGET_MB` is set; how often memory is sampled.
    ///   Unset, `0` or not a number keeps
    ///   [`SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL`].
    /// - `VELOX_SEARCH_ENGINE` — select a built-in preset by name
    ///   (`duckduckgo`/`ddg`, `google`, `bing`, `startpage`, `ecosia`;
    ///   case-insensitive). Unset or unrecognized keeps the default
    ///   ([`SearchEngine::duckduckgo`]).
    /// - `VELOX_SEARCH_ENGINE_NAME` / `VELOX_SEARCH_ENGINE_URL` — a fully
    ///   custom engine, taking priority over `VELOX_SEARCH_ENGINE` when
    ///   *both* are set to a non-empty value and the URL contains the `{}`
    ///   placeholder; otherwise this pair is ignored and `VELOX_SEARCH_ENGINE`
    ///   (or the default) applies instead.
    ///
    /// No CLI-parsing crate is introduced for this (see docs/decisions.md
    /// D6); `args` is expected to be the process arguments with argv\[0\]
    /// already stripped (e.g. `std::env::args().skip(1)`).
    pub fn from_env_and_args<I: IntoIterator<Item = String>>(args: I) -> Self {
        // Collected once: both `resolve_private` and `resolve_homepage` need
        // to walk the arguments.
        let args: Vec<String> = args.into_iter().collect();
        let private = resolve_private(
            std::env::var_os("VELOX_PRIVATE").is_some(),
            args.iter().cloned(),
        );
        let defaults = Self::default();
        let homepage = resolve_homepage(
            std::env::var("VELOX_HOMEPAGE").ok().as_deref(),
            &args,
            &defaults.homepage,
        );
        let metrics_requested = std::env::var_os("VELOX_PERF_METRICS").is_some();
        let interval_raw = std::env::var("VELOX_PERF_RSS_INTERVAL_MS").ok();
        let (perf_metrics, perf_rss_interval) =
            resolve_perf_env(metrics_requested, interval_raw.as_deref());
        let format_raw = std::env::var("VELOX_PERF_FORMAT").ok();
        let output_raw = std::env::var("VELOX_PERF_OUTPUT").ok();
        let (perf_format, perf_output_path) = resolve_perf_output(
            metrics_requested,
            format_raw.as_deref(),
            output_raw.as_deref(),
        );
        let search_engine = resolve_search_engine(
            std::env::var("VELOX_SEARCH_ENGINE").ok().as_deref(),
            std::env::var("VELOX_SEARCH_ENGINE_NAME").ok().as_deref(),
            std::env::var("VELOX_SEARCH_ENGINE_URL").ok().as_deref(),
        );
        let max_tabs_per_web_process = resolve_max_tabs_per_web_process(
            std::env::var("VELOX_MAX_TABS_PER_PROCESS").ok().as_deref(),
        );
        let suspension = resolve_suspension(
            std::env::var("VELOX_AUTO_SUSPEND_AFTER_MS").ok().as_deref(),
            std::env::var("VELOX_MAX_LIVE_TABS").ok().as_deref(),
            std::env::var("VELOX_MEMORY_BUDGET_MB").ok().as_deref(),
            std::env::var("VELOX_MEMORY_CHECK_INTERVAL_MS")
                .ok()
                .as_deref(),
        );
        Self {
            homepage,
            private,
            search_engine,
            max_tabs_per_web_process,
            suspension,
            perf_metrics,
            perf_rss_interval,
            perf_format,
            perf_output_path,
            ..defaults
        }
    }
}

/// The startup URL, given the raw ingredients (`VELOX_HOMEPAGE`, the CLI
/// arguments, and the compiled-in default). Pure so the precedence and the
/// rejection rules are unit-testable without touching the real process
/// environment, matching [`resolve_private`]/[`resolve_perf_env`].
///
/// `--homepage` wins over `VELOX_HOMEPAGE`, which wins over `default`. A
/// candidate that [`navigation::normalize_input`] rejects — an unsupported
/// or dangerous scheme, or something that is not a URL at all — is dropped
/// in favour of the next candidate rather than failing the launch: a typo in
/// a benchmark script should not leave VeloX with no page to show. The
/// returned string is always already normalized.
fn resolve_homepage(env_raw: Option<&str>, args: &[String], default: &str) -> String {
    let from_args = homepage_arg(args);
    from_args
        .as_deref()
        .and_then(navigation::normalize_input)
        .or_else(|| env_raw.and_then(navigation::normalize_input))
        .unwrap_or_else(|| {
            navigation::normalize_input(default).unwrap_or_else(|| default.to_owned())
        })
}

/// The value of a `--homepage <URL>` or `--homepage=<URL>` flag, if present.
/// The last occurrence wins, mirroring how a shell wrapper appending flags
/// would expect to override an earlier one. A bare trailing `--homepage`
/// with nothing after it yields `None`.
fn homepage_arg(args: &[String]) -> Option<String> {
    let mut found = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--homepage=") {
            found = Some(value.to_owned());
        } else if arg == "--homepage" {
            if let Some(value) = iter.next() {
                found = Some(value.clone());
            }
        }
    }
    found
}

/// Pure decision logic behind [`Config::from_env_and_args`]'s
/// `search_engine` field, factored out for the same testability reason as
/// [`resolve_perf_env`]. A valid custom name+URL pair wins over the preset
/// name; an invalid or partial custom pair is ignored rather than causing a
/// hard failure, falling back to the preset (or the default) instead — a
/// typo'd `VELOX_SEARCH_ENGINE_URL` should never stop the browser from
/// starting.
fn resolve_search_engine(
    preset_raw: Option<&str>,
    custom_name: Option<&str>,
    custom_url: Option<&str>,
) -> SearchEngine {
    let custom_name = custom_name.map(str::trim).filter(|s| !s.is_empty());
    let custom_url = custom_url.map(str::trim).filter(|s| !s.is_empty());
    if let (Some(name), Some(template)) = (custom_name, custom_url) {
        if template.contains("{}") {
            return SearchEngine {
                name: name.to_owned(),
                query_template: template.to_owned(),
            };
        }
    }
    preset_raw
        .and_then(SearchEngine::preset)
        .unwrap_or_default()
}

/// Whether private browsing should be enabled given the raw ingredients
/// (environment variable presence, CLI args). Kept separate from
/// `Config::from_env_and_args` so the decision logic is testable without
/// touching the real process environment.
fn resolve_private<I: IntoIterator<Item = String>>(env_flag_set: bool, args: I) -> bool {
    env_flag_set || args.into_iter().any(|arg| arg == "--private")
}

/// Pure decision logic behind [`Config::from_env_and_args`]'s perf-related
/// fields, factored out so it is unit-testable without touching real
/// process environment variables.
fn resolve_perf_env(
    metrics_requested: bool,
    interval_raw: Option<&str>,
) -> (bool, Option<Duration>) {
    if !metrics_requested {
        return (false, None);
    }
    let interval = match interval_raw.and_then(|value| value.parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(DEFAULT_PERF_RSS_INTERVAL),
    };
    (true, interval)
}

/// Pure decision logic behind [`Config::from_env_and_args`]'s
/// `perf_format`/`perf_output_path`, factored out for the same reason as
/// [`resolve_perf_env`]. When metrics are not requested, both env vars are
/// ignored — matches `resolve_perf_env`'s "no overrides while off" rule, so
/// a `VELOX_PERF_FORMAT=json` left set in a shell does not silently change
/// behavior the moment someone else adds `VELOX_PERF_METRICS=1` elsewhere.
fn resolve_perf_output(
    metrics_requested: bool,
    format_raw: Option<&str>,
    output_raw: Option<&str>,
) -> (PerfFormat, Option<String>) {
    if !metrics_requested {
        return (PerfFormat::Text, None);
    }
    let format = PerfFormat::parse(format_raw);
    let output_path = output_raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    (format, output_path)
}

/// Pure decision logic behind [`Config::from_env_and_args`]'s
/// `max_tabs_per_web_process` (Issue #60). Unset, empty, `0` or not a
/// number keeps [`DEFAULT_MAX_TABS_PER_WEB_PROCESS`] — the same
/// conservative rule every other knob here follows, so a typo can never
/// silently turn process sharing into something the measurements never
/// covered.
fn resolve_max_tabs_per_web_process(raw: Option<&str>) -> usize {
    raw.map(str::trim)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_TABS_PER_WEB_PROCESS)
}

/// Pure decision logic behind [`Config::from_env_and_args`]'s
/// `suspension` (Issue #63), factored out like [`resolve_perf_env`] so the
/// parsing rules are unit-tested without touching the process environment.
/// Every knob follows the same rule: unset, empty, `0`, or not a number
/// means "off" (or "default", for the interval) — a typo in a shell
/// profile must never produce a surprising policy, only the conservative
/// one.
fn resolve_suspension(
    idle_after_ms_raw: Option<&str>,
    max_live_tabs_raw: Option<&str>,
    memory_budget_mb_raw: Option<&str>,
    check_interval_ms_raw: Option<&str>,
) -> SuspensionPolicy {
    fn positive(raw: Option<&str>) -> Option<u64> {
        raw.map(str::trim)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
    }
    let defaults = SuspensionPolicy::default();
    SuspensionPolicy {
        idle_after: positive(idle_after_ms_raw).map(Duration::from_millis),
        max_live_tabs: positive(max_live_tabs_raw)
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX)),
        memory_budget_bytes: positive(memory_budget_mb_raw)
            .map(|mib| mib.saturating_mul(1024 * 1024)),
        memory_check_interval: positive(check_interval_ms_raw)
            .map(Duration::from_millis)
            .unwrap_or(defaults.memory_check_interval),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_sane() {
        let config = Config::default();
        assert!(config.homepage.starts_with("https://"));
        assert!(config.toolbar_height > 0);
        assert!(config.window_height > config.toolbar_height);
        assert!(config.panel_height > 0);
        assert!(config.bookmark_bar_height > 0);
        assert!(config.history_panel_limit > 0);
        // Automatic suspension must be opt-in: a fresh checkout should never
        // surprise a user by suspending a tab on its own.
        assert_eq!(
            config.max_tabs_per_web_process,
            DEFAULT_MAX_TABS_PER_WEB_PROCESS
        );
        assert_eq!(config.suspension, SuspensionPolicy::default());
        assert!(!config.suspension.is_enabled());
        assert!(!config.private);
        assert!(!config.perf_metrics);
        assert_eq!(config.perf_rss_interval, None);
        assert_eq!(config.perf_format, PerfFormat::Text);
        assert_eq!(config.perf_output_path, None);
        assert_eq!(config.search_engine, SearchEngine::duckduckgo());
    }

    #[test]
    fn perf_metrics_off_ignores_interval_override() {
        assert_eq!(resolve_perf_env(false, Some("100")), (false, None));
    }

    #[test]
    fn perf_metrics_on_without_interval_uses_default() {
        assert_eq!(
            resolve_perf_env(true, None),
            (true, Some(DEFAULT_PERF_RSS_INTERVAL))
        );
    }

    #[test]
    fn perf_metrics_on_with_explicit_interval() {
        assert_eq!(
            resolve_perf_env(true, Some("1500")),
            (true, Some(Duration::from_millis(1500)))
        );
    }

    #[test]
    fn zero_interval_disables_periodic_sampling_but_keeps_metrics_on() {
        assert_eq!(resolve_perf_env(true, Some("0")), (true, None));
    }

    #[test]
    fn unparseable_interval_falls_back_to_default() {
        assert_eq!(
            resolve_perf_env(true, Some("not-a-number")),
            (true, Some(DEFAULT_PERF_RSS_INTERVAL))
        );
    }

    #[test]
    fn perf_output_ignored_while_metrics_off() {
        assert_eq!(
            resolve_perf_output(false, Some("json"), Some("/tmp/perf.jsonl")),
            (PerfFormat::Text, None)
        );
    }

    #[test]
    fn perf_output_defaults_to_text_and_stderr() {
        assert_eq!(
            resolve_perf_output(true, None, None),
            (PerfFormat::Text, None)
        );
    }

    #[test]
    fn perf_output_parses_json_format_and_output_path() {
        assert_eq!(
            resolve_perf_output(true, Some("json"), Some("/tmp/perf.jsonl")),
            (PerfFormat::Json, Some("/tmp/perf.jsonl".to_owned()))
        );
    }

    #[test]
    fn perf_output_empty_path_is_treated_as_unset() {
        assert_eq!(
            resolve_perf_output(true, Some("json"), Some("   ")),
            (PerfFormat::Json, None)
        );
    }

    // -- resolve_homepage / homepage_arg (Issue #106, D40) ---------------

    const DEFAULT_HOME: &str = "https://www.google.com/";

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn homepage_falls_back_to_the_default_with_no_flag_or_env() {
        assert_eq!(
            resolve_homepage(None, &args(&[]), DEFAULT_HOME),
            DEFAULT_HOME
        );
    }

    #[test]
    fn homepage_comes_from_the_env_var_when_no_flag_is_given() {
        assert_eq!(
            resolve_homepage(Some("https://a.example/"), &args(&[]), DEFAULT_HOME),
            "https://a.example/"
        );
    }

    #[test]
    fn homepage_flag_wins_over_the_env_var() {
        assert_eq!(
            resolve_homepage(
                Some("https://env.example/"),
                &args(&["--homepage", "https://flag.example/"]),
                DEFAULT_HOME,
            ),
            "https://flag.example/"
        );
    }

    #[test]
    fn homepage_accepts_the_equals_form() {
        assert_eq!(
            resolve_homepage(
                None,
                &args(&["--homepage=https://a.example/"]),
                DEFAULT_HOME
            ),
            "https://a.example/"
        );
    }

    #[test]
    fn homepage_is_normalized_like_address_bar_input() {
        // Bare host gains a scheme; loopback defaults to http (navigation's
        // existing rules, not a second copy of them).
        assert_eq!(
            resolve_homepage(None, &args(&["--homepage", "a.example"]), DEFAULT_HOME),
            "https://a.example/"
        );
        assert_eq!(
            resolve_homepage(None, &args(&["--homepage", "127.0.0.1:8731"]), DEFAULT_HOME),
            "http://127.0.0.1:8731/"
        );
    }

    #[test]
    fn homepage_rejects_dangerous_schemes_and_falls_back() {
        for hostile in ["javascript:alert(1)", "ftp://a.example/", "   "] {
            assert_eq!(
                resolve_homepage(None, &args(&["--homepage", hostile]), DEFAULT_HOME),
                DEFAULT_HOME,
                "{hostile} should not become the homepage"
            );
        }
    }

    #[test]
    fn a_rejected_flag_does_not_shadow_a_valid_env_var() {
        assert_eq!(
            resolve_homepage(
                Some("https://env.example/"),
                &args(&["--homepage", "javascript:alert(1)"]),
                DEFAULT_HOME,
            ),
            "https://env.example/"
        );
    }

    #[test]
    fn a_trailing_homepage_flag_with_no_value_is_ignored() {
        assert_eq!(homepage_arg(&args(&["--homepage"])), None);
        assert_eq!(
            resolve_homepage(None, &args(&["--homepage"]), DEFAULT_HOME),
            DEFAULT_HOME
        );
    }

    #[test]
    fn the_last_homepage_flag_wins() {
        assert_eq!(
            homepage_arg(&args(&[
                "--homepage",
                "https://first.example/",
                "--homepage=https://second.example/",
            ])),
            Some("https://second.example/".to_owned())
        );
    }

    #[test]
    fn homepage_parsing_does_not_swallow_the_private_flag() {
        // `--private` must still be seen when it follows a `--homepage` pair.
        let list = args(&["--homepage", "https://a.example/", "--private"]);
        assert!(resolve_private(false, list.iter().cloned()));
        assert_eq!(
            resolve_homepage(None, &list, DEFAULT_HOME),
            "https://a.example/"
        );
    }

    #[test]
    fn resolve_private_is_false_with_no_flag_or_env() {
        assert!(!resolve_private(false, Vec::<String>::new()));
        assert!(!resolve_private(
            false,
            vec!["--homepage".to_owned(), "https://a.example/".to_owned()]
        ));
    }

    #[test]
    fn resolve_private_true_from_env_flag() {
        assert!(resolve_private(true, Vec::<String>::new()));
    }

    #[test]
    fn resolve_private_true_from_cli_flag() {
        assert!(resolve_private(false, vec!["--private".to_owned()]));
        assert!(resolve_private(
            false,
            vec!["-x".to_owned(), "--private".to_owned()]
        ));
    }

    #[test]
    fn content_blocking_is_on_by_default_with_no_extra_list() {
        let config = Config::default();
        assert!(config.content_blocking_enabled);
        assert_eq!(config.extra_blocklist_path, None);
    }

    #[test]
    fn search_engine_defaults_to_duckduckgo_with_no_overrides() {
        assert_eq!(
            resolve_search_engine(None, None, None),
            SearchEngine::duckduckgo()
        );
    }

    #[test]
    fn search_engine_preset_is_case_insensitive_with_aliases() {
        assert_eq!(
            resolve_search_engine(Some("Google"), None, None),
            SearchEngine::google()
        );
        assert_eq!(
            resolve_search_engine(Some("DDG"), None, None),
            SearchEngine::duckduckgo()
        );
        assert_eq!(
            resolve_search_engine(Some("bing"), None, None),
            SearchEngine::bing()
        );
        assert_eq!(
            resolve_search_engine(Some("startpage"), None, None),
            SearchEngine::startpage()
        );
        assert_eq!(
            resolve_search_engine(Some("ecosia"), None, None),
            SearchEngine::ecosia()
        );
    }

    #[test]
    fn unrecognized_preset_name_falls_back_to_the_default() {
        assert_eq!(
            resolve_search_engine(Some("altavista"), None, None),
            SearchEngine::duckduckgo()
        );
    }

    #[test]
    fn custom_engine_takes_priority_over_a_preset() {
        let custom = resolve_search_engine(
            Some("google"),
            Some("My Engine"),
            Some("https://example.com/search?q={}"),
        );
        assert_eq!(
            custom,
            SearchEngine {
                name: "My Engine".to_owned(),
                query_template: "https://example.com/search?q={}".to_owned(),
            }
        );
    }

    #[test]
    fn custom_engine_without_the_placeholder_is_ignored() {
        assert_eq!(
            resolve_search_engine(
                Some("google"),
                Some("My Engine"),
                Some("https://example.com/search?q=fixed"),
            ),
            SearchEngine::google()
        );
    }

    #[test]
    fn partial_custom_engine_override_is_ignored() {
        assert_eq!(
            resolve_search_engine(None, Some("My Engine"), None),
            SearchEngine::duckduckgo()
        );
        assert_eq!(
            resolve_search_engine(None, None, Some("https://example.com/search?q={}")),
            SearchEngine::duckduckgo()
        );
    }

    #[test]
    fn empty_custom_engine_values_are_treated_as_unset() {
        assert_eq!(
            resolve_search_engine(Some("google"), Some("  "), Some("  ")),
            SearchEngine::google()
        );
    }
    // -- resolve_max_tabs_per_web_process (Issue #60) ---------------------

    #[test]
    fn max_tabs_per_web_process_parses_a_positive_value_and_falls_back_otherwise() {
        assert_eq!(resolve_max_tabs_per_web_process(Some("1")), 1);
        assert_eq!(resolve_max_tabs_per_web_process(Some(" 16 ")), 16);
        for raw in [None, Some("0"), Some(""), Some("  "), Some("-1"), Some("x")] {
            assert_eq!(
                resolve_max_tabs_per_web_process(raw),
                DEFAULT_MAX_TABS_PER_WEB_PROCESS,
                "raw was {raw:?}"
            );
        }
    }

    // -- resolve_suspension (Issue #63) -----------------------------------

    #[test]
    fn resolve_suspension_defaults_to_everything_off() {
        let policy = resolve_suspension(None, None, None, None);
        assert_eq!(policy, SuspensionPolicy::default());
        assert!(!policy.is_enabled());
    }

    #[test]
    fn resolve_suspension_parses_each_knob_independently() {
        let policy = resolve_suspension(Some("30000"), Some("5"), Some("700"), Some("500"));
        assert_eq!(policy.idle_after, Some(Duration::from_secs(30)));
        assert_eq!(policy.max_live_tabs, Some(5));
        assert_eq!(policy.memory_budget_bytes, Some(700 * 1024 * 1024));
        assert_eq!(policy.memory_check_interval, Duration::from_millis(500));
        assert!(policy.is_enabled());

        // One knob alone is enough to enable the policy.
        let only_count = resolve_suspension(None, Some(" 3 "), None, None);
        assert_eq!(only_count.max_live_tabs, Some(3));
        assert_eq!(only_count.idle_after, None);
        assert_eq!(only_count.memory_budget_bytes, None);
        assert!(only_count.is_enabled());
    }

    #[test]
    fn resolve_suspension_treats_zero_empty_and_garbage_as_off() {
        for raw in ["0", "", "  ", "-1", "abc", "1.5"] {
            let policy = resolve_suspension(Some(raw), Some(raw), Some(raw), Some(raw));
            assert_eq!(policy, SuspensionPolicy::default(), "raw was {raw:?}");
        }
    }

    #[test]
    fn resolve_suspension_interval_falls_back_to_default_without_a_budget() {
        // The interval alone never enables anything.
        let policy = resolve_suspension(None, None, None, Some("100"));
        assert!(!policy.is_enabled());
        assert_eq!(policy.memory_check_interval, Duration::from_millis(100));
    }
}
