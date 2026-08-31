//! Startup configuration for VeloX.
//!
//! Kept as a plain struct so that a config file / CLI flags can be layered on
//! later without touching the rest of the code.

use std::time::Duration;

/// Default interval between process-tree RSS samples when performance
/// metrics are enabled but no explicit interval was requested.
const DEFAULT_PERF_RSS_INTERVAL: Duration = Duration::from_millis(5000);

/// Application configuration, currently compile-time defaults plus a handful
/// of environment-variable overrides (see [`Config::from_env`]).
#[derive(Debug, Clone)]
pub struct Config {
    /// Page loaded when the browser starts.
    pub homepage: String,
    /// Title of the browser window.
    pub window_title: String,
    /// Initial window size (logical pixels).
    pub window_width: u32,
    pub window_height: u32,
    /// Height of the toolbar strip (logical pixels).
    pub toolbar_height: u32,
    /// Enable performance metrics logging to stderr: the four startup
    /// checkpoints, per-page-load duration, and (if
    /// [`Config::perf_rss_interval`] is set) periodic process-tree RSS
    /// sampling. Off by default so a normal run pays no timestamp or
    /// thread-spawn overhead (see `docs/architecture.md`, "Performance
    /// extension points"). Enable via `VELOX_PERF_METRICS=1`
    /// ([`Config::from_env`]), following the same opt-in pattern as the
    /// existing `VELOX_DEBUG` flag in `app.rs`.
    pub perf_metrics: bool,
    /// Interval between process-tree RSS samples while `perf_metrics` is
    /// on. `None` disables the periodic sampling thread. This only gates
    /// the *periodic* logger in `app::run`; on-demand sampling via
    /// [`crate::browser::metrics::sample_process_tree_rss`] is always
    /// available regardless of this setting.
    pub perf_rss_interval: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            homepage: "https://example.com".to_owned(),
            window_title: "VeloX".to_owned(),
            window_width: 1024,
            window_height: 768,
            toolbar_height: 48,
            perf_metrics: false,
            perf_rss_interval: None,
        }
    }
}

impl Config {
    /// [`Config::default`] layered with environment-variable overrides.
    ///
    /// - `VELOX_PERF_METRICS` — any value (including empty) turns on
    ///   `perf_metrics`; unset means off. Same opt-in shape as `VELOX_DEBUG`.
    /// - `VELOX_PERF_RSS_INTERVAL_MS` — only consulted when
    ///   `VELOX_PERF_METRICS` is set; overrides the periodic RSS sampling
    ///   interval in milliseconds. `0` disables periodic sampling while
    ///   still logging startup/page-load metrics. Not a valid number falls
    ///   back to the default interval.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        let metrics_requested = std::env::var_os("VELOX_PERF_METRICS").is_some();
        let interval_raw = std::env::var("VELOX_PERF_RSS_INTERVAL_MS").ok();
        let (perf_metrics, perf_rss_interval) =
            resolve_perf_env(metrics_requested, interval_raw.as_deref());
        config.perf_metrics = perf_metrics;
        config.perf_rss_interval = perf_rss_interval;
        config
    }
}

/// Pure decision logic behind [`Config::from_env`]'s perf-related fields,
/// factored out so it is unit-testable without touching real process
/// environment variables.
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
}
