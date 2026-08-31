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
            auto_suspend_after: None,
        }
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
        // Automatic suspension must be opt-in: a fresh checkout should never
        // surprise a user by suspending a tab on its own.
        assert_eq!(config.auto_suspend_after, None);
    }
}
