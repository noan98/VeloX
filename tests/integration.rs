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

use velox::browser::benchmark::parse_jsonl;
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
