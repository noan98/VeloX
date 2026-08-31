//! Startup configuration for VeloX.
//!
//! Kept as a plain struct so that a config file / CLI flags can be layered on
//! later without touching the rest of the code.

use std::time::Duration;

/// Application configuration, currently compile-time defaults only.
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
            panel_height: 320,
            history_max_entries: 5000,
            history_panel_limit: 200,
            auto_suspend_after: None,
            private: false,
        }
    }
}

impl Config {
    /// Build a config from compiled defaults, overridden by whether private
    /// browsing was requested via the `VELOX_PRIVATE` environment variable
    /// (presence, like `VELOX_DEBUG`; see `app.rs`) or a `--private`
    /// command-line flag in `args`.
    ///
    /// No CLI-parsing crate is introduced for this (see docs/decisions.md
    /// D6); `args` is expected to be the process arguments with argv\[0\]
    /// already stripped (e.g. `std::env::args().skip(1)`).
    pub fn from_env_and_args<I: IntoIterator<Item = String>>(args: I) -> Self {
        let private = resolve_private(std::env::var_os("VELOX_PRIVATE").is_some(), args);
        Self {
            private,
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
}
