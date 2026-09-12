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
    /// Hostnames exempt from content blocking — the "サイト単位の例外"
    /// (per-site exception) Issue #22 asks for, applied to both main-frame
    /// navigation blocking (D17) and subresource blocking (D59, currently
    /// Windows/WebView2 only). Exact-host match, no subdomain expansion (see
    /// `browser::SiteExceptions`). Empty by default. Set via
    /// `VELOX_CONTENT_BLOCKING_ALLOW` (comma-separated hostnames).
    pub content_blocking_site_exceptions: Vec<String>,
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
    /// Height of the in-page find bar (Issue #43, Ctrl/Cmd+F) when open;
    /// added to `toolbar_height` the same independent, additive way
    /// `bookmark_bar_height` is (see docs/decisions.md D69) — a single
    /// compact row, not the much taller `panel_height` a history/bookmarks
    /// dropdown needs.
    pub find_bar_height: u32,
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
    /// each individually optional. **Defaults to the memory-budget signal on
    /// (700 MiB), idle time and tab-count off**
    /// ([`SuspensionPolicy::default`]) — Issue #184 / docs/decisions.md D90
    /// decided D56's Revisit condition (3), superseding D9's original
    /// "every signal off" default (D9/D56 are left as written; D90 records
    /// why and what changed). With few tabs open the process tree stays
    /// under budget and nothing is suspended, so this is unobservable for
    /// most sessions; set `VELOX_MEMORY_BUDGET_MB=0` (or the settings
    /// screen) to turn even that off. Manual suspension (the tab strip's
    /// suspend button, `ui::toolbar::ToolbarCommand::SuspendTab`) is always
    /// available regardless. Configured via `VELOX_AUTO_SUSPEND_AFTER_MS`,
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
    /// "前回のタブを復元" (Issue #25, see docs/decisions.md D65): when `true`,
    /// `app::run` loads the last-saved tab session
    /// (`browser::persistence::load_session`) instead of starting a single
    /// tab at `homepage`, provided a usable one exists (see
    /// `browser::session::SessionSnapshot::sanitize`). Off by default —
    /// same conservative-default rule as `suspension`/`private`: a fresh
    /// checkout, or a launch with this never turned on, must behave exactly
    /// as it did before this feature existed. Session data is still saved
    /// unconditionally (outside private mode) regardless of this flag, so
    /// turning it on later immediately has something to restore from.
    /// Selectable via `VELOX_RESTORE_SESSION` (presence, like
    /// `VELOX_PRIVATE`/`VELOX_PERF_METRICS`) — see
    /// [`Config::from_env_and_args`].
    pub restore_previous_session: bool,
    /// Overrides `browser::downloads::resolve_download_dir`'s platform
    /// default for every download this run (Issue #30's settings screen,
    /// Downloads tab — see docs/decisions.md D67). `None` keeps today's
    /// behavior (`VELOX_DOWNLOAD_DIR`, or the platform convention). Not
    /// settable via an env var of its own — it exists purely as the target
    /// [`Config::apply_settings`] writes into from a persisted
    /// `browser::settings::Settings`, applied once at startup like every
    /// other settings-screen field except Appearance (D67).
    pub download_dir_override: Option<String>,
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
            content_blocking_site_exceptions: Vec::new(),
            panel_height: 320,
            // A single row, roughly the height of a tab-strip row (see
            // ui/toolbar.html's #bookmark-bar rule) — enough for one line of
            // bookmark buttons.
            bookmark_bar_height: 30,
            // Same single-row sizing as `bookmark_bar_height` (see
            // ui/toolbar.html's #find-bar rule).
            find_bar_height: 34,
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
            restore_previous_session: false,
            download_dir_override: None,
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
    ///   `browser::suspension`). Off by default; unset or not a number
    ///   keeps that default (off). `0` explicitly turns it off (a no-op
    ///   today, since off is already the default — kept for symmetry with
    ///   the other two knobs and in case a future default changes this).
    /// - `VELOX_MAX_LIVE_TABS` — keep at most this many tabs alive at once
    ///   (the active tab included); the least recently used background
    ///   tabs beyond it are suspended. Off by default (see
    ///   docs/decisions.md D90 for why this one specifically is not
    ///   defaulted on); unset or not a number keeps that default (off), `0`
    ///   explicitly turns it off.
    /// - `VELOX_MEMORY_BUDGET_MB` — suspend least recently used background
    ///   tabs whenever the whole process tree's memory (PSS on Linux, RSS
    ///   on Windows/other Unix — docs/decisions.md D88/D90) exceeds this
    ///   many MiB. **On by default as of Issue #184 (700 MiB,
    ///   docs/decisions.md D90).** Unset or not a number keeps that
    ///   default; **`0` is the escape hatch that turns the memory signal
    ///   off** even though the default has it on (the settings screen's
    ///   Performance tab offers the same toggle — leave the field blank).
    ///   Any other positive value overrides the default outright.
    /// - `VELOX_MEMORY_CHECK_INTERVAL_MS` — only consulted when the memory
    ///   signal ends up on (by the `VELOX_MEMORY_BUDGET_MB` default or an
    ///   explicit override); how often memory is sampled. Unset, `0` or not
    ///   a number keeps
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
    /// - `VELOX_CONTENT_BLOCKING_ALLOW` — comma-separated hostnames exempt
    ///   from content blocking (Issue #22's per-site exception). Blank
    ///   entries and surrounding whitespace are dropped; unset means no
    ///   exceptions.
    /// - `VELOX_RESTORE_SESSION` — presence (like `VELOX_PRIVATE`) turns on
    ///   restoring the previous session's tabs at startup (Issue #25, see
    ///   docs/decisions.md D65). Unset means off.
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
            crate::browser::metrics::installed_ram_bytes(),
        );
        let content_blocking_site_exceptions = resolve_content_blocking_site_exceptions(
            std::env::var("VELOX_CONTENT_BLOCKING_ALLOW")
                .ok()
                .as_deref(),
        );
        let restore_previous_session = std::env::var_os("VELOX_RESTORE_SESSION").is_some();
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
            content_blocking_site_exceptions,
            restore_previous_session,
            ..defaults
        }
    }

    /// Merge a persisted, user-editable [`browser::settings::Settings`] (the
    /// settings screen, Issue #30) onto `self`.
    ///
    /// Called in `app::run`, right after a *real, already-existing*
    /// `settings.json` is loaded and [`browser::settings::Settings::sanitize`]d,
    /// and *before* anything downstream (`BrowserWindow::new`, the
    /// content-blocking filter list, the suspension policy) reads `self` —
    /// so every field touched here takes effect starting from the very
    /// first tab of this run, the same way an equivalent `VELOX_*`
    /// environment variable already would. This is a one-shot merge, not a
    /// live binding: a settings change made *during* a run through the
    /// settings screen only reaches `Config` (and therefore most of what it
    /// feeds) on the *next* restart — see docs/decisions.md D67 for exactly
    /// which fields are the exception (`ui::window::BrowserWindow` applies
    /// Appearance's `theme`/`show_bookmark_bar` immediately, without going
    /// through `Config` at all).
    ///
    /// **Only call this when a `settings.json` actually exists on disk** —
    /// never with a defaulted [`browser::settings::Settings`] a fresh
    /// checkout never wrote. Doing so would silently discard every
    /// `VELOX_*` environment variable/CLI flag [`Config::from_env_and_args`]
    /// just resolved (they would all read back as "unset" from a
    /// never-saved `Settings::default()`), which is exactly backwards from
    /// this issue's "ConfigとUIが適切に分離される" acceptance criterion —
    /// see [`Config::to_settings`] for the seam that keeps a fresh
    /// checkout's env-driven `Config` visible to (and preserved by) the
    /// settings screen instead.
    ///
    /// `Settings` already arrives sanitized (a rejected/malformed value
    /// already repaired to a safe fallback), so this function does no
    /// validation of its own — it is a plain field-by-field copy, using
    /// [`resolve_search_engine`] (the same resolution
    /// [`Config::from_env_and_args`] uses for `VELOX_SEARCH_ENGINE*`) so a
    /// preset name or a custom name/template pair is turned into a
    /// [`SearchEngine`] exactly one way, not two independently-maintained
    /// ones.
    pub fn apply_settings(&mut self, settings: &crate::browser::settings::Settings) {
        self.homepage = settings.general.homepage.clone();
        self.restore_previous_session = settings.general.restore_previous_session;
        self.search_engine = resolve_search_engine(
            Some(&settings.search.engine_preset),
            Some(&settings.search.custom_engine_name),
            Some(&settings.search.custom_engine_url),
        );
        self.content_blocking_enabled = settings.privacy.content_blocking_enabled;
        self.content_blocking_site_exceptions =
            settings.privacy.content_blocking_site_exceptions.clone();
        self.max_tabs_per_web_process = settings.performance.max_tabs_per_web_process;
        self.suspension = SuspensionPolicy {
            idle_after: settings
                .performance
                .auto_suspend_after_ms
                .map(Duration::from_millis),
            max_live_tabs: settings.performance.max_live_tabs,
            memory_budget_bytes: settings
                .performance
                .memory_budget_mb
                .map(|mib| mib.saturating_mul(1024 * 1024)),
            memory_check_interval: Duration::from_millis(
                settings.performance.memory_check_interval_ms,
            ),
        };
        self.download_dir_override = settings.downloads.download_dir_override.clone();
        self.perf_metrics = settings.advanced.perf_metrics_enabled;
        self.perf_format = if settings.advanced.perf_format == "json" {
            PerfFormat::Json
        } else {
            PerfFormat::Text
        };
        self.perf_output_path = settings.advanced.perf_output_path.clone();
        self.extra_blocklist_path = settings.advanced.extra_blocklist_path.clone();
    }

    /// The reverse of [`Config::apply_settings`]: build a
    /// [`browser::settings::Settings`] that reflects `self`'s *current*
    /// values, for `app::run` to seed the settings screen with the first
    /// time it runs with no `settings.json` on disk yet.
    ///
    /// Without this, `app::AppState::settings` would start from
    /// [`browser::settings::Settings::default`] regardless of any
    /// `VELOX_*` environment variable/CLI flag already in effect for this
    /// run — the settings screen would show (and, on the first "保存",
    /// permanently persist) values the user never actually asked for,
    /// silently discarding whatever got them here. Seeding from `self`
    /// instead means: open the settings screen before ever saving, and it
    /// shows exactly what is actually running; click "保存" without
    /// changing anything, and nothing changes on the next restart either.
    ///
    /// `search_engine` is matched back against every built-in preset's own
    /// value ([`SearchEngine::duckduckgo`] etc.); anything that does not
    /// match exactly (a custom engine, or a preset this version does not
    /// list) round-trips as `"custom"` plus its name/template — never lossy
    /// in a way that would change what the omnibox actually sends a search
    /// to. `appearance` has no `Config` equivalent (D67 — it never was a
    /// `Config` field, only ever a settings-screen/`ui::window` one), so it
    /// always starts at [`browser::settings::AppearanceSettings::default`].
    pub fn to_settings(&self) -> crate::browser::settings::Settings {
        use crate::browser::settings::{
            AdvancedSettings, AppearanceSettings, DownloadsSettings, GeneralSettings,
            PerformanceSettings, PrivacySettings, SearchSettings, Settings,
            SETTINGS_SCHEMA_VERSION,
        };

        let (engine_preset, custom_engine_name, custom_engine_url) =
            if self.search_engine == SearchEngine::duckduckgo() {
                ("duckduckgo".to_owned(), String::new(), String::new())
            } else if self.search_engine == SearchEngine::google() {
                ("google".to_owned(), String::new(), String::new())
            } else if self.search_engine == SearchEngine::bing() {
                ("bing".to_owned(), String::new(), String::new())
            } else if self.search_engine == SearchEngine::startpage() {
                ("startpage".to_owned(), String::new(), String::new())
            } else if self.search_engine == SearchEngine::ecosia() {
                ("ecosia".to_owned(), String::new(), String::new())
            } else {
                (
                    "custom".to_owned(),
                    self.search_engine.name.clone(),
                    self.search_engine.query_template.clone(),
                )
            };

        Settings {
            schema_version: SETTINGS_SCHEMA_VERSION,
            general: GeneralSettings {
                homepage: self.homepage.clone(),
                restore_previous_session: self.restore_previous_session,
            },
            appearance: AppearanceSettings::default(),
            search: SearchSettings {
                engine_preset,
                custom_engine_name,
                custom_engine_url,
            },
            privacy: PrivacySettings {
                content_blocking_enabled: self.content_blocking_enabled,
                content_blocking_site_exceptions: self.content_blocking_site_exceptions.clone(),
            },
            performance: PerformanceSettings {
                max_tabs_per_web_process: self.max_tabs_per_web_process,
                auto_suspend_after_ms: self.suspension.idle_after.map(|d| d.as_millis() as u64),
                max_live_tabs: self.suspension.max_live_tabs,
                memory_budget_mb: self
                    .suspension
                    .memory_budget_bytes
                    .map(|bytes| bytes / (1024 * 1024)),
                memory_check_interval_ms: self.suspension.memory_check_interval.as_millis() as u64,
            },
            downloads: DownloadsSettings {
                download_dir_override: self.download_dir_override.clone(),
            },
            advanced: AdvancedSettings {
                perf_metrics_enabled: self.perf_metrics,
                perf_format: match self.perf_format {
                    PerfFormat::Json => "json",
                    PerfFormat::Text => "text",
                }
                .to_owned(),
                perf_output_path: self.perf_output_path.clone(),
                extra_blocklist_path: self.extra_blocklist_path.clone(),
            },
        }
        // Sanitized regardless: `self` is already a valid `Config`, so this
        // is expected to be a no-op, but running it keeps the contract
        // "every `Settings` this codebase hands to the UI/persistence layer
        // has been through `sanitize`" exceptionless rather than carving
        // out "except this one, which is already fine" as a special case.
        .sanitize()
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
pub(crate) fn resolve_search_engine(
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
/// `suspension` (Issue #63, defaults revised by #184/D90), factored out
/// like [`resolve_perf_env`] so the parsing rules are unit-tested without
/// touching the process environment.
///
/// Every knob is resolved against [`SuspensionPolicy::default`], not a
/// hardcoded "off": unset, empty, or not a number keeps whatever the
/// compiled-in default already is for that field (`None` for idle time and
/// tab-count, `Some(700 MiB)` for the memory budget as of D90) — a typo in
/// a shell profile must never produce a surprising policy, only the
/// conservative one, which since D90 means "the compiled default", not
/// unconditionally "off". An explicit `0` is the one value that always
/// means "off", *even when the default for that field is on* — this is the
/// escape hatch `VELOX_MEMORY_BUDGET_MB=0` needs to exist for D90's default
/// to be turnable off at all. Any other positive number overrides the
/// default outright. `check_interval_ms_raw` is the one exception: a
/// `Duration` has no "off" state, so unset/`0`/garbage there all keep
/// [`SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL`], unchanged from
/// before D90.
fn resolve_suspension(
    idle_after_ms_raw: Option<&str>,
    max_live_tabs_raw: Option<&str>,
    memory_budget_mb_raw: Option<&str>,
    check_interval_ms_raw: Option<&str>,
    installed_ram_bytes: Option<u64>,
) -> SuspensionPolicy {
    /// A knob whose *default* may itself be `Some` (D90's memory budget):
    /// unset/blank/not-a-number keeps `default`, an explicit `0` disables
    /// the signal regardless of what `default` says, and any other
    /// positive number overrides it. Generalizes the pre-D90 rule ("unset
    /// or 0 means off") to a world where "unset" and "explicitly off" are
    /// no longer always the same outcome.
    fn overridable(raw: Option<&str>, default: Option<u64>) -> Option<u64> {
        match raw
            .map(str::trim)
            .and_then(|value| value.parse::<u64>().ok())
        {
            None => default,
            Some(0) => None,
            Some(value) => Some(value),
        }
    }
    fn positive(raw: Option<&str>) -> Option<u64> {
        raw.map(str::trim)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
    }
    // 既定のメモリ予算だけは搭載 RAM で決まる (Issue #176 / D93 案 C)。
    // **読み取りは呼び出し側が済ませて引数で渡す** — `suspension` は
    // `/proc` を自分で読まない純粋ロジックであり (D20)、この関数も
    // 同じ理由で環境をここで触らない (既存の env 値がすべて引数で
    // 渡されているのと同じ形)。
    let defaults = SuspensionPolicy::for_installed_ram(installed_ram_bytes);
    SuspensionPolicy {
        idle_after: overridable(
            idle_after_ms_raw,
            defaults.idle_after.map(|d| d.as_millis() as u64),
        )
        .map(Duration::from_millis),
        max_live_tabs: overridable(
            max_live_tabs_raw,
            defaults.max_live_tabs.map(|value| value as u64),
        )
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX)),
        memory_budget_bytes: overridable(
            memory_budget_mb_raw,
            defaults
                .memory_budget_bytes
                .map(|bytes| bytes / (1024 * 1024)),
        )
        .map(|mib| mib.saturating_mul(1024 * 1024)),
        memory_check_interval: positive(check_interval_ms_raw)
            .map(Duration::from_millis)
            .unwrap_or(defaults.memory_check_interval),
    }
}

/// Pure decision logic behind [`Config::from_env_and_args`]'s
/// `content_blocking_site_exceptions` (Issue #22), factored out for the same
/// testability reason as [`resolve_suspension`]. Splits on `,`, trims each
/// entry, and drops blanks — unset or empty input yields no exceptions
/// (content blocking stays fully active), matching every other knob here:
/// absence of the variable must never silently change behavior.
fn resolve_content_blocking_site_exceptions(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::settings::{AppearanceSettings, Settings};

    #[test]
    fn default_config_is_sane() {
        let config = Config::default();
        assert!(config.homepage.starts_with("https://"));
        assert!(config.toolbar_height > 0);
        assert!(config.window_height > config.toolbar_height);
        assert!(config.panel_height > 0);
        assert!(config.bookmark_bar_height > 0);
        assert!(config.history_panel_limit > 0);
        // Issue #184 / docs/decisions.md D90: a fresh checkout has the
        // memory-budget signal on (700 MiB) and nothing else — see
        // `browser::suspension`'s own `default_policy_enables_only_the_
        // memory_budget_signal` for the exact values. With few tabs open
        // this never actually suspends anything (D90), but `is_enabled()`
        // is true, unlike before D90.
        assert_eq!(
            config.max_tabs_per_web_process,
            DEFAULT_MAX_TABS_PER_WEB_PROCESS
        );
        assert_eq!(config.suspension, SuspensionPolicy::default());
        assert!(config.suspension.is_enabled());
        assert_eq!(config.suspension.idle_after, None);
        assert_eq!(config.suspension.max_live_tabs, None);
        assert!(config.suspension.memory_budget_bytes.is_some());
        assert!(!config.private);
        assert!(!config.perf_metrics);
        assert_eq!(config.perf_rss_interval, None);
        assert_eq!(config.perf_format, PerfFormat::Text);
        assert_eq!(config.perf_output_path, None);
        assert_eq!(config.search_engine, SearchEngine::duckduckgo());
        assert!(config.content_blocking_site_exceptions.is_empty());
        // Issue #25 (D65): a fresh checkout must never restore a session
        // the user did not ask for, mirroring the same conservative
        // default `suspension` and `private` already follow.
        assert!(!config.restore_previous_session);
    }

    // -- resolve_content_blocking_site_exceptions (Issue #22) -------------

    #[test]
    fn content_blocking_allow_unset_yields_no_exceptions() {
        assert_eq!(
            resolve_content_blocking_site_exceptions(None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn content_blocking_allow_parses_comma_separated_hosts() {
        assert_eq!(
            resolve_content_blocking_site_exceptions(Some("example.com,news.example")),
            vec!["example.com".to_owned(), "news.example".to_owned()]
        );
    }

    #[test]
    fn content_blocking_allow_trims_whitespace_and_drops_blank_entries() {
        assert_eq!(
            resolve_content_blocking_site_exceptions(Some(" example.com , , news.example ")),
            vec!["example.com".to_owned(), "news.example".to_owned()]
        );
    }

    #[test]
    fn content_blocking_allow_empty_string_yields_no_exceptions() {
        assert_eq!(
            resolve_content_blocking_site_exceptions(Some("")),
            Vec::<String>::new()
        );
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

    // -- resolve_suspension (Issue #63, defaults revised by #184/D90) -----

    #[test]
    fn resolve_suspension_with_no_env_vars_matches_the_compiled_default() {
        // Since D90, "nothing set" no longer means "everything off" — it
        // means "whatever `SuspensionPolicy::default` already is", which as
        // of D90 has the memory-budget signal on.
        let policy = resolve_suspension(None, None, None, None, None);
        assert_eq!(policy, SuspensionPolicy::default());
        assert!(policy.is_enabled());
        assert_eq!(policy.idle_after, None);
        assert_eq!(policy.max_live_tabs, None);
        assert_eq!(
            policy.memory_budget_bytes,
            Some(crate::browser::suspension::DEFAULT_MEMORY_BUDGET_BYTES)
        );
    }

    #[test]
    fn resolve_suspension_scales_the_default_budget_to_installed_ram() {
        // Issue #176 / D93 案 C: **RAM 相対の既定値が製品に入る唯一の
        // 経路がここである。** `SuspensionPolicy::default()` も
        // `Settings::default()` も RAM を見ないので、この関数が壊れると
        // 利用者には従来の 700 MiB が黙って戻る (テストは全部緑のまま)。
        const GIB: u64 = 1024 * 1024 * 1024;
        let policy = resolve_suspension(None, None, None, None, Some(32 * GIB));
        assert_eq!(policy.memory_budget_bytes, Some(2048 * 1024 * 1024));
        // 予算以外は D90 の既定のまま。
        assert_eq!(policy.idle_after, None);
        assert_eq!(policy.max_live_tabs, None);
        assert_eq!(
            policy.memory_check_interval,
            SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL
        );
    }

    #[test]
    fn an_explicit_memory_budget_wins_over_the_ram_relative_default() {
        // 搭載 RAM は**既定値**を決めるだけで、明示指定
        // (`VELOX_MEMORY_BUDGET_MB` / 設定画面) には一切かからない —
        // 上限 2048 MiB も下限 700 MiB もここでは効かない。
        const GIB: u64 = 1024 * 1024 * 1024;
        let big = resolve_suspension(None, None, Some("4096"), None, Some(64 * GIB));
        assert_eq!(big.memory_budget_bytes, Some(4096 * 1024 * 1024));
        let small = resolve_suspension(None, None, Some("300"), None, Some(64 * GIB));
        assert_eq!(small.memory_budget_bytes, Some(300 * 1024 * 1024));
        // `0` による無効化も RAM に関係なく効き続ける。
        let off = resolve_suspension(None, None, Some("0"), None, Some(64 * GIB));
        assert_eq!(off.memory_budget_bytes, None);
        assert!(!off.is_enabled());
    }

    #[test]
    fn a_small_machine_keeps_exactly_todays_default_budget() {
        // D93 が「裸の比率」を退けた理由そのもの: 4 GiB / 8 GiB 機で
        // 予算が下限を割ってはならない。ここが緩むと、小容量機ほど
        // 休止が増えるという最悪の向きの退行になる。
        const GIB: u64 = 1024 * 1024 * 1024;
        for ram in [2 * GIB, 4 * GIB, 8 * GIB] {
            let policy = resolve_suspension(None, None, None, None, Some(ram));
            assert_eq!(
                policy,
                SuspensionPolicy::default(),
                "{} GiB 機で既定が変わってしまった",
                ram / GIB
            );
        }
    }

    #[test]
    fn resolve_suspension_parses_each_knob_independently() {
        let policy = resolve_suspension(Some("30000"), Some("5"), Some("700"), Some("500"), None);
        assert_eq!(policy.idle_after, Some(Duration::from_secs(30)));
        assert_eq!(policy.max_live_tabs, Some(5));
        assert_eq!(policy.memory_budget_bytes, Some(700 * 1024 * 1024));
        assert_eq!(policy.memory_check_interval, Duration::from_millis(500));
        assert!(policy.is_enabled());

        // One knob alone is enough to enable the policy — isolated here by
        // explicitly turning the now-default-on memory signal off (`"0"`),
        // so this only demonstrates the tab-count knob.
        let only_count = resolve_suspension(None, Some(" 3 "), Some("0"), None, None);
        assert_eq!(only_count.max_live_tabs, Some(3));
        assert_eq!(only_count.idle_after, None);
        assert_eq!(only_count.memory_budget_bytes, None);
        assert!(only_count.is_enabled());
    }

    #[test]
    fn resolve_suspension_empty_and_garbage_are_treated_as_unset_so_the_default_applies() {
        // Blank/unparseable input is indistinguishable from "not set" —
        // every knob falls back to `SuspensionPolicy::default()`, exactly
        // as an entirely absent env var would (this is what changed with
        // D90: falling back to "the default" is no longer always the same
        // as falling back to "off").
        for raw in ["", "  ", "-1", "abc", "1.5"] {
            let policy = resolve_suspension(Some(raw), Some(raw), Some(raw), Some(raw), None);
            assert_eq!(policy, SuspensionPolicy::default(), "raw was {raw:?}");
        }
    }

    #[test]
    fn resolve_suspension_explicit_zero_disables_every_signal_even_ones_defaulted_on() {
        // The escape hatch D90 requires: `0` always means "off", even for
        // the memory-budget signal whose *default* is on. Without this,
        // there would be no way to turn D90's default off via env var.
        let policy = resolve_suspension(Some("0"), Some("0"), Some("0"), Some("0"), None);
        assert!(!policy.is_enabled());
        assert_eq!(policy.idle_after, None);
        assert_eq!(policy.max_live_tabs, None);
        assert_eq!(policy.memory_budget_bytes, None);
        // The interval has no "off" state — explicit `0` there keeps the
        // compiled default, same as before D90.
        assert_eq!(
            policy.memory_check_interval,
            SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL
        );
    }

    #[test]
    fn resolve_suspension_memory_budget_zero_alone_is_the_real_world_off_switch() {
        // The exact env var a user (or docs/README) would actually set:
        // `VELOX_MEMORY_BUDGET_MB=0`, nothing else. This must fully turn
        // automatic suspension off again, matching pre-D90 behavior.
        let policy = resolve_suspension(None, None, Some("0"), None, None);
        assert!(!policy.is_enabled());
        assert_eq!(policy.memory_budget_bytes, None);
        assert_eq!(policy.idle_after, None);
        assert_eq!(policy.max_live_tabs, None);
    }

    #[test]
    fn resolve_suspension_interval_override_applies_independently_of_the_memory_signal() {
        // With the memory signal left at its default (on), an interval
        // override still applies on top of it.
        let with_default_memory = resolve_suspension(None, None, None, Some("100"), None);
        assert!(with_default_memory.is_enabled());
        assert_eq!(
            with_default_memory.memory_check_interval,
            Duration::from_millis(100)
        );
        // And with the memory signal explicitly off, the interval override
        // still applies (it is simply irrelevant — no sampler runs).
        let with_memory_off = resolve_suspension(None, None, Some("0"), Some("100"), None);
        assert!(!with_memory_off.is_enabled());
        assert_eq!(
            with_memory_off.memory_check_interval,
            Duration::from_millis(100)
        );
    }

    // --- Robustness against hostile/malformed env values (Issue #35): a
    // config knob is external input the same way a URL or an IPC message
    // is — a broken/adversarial environment must never panic the browser
    // at startup, only ever fall back to a safe default. ---

    #[test]
    fn numeric_env_knobs_do_not_panic_on_a_value_that_overflows_its_integer_type() {
        // One digit past u64::MAX — `str::parse` must return `Err`, not
        // panic, and every resolver here already treats a parse failure as
        // "use the default".
        let overflowing = format!("{}0", u64::MAX);
        assert_eq!(
            resolve_max_tabs_per_web_process(Some(&overflowing)),
            DEFAULT_MAX_TABS_PER_WEB_PROCESS
        );
        let policy = resolve_suspension(
            Some(&overflowing),
            Some(&overflowing),
            Some(&overflowing),
            Some(&overflowing),
            None,
        );
        assert_eq!(policy, SuspensionPolicy::default());
        // `resolve_perf_env` treats an unparseable value the same as an
        // absent one (falls back to the default interval, not "off" — see
        // `unparseable_interval_falls_back_to_default` above), so an
        // overflowing value follows that same documented rule.
        assert_eq!(
            resolve_perf_env(true, Some(&overflowing)).1,
            Some(DEFAULT_PERF_RSS_INTERVAL)
        );
    }

    #[test]
    fn resolve_homepage_does_not_panic_on_an_extremely_long_or_hostile_value() {
        let huge = format!("https://example.com/{}", "a".repeat(2_000_000));
        assert_eq!(
            resolve_homepage(None, &args(&["--homepage", &huge]), DEFAULT_HOME),
            huge
        );

        for hostile in [
            "javascript:alert(document.cookie)",
            "\0\0\0",
            "   \n\t  ",
            &"a".repeat(2_000_000), // not URL-shaped at all once huge
        ] {
            // Must not panic; a rejected/unparseable value always falls
            // back to the compiled-in default (see
            // `homepage_rejects_dangerous_schemes_and_falls_back` above).
            let _ = resolve_homepage(None, &args(&["--homepage", hostile]), DEFAULT_HOME);
        }
    }

    #[test]
    fn search_engine_env_values_do_not_panic_on_extreme_or_unicode_input() {
        let huge_name = "エ".repeat(500_000);
        let huge_template = format!("https://example.com/?q={{}}&pad={}", "a".repeat(500_000));
        let engine = resolve_search_engine(None, Some(&huge_name), Some(&huge_template));
        assert_eq!(engine.name, huge_name);
        assert_eq!(engine.query_template, huge_template);

        // A template with no placeholder, however large, is still rejected
        // the same way a short one is.
        let huge_template_no_placeholder = "a".repeat(500_000);
        assert_eq!(
            resolve_search_engine(None, Some("name"), Some(&huge_template_no_placeholder)),
            SearchEngine::duckduckgo()
        );
    }

    // --- Config::apply_settings (Issue #30, D67) ---

    #[test]
    fn apply_settings_with_default_settings_leaves_config_at_its_own_defaults() {
        // The most important property: a fresh checkout with no
        // settings.json yet must merge in `Settings::default()` (what
        // `app::run` uses when nothing was persisted) and end up exactly
        // where `Config::default()` already was — otherwise adding the
        // settings screen would itself be a behavior change for everyone
        // who never opens it.
        let mut config = Config::default();
        let before = config.clone();
        config.apply_settings(&Settings::default());
        assert_eq!(config.homepage, before.homepage);
        assert_eq!(
            config.restore_previous_session,
            before.restore_previous_session
        );
        assert_eq!(config.search_engine, before.search_engine);
        assert_eq!(
            config.content_blocking_enabled,
            before.content_blocking_enabled
        );
        assert_eq!(
            config.content_blocking_site_exceptions,
            before.content_blocking_site_exceptions
        );
        assert_eq!(
            config.max_tabs_per_web_process,
            before.max_tabs_per_web_process
        );
        assert_eq!(config.suspension, before.suspension);
        assert_eq!(config.download_dir_override, before.download_dir_override);
        assert_eq!(config.perf_metrics, before.perf_metrics);
        assert_eq!(config.perf_format, before.perf_format);
        assert_eq!(config.perf_output_path, before.perf_output_path);
        assert_eq!(config.extra_blocklist_path, before.extra_blocklist_path);
    }

    #[test]
    fn apply_settings_copies_general_and_search_fields() {
        let mut config = Config::default();
        let mut settings = Settings::default();
        settings.general.homepage = "https://example.com/".to_owned();
        settings.general.restore_previous_session = true;
        settings.search.engine_preset = "google".to_owned();
        config.apply_settings(&settings);
        assert_eq!(config.homepage, "https://example.com/");
        assert!(config.restore_previous_session);
        assert_eq!(config.search_engine, SearchEngine::google());
    }

    #[test]
    fn apply_settings_resolves_a_custom_search_engine() {
        let mut config = Config::default();
        let mut settings = Settings::default();
        settings.search.engine_preset = "custom".to_owned();
        settings.search.custom_engine_name = "My Engine".to_owned();
        settings.search.custom_engine_url = "https://example.com/search?q={}".to_owned();
        config.apply_settings(&settings);
        assert_eq!(
            config.search_engine,
            SearchEngine {
                name: "My Engine".to_owned(),
                query_template: "https://example.com/search?q={}".to_owned(),
            }
        );
    }

    #[test]
    fn apply_settings_copies_privacy_fields() {
        let mut config = Config::default();
        let mut settings = Settings::default();
        settings.privacy.content_blocking_enabled = false;
        settings.privacy.content_blocking_site_exceptions =
            vec!["example.com".to_owned(), "news.example".to_owned()];
        config.apply_settings(&settings);
        assert!(!config.content_blocking_enabled);
        assert_eq!(
            config.content_blocking_site_exceptions,
            vec!["example.com".to_owned(), "news.example".to_owned()]
        );
    }

    #[test]
    fn apply_settings_copies_performance_fields_into_the_suspension_policy() {
        let mut config = Config::default();
        let mut settings = Settings::default();
        settings.performance.max_tabs_per_web_process = 8;
        settings.performance.auto_suspend_after_ms = Some(30_000);
        settings.performance.max_live_tabs = Some(6);
        settings.performance.memory_budget_mb = Some(512);
        settings.performance.memory_check_interval_ms = 5_000;
        config.apply_settings(&settings);
        assert_eq!(config.max_tabs_per_web_process, 8);
        assert_eq!(config.suspension.idle_after, Some(Duration::from_secs(30)));
        assert_eq!(config.suspension.max_live_tabs, Some(6));
        assert_eq!(
            config.suspension.memory_budget_bytes,
            Some(512 * 1024 * 1024)
        );
        assert_eq!(
            config.suspension.memory_check_interval,
            Duration::from_secs(5)
        );
        assert!(config.suspension.is_enabled());
    }

    #[test]
    fn apply_settings_with_default_settings_leaves_the_default_on_memory_signal_enabled() {
        // Since D90, `Settings::default()` (a settings screen never opened,
        // or opened and saved without changing Performance) carries the
        // same memory-budget-on default `SuspensionPolicy::default` does
        // (`PerformanceSettings::default`'s `memory_budget_mb` mirrors it —
        // see that constant's doc comment) — applying it must not silently
        // disable what a fresh checkout already has on.
        let mut config = Config {
            suspension: SuspensionPolicy {
                idle_after: Some(Duration::from_secs(10)),
                max_live_tabs: Some(3),
                memory_budget_bytes: Some(100),
                memory_check_interval: Duration::from_secs(1),
            },
            ..Config::default()
        };
        config.apply_settings(&Settings::default());
        assert_eq!(config.suspension, SuspensionPolicy::default());
        assert!(config.suspension.is_enabled());
    }

    #[test]
    fn apply_settings_explicit_none_signals_disable_the_suspension_policy() {
        // The actual "turn it off in the settings screen" path: every
        // Performance-tab field explicitly `None` (what saving the
        // Performance tab with every suspension field left blank produces,
        // `PerformanceSettings::sanitize`'s `Some(0)` -> `None` collapse
        // included) must overwrite an already-on policy with a fully
        // disabled one — proving `apply_settings` overwrites rather than
        // merges, and that D90's default-on memory signal really can be
        // turned off from the UI, not just via `VELOX_MEMORY_BUDGET_MB=0`.
        let mut config = Config {
            suspension: SuspensionPolicy {
                idle_after: Some(Duration::from_secs(10)),
                max_live_tabs: Some(3),
                memory_budget_bytes: Some(100),
                memory_check_interval: Duration::from_secs(1),
            },
            ..Config::default()
        };
        let mut settings = Settings::default();
        settings.performance.auto_suspend_after_ms = None;
        settings.performance.max_live_tabs = None;
        settings.performance.memory_budget_mb = None;
        config.apply_settings(&settings);
        assert!(!config.suspension.is_enabled());
        assert_eq!(config.suspension.idle_after, None);
        assert_eq!(config.suspension.max_live_tabs, None);
        assert_eq!(config.suspension.memory_budget_bytes, None);
    }

    #[test]
    fn apply_settings_copies_downloads_and_advanced_fields() {
        let mut config = Config::default();
        let mut settings = Settings::default();
        settings.downloads.download_dir_override = Some("/custom/downloads".to_owned());
        settings.advanced.perf_metrics_enabled = true;
        settings.advanced.perf_format = "json".to_owned();
        settings.advanced.perf_output_path = Some("/tmp/perf.jsonl".to_owned());
        settings.advanced.extra_blocklist_path = Some("/etc/velox/extra.txt".to_owned());
        config.apply_settings(&settings);
        assert_eq!(
            config.download_dir_override,
            Some("/custom/downloads".to_owned())
        );
        assert!(config.perf_metrics);
        assert_eq!(config.perf_format, PerfFormat::Json);
        assert_eq!(config.perf_output_path, Some("/tmp/perf.jsonl".to_owned()));
        assert_eq!(
            config.extra_blocklist_path,
            Some("/etc/velox/extra.txt".to_owned())
        );
    }

    #[test]
    fn apply_settings_text_perf_format_for_anything_other_than_json() {
        let mut config = Config {
            perf_format: PerfFormat::Json,
            ..Config::default()
        };
        let mut settings = Settings::default();
        settings.advanced.perf_format = "text".to_owned();
        config.apply_settings(&settings);
        assert_eq!(config.perf_format, PerfFormat::Text);
    }

    // --- Config::to_settings (Issue #30, D67) -----------------------------
    //
    // The bug these guard against: an env-var/CLI-driven `Config` must
    // survive being round-tripped through the settings screen's seed step
    // (`app::run` calls this only when no `settings.json` exists yet) —
    // otherwise the very first "設定を開いて何も変えず保存" (open settings,
    // change nothing, save) would silently reset every `VELOX_*` override
    // on the next launch. This is exactly what broke this project's own
    // integration test suite (env vars like `VELOX_RESTORE_SESSION`/
    // `VELOX_MAX_LIVE_TABS`/`VELOX_PERF_METRICS` going inert) before
    // `app::run` was fixed to call `to_settings` instead of
    // `Settings::default()` when no settings.json exists.

    #[test]
    fn to_settings_on_a_default_config_matches_settings_default() {
        // The other half of `apply_settings_with_default_settings_leaves_
        // config_at_its_own_defaults` above: a fresh `Config` must seed a
        // `Settings` indistinguishable from `Settings::default()` (modulo
        // `appearance`, which has no `Config` equivalent and is asserted
        // separately below), so opening the settings screen on a totally
        // fresh checkout shows exactly what it always has.
        let settings = Config::default().to_settings();
        assert_eq!(settings.general, Settings::default().general);
        assert_eq!(settings.search, Settings::default().search);
        assert_eq!(settings.privacy, Settings::default().privacy);
        assert_eq!(settings.performance, Settings::default().performance);
        assert_eq!(settings.downloads, Settings::default().downloads);
        assert_eq!(settings.advanced, Settings::default().advanced);
        assert_eq!(settings.appearance, AppearanceSettings::default());
    }

    #[test]
    fn to_settings_round_trips_back_through_apply_settings() {
        // For every field `apply_settings` actually copies, `Config ->
        // to_settings -> apply_settings` must be a no-op — the property
        // that keeps "open settings, save without changing anything" safe
        // for a `Config` built from arbitrary env vars/CLI flags, not just
        // the default one.
        let original = Config {
            homepage: "https://example.com/".to_owned(),
            restore_previous_session: true,
            search_engine: SearchEngine::bing(),
            content_blocking_enabled: false,
            content_blocking_site_exceptions: vec!["a.example".to_owned(), "b.example".to_owned()],
            max_tabs_per_web_process: 8,
            suspension: SuspensionPolicy {
                idle_after: Some(Duration::from_secs(45)),
                max_live_tabs: Some(6),
                memory_budget_bytes: Some(512 * 1024 * 1024),
                memory_check_interval: Duration::from_secs(3),
            },
            download_dir_override: Some("/custom/downloads".to_owned()),
            perf_metrics: true,
            perf_format: PerfFormat::Json,
            perf_output_path: Some("/tmp/perf.jsonl".to_owned()),
            extra_blocklist_path: Some("/etc/velox/extra.txt".to_owned()),
            ..Config::default()
        };
        let settings = original.to_settings();
        let mut round_tripped = Config::default();
        round_tripped.apply_settings(&settings);

        assert_eq!(round_tripped.homepage, original.homepage);
        assert_eq!(
            round_tripped.restore_previous_session,
            original.restore_previous_session
        );
        assert_eq!(round_tripped.search_engine, original.search_engine);
        assert_eq!(
            round_tripped.content_blocking_enabled,
            original.content_blocking_enabled
        );
        assert_eq!(
            round_tripped.content_blocking_site_exceptions,
            original.content_blocking_site_exceptions
        );
        assert_eq!(
            round_tripped.max_tabs_per_web_process,
            original.max_tabs_per_web_process
        );
        assert_eq!(round_tripped.suspension, original.suspension);
        assert_eq!(
            round_tripped.download_dir_override,
            original.download_dir_override
        );
        assert_eq!(round_tripped.perf_metrics, original.perf_metrics);
        assert_eq!(round_tripped.perf_format, original.perf_format);
        assert_eq!(round_tripped.perf_output_path, original.perf_output_path);
        assert_eq!(
            round_tripped.extra_blocklist_path,
            original.extra_blocklist_path
        );
    }

    #[test]
    fn to_settings_detects_every_built_in_search_engine_preset() {
        for (engine, preset) in [
            (SearchEngine::duckduckgo(), "duckduckgo"),
            (SearchEngine::google(), "google"),
            (SearchEngine::bing(), "bing"),
            (SearchEngine::startpage(), "startpage"),
            (SearchEngine::ecosia(), "ecosia"),
        ] {
            let config = Config {
                search_engine: engine,
                ..Config::default()
            };
            let settings = config.to_settings();
            assert_eq!(settings.search.engine_preset, preset, "preset was {preset}");
            assert_eq!(settings.search.custom_engine_name, "");
            assert_eq!(settings.search.custom_engine_url, "");
        }
    }

    #[test]
    fn to_settings_round_trips_a_custom_search_engine() {
        let config = Config {
            search_engine: SearchEngine {
                name: "My Engine".to_owned(),
                query_template: "https://example.com/search?q={}".to_owned(),
            },
            ..Config::default()
        };
        let settings = config.to_settings();
        assert_eq!(settings.search.engine_preset, "custom");
        assert_eq!(settings.search.custom_engine_name, "My Engine");
        assert_eq!(
            settings.search.custom_engine_url,
            "https://example.com/search?q={}"
        );
    }

    #[test]
    fn to_settings_is_already_sanitized() {
        let settings = Config::default().to_settings();
        assert_eq!(settings.clone().sanitize(), settings);
    }
}
