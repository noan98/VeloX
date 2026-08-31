//! Startup configuration for VeloX.
//!
//! Kept as a plain struct so that a config file / CLI flags can be layered on
//! later without touching the rest of the code.

use std::time::Duration;

/// Default interval between process-tree RSS samples when performance
/// metrics are enabled but no explicit interval was requested.
const DEFAULT_PERF_RSS_INTERVAL: Duration = Duration::from_millis(5000);

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
    /// Height of the history/bookmarks dropdown panel (logical pixels) when
    /// open; added to `toolbar_height` while a panel is showing.
    pub panel_height: u32,
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
            history_max_entries: 5000,
            history_panel_limit: 200,
            auto_suspend_after: None,
            private: false,
            perf_metrics: false,
            perf_rss_interval: None,
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
        Self {
            private,
            perf_metrics,
            perf_rss_interval,
            ..Self::default()
        }
    }
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
        assert!(config.history_panel_limit > 0);
        // Automatic suspension must be opt-in: a fresh checkout should never
        // surprise a user by suspending a tab on its own.
        assert_eq!(config.auto_suspend_after, None);
        assert!(!config.private);
        assert!(!config.perf_metrics);
        assert_eq!(config.perf_rss_interval, None);
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
}
