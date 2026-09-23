//! アクティブタブのページに対する操作: 印刷/PDF 書き出し (Issue #40, D75)・
//! 名前を付けて保存 (Issue #46, D76)・ソース表示 (Issue #45, D72)。
//!
//! Print / PDF export (Issue #40), see docs/decisions.md D75:
//! `ToolbarCommand::Print`/`ContentShortcut::Print` (Ctrl/Cmd+P, D18/D23's
//! usual dual-channel shortcut delivery, assigned directly here for now
//! rather than through a keybinding-config layer — same "not blocked on
//! Issue #38 yet" reasoning D69 already used for Ctrl/Cmd+F) both call
//! `print_active_tab`. `ToolbarCommand::SaveAsPdf` (a toolbar button only —
//! no keyboard shortcut, see D75) calls `save_active_tab_as_pdf`.

use super::*;

/// 印刷/PDF 書き出しの結果を、そのウィンドウの共有ステータス表示に出す。
pub(super) fn show_print_status(window: &BrowserWindow, message: &str) {
    log_failure("show print status", window.set_print_status(Some(message)));
}

/// Print the active tab's page (Ctrl/Cmd+P, or the toolbar's print button)
/// via the OS's native print UI — see
/// `ui::window::BrowserWindow::print_tab`'s doc comment for exactly what
/// that means on each platform and its one caveat (a real print-job
/// failure is invisible to wry's `Result`, only a failure to even dispatch
/// the call is not).
pub(super) fn print_active_tab(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
) {
    let tab_id = tabs_of(state, window_id).active_id();
    if let Err(err) = window.print_tab(tab_id) {
        eprintln!("velox: failed to print the active tab: {err}");
        show_print_status(window, &format!("印刷を開始できませんでした: {err}"));
    }
}

/// Export the active tab's page straight to a PDF file with no dialog (the
/// toolbar's "PDFとして保存" button) — Windows-only
/// (`ui::window::BrowserWindow::export_tab_as_pdf`, see docs/decisions.md
/// D75); macOS/Linux answer with a status message pointing at
/// [`print_active_tab`]'s dialog instead, which itself offers a "save as
/// PDF" destination on every platform VeloX ships on.
///
/// The destination directory reuses `browser::downloads`' existing assets
/// wholesale — `resolve_download_dir_with_override` (the same
/// `Config::download_dir_override`/`VELOX_DOWNLOAD_DIR`/platform-default
/// resolution downloads already use, Issue #16/#30) and
/// `prepare_destination` (sanitizes the suggested filename, creates the
/// directory if missing, and avoids clobbering an existing file the same
/// `report (1).pdf` way a same-named download would) — rather than
/// re-deriving either.
pub(super) fn save_active_tab_as_pdf(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    config: &Config,
) {
    let tab = tabs_of(state, window_id).active();
    let tab_id = tab.id();
    let url = tab.current_url().to_owned();
    let title = tab.title().map(str::to_owned);

    let Some(dir) =
        downloads::resolve_download_dir_with_override(config.download_dir_override.as_deref())
    else {
        show_print_status(window, "PDF の保存先フォルダを特定できませんでした");
        return;
    };
    let raw_name = print::suggest_pdf_filename(title.as_deref(), &url);
    let destination = match downloads::prepare_destination(&dir, &raw_name) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("velox: failed to prepare the PDF export destination: {err}");
            show_print_status(
                window,
                &format!("PDF の保存先を準備できませんでした: {err}"),
            );
            return;
        }
    };

    let settings = print::PdfExportSettings::default().sanitize();
    match window.export_tab_as_pdf(tab_id, destination, &settings) {
        PdfExportRequest::Started => {
            // The real result arrives later as `UserEvent::PdfExportFinished`.
        }
        PdfExportRequest::NoWebview => {
            show_print_status(window, "このタブは休止中のため PDF に保存できません");
        }
        PdfExportRequest::UnsupportedPlatform => {
            show_print_status(
                window,
                "この OS では PDF への直接保存に対応していません。印刷 (Ctrl/Cmd+P) \
                     のダイアログから PDF に保存してください。",
            );
        }
        PdfExportRequest::Failed { message } => {
            show_print_status(window, &format!("PDF の書き出しに失敗しました: {message}"));
        }
    }
}

// --- Save page (Issue #46, "名前を付けて保存"), see docs/decisions.md D76 ---

/// Save the active tab's current page (Ctrl/Cmd+S from either the toolbar —
/// `ToolbarCommand::SavePage` — or a content webview —
/// `ContentShortcut::SavePage`, D18/D23's usual dual-channel shortcut
/// delivery). Resolves the active tab's URL/title from `state` (the same
/// `Tab::current_url`/`Tab::title` the tab strip already shows) and the
/// configured download-directory override (Issue #30/D67 — only consulted
/// by `BrowserWindow::request_save_page`'s non-Windows fallback), then hands
/// the rest to that method: it decides the save format/destination itself
/// (platform-specific, see D76) and reports the outcome asynchronously via
/// [`UserEvent::SavePageStarted`]/[`UserEvent::SavePageFinished`] —
/// nothing further to do here.
pub(super) fn request_save_page(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    config: &Config,
) {
    let tabs = tabs_of(state, window_id);
    let tab_id = tabs.active_id();
    let tab = tabs.active();
    let url = tab.current_url().to_owned();
    let title = tab.title().map(str::to_owned);
    window.request_save_page(tab_id, url, title, config.download_dir_override.clone());
}

// --- View Source (Issue #45, Ctrl/Cmd+U), see docs/decisions.md D72 ---
//
// `ToolbarCommand::ViewSource`/`ContentShortcut::ViewSource` (D18/D23's usual
// dual-channel shortcut delivery — Ctrl/Cmd+U assigned directly here rather
// than through a keybinding-config layer, since Issue #38 (keyboard shortcut
// management) has not landed yet, exactly like D69's Ctrl/Cmd+F before it)
// both call `request_view_source`. The actual tab only gets built once the
// asynchronous source fetch comes back as `UserEvent::ViewSourceReady`,
// handled by `open_view_source_tab` below.

/// Kick off View Source for the active tab: ask its content webview for its
/// current markup (`BrowserWindow::fetch_page_source`); `open_view_source_tab`
/// finishes the job once `UserEvent::ViewSourceReady` reports the result.
///
/// `page_url` is captured *now*, from `Tabs`' own state — not re-read later
/// from the webview — so the source that eventually comes back is always
/// correctly labeled with the page it was actually requested for, even if
/// the user switches tabs or that tab navigates again while the (async)
/// fetch is in flight (see `BrowserWindow::fetch_page_source`'s doc comment).
/// A no-op — not a crash — when the active tab has no live webview
/// (suspended): the same "nothing to read from yet" contract every other
/// `fetch_*` call in this file already uses.
pub(super) fn request_view_source(window: &BrowserWindow, window_id: WindowId, state: &AppState) {
    // Issue #29/D68: read the active tab of *this* window, not a
    // process-global "the tabs". A window that is already gone is a no-op.
    let Some(tabs) = state.windows.tabs(window_id) else {
        return;
    };
    let tab = tabs.active();
    let page_url = tab.current_url().to_owned();
    log_failure(
        "fetch page source",
        window.fetch_page_source(tab.id(), page_url),
    );
}

/// Finish View Source once the requested page's markup has come back
/// (`UserEvent::ViewSourceReady`): escape/number/truncate it into a safe
/// document (`browser::view_source::build_view_source_document` — see its
/// doc comment and docs/decisions.md D72 for why escaping here is what
/// keeps this feature from being an XSS vector), encode that document as a
/// `data:` URL, and open it exactly the way every other new tab opens
/// (`open_new_tab` — the toolbar's "+" button, Ctrl/Cmd+T,
/// `target="_blank"`, ...), so it inherits the same process-placement,
/// activation, and latency-logging behavior as any other new tab, and never
/// touches the tab the source was read from.
pub(super) fn open_view_source_tab(
    window: &mut BrowserWindow,
    window_id: WindowId,
    state: &mut AppState,
    page_url: &str,
    html: &str,
) {
    let document =
        view_source::build_view_source_document(page_url, html, view_source::MAX_SOURCE_BYTES);
    let data_url = view_source::to_data_url(&document);
    open_new_tab(window, window_id, state, &data_url);
}
