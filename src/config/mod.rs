//! Startup configuration for VeloX.
//!
//! Kept as a plain struct so that a config file / CLI flags can be layered on
//! later without touching the rest of the code.

use std::time::Duration;

use crate::browser::metrics::PerfFormat;

/// Default interval between process-tree RSS samples when performance
/// metrics are enabled but no explicit interval was requested.
const DEFAULT_PERF_RSS_INTERVAL: Duration = Duration::from_millis(5000);

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
    /// Tab suspension policy: how long a background tab must sit idle
    /// (elapsed time since it was last the active tab) before it becomes
    /// eligible for *automatic* suspension — see
    /// `browser::tabs::Tabs::idle_background_tabs`.
    ///
    /// `None` disables automatic suspension entirely; manual suspension
    /// (the tab strip's suspend button, `ui::toolbar::ToolbarCommand::SuspendTab`)
    /// is always available regardless of this setting. Defaults to `None`
    /// so a fresh checkout never suspends a tab the user did not ask to
    /// suspend — see docs/decisions.md D9 for why automatic suspension is
    /// opt-in for now.
    pub auto_suspend_after: Option<Duration>,
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
            homepage: "https://example.com".to_owned(),
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
            auto_suspend_after: None,
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
        let private = resolve_private(std::env::var_os("VELOX_PRIVATE").is_some(), args);
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
        Self {
            private,
            search_engine,
            perf_metrics,
            perf_rss_interval,
            perf_format,
            perf_output_path,
            ..Self::default()
        }
    }
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
        assert_eq!(config.auto_suspend_after, None);
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
}
