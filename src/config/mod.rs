//! Startup configuration for VeloX.
//!
//! Kept as a plain struct so that a config file / CLI flags can be layered on
//! later without touching the rest of the code.

use std::time::Duration;

use crate::browser::metrics::PerfFormat;
use crate::browser::navigation;
use crate::browser::suspension::{
    BackgroundMemoryTarget, MemoryBudgetInput, SuspendMechanism, SuspensionPolicy,
};

mod search_engine;

pub use search_engine::SearchEngine;

/// Default interval between process-tree RSS samples when performance
/// metrics are enabled but no explicit interval was requested.
const DEFAULT_PERF_RSS_INTERVAL: Duration = Duration::from_millis(5000);

/// Default for [`Config::max_tabs_per_web_process`] — D54's original
/// compile-time constant, kept as the default so behavior is unchanged
/// unless someone sets `VELOX_MAX_TABS_PER_PROCESS`.
const DEFAULT_MAX_TABS_PER_WEB_PROCESS: usize = 4;

/// MiB とバイトの換算係数。メモリ予算は設定・環境変数では MiB、
/// [`SuspensionPolicy`] ではバイトで持つ。
const BYTES_PER_MIB: u64 = 1024 * 1024;

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
    /// **How** a suspended tab's memory is reclaimed (Issue #243) — the
    /// policy above decides *which* tabs, this decides what is done to them.
    ///
    /// Defaults to [`SuspendMechanism::Discard`], the only thing VeloX did
    /// before #243: throw the webview away. `VELOX_SUSPEND_MECHANISM=freeze`
    /// switches to `ICoreWebView2_3::TrySuspend` on Windows, which keeps the
    /// page's state and makes coming back a `Resume` instead of a rebuild.
    ///
    /// **This is a measurement knob, not a recommendation — and the
    /// measurement came back against `Freeze`.** At 20 tabs it used 1.888×
    /// the memory of `Discard`, because `TrySuspend` suspends the renderer
    /// instead of ending it (docs/decisions.md D121,
    /// `docs/performance-targets.md` §37). It did cut `tab_resume_ms` from
    /// 117.5 to 6.25 ms, which is why the knob still exists (D121 決定2),
    /// but the default must stay `Discard`.
    pub suspend_mechanism: SuspendMechanism,
    /// What a **background but still awake** tab is told about memory
    /// (Issue #242) — a different axis from `suspend_mechanism` above, which
    /// only concerns tabs the policy picked for reclaim. This hint applies to
    /// every tab that is merely off screen, and with the memory budget off it
    /// is the *only* thing acting on them.
    ///
    /// **Defaults to `Low` since docs/decisions.md D123**; set
    /// `VELOX_BACKGROUND_MEMORY_TARGET=normal` to opt out. On Windows this
    /// becomes `ICoreWebView2_19::SetMemoryUsageTargetLevel(LOW)` for every
    /// tab that leaves the screen; elsewhere it is inert.
    ///
    /// Measured with the budget *on* — the configuration users actually run
    /// — it lands at 0.814× / 0.647× of the old default at 20 tabs
    /// (`docs/performance-targets.md` §39), and the footprint stays so far
    /// under `suspension`'s budget that **nothing is ever suspended**
    /// (§39.1). Suspension is now the safety net for workloads the hint
    /// cannot carry rather than the everyday mechanism (D123 決定2).
    pub background_memory_target: BackgroundMemoryTarget,
    /// Which memory figure `suspension`'s memory budget is compared against
    /// (Issue #176 Stage 3, docs/decisions.md D151). Only Windows has more
    /// than one to choose from today (working set vs. private commit,
    /// D150); elsewhere both spellings read the same PSS/RSS.
    ///
    /// **Defaults to [`MemoryBudgetInput::PrivateCommit`] since D152**;
    /// `VELOX_MEMORY_BUDGET_INPUT=resident` is the opt-out (the pre-D151
    /// behavior, the working set on Windows). `docs/performance-targets.md`
    /// §47.9 found that the working set grows for ~75 s after a renderer
    /// starts without the process allocating anything, and that the budget
    /// was discarding tabs to pay for that; the A/B in §47.10 then showed
    /// the private-commit input converging while the tabs were still being
    /// opened and ordering nothing afterwards (20 tabs suspended instead of
    /// 28, no "揺り戻し").
    pub memory_budget_input: MemoryBudgetInput,
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
    /// Whether a tab whose page reported form input is kept out of the
    /// automatic-suspension candidates (Issue #272, docs/decisions.md
    /// D142). **Defaults to on**: suspending a tab the user is typing in
    /// destroys what they wrote (D105), and that is the expensive way to
    /// be wrong.
    ///
    /// `VELOX_PROTECT_FORM_INPUT=0` turns it off. That exists so #272's
    /// measurement can run both arms — D142 決定3 recorded that how much
    /// this protection costs in suspensions (and so in the memory effect
    /// D114/§35 measured) is *not* yet measured. The injected script runs
    /// in both arms either way, so the arms differ in the policy and not
    /// in what the page is doing.
    pub protect_form_input: bool,
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
            suspend_mechanism: SuspendMechanism::default(),
            background_memory_target: BackgroundMemoryTarget::default(),
            memory_budget_input: MemoryBudgetInput::default(),
            private: false,
            search_engine: SearchEngine::default(),
            perf_metrics: false,
            perf_rss_interval: None,
            perf_format: PerfFormat::Text,
            perf_output_path: None,
            restore_previous_session: false,
            protect_form_input: true,
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
    /// - `VELOX_SUSPEND_MECHANISM` — how a suspended tab's memory is
    ///   reclaimed (Issue #243): `discard` throws the webview away (the
    ///   default, and everything VeloX did before #243), `freeze` asks the
    ///   engine to suspend it in place, keeping the page's state
    ///   (`ICoreWebView2_3::TrySuspend`, Windows only). Unset or an
    ///   unrecognized spelling keeps `discard`. On a platform without a
    ///   freeze path, and for any tab the engine refuses to freeze, VeloX
    ///   falls back to `discard` for that tab so it is still suspended.
    /// - `VELOX_BACKGROUND_MEMORY_TARGET` — what a background but still
    ///   awake tab is told about memory (Issue #242): `low` asks the engine
    ///   to economize (`ICoreWebView2_19::SetMemoryUsageTargetLevel`, Windows
    ///   only) and is **the default since D123**; `normal` says nothing,
    ///   which is what VeloX did before #242. A different axis from
    ///   `VELOX_SUSPEND_MECHANISM`: this applies to tabs that are merely off
    ///   screen, suspended or not. Unset or an unrecognized spelling keeps
    ///   the default (`low`).
    /// - `VELOX_MEMORY_BUDGET_INPUT` — which memory figure the memory
    ///   budget below is compared against (Issue #176 Stage 3, D151/D152):
    ///   `private` is the private commit on Windows (`PagefileUsage`, D150)
    ///   and **the default since D152** (§47.10); `resident` is PSS on
    ///   Linux and the working set on Windows — everything VeloX did before
    ///   D151, kept as the opt-out. The two are identical everywhere but
    ///   Windows. Unset or an unrecognized spelling keeps `private`.
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
    /// - `VELOX_PROTECT_FORM_INPUT` — `0`/`off`/`false` stops a tab whose
    ///   page reported form input from being protected from automatic
    ///   suspension (Issue #272, D142). **Defaults to on**; this exists to
    ///   run the other arm of #272's measurement.
    ///
    /// No CLI-parsing crate is introduced for this (see docs/decisions.md
    /// D6); `args` is expected to be the process arguments with argv\[0\]
    /// already stripped (e.g. `std::env::args().skip(1)`).
    pub fn from_env_and_args<I: IntoIterator<Item = String>>(args: I) -> Self {
        // Collected once: both `resolve_private` and `resolve_homepage` need
        // to walk the arguments.
        let args: Vec<String> = args.into_iter().collect();
        let private = resolve_private(env_flag("VELOX_PRIVATE"), &args);
        let defaults = Self::default();
        let homepage = resolve_homepage(
            env_var("VELOX_HOMEPAGE").as_deref(),
            &args,
            &defaults.homepage,
        );
        let metrics_requested = env_flag("VELOX_PERF_METRICS");
        let (perf_metrics, perf_rss_interval) = resolve_perf_env(
            metrics_requested,
            env_var("VELOX_PERF_RSS_INTERVAL_MS").as_deref(),
        );
        let (perf_format, perf_output_path) = resolve_perf_output(
            metrics_requested,
            env_var("VELOX_PERF_FORMAT").as_deref(),
            env_var("VELOX_PERF_OUTPUT").as_deref(),
        );
        let search_engine = resolve_search_engine(
            env_var("VELOX_SEARCH_ENGINE").as_deref(),
            env_var("VELOX_SEARCH_ENGINE_NAME").as_deref(),
            env_var("VELOX_SEARCH_ENGINE_URL").as_deref(),
        );
        let max_tabs_per_web_process =
            resolve_max_tabs_per_web_process(env_var("VELOX_MAX_TABS_PER_PROCESS").as_deref());
        let suspension = resolve_suspension(
            env_var("VELOX_AUTO_SUSPEND_AFTER_MS").as_deref(),
            env_var("VELOX_MAX_LIVE_TABS").as_deref(),
            env_var("VELOX_MEMORY_BUDGET_MB").as_deref(),
            env_var("VELOX_MEMORY_CHECK_INTERVAL_MS").as_deref(),
            crate::browser::metrics::installed_ram_bytes(),
        );
        let content_blocking_site_exceptions = resolve_content_blocking_site_exceptions(
            env_var("VELOX_CONTENT_BLOCKING_ALLOW").as_deref(),
        );
        // Issue #243. Unrecognized spellings keep the default rather than
        // erroring: a typo must never silently pick a mechanism the
        // measurements never covered (the same rule as every knob above).
        let suspend_mechanism = env_parsed("VELOX_SUSPEND_MECHANISM", SuspendMechanism::parse);
        // Issue #242. Same conservative rule as `suspend_mechanism` above:
        // an unrecognized spelling keeps the default rather than erroring.
        let background_memory_target = env_parsed(
            "VELOX_BACKGROUND_MEMORY_TARGET",
            BackgroundMemoryTarget::parse,
        );
        // Issue #176 Stage 3 (D151/D152). Same conservative rule again: a
        // typo keeps the measured default rather than picking an arm.
        let memory_budget_input = env_parsed("VELOX_MEMORY_BUDGET_INPUT", MemoryBudgetInput::parse);
        let restore_previous_session = env_flag("VELOX_RESTORE_SESSION");
        let protect_form_input =
            resolve_protect_form_input(env_var("VELOX_PROTECT_FORM_INPUT").as_deref());
        Self {
            homepage,
            private,
            search_engine,
            max_tabs_per_web_process,
            suspension,
            suspend_mechanism,
            background_memory_target,
            memory_budget_input,
            perf_metrics,
            perf_rss_interval,
            perf_format,
            perf_output_path,
            content_blocking_site_exceptions,
            restore_previous_session,
            protect_form_input,
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
                .map(|mib| mib.saturating_mul(BYTES_PER_MIB)),
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
            match self.search_engine.preset_key() {
                Some(preset) => (preset.to_owned(), String::new(), String::new()),
                None => (
                    "custom".to_owned(),
                    self.search_engine.name.clone(),
                    self.search_engine.query_template.clone(),
                ),
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
                    .map(|bytes| bytes / BYTES_PER_MIB),
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
            return SearchEngine::new(name, template);
        }
    }
    preset_raw
        .and_then(SearchEngine::preset)
        .unwrap_or_default()
}

/// Whether form-input protection stays on, from `VELOX_PROTECT_FORM_INPUT`
/// (Issue #272, docs/decisions.md D142).
///
/// **Only an explicit, recognized "off" turns it off.** Unset keeps it on,
/// and so does an unrecognized spelling — the same conservative rule every
/// other knob here follows (`VELOX_SUSPEND_MECHANISM`, `VELOX_BACKGROUND_
/// MEMORY_TARGET`): a typo must never silently pick the arm that can lose
/// the user's typing. Case and surrounding whitespace are ignored, since
/// `VELOX_PROTECT_FORM_INPUT=Off` from a shell script is plainly "off" and
/// treating it as a typo would be the wrong kind of strict.
fn resolve_protect_form_input(raw: Option<&str>) -> bool {
    !matches!(
        raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "off" | "false" | "no")
    )
}

/// Whether private browsing should be enabled given the raw ingredients
/// (environment variable presence, CLI args). Kept separate from
/// `Config::from_env_and_args` so the decision logic is testable without
/// touching the real process environment.
fn resolve_private(env_flag_set: bool, args: &[String]) -> bool {
    env_flag_set || args.iter().any(|arg| arg == "--private")
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
    parse_trimmed::<usize>(raw)
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
        match parse_trimmed::<u64>(raw) {
            None => default,
            Some(0) => None,
            Some(value) => Some(value),
        }
    }
    fn positive(raw: Option<&str>) -> Option<u64> {
        parse_trimmed::<u64>(raw).filter(|value| *value > 0)
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
                .map(|bytes| bytes / BYTES_PER_MIB),
        )
        .map(|mib| mib.saturating_mul(BYTES_PER_MIB)),
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

/// 前後の空白を除いてから数値として読む。未設定・空・数値でないものは
/// すべて `None` — 各 `resolve_*` の「読めない値は未設定と同じ」規則の
/// 共通部分。
fn parse_trimmed<T: std::str::FromStr>(raw: Option<&str>) -> Option<T> {
    raw.map(str::trim).and_then(|value| value.parse().ok())
}

/// 環境変数の値。未設定 (と UTF-8 でない値) は `None`。
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// 値を問わず、環境変数が設定されているか (`VELOX_PRIVATE` などの
/// 「存在すればオン」型のフラグ)。
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// 列挙型のノブを `parse` で読む。未設定や認識できない綴りは型の既定値に
/// 落とす — 打ち間違いで計測していない選択肢に切り替わらないための、
/// 各ノブ共通の保守的な規則。
fn env_parsed<T: Default>(name: &str, parse: impl FnOnce(&str) -> Option<T>) -> T {
    env_var(name).as_deref().and_then(parse).unwrap_or_default()
}

#[cfg(test)]
mod tests;
