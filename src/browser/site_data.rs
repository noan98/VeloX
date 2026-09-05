//! Pure, engine-agnostic judgement for "clear site data" (Issue #26).
//!
//! The actual deletion is entirely `wry`'s job: `wry::WebView` exposes a
//! public, safe, cross-platform `clear_all_browsing_data()` method (verified
//! against wry 0.56.1's source — see docs/decisions.md D66 for the exact
//! per-backend call sites) that wipes cookies/cache/local storage/session
//! storage/IndexedDB/service workers for the webview's data store. VeloX
//! does not reimplement any of that; `ui::window::BrowserWindow::
//! clear_all_site_data` just calls it once per webview the window currently
//! holds a handle to (the toolbar, plus every awake tab) and reports how
//! many attempts succeeded.
//!
//! What *is* engine-agnostic, and therefore lives here per
//! docs/architecture.md's "judgement goes in `browser/`" rule, is turning
//! "N attempts, M of them failed" into a verdict `app.rs` can act on (log
//! nothing on full success, log a summary otherwise) — that judgement does
//! not need `wry` at all, so it is tested here rather than only reachable
//! through a running window.

/// The verdict for one `clear_all_site_data` call, derived from how many
/// webviews were attempted and how many of those failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearOutcome {
    /// There was nothing to clear. Not reachable in practice today — a
    /// `BrowserWindow` always has at least its toolbar webview — kept so
    /// [`summarize`] is a total function instead of panicking on `0, 0`.
    Nothing,
    /// Every attempted webview cleared successfully.
    Success,
    /// At least one webview cleared successfully and at least one failed.
    Partial,
    /// Every attempted webview failed; nothing was cleared.
    AllFailed,
}

/// Turn "`attempted` webviews were asked to clear their site data, `failed`
/// of them returned an error" into a [`ClearOutcome`].
///
/// `failed` must not exceed `attempted`; a caller that could only ever pass
/// a well-formed pair (as `ui::window::BrowserWindow::clear_all_site_data`
/// does — it increments `attempted` for every webview it tries and `failed`
/// only among those) never triggers the `debug_assert!` below.
pub fn summarize(attempted: usize, failed: usize) -> ClearOutcome {
    debug_assert!(failed <= attempted, "failed count cannot exceed attempted");
    match attempted {
        0 => ClearOutcome::Nothing,
        _ if failed == 0 => ClearOutcome::Success,
        _ if failed == attempted => ClearOutcome::AllFailed,
        _ => ClearOutcome::Partial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_attempts_is_nothing() {
        assert_eq!(summarize(0, 0), ClearOutcome::Nothing);
    }

    #[test]
    fn zero_failures_is_success() {
        assert_eq!(summarize(1, 0), ClearOutcome::Success);
        assert_eq!(summarize(5, 0), ClearOutcome::Success);
    }

    #[test]
    fn all_attempts_failing_is_all_failed() {
        assert_eq!(summarize(1, 1), ClearOutcome::AllFailed);
        assert_eq!(summarize(3, 3), ClearOutcome::AllFailed);
    }

    #[test]
    fn some_but_not_all_failing_is_partial() {
        assert_eq!(summarize(3, 1), ClearOutcome::Partial);
        assert_eq!(summarize(5, 4), ClearOutcome::Partial);
    }
}
