//! Pure decision logic behind the `tests/` integration suite's "can this
//! environment actually launch a GUI window?" preflight check (Issue #34,
//! see `docs/decisions.md` D47).
//!
//! Launching `velox` itself needs a real display (X11/Wayland) and, on
//! Linux, a D-Bus session bus for WebKitGTK's web process — see
//! `docs/benchmarking.md`'s "実行環境要件" for the background and Issue #72
//! (`docs/decisions.md` D46) for what happens without one: not a crash, not
//! an error, just a window that is created and then never proceeds any
//! further (no perf log line, ever) until something kills it. A developer
//! machine without an X session, or a CI runner without Xvfb/D-Bus, must
//! not see `cargo test` turn red over this — so the integration tests
//! check for the two signals below *before* spawning `velox`, and skip
//! (printing why, to stdout) instead of launching into a guaranteed hang.
//!
//! [`gui_probe_reason`] takes already-read booleans rather than calling
//! `std::env::var` itself, so the decision is unit-testable here without
//! touching real process environment variables (matching this crate's
//! existing `resolve_homepage`/`resolve_perf_env`-style pure-decision
//! functions in `src/config/mod.rs`); reading the actual environment and
//! deciding which platform even needs this check at all is left to the
//! (impure, integration-test-only) caller.

/// Whether this environment is missing something `velox` needs merely to
/// *launch* a window and make progress past `BrowserWindow::new` — not
/// whether it can be assumed to work perfectly, just whether it is even
/// worth trying instead of guaranteeing a multi-second hang.
///
/// `has_display` should be true when `DISPLAY` or `WAYLAND_DISPLAY` is set
/// (Xvfb sets `DISPLAY`); `has_dbus_session` should be true when
/// `DBUS_SESSION_BUS_ADDRESS` is set (`dbus-run-session` sets this) — see
/// docs/benchmarking.md's "実行環境要件" for why these two specific
/// variables, and D46/#72 for the observed failure mode
/// (`dbus-run-session`'s absence: a window that never proceeds past
/// creation and writes zero perf records) that made D-Bus a second,
/// separate check from the display one.
///
/// Returns `None` when neither signal is missing (this function's whole
/// job is only to say "do not even try"; it makes no promise beyond that —
/// a display and a session bus being present does not guarantee
/// WebKitGTK will not fail for some other reason, only that this specific,
/// previously-observed failure mode is not obviously about to repeat).
pub fn gui_probe_reason(has_display: bool, has_dbus_session: bool) -> Option<&'static str> {
    if !has_display {
        return Some(
            "DISPLAY/WAYLAND_DISPLAY が設定されていません (Xvfb 等のディスプレイが無い環境)",
        );
    }
    if !has_dbus_session {
        return Some(
            "DBUS_SESSION_BUS_ADDRESS が設定されていません (dbus-run-session 等の D-Bus \
             セッションバスが無い環境。WebKitGTK の web process が起動できず、#72 と同じ \
             無応答になります — docs/decisions.md D46 参照)",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_display_is_reported_first() {
        let reason = gui_probe_reason(false, false).expect("should be Some");
        assert!(reason.contains("DISPLAY"));
    }

    #[test]
    fn missing_dbus_alone_is_reported() {
        let reason = gui_probe_reason(true, false).expect("should be Some");
        assert!(reason.contains("DBUS_SESSION_BUS_ADDRESS"));
    }

    #[test]
    fn both_present_means_no_reason_to_skip() {
        assert_eq!(gui_probe_reason(true, true), None);
    }

    #[test]
    fn display_alone_without_dbus_is_still_a_skip() {
        // The exact #72 failure mode: a display exists but no session bus,
        // so a launch would hang rather than fail fast.
        assert!(gui_probe_reason(true, false).is_some());
    }
}
