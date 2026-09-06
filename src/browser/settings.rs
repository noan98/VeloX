//! Persisted, user-editable browser settings (Issue #30).
//!
//! This is the data model behind the settings screen
//! (`ui/toolbar.html`'s "設定" panel): General / Appearance / Search /
//! Privacy / Performance / Downloads / Advanced fields the user can change
//! from the UI and that survive a restart. Security and Shortcuts are
//! deliberately **not** part of this persisted shape — the settings screen
//! still shows a tab for each, but as a read-only view over data that
//! already exists elsewhere ([`shortcut_reference`]'s static table, and
//! `browser::site_permissions::SitePermissionStore`, already loaded by
//! `app::AppState`) rather than a new preference. See docs/decisions.md D67
//! for the full reasoning behind that split, and for which fields below
//! take effect immediately vs. only after the next restart.
//!
//! Shaped like [`crate::browser::session::SessionSnapshot`]: plain,
//! serde-derived, UI/engine-independent data with a [`Settings::sanitize`]
//! step that repairs bad *values* rather than ever failing outright — the
//! Issue #35/D62 "壊れた設定で起動できなくならない" rule this issue's own
//! acceptance criteria repeat. `browser::persistence` loads/saves this as
//! `settings.json`; a `settings.json` that fails to *parse* at all (missing,
//! truncated, wrong shape) already yields `None` there, the same contract
//! `load_session` uses, so `app::run` falls back to [`Settings::default`]
//! wholesale in that case — never a failed startup.
//!
//! **Forward/backward compatibility**: every field carries
//! `#[serde(default)]`, so a `settings.json` written by an older VeloX
//! (missing fields this version added) deserializes cleanly, filling in
//! today's defaults for whatever is missing — this is VeloX's answer to the
//! issue's "デフォルト値/マイグレーション" acceptance criterion: additive
//! schema growth needs no explicit migration step at all. A `settings.json`
//! written by a *newer* VeloX (extra fields this version has never heard of)
//! also deserializes cleanly, since serde ignores unknown fields by default
//! (this struct does not set `deny_unknown_fields`, unlike
//! `ui::toolbar::ToolbarCommand` — that enum's `deny_unknown_fields` is
//! about rejecting a malformed *IPC command*, a different concern from
//! tolerating a settings file from a different VeloX version).
//! [`SETTINGS_SCHEMA_VERSION`]/[`Settings::schema_version`] is the seam a
//! future *structural* (not just additive) change would migrate through —
//! not needed yet, since every change so far has been additive.

use crate::browser::navigation;

/// Bumped only for a structural (not just additive) change to this shape —
/// see the module doc comment. Every field added so far has been additive
/// (`#[serde(default)]` handles it), so nothing currently branches on this
/// value; it exists as the documented seam for when one eventually does.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// Kept in sync with `config::Config::default().homepage` by convention
/// (documented here and there) rather than by a shared constant: `browser::`
/// must not depend on `config::` (config depends on browser, never the
/// other way — see docs/architecture.md's four-layer split), so this module
/// cannot reference `Config` at all.
const DEFAULT_HOMEPAGE: &str = "https://www.google.com/";

/// Kept in sync with `config::DEFAULT_MAX_TABS_PER_WEB_PROCESS` — same
/// cross-layer-constant caveat as [`DEFAULT_HOMEPAGE`].
const DEFAULT_MAX_TABS_PER_WEB_PROCESS: usize = 4;

/// Kept in sync with `browser::suspension::SuspensionPolicy::
/// DEFAULT_MEMORY_CHECK_INTERVAL` (2 seconds), in milliseconds since that is
/// the unit every other Performance-tab field here uses.
const DEFAULT_MEMORY_CHECK_INTERVAL_MS: u64 = 2000;

const SEARCH_ENGINE_PRESETS: &[&str] = &["duckduckgo", "google", "bing", "startpage", "ecosia"];
const CUSTOM_SEARCH_ENGINE_PRESET: &str = "custom";
const DEFAULT_SEARCH_ENGINE_PRESET: &str = "duckduckgo";

const PERF_FORMAT_TEXT: &str = "text";
const PERF_FORMAT_JSON: &str = "json";

/// The full persisted settings document — see the module doc comment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub general: GeneralSettings,
    #[serde(default)]
    pub appearance: AppearanceSettings,
    #[serde(default)]
    pub search: SearchSettings,
    #[serde(default)]
    pub privacy: PrivacySettings,
    #[serde(default)]
    pub performance: PerformanceSettings,
    #[serde(default)]
    pub downloads: DownloadsSettings,
    #[serde(default)]
    pub advanced: AdvancedSettings,
}

fn default_schema_version() -> u32 {
    SETTINGS_SCHEMA_VERSION
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            general: GeneralSettings::default(),
            appearance: AppearanceSettings::default(),
            search: SearchSettings::default(),
            privacy: PrivacySettings::default(),
            performance: PerformanceSettings::default(),
            downloads: DownloadsSettings::default(),
            advanced: AdvancedSettings::default(),
        }
    }
}

impl Settings {
    /// Repair (never reject) every field, in place, then return `self`.
    /// Called on every settings.json load and on every `update_settings` IPC
    /// command, so a hand-edited file or a hostile/malformed IPC payload
    /// (Issue #35's threat model — a settings file is external input the
    /// same way a URL or an IPC message is) can never leave VeloX with a
    /// value one of its own consumers (`navigation::normalize_input`,
    /// `Config::apply_settings`, a numeric divide/allocation) is not
    /// prepared for. Always succeeds — there is no "reject the whole file"
    /// case here (unlike `SessionSnapshot::sanitize`'s `Option`), since every
    /// individual field has an unambiguous safe fallback.
    #[must_use]
    pub fn sanitize(mut self) -> Self {
        self.schema_version = SETTINGS_SCHEMA_VERSION;
        self.general.sanitize();
        self.appearance.sanitize();
        self.search.sanitize();
        self.privacy.sanitize();
        self.performance.sanitize();
        self.downloads.sanitize();
        self.advanced.sanitize();
        self
    }
}

/// "General" tab. Both fields take effect only after the next restart —
/// `homepage` is read once when the very first tab is created,
/// `restore_previous_session` only at startup — see docs/decisions.md D67.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GeneralSettings {
    #[serde(default = "default_homepage")]
    pub homepage: String,
    #[serde(default)]
    pub restore_previous_session: bool,
}

fn default_homepage() -> String {
    DEFAULT_HOMEPAGE.to_owned()
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            homepage: default_homepage(),
            restore_previous_session: false,
        }
    }
}

impl GeneralSettings {
    fn sanitize(&mut self) {
        // Same gate address-bar input goes through (`browser::navigation`'s
        // one scheme allow-list) — a rejected/unparseable/dangerous scheme
        // (`javascript:` and friends) falls back to the compiled-in default
        // rather than becoming a startup URL, mirroring
        // `config::resolve_homepage`.
        self.homepage =
            navigation::normalize_input(&self.homepage).unwrap_or_else(default_homepage);
    }
}

/// Chrome theme: only affects VeloX's own UI (`ui/toolbar.html`'s
/// toolbar/tab-strip/bookmark-bar/panels, and — since Issue #31, see
/// docs/decisions.md D71 — the native window decorations `tao` draws around
/// it), never web page content — a page's own `prefers-color-scheme` is
/// entirely up to the OS/engine, which wry 0.56 exposes no per-webview
/// override for. `System` (the default) keeps today's behavior (the
/// toolbar's existing `@media (prefers-color-scheme: dark)` rule, and
/// `tao`'s own default of auto-tracking the OS theme for the window frame)
/// unchanged for anyone who never opens the settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    /// The string `ui::toolbar::set_theme_script` embeds into the
    /// `veloxSetTheme(...)` call, and the toolbar JS's own `data-theme`
    /// attribute value.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

/// A concrete (never "follow the OS") light/dark theme — what
/// [`native_window_theme`] resolves a [`Theme`] setting to when a surface
/// needs an actual answer rather than a further layer of "system" deferral.
/// A small mirror of `tao::window::Theme`'s two variants: `browser::` must
/// not depend on `tao` (see docs/architecture.md's four-layer split), so
/// `ui::window` is what converts this to the real `tao::window::Theme` at
/// the one call site that needs it
/// (`ui::window::BrowserWindow::set_theme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedTheme {
    Light,
    Dark,
}

/// What a UI surface that can only be told "explicit light/dark, or defer to
/// the OS yourself" — exactly the shape of `tao::window::Window::
/// set_theme(Option<tao::window::Theme>)` — should be given for a
/// [`Theme`] setting (Issue #31, see docs/decisions.md D71).
///
/// `None` for [`Theme::System`] is deliberate, not a missing case: `tao`'s
/// `WindowBuilder` already defaults every window's `preferred_theme` to
/// `None`, which makes it auto-track the OS theme for the whole lifetime of
/// the window (reacting to a live OS theme change with no polling needed —
/// see `docs/decisions.md` D71 for the `tao` 0.37 source references this
/// claim is based on). VeloX does not need to *resolve* "what is the OS
/// theme right now" itself to keep that behavior; it only needs to get out
/// of the way by passing `None` through whenever the user has not
/// overridden it. [`Theme::Light`]/[`Theme::Dark`] map straight across as an
/// explicit override — the same two states [`Theme::as_str`] already
/// pushes into the toolbar's own `data-velox-theme` attribute, now also
/// applied to the window frame itself so the two never disagree (previously
/// an explicit Light/Dark choice only ever reached the in-page toolbar,
/// leaving the OS-drawn title bar tracking the OS regardless — the concrete
/// gap this function closes).
pub fn native_window_theme(theme: Theme) -> Option<ResolvedTheme> {
    match theme {
        Theme::System => None,
        Theme::Light => Some(ResolvedTheme::Light),
        Theme::Dark => Some(ResolvedTheme::Dark),
    }
}

/// "Appearance" tab. Both fields apply **immediately**, with no restart
/// needed — unlike every other tab, neither is baked into `Config` or into
/// any per-tab webview closure at startup; `ui::window::BrowserWindow`
/// applies them straight from an `update_settings` command via
/// `set_theme`/`set_bookmark_bar_visible` (docs/decisions.md D67).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppearanceSettings {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub show_bookmark_bar: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            // Matches `ui::window::BrowserWindow::new`'s own
            // `bookmark_bar_visible: Cell::new(false)` — a fresh checkout
            // (no settings.json yet) must behave exactly as it did before
            // this setting existed.
            show_bookmark_bar: false,
        }
    }
}

impl AppearanceSettings {
    fn sanitize(&mut self) {
        // `Theme` is a closed, exhaustively-matched enum and `bool` cannot
        // hold an invalid value — nothing to repair. Kept for symmetry with
        // every other category's `sanitize`, and as the obvious place a
        // future Appearance field's validation would go.
    }
}

/// "Search" tab — settable engine mirrors `config::SearchEngine`'s presets
/// plus a fully custom name/template pair, but as plain strings: this
/// module cannot depend on `config::SearchEngine` (`config` depends on
/// `browser`, never the reverse), so `config::Config::apply_settings`
/// (`config::resolve_search_engine`, already unit-tested there) is what
/// turns these three fields into an actual `SearchEngine` — this struct is
/// deliberately just storage, not resolution. Applies after the next
/// restart only: `Config::search_engine` is read once at startup and not
/// re-consulted afterwards (docs/decisions.md D67).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchSettings {
    /// One of `"duckduckgo"`/`"google"`/`"bing"`/`"startpage"`/`"ecosia"`, or
    /// `"custom"` to use `custom_engine_name`/`custom_engine_url` instead.
    #[serde(default = "default_search_engine_preset")]
    pub engine_preset: String,
    #[serde(default)]
    pub custom_engine_name: String,
    #[serde(default)]
    pub custom_engine_url: String,
}

fn default_search_engine_preset() -> String {
    DEFAULT_SEARCH_ENGINE_PRESET.to_owned()
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            engine_preset: default_search_engine_preset(),
            custom_engine_name: String::new(),
            custom_engine_url: String::new(),
        }
    }
}

impl SearchSettings {
    fn sanitize(&mut self) {
        let normalized = self.engine_preset.trim().to_ascii_lowercase();
        self.engine_preset = if normalized == CUSTOM_SEARCH_ENGINE_PRESET
            || SEARCH_ENGINE_PRESETS.contains(&normalized.as_str())
        {
            normalized
        } else {
            // An unrecognized preset name (a typo'd hand edit, or a
            // `settings.json` from a VeloX version with presets this one
            // does not know) falls back to the default rather than being
            // stored as garbage — `Config::apply_settings`'s own resolution
            // would already treat it this way, but sanitizing here keeps
            // what the settings screen echoes back consistent too.
            default_search_engine_preset()
        };
        // Deliberately not validated further here (e.g. requiring `{}` in
        // `custom_engine_url`): an incomplete custom pair is safe to store
        // as-is (the user may be mid-edit) and `config::resolve_search_engine`
        // already falls back to the preset/default whenever the pair is not
        // usable — see its own tests.
    }
}

/// "Privacy" tab. Applies after the next restart only: the content-blocking
/// filter list and its per-site exception set are both built once at
/// startup and baked into every content webview's navigation hook
/// (docs/decisions.md D67; see `ui::window::content_webview_builder`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrivacySettings {
    #[serde(default = "default_true")]
    pub content_blocking_enabled: bool,
    #[serde(default)]
    pub content_blocking_site_exceptions: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self {
            content_blocking_enabled: true,
            content_blocking_site_exceptions: Vec::new(),
        }
    }
}

impl PrivacySettings {
    fn sanitize(&mut self) {
        // Same trim-and-drop-blank rule as
        // `config::resolve_content_blocking_site_exceptions` (which parses
        // the equivalent `VELOX_CONTENT_BLOCKING_ALLOW` env var) — kept
        // consistent so a host name behaves the same way regardless of
        // which of the two input paths set it.
        self.content_blocking_site_exceptions = self
            .content_blocking_site_exceptions
            .iter()
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty())
            .collect();
    }
}

/// "Performance" tab — mirrors `Config::max_tabs_per_web_process` and
/// `browser::suspension::SuspensionPolicy`'s three independent signals, as
/// plain optional numbers rather than the `Duration`/`usize` types those use
/// internally (again: this module cannot depend on `config`, and keeping
/// wire types boring simplifies both the JSON shape and the settings UI's
/// own number inputs). `None`/`0` uniformly means "this signal is off",
/// matching every `VELOX_*` env var's existing "unset or 0 means off" rule.
/// Applies after the next restart only (docs/decisions.md D67): the
/// suspension policy and the tab/process-sharing cap are both read once at
/// startup.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PerformanceSettings {
    #[serde(default = "default_max_tabs_per_web_process")]
    pub max_tabs_per_web_process: usize,
    #[serde(default)]
    pub auto_suspend_after_ms: Option<u64>,
    #[serde(default)]
    pub max_live_tabs: Option<usize>,
    #[serde(default)]
    pub memory_budget_mb: Option<u64>,
    #[serde(default = "default_memory_check_interval_ms")]
    pub memory_check_interval_ms: u64,
}

fn default_max_tabs_per_web_process() -> usize {
    DEFAULT_MAX_TABS_PER_WEB_PROCESS
}

fn default_memory_check_interval_ms() -> u64 {
    DEFAULT_MEMORY_CHECK_INTERVAL_MS
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            max_tabs_per_web_process: default_max_tabs_per_web_process(),
            auto_suspend_after_ms: None,
            max_live_tabs: None,
            memory_budget_mb: None,
            memory_check_interval_ms: default_memory_check_interval_ms(),
        }
    }
}

impl PerformanceSettings {
    fn sanitize(&mut self) {
        if self.max_tabs_per_web_process == 0 {
            self.max_tabs_per_web_process = default_max_tabs_per_web_process();
        }
        // `Some(0)` means the same thing an unset `VELOX_*` env var already
        // means for these three signals ("off") — collapse it to `None`
        // rather than storing a value every consumer would have to special-
        // case, mirroring `config::resolve_suspension`'s `positive()` filter.
        if self.auto_suspend_after_ms == Some(0) {
            self.auto_suspend_after_ms = None;
        }
        if self.max_live_tabs == Some(0) {
            self.max_live_tabs = None;
        }
        if self.memory_budget_mb == Some(0) {
            self.memory_budget_mb = None;
        }
        if self.memory_check_interval_ms == 0 {
            self.memory_check_interval_ms = default_memory_check_interval_ms();
        }
    }
}

/// "Downloads" tab. Applies after the next restart only: `Config::
/// apply_settings` copies this into `Config::download_dir_override`, which
/// `ui::window::with_download_handlers` is built with once per window/tab
/// (`browser::downloads::resolve_download_dir_with_override`) — see
/// docs/decisions.md D67.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DownloadsSettings {
    /// `None` keeps `resolve_download_dir`'s existing
    /// `VELOX_DOWNLOAD_DIR`/platform-default behavior.
    #[serde(default)]
    pub download_dir_override: Option<String>,
}

impl DownloadsSettings {
    fn sanitize(&mut self) {
        self.download_dir_override = self
            .download_dir_override
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }
}

/// "Advanced" tab — the power-user knobs that already existed as
/// `VELOX_PERF_*`/`VELOX_CONTENT_BLOCKING_*`-adjacent env vars
/// (`Config::perf_metrics`/`perf_format`/`perf_output_path`/
/// `extra_blocklist_path`). Applies after the next restart only: the perf
/// logger and the merged blocklist are both built once at startup
/// (docs/decisions.md D67).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdvancedSettings {
    #[serde(default)]
    pub perf_metrics_enabled: bool,
    /// `"text"` or `"json"` — see `browser::metrics::PerfFormat`. Anything
    /// else is sanitized back to `"text"`, matching `PerfFormat::parse`'s
    /// own "unrecognized falls back to Text" rule.
    #[serde(default = "default_perf_format")]
    pub perf_format: String,
    #[serde(default)]
    pub perf_output_path: Option<String>,
    #[serde(default)]
    pub extra_blocklist_path: Option<String>,
}

fn default_perf_format() -> String {
    PERF_FORMAT_TEXT.to_owned()
}

impl Default for AdvancedSettings {
    fn default() -> Self {
        Self {
            perf_metrics_enabled: false,
            perf_format: default_perf_format(),
            perf_output_path: None,
            extra_blocklist_path: None,
        }
    }
}

impl AdvancedSettings {
    fn sanitize(&mut self) {
        let normalized = self.perf_format.trim().to_ascii_lowercase();
        self.perf_format = if normalized == PERF_FORMAT_JSON {
            PERF_FORMAT_JSON.to_owned()
        } else {
            PERF_FORMAT_TEXT.to_owned()
        };
        self.perf_output_path = sanitize_optional_path(&self.perf_output_path);
        self.extra_blocklist_path = sanitize_optional_path(&self.extra_blocklist_path);
    }
}

fn sanitize_optional_path(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

// --- Shortcuts tab: a static reference table, not a persisted setting ---
//
// Every binding here already exists (`ui/toolbar.html`'s keydown listener,
// `ui::window::ContentShortcut` — see docs/decisions.md D18/D23) and is not
// user-remappable by this issue; the settings screen only displays it for
// discoverability. See docs/decisions.md D67 for why remapping is out of
// scope here.

/// One row of the Shortcuts tab's reference table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ShortcutInfo {
    pub action: &'static str,
    pub keys: &'static str,
}

/// Every keyboard shortcut VeloX currently wires up, in the order the
/// Shortcuts tab lists them. `Ctrl` reads as `Cmd` on macOS throughout (both
/// channels that implement these already accept either modifier — see
/// docs/decisions.md D18).
pub fn shortcut_reference() -> &'static [ShortcutInfo] {
    &[
        ShortcutInfo {
            action: "新しいタブ",
            keys: "Ctrl/Cmd+T",
        },
        ShortcutInfo {
            action: "タブを閉じる",
            keys: "Ctrl/Cmd+W",
        },
        ShortcutInfo {
            action: "閉じたタブを再度開く",
            keys: "Ctrl/Cmd+Shift+T",
        },
        ShortcutInfo {
            action: "次のタブ",
            keys: "Ctrl/Cmd+Tab",
        },
        ShortcutInfo {
            action: "前のタブ",
            keys: "Ctrl/Cmd+Shift+Tab",
        },
        ShortcutInfo {
            action: "1〜8番目のタブに切り替え",
            keys: "Ctrl/Cmd+1〜8",
        },
        ShortcutInfo {
            action: "最後のタブに切り替え",
            keys: "Ctrl/Cmd+9",
        },
        ShortcutInfo {
            action: "アドレスバーにフォーカス",
            keys: "Ctrl/Cmd+L",
        },
        ShortcutInfo {
            action: "ブックマークの追加/削除",
            keys: "Ctrl/Cmd+D",
        },
        ShortcutInfo {
            action: "ブックマークバーの表示切替",
            keys: "Ctrl/Cmd+Shift+B",
        },
        ShortcutInfo {
            action: "DevTools を開く",
            keys: "F12 (macOS: Cmd+Option+I)",
        },
        ShortcutInfo {
            action: "ページを保存",
            keys: "Ctrl/Cmd+S",
        },
        ShortcutInfo {
            action: "ページのソースを表示",
            keys: "Ctrl/Cmd+U",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Settings::default / round-trip ---

    #[test]
    fn default_settings_matches_pre_issue_30_behavior() {
        let settings = Settings::default();
        assert_eq!(settings.schema_version, SETTINGS_SCHEMA_VERSION);
        assert_eq!(settings.general.homepage, DEFAULT_HOMEPAGE);
        assert!(!settings.general.restore_previous_session);
        assert_eq!(settings.appearance.theme, Theme::System);
        assert!(!settings.appearance.show_bookmark_bar);
        assert_eq!(settings.search.engine_preset, "duckduckgo");
        assert!(settings.privacy.content_blocking_enabled);
        assert!(settings.privacy.content_blocking_site_exceptions.is_empty());
        assert_eq!(
            settings.performance.max_tabs_per_web_process,
            DEFAULT_MAX_TABS_PER_WEB_PROCESS
        );
        assert_eq!(settings.performance.auto_suspend_after_ms, None);
        assert_eq!(settings.performance.max_live_tabs, None);
        assert_eq!(settings.performance.memory_budget_mb, None);
        assert_eq!(
            settings.performance.memory_check_interval_ms,
            DEFAULT_MEMORY_CHECK_INTERVAL_MS
        );
        assert_eq!(settings.downloads.download_dir_override, None);
        assert!(!settings.advanced.perf_metrics_enabled);
        assert_eq!(settings.advanced.perf_format, "text");
        assert_eq!(settings.advanced.perf_output_path, None);
        assert_eq!(settings.advanced.extra_blocklist_path, None);
    }

    #[test]
    fn default_settings_is_already_sanitized() {
        let settings = Settings::default();
        assert_eq!(settings.clone().sanitize(), settings);
    }

    #[test]
    fn settings_round_trip_through_json() {
        let mut settings = Settings::default();
        settings.general.homepage = "https://example.com/".to_owned();
        settings.privacy.content_blocking_site_exceptions =
            vec!["example.com".to_owned(), "news.example".to_owned()];
        let json = serde_json::to_string(&settings).unwrap();
        let parsed: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, settings);
    }

    // --- Forward/backward compatibility ---

    #[test]
    fn missing_object_deserializes_to_every_default() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn a_settings_file_missing_fields_this_version_added_fills_in_defaults() {
        // Simulates an older VeloX's settings.json: only `general` is
        // present, and even that is missing `restore_previous_session`.
        let json = r#"{"general":{"homepage":"https://old.example/"}}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.general.homepage, "https://old.example/");
        assert!(!settings.general.restore_previous_session);
        assert_eq!(settings.appearance, AppearanceSettings::default());
        assert_eq!(settings.performance, PerformanceSettings::default());
    }

    #[test]
    fn unknown_fields_from_a_newer_settings_file_are_ignored() {
        let json = r#"{
            "schema_version": 999,
            "general": {"homepage": "https://example.com/", "restore_previous_session": false},
            "a_future_top_level_field": {"whatever": true}
        }"#;
        let settings: Settings =
            serde_json::from_str(json).expect("unknown fields must not fail deserialization");
        assert_eq!(settings.general.homepage, "https://example.com/");
    }

    // --- General ---

    #[test]
    fn sanitize_rejects_a_dangerous_homepage_scheme() {
        let mut settings = Settings::default();
        settings.general.homepage = "javascript:alert(1)".to_owned();
        let sanitized = settings.sanitize();
        assert_eq!(sanitized.general.homepage, DEFAULT_HOMEPAGE);
    }

    #[test]
    fn sanitize_normalizes_a_bare_host_homepage() {
        let mut settings = Settings::default();
        settings.general.homepage = "example.com".to_owned();
        let sanitized = settings.sanitize();
        assert_eq!(sanitized.general.homepage, "https://example.com/");
    }

    // --- Search ---

    #[test]
    fn sanitize_keeps_a_known_preset_case_insensitively() {
        let mut settings = Settings::default();
        settings.search.engine_preset = "  Google ".to_owned();
        assert_eq!(settings.sanitize().search.engine_preset, "google");
    }

    #[test]
    fn sanitize_falls_back_to_the_default_preset_for_an_unknown_name() {
        let mut settings = Settings::default();
        settings.search.engine_preset = "altavista".to_owned();
        assert_eq!(
            settings.sanitize().search.engine_preset,
            DEFAULT_SEARCH_ENGINE_PRESET
        );
    }

    #[test]
    fn sanitize_keeps_custom_as_a_valid_preset_even_with_an_incomplete_pair() {
        let mut settings = Settings::default();
        settings.search.engine_preset = "custom".to_owned();
        settings.search.custom_engine_name = String::new();
        settings.search.custom_engine_url = String::new();
        let sanitized = settings.sanitize();
        assert_eq!(sanitized.search.engine_preset, "custom");
        assert_eq!(sanitized.search.custom_engine_name, "");
    }

    // --- Privacy ---

    #[test]
    fn sanitize_trims_and_drops_blank_site_exceptions() {
        let mut settings = Settings::default();
        settings.privacy.content_blocking_site_exceptions =
            vec![" example.com ".to_owned(), "".to_owned(), "   ".to_owned()];
        assert_eq!(
            settings.sanitize().privacy.content_blocking_site_exceptions,
            vec!["example.com".to_owned()]
        );
    }

    // --- Performance ---

    #[test]
    fn sanitize_replaces_a_zero_max_tabs_per_web_process_with_the_default() {
        let mut settings = Settings::default();
        settings.performance.max_tabs_per_web_process = 0;
        assert_eq!(
            settings.sanitize().performance.max_tabs_per_web_process,
            DEFAULT_MAX_TABS_PER_WEB_PROCESS
        );
    }

    #[test]
    fn sanitize_keeps_a_positive_max_tabs_per_web_process() {
        let mut settings = Settings::default();
        settings.performance.max_tabs_per_web_process = 16;
        assert_eq!(settings.sanitize().performance.max_tabs_per_web_process, 16);
    }

    #[test]
    fn sanitize_treats_a_zero_optional_performance_signal_as_off() {
        let mut settings = Settings::default();
        settings.performance.auto_suspend_after_ms = Some(0);
        settings.performance.max_live_tabs = Some(0);
        settings.performance.memory_budget_mb = Some(0);
        let sanitized = settings.sanitize();
        assert_eq!(sanitized.performance.auto_suspend_after_ms, None);
        assert_eq!(sanitized.performance.max_live_tabs, None);
        assert_eq!(sanitized.performance.memory_budget_mb, None);
    }

    #[test]
    fn sanitize_keeps_positive_performance_signals() {
        let mut settings = Settings::default();
        settings.performance.auto_suspend_after_ms = Some(30_000);
        settings.performance.max_live_tabs = Some(5);
        settings.performance.memory_budget_mb = Some(700);
        let sanitized = settings.sanitize();
        assert_eq!(sanitized.performance.auto_suspend_after_ms, Some(30_000));
        assert_eq!(sanitized.performance.max_live_tabs, Some(5));
        assert_eq!(sanitized.performance.memory_budget_mb, Some(700));
    }

    #[test]
    fn sanitize_replaces_a_zero_memory_check_interval_with_the_default() {
        let mut settings = Settings::default();
        settings.performance.memory_check_interval_ms = 0;
        assert_eq!(
            settings.sanitize().performance.memory_check_interval_ms,
            DEFAULT_MEMORY_CHECK_INTERVAL_MS
        );
    }

    // --- Downloads / Advanced optional-path fields ---

    #[test]
    fn sanitize_treats_a_blank_download_dir_override_as_unset() {
        let mut settings = Settings::default();
        settings.downloads.download_dir_override = Some("   ".to_owned());
        assert_eq!(settings.sanitize().downloads.download_dir_override, None);
    }

    #[test]
    fn sanitize_trims_a_download_dir_override() {
        let mut settings = Settings::default();
        settings.downloads.download_dir_override = Some("  /tmp/dl  ".to_owned());
        assert_eq!(
            settings.sanitize().downloads.download_dir_override,
            Some("/tmp/dl".to_owned())
        );
    }

    #[test]
    fn sanitize_falls_back_to_text_for_an_unrecognized_perf_format() {
        let mut settings = Settings::default();
        settings.advanced.perf_format = "xml".to_owned();
        assert_eq!(settings.sanitize().advanced.perf_format, "text");
    }

    #[test]
    fn sanitize_accepts_json_perf_format_case_insensitively() {
        let mut settings = Settings::default();
        settings.advanced.perf_format = "JSON".to_owned();
        assert_eq!(settings.sanitize().advanced.perf_format, "json");
    }

    #[test]
    fn sanitize_treats_blank_advanced_paths_as_unset() {
        let mut settings = Settings::default();
        settings.advanced.perf_output_path = Some("".to_owned());
        settings.advanced.extra_blocklist_path = Some("   ".to_owned());
        let sanitized = settings.sanitize();
        assert_eq!(sanitized.advanced.perf_output_path, None);
        assert_eq!(sanitized.advanced.extra_blocklist_path, None);
    }

    // --- Robustness against hostile/malformed input (Issue #35's threat
    // model, same posture as `config`'s own tests) ---

    #[test]
    fn sanitize_does_not_panic_on_extreme_or_unicode_input() {
        let mut settings = Settings::default();
        settings.general.homepage = "a".repeat(2_000_000);
        settings.search.engine_preset = "エ".repeat(1000);
        settings.privacy.content_blocking_site_exceptions =
            vec!["a".repeat(500_000), String::new(), "  ".repeat(10_000)];
        settings.performance.auto_suspend_after_ms = Some(u64::MAX);
        settings.downloads.download_dir_override = Some("\0\0\0".to_owned());
        let _ = settings.sanitize();
    }

    #[test]
    fn deserializing_wrong_field_types_fails_cleanly_not_a_panic() {
        // `max_tabs_per_web_process` should be a number, not a string —
        // this must fail deserialization (letting the caller, e.g.
        // `persistence::load_settings`, fall back to defaults) rather than
        // panicking or silently coercing.
        let json = r#"{"performance":{"max_tabs_per_web_process":"not-a-number"}}"#;
        assert!(serde_json::from_str::<Settings>(json).is_err());
    }

    // --- shortcut_reference ---

    #[test]
    fn shortcut_reference_is_non_empty_with_no_blank_entries() {
        let shortcuts = shortcut_reference();
        assert!(!shortcuts.is_empty());
        for shortcut in shortcuts {
            assert!(!shortcut.action.trim().is_empty());
            assert!(!shortcut.keys.trim().is_empty());
        }
    }

    #[test]
    fn shortcut_reference_actions_are_unique() {
        let shortcuts = shortcut_reference();
        let mut actions: Vec<&str> = shortcuts.iter().map(|s| s.action).collect();
        let before = actions.len();
        actions.sort_unstable();
        actions.dedup();
        assert_eq!(
            actions.len(),
            before,
            "duplicate action in shortcut_reference"
        );
    }

    // --- Theme ---

    #[test]
    fn theme_as_str_round_trips_through_serde_rename() {
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            let json = serde_json::to_string(&theme).unwrap();
            assert_eq!(json, format!("\"{}\"", theme.as_str()));
            let parsed: Theme = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, theme);
        }
    }

    // --- native_window_theme (Issue #31, D71) ---

    #[test]
    fn native_window_theme_defers_to_the_os_for_system() {
        // `None` here is what tells `tao::window::Window::set_theme` to keep
        // auto-tracking the OS theme itself — see the doc comment.
        assert_eq!(native_window_theme(Theme::System), None);
    }

    #[test]
    fn native_window_theme_maps_light_and_dark_straight_across() {
        assert_eq!(
            native_window_theme(Theme::Light),
            Some(ResolvedTheme::Light)
        );
        assert_eq!(native_window_theme(Theme::Dark), Some(ResolvedTheme::Dark));
    }

    #[test]
    fn native_window_theme_is_a_pure_function_of_its_input() {
        // Same input, same output, every time — no hidden clock/OS query.
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            assert_eq!(native_window_theme(theme), native_window_theme(theme));
        }
    }
}
