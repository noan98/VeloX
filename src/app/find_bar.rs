//! ページ内検索バー (Issue #43, Ctrl/Cmd+F, docs/decisions.md D69)。
//!
//! `ToolbarCommand::OpenFindBar`/`ContentShortcut::OpenFindBar` (D18/D23's
//! usual dual-channel shortcut delivery — Ctrl/Cmd+F assigned directly here
//! for now rather than through a keybinding-config layer, since Issue #38
//! (keyboard shortcut management) has not landed yet; see this issue's PR for
//! what makes this easy to move under #38 later) both call `open_find_bar`.
//! `FindQuery`/`FindNext`/`FindPrevious`/`FindClose` call the other three
//! helpers below; `UserEvent::FindMatchesUpdated` (the DOM search's async
//! result) is handled directly in `handle_user_event`.

use super::*;

/// Open the find bar for `window_id`'s active tab: starts a fresh
/// `browser::find::FindState` session for *that window* (discarding any
/// previous one it had open — e.g. re-pressing Ctrl/Cmd+F while already open
/// just resets to an empty query, matching mainstream browsers), shows the
/// bar, and resets the "N/M" counter to blank. The bar's own JS
/// focuses/selects its input as soon as it becomes visible
/// (`veloxSetFindBarVisible`), so nothing else to do here.
///
/// Multi-window (Issue #29/D68): the session is stored on `window_id`'s own
/// `WindowEntry` (`Windows::set_find`), never a single global slot — opening
/// find in one window must never disturb another window's independent
/// search.
pub(super) fn open_find_bar(window: &mut BrowserWindow, window_id: WindowId, state: &mut AppState) {
    let tab_id = tabs_of(state, window_id).active_id();
    state
        .windows
        .set_find(window_id, find::FindState::new(tab_id));
    log_failure("show find bar", window.set_find_bar_visible(true));
    log_failure("reset find status", window.set_find_status(0, None));
}

/// Close `window_id`'s find bar: clears any DOM highlight left in the
/// session's tab (a no-op if that tab has since closed —
/// `clear_find_highlights` already handles an unknown id), drops the
/// session, and hides the bar. A no-op if that window's find bar was not
/// open (see `Windows::take_find`).
pub(super) fn close_find_bar(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
) {
    let Some(session) = state.windows.take_find(window_id) else {
        return;
    };
    log_failure(
        "clear find highlights",
        window.clear_find_highlights(session.tab_id()),
    );
    log_failure("hide find bar", window.set_find_bar_visible(false));
}

/// `window_id`'s find bar input changed, or its case-sensitivity toggle
/// flipped (`ToolbarCommand::FindQuery`). A no-op if that window's find bar
/// is not open (the bar's own JS should never send this then, but a
/// stray/racy message must not panic). An empty/whitespace-only `query`
/// (`browser::find::normalize_query` returns `None`) clears any existing
/// highlight instead of asking the DOM to search for nothing.
pub(super) fn update_find_query(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    query: String,
    case_sensitive: bool,
) {
    let Some(session) = state.windows.find_mut(window_id) else {
        return;
    };
    let tab_id = session.tab_id();
    match find::normalize_query(&query) {
        Some(normalized) => {
            session.set_query(normalized.clone(), case_sensitive);
            log_failure("reset find status", window.set_find_status(0, None));
            log_failure(
                "search in page",
                window.search_in_page(tab_id, &normalized, case_sensitive),
            );
        }
        None => {
            session.set_query(String::new(), case_sensitive);
            log_failure(
                "clear find highlights",
                window.clear_find_highlights(tab_id),
            );
            log_failure("reset find status", window.set_find_status(0, None));
        }
    }
}

/// Which direction `step_find` moves — kept as a tiny enum rather than a
/// `bool` so the two `ToolbarCommand` call sites below read as `Next`/
/// `Previous`, not `true`/`false`.
pub(super) enum FindDirection {
    Next,
    Previous,
}

/// "▼"/"▲" in `window_id`'s find bar, or Enter/Shift+Enter in its input
/// (`ToolbarCommand::FindNext`/`FindPrevious`). A no-op if that window's
/// find bar is not open or its query is empty (nothing searched for yet).
pub(super) fn step_find(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    direction: FindDirection,
) {
    let Some(session) = state.windows.find_mut(window_id) else {
        return;
    };
    if session.query().is_empty() {
        return;
    }
    let tab_id = session.tab_id();
    let active = match direction {
        FindDirection::Next => session.next_match(),
        FindDirection::Previous => session.previous_match(),
    };
    let total = session.total();
    log_failure("update find status", window.set_find_status(total, active));
    if let Some(index) = active {
        log_failure(
            "highlight find match",
            window.highlight_find_match(tab_id, index),
        );
    }
}
