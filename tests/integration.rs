//! Integration tests (Issue #34).
//!
//! Every other test in this crate (`cargo test`'s ~470+ unit tests) lives
//! under `src/browser/` and exercises pure logic in-process — none of them
//! ever launches the real `velox` binary. That gap is exactly what let
//! Issue #72 slip through: all unit tests green, while CI's VeloX never
//! got past `BrowserWindow::new` (no D-Bus session bus for WebKitGTK's web
//! process — see docs/decisions.md D46). This file is the fix: it spawns
//! the compiled `velox` binary and checks it from the outside, the way a
//! user (or `velox-bench`) actually would.
//!
//! **Driving mechanism**: `VELOX_AUTOMATION_SCRIPT` (Issue #112,
//! docs/decisions.md D44) — the file-driven automation hook `velox-bench`
//! already uses. These tests write their own small scripts in the
//! documented `open`/`switch`/`close`/`navigate`/`wait`/`quit` format and
//! hand them to `velox` the exact same way. No new control channel is
//! introduced — see D44 for why a listening socket/RPC server was
//! deliberately rejected as VeloX's automation mechanism, and D47 for why
//! these tests reuse it rather than inventing a second one.
//!
//! **Preflight / skipping**: every test starts by calling
//! `skip_without_gui!()`, which checks the real process environment
//! (`DISPLAY`/`WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS` on Linux)
//! through `velox::browser::gui_probe::gui_probe_reason` — a pure decision
//! function, unit-tested on its own in `src/browser/gui_probe.rs` — and
//! prints why (to stdout) and returns (a pass, not a failure/ignore) when
//! this environment cannot even attempt to launch a window. A developer
//! machine with no X session, and any GUI-less CI job, must see `cargo
//! test` stay green. See docs/decisions.md D47 for the full reasoning,
//! including why this is a *runtime* check rather than `#[ignore]` (a
//! fixed annotation cannot see whether Xvfb/D-Bus happen to be available
//! this run).
//!
//! **Fixed test pages**: `scripts/bench/pages/*.html`, loaded via
//! `file://` URLs — no network, and no loopback HTTP server means no port
//! for tests running concurrently to collide over (`cargo test` runs
//! `#[test]` functions in parallel by default). `GUI_TEST_LOCK` below
//! still serializes the tests in this file against each other, purely to
//! avoid piling up several concurrent WebKitGTK processes in a
//! resource-constrained container — not for correctness.
//!
//! **What these tests do *not* claim**: they are an external, black-box
//! smoke/regression layer, not a substitute for the unit tests under
//! `src/browser/`. They do not exercise every automation command, every
//! perf metric, or UI-level correctness (toolbar rendering, omnibox
//! ranking, …) — see docs/architecture.md's "Test strategy" section for
//! the intended division of labor.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use velox::browser::benchmark::{aggregate_trials, parse_jsonl};
use velox::browser::{gui_probe_reason, navigation, persistence};

/// Serializes the GUI-launching tests in this file against each other —
/// not a correctness requirement (each test uses its own temp dir and its
/// own `velox` process, never a shared port), just a way to avoid running
/// several WebKitGTK instances at once in a constrained container.
static GUI_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Skip the calling test (printing why, to stdout, and returning — a
/// pass) when this environment cannot attempt to launch a GUI window. See
/// this file's module doc comment and docs/decisions.md D47.
///
/// **Unless `VELOX_INTEGRATION_REQUIRE_GUI` is set**, in which case a skip
/// becomes a hard failure instead. A skip is reported by `cargo test` as an
/// ordinary pass, which is exactly right on a developer machine with no X
/// session — and exactly wrong in CI, where it would mean this whole file
/// quietly stops testing anything while the job stays green. That is the
/// same shape of silent, green-but-untested failure as #72 itself (all unit
/// tests passing while `velox` never started), so the CI job that provides
/// `xvfb-run`/`dbus-run-session` also sets this variable: if that setup is
/// ever removed or breaks, CI fails loudly rather than skipping.
macro_rules! skip_without_gui {
    ($test_name:literal) => {
        if let Some(reason) = gui_skip_reason() {
            assert!(
                !env_is_set("VELOX_INTEGRATION_REQUIRE_GUI"),
                "VELOX_INTEGRATION_REQUIRE_GUI が設定されているのに GUI を起動できません \
                 ({}): {reason}。この環境は統合テストを実行する前提で構成されている \
                 はずです — xvfb-run / dbus-run-session の設定を確認してください \
                 (docs/decisions.md D47)。",
                $test_name,
            );
            println!("skip: {} — {reason}", $test_name);
            return;
        }
    };
}

/// The Linux-specific environment signals `gui_probe_reason` needs;
/// unconditionally `None` (never skip) on macOS/Windows, which need
/// neither an X display nor a D-Bus session bus to run a normal desktop
/// application.
fn gui_skip_reason() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        let has_display = env_is_set("DISPLAY") || env_is_set("WAYLAND_DISPLAY");
        let has_dbus_session = env_is_set("DBUS_SESSION_BUS_ADDRESS");
        gui_probe_reason(has_display, has_dbus_session)
    } else {
        None
    }
}

fn env_is_set(key: &str) -> bool {
    env::var_os(key).is_some_and(|value| !value.is_empty())
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

/// A fresh, empty directory under the OS temp dir, unique to this test
/// process + label + timestamp, so parallel test runs (and repeated local
/// runs) never collide. Used both as the automation-script/perf-output
/// location and (in a `data` subdirectory) as `VELOX_DATA_DIR`.
fn unique_dir(label: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "velox-integration-test-{}-{label}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create a temp directory for an integration test");
    dir
}

/// `file://` URL for a fixed page under `scripts/bench/pages/`, normalized
/// through the exact same `browser::navigation::normalize_input` the
/// address bar and `VELOX_HOMEPAGE` already go through — so it matches,
/// character for character, whatever `velox` itself ends up recording
/// (e.g. in `history.json`) for the same page.
fn fixture_url(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/bench/pages")
        .join(name);
    assert!(
        path.is_file(),
        "fixture page not found: {} (expected under scripts/bench/pages/)",
        path.display()
    );
    let raw = format!("file://{}", path.display());
    navigation::normalize_input(&raw)
        .unwrap_or_else(|| panic!("fixture URL failed to normalize: {raw:?}"))
}

fn write_script(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("script.txt");
    fs::write(&path, contents).expect("write automation script file");
    path
}

/// What one `launch_and_wait` call observed.
struct Launch {
    /// Every `PerfRecord` line `velox` wrote to `VELOX_PERF_OUTPUT`,
    /// parsed via the same `benchmark::parse_jsonl` `velox-bench` uses.
    perf_records: Vec<serde_json::Value>,
    /// `Some` only if the process exited *on its own* within the timeout
    /// (observed via `Child::try_wait`, never forced). `None` means the
    /// timeout elapsed and this helper killed it — callers must treat that
    /// as a hard failure, not a soft "it eventually went away": see this
    /// file's module doc comment and the #72 regression it exists to
    /// catch.
    exit_status: Option<ExitStatus>,
}

/// Launch `velox` with performance metrics on, `VELOX_DATA_DIR` pointed at
/// `data_dir`, `VELOX_HOMEPAGE` set to `homepage`, and
/// `VELOX_AUTOMATION_SCRIPT` pointed at `script_path`; wait up to `timeout`
/// for it to exit on its own.
fn launch_and_wait(
    perf_output: &Path,
    data_dir: &Path,
    homepage: &str,
    script_path: &Path,
    timeout: Duration,
) -> Launch {
    launch_and_wait_with(
        perf_output,
        data_dir,
        homepage,
        script_path,
        timeout,
        &[],
        None,
    )
}

/// [`launch_and_wait`], plus `extra_env` (additional environment variables
/// for the child, e.g. `VELOX_DOWNLOAD_DIR`) and, when `stderr_path` is
/// `Some`, the child's stderr redirected to that file so a test can assert
/// on `velox: ...` log lines afterwards. A file rather than a pipe: nothing
/// here reads the pipe while the child runs, so a chatty WebKitGTK could
/// otherwise fill it and block the child forever.
fn launch_and_wait_with(
    perf_output: &Path,
    data_dir: &Path,
    homepage: &str,
    script_path: &Path,
    timeout: Duration,
    extra_env: &[(&str, &Path)],
    stderr_path: Option<&Path>,
) -> Launch {
    let velox_bin = env!("CARGO_BIN_EXE_velox");
    let mut command = Command::new(velox_bin);
    command
        .env("VELOX_PERF_METRICS", "1")
        .env("VELOX_PERF_FORMAT", "json")
        .env("VELOX_PERF_OUTPUT", perf_output)
        .env("VELOX_DATA_DIR", data_dir)
        .env("VELOX_HOMEPAGE", homepage)
        .env("VELOX_AUTOMATION_SCRIPT", script_path);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    if let Some(stderr_path) = stderr_path {
        let stderr_file = fs::File::create(stderr_path)
            .unwrap_or_else(|err| panic!("create {}: {err}", stderr_path.display()));
        command.stderr(stderr_file);
    }

    let mut child = command
        .spawn()
        .unwrap_or_else(|err| panic!("failed to launch {velox_bin}: {err}"));

    let exit_status = wait_for_exit(&mut child, timeout);
    if exit_status.is_none() {
        // Did not exit on its own within `timeout`. Kill it so this test
        // process itself does not hang forever, but this condition is
        // exactly the regression this test suite exists to catch — the
        // caller must fail the test, never quietly treat this as success.
        let _ = child.kill();
        let _ = child.wait();
    }

    let text = fs::read_to_string(perf_output).unwrap_or_default();
    Launch {
        perf_records: parse_jsonl(&text),
        exit_status,
    }
}

/// Poll `child` (never blocking indefinitely) until it exits on its own or
/// `timeout` elapses, whichever comes first. Mirrors
/// `src/bin/velox-bench.rs`'s `wait_for_exit_or_timeout`, except this one
/// reports which case happened (`velox-bench` never needs to — it always
/// force-terminates right after) so callers here can fail loudly on a
/// timeout instead of treating both cases alike.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn events_named<'a>(
    records: &'a [serde_json::Value],
    name: &'a str,
) -> impl Iterator<Item = &'a serde_json::Value> + 'a {
    records.iter().filter(move |record| record["event"] == name)
}

// ---------------------------------------------------------------------
// 1. Startup completes and is observable — the #72 regression itself.
// ---------------------------------------------------------------------

/// Guarantees: launching `velox` actually reaches the point of finishing
/// its first page load and reporting a `startup` perf record — not just
/// that the process exists, but that it made real progress past
/// `BrowserWindow::new`. If it hangs instead (#72's failure mode: no
/// crash, no perf line, ever, just silence), this test fails with an
/// explicit message rather than the suite quietly passing on 470+ unit
/// tests that never touched this path at all.
#[test]
fn startup_completes_and_records_a_startup_event() {
    skip_without_gui!("startup_completes_and_records_a_startup_event");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("startup");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let homepage = fixture_url("minimal.html");
    // Enough time for the first (tiny, local) page load to finish before
    // asking the process to quit.
    let script_path = write_script(&dir, "wait 1500\nquit\n");

    let launch = launch_and_wait(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(30),
    );

    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 30s after startup — this is the #72 \
             regression shape (window created, then unresponsive). Perf records observed \
             before the forced kill: {} ({:?})",
            launch.perf_records.len(),
            launch.perf_records
        );
    };
    assert!(status.success(), "velox exited abnormally: {status:?}");

    let startup_records: Vec<_> = events_named(&launch.perf_records, "startup").collect();
    assert_eq!(
        startup_records.len(),
        1,
        "expected exactly one `startup` perf record, got {}: {:?}",
        startup_records.len(),
        launch.perf_records
    );
    let startup = startup_records[0];
    for field in ["window_created_ms", "toolbar_ready_ms", "first_load_ms"] {
        let value = startup[field]
            .as_f64()
            .unwrap_or_else(|| panic!("startup record is missing `{field}`: {startup}"));
        assert!(
            value.is_finite() && value >= 0.0,
            "{field} = {value} is not sane"
        );
    }
}

// ---------------------------------------------------------------------
// 2. Tab operations reach real tab-management code.
// ---------------------------------------------------------------------

/// Guarantees: `VELOX_AUTOMATION_SCRIPT`'s `open`/`switch`/`close`
/// commands drive the *real* tab-management path
/// (`app::handle_automation_command` → `app::open_new_tab` /
/// `Tabs::activate_at` / `app::close_tab`) all the way through a live
/// window — not just that `browser::automation::parse_script` parses the
/// commands (already fully covered, without a display, by that module's
/// own unit tests). `tab_create`/`tab_switch` perf records are the
/// externally observable proof this actually happened.
#[test]
fn tab_operations_produce_expected_tab_create_and_tab_switch_records() {
    skip_without_gui!("tab_operations_produce_expected_tab_create_and_tab_switch_records");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("tabs");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let homepage = fixture_url("minimal.html");
    let page_a = fixture_url("text.html");
    let page_b = fixture_url("dom_heavy.html");

    // Tab strip after each step: [home] -> open a -> [home, a] (a active)
    // -> open b -> [home, a, b] (b active) -> switch 0/1/2 visits every
    // tab once more -> close 2 drops `b`.
    let script = format!(
        "open {page_a}\n\
         wait 400\n\
         open {page_b}\n\
         wait 400\n\
         switch 0\n\
         wait 300\n\
         switch 1\n\
         wait 300\n\
         switch 2\n\
         wait 300\n\
         close 2\n\
         wait 300\n\
         quit\n"
    );
    let script_path = write_script(&dir, &script);

    let launch = launch_and_wait(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(30),
    );
    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 30s while running the tab-operations \
             script. Perf records observed before the forced kill: {:?}",
            launch.perf_records
        );
    };
    assert!(status.success(), "velox exited abnormally: {status:?}");

    let tab_create: Vec<_> = events_named(&launch.perf_records, "tab_create").collect();
    let tab_switch: Vec<_> = events_named(&launch.perf_records, "tab_switch").collect();
    assert_eq!(
        tab_create.len(),
        2,
        "2 `open` commands should yield 2 `tab_create` records, got {}: {:?}",
        tab_create.len(),
        launch.perf_records
    );
    assert_eq!(
        tab_switch.len(),
        3,
        "3 `switch` commands should yield 3 `tab_switch` records, got {}: {:?}",
        tab_switch.len(),
        launch.perf_records
    );

    // Each `open` created a genuinely distinct tab (never id 0 twice, or
    // reusing the switched-to tab's id) — a loose sanity check that these
    // are real per-tab ids, not a metric that always reports the same
    // value regardless of which tab actually changed.
    let create_ids: HashSet<_> = tab_create
        .iter()
        .filter_map(|r| r["tab_id"].as_u64())
        .collect();
    assert_eq!(
        create_ids.len(),
        2,
        "expected 2 distinct tab_create ids, got {create_ids:?}"
    );
}

// ---------------------------------------------------------------------
// 2b. Multiple windows (Issue #29, see docs/decisions.md D68).
// ---------------------------------------------------------------------

/// Guarantees: `new_window` (`AutomationCommand::NewWindow`) drives the real
/// multi-window path end to end — a second, genuinely separate
/// `ui::window::BrowserWindow` gets built (`app::open_new_window`), and
/// every automation command after it (`open`/`switch`) is retargeted to
/// that new window's own `Tabs` rather than the first window's — not just
/// that `browser::automation::parse_script`/`browser::Windows` parse and
/// track this in isolation (already fully covered, without a display, by
/// their own unit tests). If retargeting silently failed (`automation_window`
/// never updated, or the new `BrowserWindow` never actually inserted into
/// `ui_windows`), the `open`/`switch` commands below would find no tab to
/// act on and produce zero `tab_create`/`tab_switch` records instead of
/// exactly one/two. The process exiting cleanly with two windows still open
/// (`quit` before either window is closed by hand) is this test's other
/// guarantee — the "終了時のリソース解放が正常" acceptance criterion:
/// nothing here should crash, hang, or leak the second window's resources
/// past the process ending.
#[test]
fn new_window_retargets_automation_and_shuts_down_cleanly() {
    skip_without_gui!("new_window_retargets_automation_and_shuts_down_cleanly");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("multi-window");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let homepage = fixture_url("minimal.html");
    let page_a = fixture_url("text.html");

    // Window 1 opens at `homepage` (tab 0). `new_window` opens window 2,
    // also at `homepage` (its own tab 0) — every command after this line
    // targets window 2, not window 1. `open` there creates window 2's tab
    // 1 (one `tab_create`); the two `switch`es revisit window 2's tab 0
    // and then tab 1 again (two `tab_switch`es). Window 1 is never touched
    // again and is still open when `quit` runs.
    let script = format!(
        "new_window\n\
         wait 500\n\
         open {page_a}\n\
         wait 400\n\
         switch 0\n\
         wait 300\n\
         switch 1\n\
         wait 300\n\
         quit\n"
    );
    let script_path = write_script(&dir, &script);

    let launch = launch_and_wait(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(30),
    );
    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 30s with two windows open — this is exactly \
             the kind of hang/leak the multi-window shutdown path (Issue #29) must not have. \
             Perf records observed before the forced kill: {:?}",
            launch.perf_records
        );
    };
    assert!(
        status.success(),
        "velox exited abnormally with two windows open: {status:?}"
    );

    let tab_create: Vec<_> = events_named(&launch.perf_records, "tab_create").collect();
    let tab_switch: Vec<_> = events_named(&launch.perf_records, "tab_switch").collect();
    assert_eq!(
        tab_create.len(),
        1,
        "the `open` after `new_window` should yield exactly 1 `tab_create` record \
         (window 2's second tab) — 0 would mean automation never actually retargeted to the \
         new window; got {}: {:?}",
        tab_create.len(),
        launch.perf_records
    );
    assert_eq!(
        tab_switch.len(),
        2,
        "the two `switch` commands after `new_window` should yield 2 `tab_switch` records \
         against window 2's tabs, got {}: {:?}",
        tab_switch.len(),
        launch.perf_records
    );

    // Exactly one `startup` record: multi-window does not change how the
    // *first* window's startup timing is measured (Issue #59/D43 is
    // unaffected — window 2 is not instrumented the same way, which is
    // fine, it is not the process's startup).
    let startup_records: Vec<_> = events_named(&launch.perf_records, "startup").collect();
    assert_eq!(
        startup_records.len(),
        1,
        "expected exactly one `startup` perf record even with two windows opened, got {}: {:?}",
        startup_records.len(),
        launch.perf_records
    );
}

// ---------------------------------------------------------------------
// 3. Visiting pages persists history.json.
// ---------------------------------------------------------------------

/// Guarantees: `VELOX_DATA_DIR` is honored end to end — a real page load
/// through a real content webview reaches
/// `app::record_visit_if_enabled`/`app::persist_history`, and the
/// resulting `history.json` is both present on disk and structurally
/// correct once read back through `browser::persistence::load_history`.
/// `src/browser/history.rs`'s own unit tests call `HistoryStore` directly
/// and never touch a filesystem or a webview, so they cannot exercise this
/// path at all.
#[test]
fn visiting_pages_persists_history_json() {
    skip_without_gui!("visiting_pages_persists_history_json");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("history");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let homepage = fixture_url("minimal.html");
    let second_page = fixture_url("text.html");

    // Wait for the homepage's own load to finish and be recorded before
    // navigating away, so both visits land as separate history entries in
    // a deterministic order.
    let script = format!("wait 1000\nnavigate {second_page}\nwait 700\nquit\n");
    let script_path = write_script(&dir, &script);

    let launch = launch_and_wait(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(30),
    );
    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 30s during the history-persistence test. \
             Perf records observed before the forced kill: {:?}",
            launch.perf_records
        );
    };
    assert!(status.success(), "velox exited abnormally: {status:?}");

    let history_path = data_dir.join("history.json");
    assert!(
        history_path.is_file(),
        "history.json was not written to {}",
        history_path.display()
    );

    let store = persistence::load_history(&data_dir);
    let urls: Vec<&str> = store
        .entries()
        .iter()
        .map(|entry| entry.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec![homepage.as_str(), second_page.as_str()],
        "history.json's entries did not match the two pages visited: {:?}",
        store.entries()
    );
    for entry in store.entries() {
        assert!(
            entry.visited_at > 0,
            "a history entry has visited_at == 0: {entry:?}"
        );
        assert_eq!(
            entry.visit_count, 1,
            "each URL was visited exactly once: {entry:?}"
        );
    }
}

// ---------------------------------------------------------------------
// 3b. Private windows (Issue #27, D74): a private window's page visits
//     never reach history.json, even while a normal window in the same
//     process keeps recording its own.
// ---------------------------------------------------------------------

/// Guarantees: `new_private_window` (`AutomationCommand::NewPrivateWindow`)
/// drives the real private-window path end to end — a second, genuinely
/// private `ui::window::BrowserWindow` gets built (`app::open_new_window`
/// with `private: true`) — and, critically, a page visited in *that* window
/// never reaches `history.json`, while a page visited beforehand in the
/// first (normal) window still does. This is the concrete, externally
/// observable version of the acceptance criterion "閲覧履歴がアプリ側に残らない"
/// (Issue #27): `record_visit_if_enabled`'s own unit tests already
/// cover the same logic against a bare `AppState` (see
/// `a_private_window_records_no_visit_while_a_normal_window_with_the_same_tab_id_still_does`
/// in `src/app.rs`), but only a real second `ui::window::BrowserWindow`
/// actually built with `.with_incognito(true)` and a real page load through
/// it proves the full path — construction, navigation, and
/// `UserEvent::LoadFinished` — wires up the same way outside of a unit test.
#[test]
fn a_private_windows_page_visit_never_reaches_history_json() {
    skip_without_gui!("a_private_windows_page_visit_never_reaches_history_json");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("private-window-history");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let homepage = fixture_url("minimal.html");
    let normal_page = fixture_url("text.html");
    let private_page = fixture_url("dom_heavy.html");

    // Window 1 (normal) opens at `homepage`, then navigates to
    // `normal_page` — both visits should land in history.json. Window 2,
    // opened private by `new_private_window`, then navigates to
    // `private_page` — that visit must never appear.
    let script = format!(
        "wait 1000\n\
         navigate {normal_page}\n\
         wait 700\n\
         new_private_window\n\
         wait 500\n\
         navigate {private_page}\n\
         wait 700\n\
         quit\n"
    );
    let script_path = write_script(&dir, &script);

    let launch = launch_and_wait(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(30),
    );
    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 30s with a private window open. Perf records \
             observed before the forced kill: {:?}",
            launch.perf_records
        );
    };
    assert!(
        status.success(),
        "velox exited abnormally with a private window open: {status:?}"
    );

    let history_path = data_dir.join("history.json");
    assert!(
        history_path.is_file(),
        "history.json was not written to {} (the normal window's visits should have created it)",
        history_path.display()
    );

    let store = persistence::load_history(&data_dir);
    let urls: Vec<&str> = store
        .entries()
        .iter()
        .map(|entry| entry.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec![homepage.as_str(), normal_page.as_str()],
        "history.json must contain only the normal window's two visits — the private window's \
         visit to {private_page:?} must never appear: {:?}",
        store.entries()
    );
    assert!(
        !urls.contains(&private_page.as_str()),
        "the private window's page visit leaked into history.json: {:?}",
        store.entries()
    );
}

// ---------------------------------------------------------------------
// 4. Downloads: one download → one panel entry, saved where VeloX says.
// ---------------------------------------------------------------------

/// Guarantees: with several tabs open on the shared `WebContext`
/// (docs/decisions.md D49), a download started from a page reaches
/// *VeloX's* download handler exactly once — not wry's do-nothing default
/// handler, and not N copies of VeloX's (see D53 for the bug this pins
/// down: before D53, on WebKitGTK, `UserEvent::DownloadStarted` never
/// fired at all and `DownloadCompleted` fired once per content webview
/// ever built).
///
/// Externally observable proof, without reaching into `DownloadStore`:
///
/// - The file lands in `VELOX_DOWNLOAD_DIR`, under the sanitized/
///   collision-checked name `browser::downloads::prepare_destination`
///   picks. Only VeloX's started handler honors that variable; wry's
///   default would have written into `dirs::download_dir()`/the current
///   directory instead. Two downloads of the same suggested name must
///   therefore yield exactly `velox-test.txt` and `velox-test (1).txt`.
/// - stderr carries no `could not correlate download completion` line:
///   `app.rs` logs that for every `DownloadCompleted` it cannot match to an
///   in-progress entry, which is exactly what each duplicate completion
///   (or a completion with no preceding `DownloadStarted`) produces.
#[test]
fn downloads_with_several_tabs_open_are_handled_exactly_once() {
    skip_without_gui!("downloads_with_several_tabs_open_are_handled_exactly_once");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("downloads");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let download_dir = dir.join("downloads");
    let stderr_path = dir.join("stderr.log");
    let homepage = fixture_url("minimal.html");
    let download_page = fixture_url("download.html");

    // Three tabs on the shared context (toolbar + 3 content webviews), then
    // the same download twice from the active tab, with a detour through
    // `minimal.html` in between so the second `navigate` is a real
    // navigation and not a same-URL no-op.
    let script = format!(
        "open {homepage}\nwait 1000\nopen {homepage}\nwait 1000\n\
         navigate {download_page}\nwait 2500\n\
         navigate {homepage}\nwait 700\n\
         navigate {download_page}\nwait 2500\nquit\n"
    );
    let script_path = write_script(&dir, &script);

    let launch = launch_and_wait_with(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(40),
        &[("VELOX_DOWNLOAD_DIR", download_dir.as_path())],
        Some(&stderr_path),
    );
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 40s during the downloads test. \
             stderr:\n{stderr}"
        );
    };
    assert!(
        status.success(),
        "velox exited abnormally: {status:?}\nstderr:\n{stderr}"
    );

    let mut names: Vec<String> = fs::read_dir(&download_dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", download_dir.display()))
        .map(|entry| {
            entry
                .expect("read dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["velox-test (1).txt".to_owned(), "velox-test.txt".to_owned()],
        "expected exactly the two downloads, saved under VELOX_DOWNLOAD_DIR with \
         prepare_destination's collision suffix; got {names:?}.\nstderr:\n{stderr}"
    );
    for name in &names {
        let content = fs::read_to_string(download_dir.join(name)).expect("read downloaded file");
        assert_eq!(
            content, "velox download test\n",
            "unexpected contents in {name}"
        );
    }

    let uncorrelated = stderr
        .lines()
        .filter(|line| line.contains("could not correlate download completion"))
        .count();
    assert_eq!(
        uncorrelated, 0,
        "every DownloadCompleted must match the one DownloadStarted that preceded it — \
         {uncorrelated} uncorrelated completion(s) means duplicate or orphaned completion \
         events (D53).\nstderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------
// 5. `quit` ends the process on its own, exit code 0.
// ---------------------------------------------------------------------

/// Guarantees: the `quit` automation command genuinely ends the process
/// itself (`ControlFlow::Exit` in `app::run`'s event loop — see
/// docs/decisions.md D44) with a clean exit status, distinct from this
/// test file's own timeout-then-kill fallback. Checking
/// `exit_status.is_some()` — not just `.success()` — is what actually
/// distinguishes "quit worked" from "the timeout fallback silently killed
/// it and the test passed anyway"; a `quit` regressed into a no-op would
/// still eventually make `wait_for_exit` return `None`, which is treated
/// as a hard failure here (see `launch_and_wait`'s doc comment), not a
/// soft pass. The Unix-only signal check further rules out "successfully
/// killed" masquerading as "successfully exited".
#[test]
fn quit_command_exits_the_process_with_code_zero() {
    skip_without_gui!("quit_command_exits_the_process_with_code_zero");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("quit");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let homepage = fixture_url("minimal.html");
    let script_path = write_script(&dir, "wait 200\nquit\n");

    let launch = launch_and_wait(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(20),
    );

    let status = launch.exit_status.unwrap_or_else(|| {
        panic!(
            "quit did not make velox exit on its own within 20s (perf records observed: {:?})",
            launch.perf_records
        )
    });
    assert!(
        status.success(),
        "exit status after quit was not success: {status:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            None,
            "process ended via signal, not a clean self-exit: {status:?}"
        );
    }
}

// ---------------------------------------------------------------------
// 6. Adaptive tab suspension (Issue #63): the live-tab cap suspends
//    background tabs, and switching back to one resumes it.
// ---------------------------------------------------------------------

/// Guarantees: with `VELOX_MAX_LIVE_TABS=2`, opening a third tab makes
/// the automatic policy (`browser::suspension`, docs/decisions.md D56)
/// suspend the least recently used background tab — observable from the
/// outside as a `tab_suspend` perf record with `reason: "tab_count"` —
/// and a later `switch` to a suspended tab is reported as `tab_resume`
/// (not `tab_switch`), followed by that tab's page reloading
/// (`page_load`). Also checks that the active tab is never the one
/// suspended, which the policy promises but only the real event loop can
/// demonstrate end to end (the unit tests cover the pure planning).
///
/// The `suspend <index>` automation command is exercised too: a manual
/// suspension of an already-suspended or active tab must be a silent
/// no-op, exactly like the tab strip's button.
#[test]
fn live_tab_cap_suspends_background_tabs_and_switching_back_resumes_them() {
    skip_without_gui!("live_tab_cap_suspends_background_tabs_and_switching_back_resumes_them");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("suspension");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let stderr_path = dir.join("stderr.log");
    let homepage = fixture_url("minimal.html");
    let page_a = fixture_url("text.html");
    let page_b = fixture_url("dom_heavy.html");

    // `VELOX_MAX_TABS_PER_PROCESS=1` below is what makes the *choice* of
    // victim deterministic, and it is load-bearing rather than incidental.
    // D56 reclaims whole process groups before individual tabs, and D54
    // decides grouping by whether the previous tab was still loading when
    // the next one opened — so with the default cap the grouping, and
    // therefore which tab the policy picks, depends on how fast this
    // machine loads a local file. (That is not hypothetical: this test
    // first shipped without it and passed here while failing on the
    // slower CI runner, which grouped the tabs differently.) One tab per
    // process makes every group a single tab, so group order collapses to
    // plain least-recently-used and the assertions below hold at any
    // speed. The live-tab cap and the resume path — what this test is
    // actually about — are unaffected by the process layout.
    //
    // Tab strip after each step (cap = 2 live tabs):
    //   [home]                 home active, 1 live
    //   open a -> [home, a]    a active, 2 live — at the cap, nothing to do
    //   open b -> [home, a, b] b active, 3 live -> `home` (idle longest)
    //                          is suspended by the sweep that follows.
    //   suspend 2              -> active tab: refused, no-op
    //   suspend 0              -> already suspended: no-op
    //   switch 0               -> resumes `home` (tab_resume + page_load);
    //                          now 3 live again -> `a` (idle longest of the
    //                          background tabs) is suspended.
    let script = format!(
        "wait 800\n\
         open {page_a}\n\
         wait 600\n\
         open {page_b}\n\
         wait 800\n\
         suspend 2\n\
         suspend 0\n\
         wait 200\n\
         switch 0\n\
         wait 1200\n\
         quit\n"
    );
    let script_path = write_script(&dir, &script);

    let launch = launch_and_wait_with(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(30),
        &[
            ("VELOX_MAX_LIVE_TABS", Path::new("2")),
            ("VELOX_MAX_TABS_PER_PROCESS", Path::new("1")),
        ],
        Some(&stderr_path),
    );
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 30s during the suspension test. \
             Perf records: {:?}\nstderr:\n{stderr}",
            launch.perf_records
        );
    };
    assert!(
        status.success(),
        "velox exited abnormally: {status:?}\nstderr:\n{stderr}"
    );

    let records = &launch.perf_records;
    let tab_create: Vec<_> = events_named(records, "tab_create").collect();
    assert_eq!(
        tab_create.len(),
        2,
        "2 `open` commands should yield 2 `tab_create` records: {records:?}"
    );
    let created_ids: Vec<u64> = tab_create
        .iter()
        .filter_map(|r| r["tab_id"].as_u64())
        .collect();
    let home_id = 0;
    assert!(
        !created_ids.contains(&home_id),
        "the initial tab is id 0 and is never re-created: {created_ids:?}"
    );

    let suspends: Vec<_> = events_named(records, "tab_suspend").collect();
    assert_eq!(
        suspends.len(),
        2,
        "expected exactly two automatic suspensions (home after the 3rd tab opened, \
         then `a` after home was resumed), got {}: {records:?}\nstderr:\n{stderr}",
        suspends.len()
    );
    assert!(
        suspends
            .iter()
            .all(|r| r["reason"].as_str() == Some("tab_count")),
        "every suspension here is driven by the live-tab cap: {suspends:?}"
    );
    assert_eq!(
        suspends[0]["tab_id"].as_u64(),
        Some(home_id),
        "the first tab to go must be the least recently used one (home): {suspends:?}"
    );
    assert_eq!(
        suspends[1]["tab_id"].as_u64(),
        Some(created_ids[0]),
        "after home is resumed, `a` is the idle-longest background tab: {suspends:?}"
    );

    let resumes: Vec<_> = events_named(records, "tab_resume").collect();
    let switches: Vec<_> = events_named(records, "tab_switch").collect();
    assert_eq!(
        resumes.len(),
        1,
        "`switch 0` onto the suspended home tab must be reported as `tab_resume`: {records:?}"
    );
    assert_eq!(resumes[0]["tab_id"].as_u64(), Some(home_id));
    assert!(
        switches.is_empty(),
        "no plain `tab_switch` is expected (the only switch was a resume): {switches:?}"
    );

    // Resuming reloads the page: at least one `page_load` for the homepage
    // must arrive *after* the resume.
    let resume_ts = resumes[0]["ts_ms"].as_f64().unwrap_or(0.0);
    let reloaded = events_named(records, "page_load").any(|r| {
        r["url"].as_str() == Some(homepage.as_str())
            && r["ts_ms"].as_f64().unwrap_or(0.0) > resume_ts
    });
    assert!(
        reloaded,
        "expected the resumed tab to reload {homepage} after ts={resume_ts}: {records:?}"
    );

    // The two manual no-op `suspend` commands must not have produced an
    // error line (only an out-of-range index does), and the sampler must
    // not have started (no memory budget was set).
    assert!(
        !stderr.contains("automation: suspend"),
        "in-range `suspend` on the active/already-suspended tab must be silent:\n{stderr}"
    );
    assert!(
        !stderr.contains("memory sampling for tab suspension"),
        "no memory budget => no sampler:\n{stderr}"
    );
}

// ---------------------------------------------------------------------
// 7. Session restore (Issue #25): a saved session survives a real second
//    launch of the binary, and a restored background tab is genuinely
//    suspended (rebuilds its webview through the ordinary resume path).
// ---------------------------------------------------------------------

/// Guarantees, across two real, separate launches of `velox` sharing the
/// same `VELOX_DATA_DIR`:
///
/// - The first launch's `session.json` (`browser::persistence::
///   load_session`) records both open tabs, in order, with the correct
///   `active_index` — not just that *a* file was written (already covered
///   for `history.json`/`bookmarks.json` by
///   `visiting_pages_persists_history_json`), but that its content matches
///   what was actually open.
/// - The second launch, with `VELOX_RESTORE_SESSION=1` and a *different*
///   `VELOX_HOMEPAGE` than anything in the saved session, loads the
///   previously active tab's own URL instead of the configured homepage
///   (`page_load` for it, never for the homepage) — this is the
///   `ui::window::BrowserWindow::new` fix this issue made (the initial
///   webview used to always load `config.homepage`, ignoring a restored
///   tab's real URL).
/// - The other restored tab starts genuinely suspended, not merely
///   "not yet opened": switching to it is reported as `tab_resume` (via
///   the exact same automatic-suspension resume path D56 already uses),
///   and it reloads *its own* URL, not the homepage either.
#[test]
fn restoring_the_previous_session_reopens_its_tabs_across_a_real_relaunch() {
    skip_without_gui!("restoring_the_previous_session_reopens_its_tabs_across_a_real_relaunch");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("session-restore");
    let data_dir = dir.join("data");
    let home = fixture_url("minimal.html");
    let page_a = fixture_url("text.html");
    // Deliberately unrelated to anything in the saved session: if this
    // shows up as a `page_load` in the second launch, restore did not
    // actually take priority over `VELOX_HOMEPAGE`.
    let unused_homepage = fixture_url("dom_heavy.html");

    // --- Launch 1: open a second tab, then quit — no `restore` setting
    //     needed here (there is nothing to restore from yet). ---
    let perf_output_1 = dir.join("perf1.jsonl");
    let script_1 = format!("wait 800\nopen {page_a}\nwait 600\nquit\n");
    let script_path_1 = write_script(&dir, &script_1);
    let launch_1 = launch_and_wait(
        &perf_output_1,
        &data_dir,
        &home,
        &script_path_1,
        Duration::from_secs(30),
    );
    let Some(status_1) = launch_1.exit_status else {
        panic!(
            "velox (launch 1) did not exit on its own within 30s: {:?}",
            launch_1.perf_records
        );
    };
    assert!(
        status_1.success(),
        "launch 1 exited abnormally: {status_1:?}"
    );

    let snapshot = persistence::load_session(&data_dir)
        .expect("session.json should have been written and parse after launch 1");
    assert_eq!(
        snapshot
            .tabs
            .iter()
            .map(|t| t.url.as_str())
            .collect::<Vec<_>>(),
        vec![home.as_str(), page_a.as_str()],
        "the saved session should list both tabs in the order they were opened"
    );
    assert_eq!(
        snapshot.active_index, 1,
        "`page_a` (opened last) should be the active tab in the saved session"
    );

    // --- Launch 2: restore is on, and the homepage is a third, unrelated
    //     page that must never actually load. ---
    let perf_output_2 = dir.join("perf2.jsonl");
    // `switch 0` targets the restored `home` tab, which must start
    // suspended for this to exercise a resume rather than a plain switch.
    let script_2 = "wait 800\nswitch 0\nwait 800\nquit\n";
    let script_path_2 = write_script(&dir, script_2);
    // `write_script` always writes to the same `script.txt` inside `dir`;
    // launch 1 already consumed its own copy, so this just overwrites it
    // with the second script before launch 2 reads it.
    let launch_2 = launch_and_wait_with(
        &perf_output_2,
        &data_dir,
        &unused_homepage,
        &script_path_2,
        Duration::from_secs(30),
        &[("VELOX_RESTORE_SESSION", Path::new("1"))],
        None,
    );
    let Some(status_2) = launch_2.exit_status else {
        panic!(
            "velox (launch 2) did not exit on its own within 30s: {:?}",
            launch_2.perf_records
        );
    };
    assert!(
        status_2.success(),
        "launch 2 exited abnormally: {status_2:?}"
    );

    let records_2 = &launch_2.perf_records;
    assert!(
        events_named(records_2, "page_load")
            .all(|r| r["url"].as_str() != Some(unused_homepage.as_str())),
        "the configured homepage must never load once a session was restored: {records_2:?}"
    );
    assert!(
        events_named(records_2, "page_load").any(|r| r["url"].as_str() == Some(page_a.as_str())),
        "the restored active tab (page_a) should reload on startup: {records_2:?}"
    );

    let resumes: Vec<_> = events_named(records_2, "tab_resume").collect();
    assert_eq!(
        resumes.len(),
        1,
        "`switch 0` onto the restored, suspended `home` tab must be reported as \
         `tab_resume`, proving it started genuinely suspended: {records_2:?}"
    );
    let resume_ts = resumes[0]["ts_ms"].as_f64().unwrap_or(0.0);
    assert!(
        events_named(records_2, "page_load").any(|r| {
            r["url"].as_str() == Some(home.as_str())
                && r["ts_ms"].as_f64().unwrap_or(0.0) > resume_ts
        }),
        "resuming the restored `home` tab should reload its own (correct) URL \
         after ts={resume_ts}: {records_2:?}"
    );
}

// ---------------------------------------------------------------------
// 8. The `mark` command cuts warm-up out of the aggregated numbers
//    (Issue #60).
// ---------------------------------------------------------------------

/// Guarantees: `mark` reaches the perf log as a `measure_start` record at
/// the point the script asked for, and `benchmark::aggregate_trials` — the
/// same function `velox-bench aggregate` uses — pools only what came after
/// it. This is what makes `tab_create_20` mean "creating a tab with 20 open"
/// rather than "the average of creating tabs 2 through 21"; the unit tests
/// cover the cut itself, and this one covers the whole path from an
/// automation script through the running browser to the aggregate.
#[test]
fn mark_excludes_warm_up_tabs_from_the_aggregated_metrics() {
    skip_without_gui!("mark_excludes_warm_up_tabs_from_the_aggregated_metrics");
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let dir = unique_dir("mark");
    let perf_output = dir.join("perf.jsonl");
    let data_dir = dir.join("data");
    let homepage = fixture_url("minimal.html");
    let page = fixture_url("text.html");

    // Three tabs opened as warm-up, then the marker, then two more.
    let script = format!(
        "open {page}\nwait 400\n\
         open {page}\nwait 400\n\
         open {page}\nwait 400\n\
         mark\n\
         open {page}\nwait 400\n\
         open {page}\nwait 400\n\
         quit\n"
    );
    let script_path = write_script(&dir, &script);

    let launch = launch_and_wait(
        &perf_output,
        &data_dir,
        &homepage,
        &script_path,
        Duration::from_secs(30),
    );
    let Some(status) = launch.exit_status else {
        panic!(
            "velox did not exit on its own within 30s during the mark test. \
             Perf records: {:?}",
            launch.perf_records
        );
    };
    assert!(status.success(), "velox exited abnormally: {status:?}");

    let records = &launch.perf_records;
    let markers: Vec<_> = events_named(records, "measure_start").collect();
    assert_eq!(
        markers.len(),
        1,
        "`mark` must produce exactly one measure_start record: {records:?}"
    );
    let all_creates: Vec<_> = events_named(records, "tab_create").collect();
    assert_eq!(
        all_creates.len(),
        5,
        "5 `open` commands should still log 5 tab_create records: {records:?}"
    );

    // The aggregate — what a benchmark actually reads — sees only the two
    // creations after the marker.
    let aggregated = aggregate_trials(std::slice::from_ref(records));
    let stats = aggregated
        .get("tab_create_ms")
        .expect("tab_create_ms should be present");
    assert_eq!(
        stats.count, 2,
        "aggregate must drop the 3 warm-up creations, got {} samples",
        stats.count
    );

    // And the ones it kept are the later tabs. The initial tab is id 0 and
    // each `open` takes the next id, so the five opens are ids 1..=5 and
    // the two after the marker are 4 and 5 — never the warm-up 1, 2, 3.
    let after_marker = markers[0]["ts_ms"].as_f64().unwrap_or(0.0);
    let kept: HashSet<u64> = all_creates
        .iter()
        .filter(|r| r["ts_ms"].as_f64().unwrap_or(0.0) > after_marker)
        .filter_map(|r| r["tab_id"].as_u64())
        .collect();
    assert_eq!(
        kept,
        HashSet::from([4, 5]),
        "the measured phase should be the last two tabs, got {kept:?}"
    );
}
