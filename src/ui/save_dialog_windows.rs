//! Windows-only (WebView2) "名前を付けて保存" (Issue #46): the native
//! Save-As file picker, and capturing the active page as MHTML.
//!
//! See docs/decisions.md D76 for the full design rationale. In short:
//!
//! - [`show_save_dialog`] uses the classic Shell Common Item Dialog API
//!   (`IFileSaveDialog`) — the same picker every native Windows app uses for
//!   Open/Save, not anything WebView2-specific — to satisfy the issue's
//!   "保存先を選択できる" acceptance criterion, including the OS's own
//!   built-in "this file already exists — replace it?" prompt (D76's answer
//!   to "同名ファイルを安全に扱える" on this platform).
//! - [`capture_and_write_mhtml`] uses [`wry::WebViewExtWindows::webview`]
//!   (stable, safe, public API — the same escape hatch
//!   `ui::webview2_blocking` already uses, see its module doc comment) to
//!   reach the raw `ICoreWebView2` and call
//!   `ICoreWebView2::CallDevToolsProtocolMethod("Page.captureSnapshot", …)`,
//!   Chromium DevTools Protocol's page-to-MHTML capture — part of the base
//!   `ICoreWebView2` interface, present since WebView2's very first stable
//!   release, so (unlike the newer, `ICoreWebView2_25`-gated native
//!   `ShowSaveAsUI`/`SaveAsUIShowing` this module deliberately does *not*
//!   use — too new a generation to rely on, the same call D69 already made
//!   for a different API) this needs no interface-generation downcast at
//!   all.
//!
//! Like `ui::webview2_blocking`, this has not been exercised on an actual
//! Windows machine — this project's development environment is Linux-only,
//! so verification stopped at `cargo check --target x86_64-pc-windows-msvc`
//! (type-checks, does not link or run); see D76 for what was and was not
//! verified.

use std::path::PathBuf;

use tao::event_loop::EventLoopProxy;
use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
use windows::core::{HRESULT, HSTRING, PCWSTR, PWSTR};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileSaveDialog, IFileSaveDialog, IShellItem, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT,
    SIGDN_FILESYSPATH,
};
use wry::{WebView, WebViewExtWindows};

use crate::app::UserEvent;
use crate::browser::save_page;
use crate::browser::WindowId;

/// `IFileDialog::Show`'s `HRESULT` when the user cancels (closes the dialog
/// or presses Cancel) — `HRESULT_FROM_WIN32(ERROR_CANCELLED)`. Not exposed
/// as a named constant by `windows`/`webview2-com`'s Shell bindings, so
/// spelled out here the same way well-known fixed HRESULTs are elsewhere in
/// the Windows ecosystem.
const ERROR_CANCELLED_HRESULT: HRESULT = HRESULT(0x800704C7_u32 as i32);

/// Show the native Windows "名前を付けて保存" file picker, pre-filled with
/// `default_file_name` (already run through
/// `browser::save_page::suggested_file_name` by the caller — this function
/// does not itself sanitize anything, matching how `ui::webview2_blocking`
/// does not re-validate what `browser::subresource` already decided).
///
/// Returns `Ok(None)` if the user cancels (not an error); `Ok(Some(path))`
/// with the chosen absolute path otherwise. The dialog is configured with
/// `FOS_OVERWRITEPROMPT`, so Windows itself asks "replace this file?" when
/// the chosen path already exists — this is Issue #46's
/// "同名ファイルを安全に扱える" answer on this platform, no VeloX-side
/// collision logic needed here (contrast the non-Windows fallback, which
/// reuses `browser::downloads::unique_filename`).
pub fn show_save_dialog(default_file_name: &str) -> Result<Option<PathBuf>, String> {
    // SAFETY: every call below is a plain COM method call on either a
    // freshly created `IFileSaveDialog` (this function's own, not retained
    // past its return) or an `IShellItem` COM hands back from `GetResult`
    // — no pointer here outlives the scope that created it, and none is
    // shared with any other thread. `filter_name`/`filter_pattern` are kept
    // alive (as local `HSTRING`s) for exactly as long as the `PCWSTR`s
    // pointing into them are used (`SetFileTypes`, before either variable
    // goes out of scope), the standard windows-rs "the HSTRING backing a
    // PCWSTR must outlive the call" rule.
    unsafe {
        let dialog: IFileSaveDialog = CoCreateInstance(
            &FileSaveDialog,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
        .map_err(|err| format!("保存ダイアログを作成できませんでした: {err}"))?;

        let filter_name = HSTRING::from("ウェブページ、MHTML 形式 (*.mhtml)");
        let filter_pattern = HSTRING::from("*.mhtml");
        let filters = [COMDLG_FILTERSPEC {
            pszName: PCWSTR(filter_name.as_ptr()),
            pszSpec: PCWSTR(filter_pattern.as_ptr()),
        }];
        dialog
            .SetFileTypes(&filters)
            .map_err(|err| format!("保存ダイアログの初期化に失敗しました: {err}"))?;
        // Best-effort cosmetics: a failure here would only mean a slightly
        // less convenient dialog (wrong default extension pre-selected, or
        // no pre-filled name), never an unsafe one — so these are logged
        // rather than turned into a hard error, matching this file's module
        // doc comment on `capture_and_write_mhtml`'s own error handling
        // philosophy.
        if let Err(err) = dialog.SetFileTypeIndex(1) {
            eprintln!("velox: SetFileTypeIndex に失敗しました: {err}");
        }
        if let Err(err) = dialog.SetDefaultExtension(&HSTRING::from(save_page::MHTML_EXTENSION)) {
            eprintln!("velox: SetDefaultExtension に失敗しました: {err}");
        }
        if let Err(err) = dialog.SetFileName(&HSTRING::from(default_file_name)) {
            eprintln!("velox: SetFileName に失敗しました: {err}");
        }
        match dialog.GetOptions() {
            Ok(options) => {
                if let Err(err) =
                    dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_OVERWRITEPROMPT)
                {
                    eprintln!("velox: SetOptions に失敗しました: {err}");
                }
            }
            Err(err) => eprintln!("velox: GetOptions に失敗しました: {err}"),
        }

        match dialog.Show(None) {
            Ok(()) => {}
            Err(err) if err.code() == ERROR_CANCELLED_HRESULT => return Ok(None),
            Err(err) => return Err(format!("保存ダイアログの表示に失敗しました: {err}")),
        }

        let item: IShellItem = dialog
            .GetResult()
            .map_err(|err| format!("保存先を取得できませんでした: {err}"))?;
        let display_name = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|err| format!("保存先のパスを取得できませんでした: {err}"))?;
        let path = pwstr_to_string(display_name);
        CoTaskMemFree(Some(display_name.0 as *const _));
        Ok(Some(PathBuf::from(path)))
    }
}

/// Read a COM-owned, null-terminated UTF-16 string pointed to by `pwstr`
/// (e.g. `IShellItem::GetDisplayName`'s result) without transferring
/// ownership — the caller is still responsible for freeing `pwstr` itself
/// (via `CoTaskMemFree`) once this returns.
///
/// # Safety
/// `pwstr` must be null, or point to a valid null-terminated UTF-16 buffer
/// that stays valid for the duration of this call (true for a COM
/// allocator's out-parameter that has not been freed yet).
unsafe fn pwstr_to_string(pwstr: PWSTR) -> String {
    if pwstr.0.is_null() {
        return String::new();
    }
    // SAFETY: caller guarantees `pwstr` points at a valid null-terminated
    // UTF-16 buffer; reading one `u16` at a time until the terminator never
    // reads past its end.
    let len = unsafe { (0..).take_while(|&i| *pwstr.0.add(i) != 0).count() };
    // SAFETY: `len` was just measured from the same valid buffer above.
    let slice = unsafe { std::slice::from_raw_parts(pwstr.0, len) };
    String::from_utf16_lossy(slice)
}

/// Capture `webview`'s current page as MHTML (via Chromium DevTools
/// Protocol's `Page.captureSnapshot`, see this module's doc comment) and
/// write it to `destination`, reporting the outcome as
/// [`UserEvent::SavePageFinished`] — always exactly once, whether the COM
/// call itself fails synchronously or its completion handler fires
/// (successfully or not) later. Never blocks: the completion handler runs
/// asynchronously, dispatched through the normal `tao`/WebView2 message
/// loop the same way `wry::WebView::evaluate_script_with_callback`'s own
/// callbacks already are — no nested message pump of our own (contrast
/// `webview2-com`'s own `wait_for_async_operation` helper, deliberately not
/// used here).
pub fn capture_and_write_mhtml(
    webview: &WebView,
    proxy: EventLoopProxy<UserEvent>,
    window_id: WindowId,
    url: String,
    destination: PathBuf,
) {
    let core = webview.webview();
    // Kept for the synchronous-failure branch below: `proxy` itself is
    // moved into the completion closure, which never runs at all if the
    // COM call below fails outright, so that branch needs its own handle.
    let sync_failure_proxy = proxy.clone();
    let closure_url = url.clone();
    let closure_destination = destination.clone();

    // SAFETY: `core` is a live `ICoreWebView2` obtained through wry's own
    // `WebViewExtWindows::webview()`, exactly like `ui::webview2_blocking`'s
    // `attach` (see its module doc comment for the general reasoning this
    // mirrors). `CallDevToolsProtocolMethod` is a plain COM method call; the
    // completion handler object it is handed is heap-allocated and kept
    // alive by WebView2 itself until it is invoked (the same contract every
    // other `webview2-com` `*CompletedHandler::create` callback in this
    // codebase relies on — see `ui::window`'s `evaluate_script_with_callback`
    // uses, which wry implements the exact same way internally).
    let result = unsafe {
        core.CallDevToolsProtocolMethod(
            &HSTRING::from(save_page::CAPTURE_SNAPSHOT_METHOD),
            &HSTRING::from(save_page::CAPTURE_SNAPSHOT_PARAMS),
            &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                move |hr: windows::core::Result<()>, json: String| {
                    let error = match hr {
                        Ok(()) => match save_page::extract_mhtml(&json) {
                            Ok(mhtml) => std::fs::write(&closure_destination, mhtml.as_bytes())
                                .err()
                                .map(|err| format!("ファイルの書き込みに失敗しました: {err}")),
                            Err(err) => Some(err),
                        },
                        Err(err) => Some(format!("MHTML の取得に失敗しました: {err}")),
                    };
                    let _ = proxy.send_event(UserEvent::SavePageFinished {
                        window_id,
                        url: closure_url.clone(),
                        destination: closure_destination.clone(),
                        error,
                    });
                    Ok(())
                },
            )),
        )
    };

    if let Err(err) = result {
        let _ = sync_failure_proxy.send_event(UserEvent::SavePageFinished {
            window_id,
            url,
            destination,
            error: Some(format!("MHTML の取得を開始できませんでした: {err}")),
        });
    }
}
