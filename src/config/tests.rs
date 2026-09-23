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
    assert!(resolve_private(false, &list));
    assert_eq!(
        resolve_homepage(None, &list, DEFAULT_HOME),
        "https://a.example/"
    );
}

#[test]
fn resolve_private_is_false_with_no_flag_or_env() {
    assert!(!resolve_private(false, &[]));
    assert!(!resolve_private(
        false,
        &args(&["--homepage", "https://a.example/"])
    ));
}

#[test]
fn resolve_private_true_from_env_flag() {
    assert!(resolve_private(true, &[]));
}

#[test]
fn resolve_private_true_from_cli_flag() {
    assert!(resolve_private(false, &args(&["--private"])));
    assert!(resolve_private(false, &args(&["-x", "--private"])));
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

const GIB: u64 = 1024 * 1024 * 1024;

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

/// [`Config::apply_settings`] が書き込む (= `Settings` と往復する) フィールド
/// がすべて一致することを検査する。`Config` は `PartialEq` を持たないので
/// フィールドごとに比べる。
#[track_caller]
fn assert_settings_backed_fields_eq(actual: &Config, expected: &Config) {
    assert_eq!(actual.homepage, expected.homepage);
    assert_eq!(
        actual.restore_previous_session,
        expected.restore_previous_session
    );
    assert_eq!(actual.search_engine, expected.search_engine);
    assert_eq!(
        actual.content_blocking_enabled,
        expected.content_blocking_enabled
    );
    assert_eq!(
        actual.content_blocking_site_exceptions,
        expected.content_blocking_site_exceptions
    );
    assert_eq!(
        actual.max_tabs_per_web_process,
        expected.max_tabs_per_web_process
    );
    assert_eq!(actual.suspension, expected.suspension);
    assert_eq!(actual.download_dir_override, expected.download_dir_override);
    assert_eq!(actual.perf_metrics, expected.perf_metrics);
    assert_eq!(actual.perf_format, expected.perf_format);
    assert_eq!(actual.perf_output_path, expected.perf_output_path);
    assert_eq!(actual.extra_blocklist_path, expected.extra_blocklist_path);
}

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
    assert_settings_backed_fields_eq(&config, &before);
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
    assert_settings_backed_fields_eq(&round_tripped, &original);
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

// -- suspend_mechanism (Issue #243) -----------------------------------

#[test]
fn background_memory_target_defaults_to_low_since_d123() {
    // Unlike `suspend_mechanism` below, this default *did* move: §39
    // measured it with the budget on and it beat the old default on
    // every axis, so D123 flipped it. `normal` is the opt-out.
    assert_eq!(
        Config::default().background_memory_target,
        BackgroundMemoryTarget::Low
    );
}

#[test]
fn suspend_mechanism_defaults_to_the_pre_243_behavior() {
    // The knob exists to *measure* `Freeze`, not to ship it: nothing had
    // measured how much memory it returns when it was added (D120 決定3),
    // so an unset `VELOX_SUSPEND_MECHANISM` must keep discarding.
    assert_eq!(
        Config::default().suspend_mechanism,
        SuspendMechanism::Discard
    );
}

// -- Issue #176 Stage 3 (D151): VELOX_MEMORY_BUDGET_INPUT ----------

#[test]
fn memory_budget_input_defaults_to_private_commit_since_d152() {
    // Same shape as `background_memory_target`: the knob measured the
    // other arm first (D151), and the A/B (§47.10) moved the default.
    // `resident` is the opt-out, and the reading §46〜§47.9 were taken
    // with.
    assert_eq!(
        Config::default().memory_budget_input,
        MemoryBudgetInput::PrivateCommit
    );
}

// -- Issue #272 (D142): VELOX_PROTECT_FORM_INPUT -------------------

#[test]
fn form_input_protection_is_on_unless_explicitly_turned_off() {
    // Unset, and every recognized spelling of "off".
    assert!(resolve_protect_form_input(None));
    for off in ["0", "off", "false", "no", " Off ", "FALSE"] {
        assert!(
            !resolve_protect_form_input(Some(off)),
            "{off:?} should turn the protection off"
        );
    }
    // Explicit "on" spellings, and — the point of the conservative
    // rule — anything unrecognized. A typo must not silently pick the
    // arm that can lose what the user typed.
    for on in ["1", "on", "true", "yes", "", "offf", "disable", "0 0"] {
        assert!(
            resolve_protect_form_input(Some(on)),
            "{on:?} should leave the protection on"
        );
    }
}
