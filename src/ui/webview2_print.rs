//! Windows-only (WebView2) headless PDF export (Issue #40).
//!
//! See docs/decisions.md D75 for the full investigation. In short:
//!
//! - `wry::WebViewExtWindows::webview()`/`environment()` (stable, safe,
//!   public API, the same entry point D59/D66/D69 already used) hand back
//!   the raw `ICoreWebView2`/`ICoreWebView2Environment` COM objects wry's
//!   own webview2 backend already owns internally.
//! - `ICoreWebView2Environment6::CreatePrintSettings` and
//!   `ICoreWebView2_7::PrintToPdf` are both older, lower-numbered COM
//!   interface generations than `ICoreWebView2_13`, which D59/D66 already
//!   established as reachable on this same wry/webview2-com version pair —
//!   a runtime new enough for `_13` is new enough for `_7` too, since
//!   WebView2's interface numbering is cumulative (each `_N` is a strict
//!   superset of `_N-1`). This is a *safer* bet than D66's own `_13` cast,
//!   not a riskier one.
//! - This is the opposite finding from D69's in-page-find investigation,
//!   where the only reachable native API (`ICoreWebView2Find`) needed
//!   `ICoreWebView2_28` — a generation far *newer* than anything this
//!   project has ever verified reaches a real WebView2 Runtime — and was
//!   declined for that reason. `PrintToPdf`'s `_7` requirement is on the
//!   opposite side of that line, so the same "Windows-first, but not on an
//!   unverifiable API" reasoning that declined D69's Find leads here to
//!   *accepting* PrintToPdf.
//! - The *interactive* print dialog (`ICoreWebView2_16::Print`/
//!   `ShowPrintUI`) sits in the same too-new-to-verify territory D69 already
//!   declined for Find (`_16` > `_13`) — see `ui::window::BrowserWindow::
//!   print_tab` (which uses `wry::WebView::print()` instead, needing no COM
//!   at all) for why VeloX does not call it.
//!
//! `browser::print::PdfExportSettings` is the pure, unit-tested settings
//! shape; everything here is the glue that feeds it into the real COM call
//! and reports the result back as [`crate::app::UserEvent::PdfExportFinished`].
//! Like `ui::webview2_blocking` (D59), this has only been type-checked via
//! `cargo check --target x86_64-pc-windows-msvc` — this project's
//! development environment is Linux-only, so the actual COM calls have
//! never run.

use std::path::Path;

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Environment6, ICoreWebView2PrintSettings, ICoreWebView2_7,
    COREWEBVIEW2_PRINT_ORIENTATION, COREWEBVIEW2_PRINT_ORIENTATION_LANDSCAPE,
    COREWEBVIEW2_PRINT_ORIENTATION_PORTRAIT,
};
use webview2_com::PrintToPdfCompletedHandler;
use windows::core::{Interface, HSTRING};
use wry::{WebView, WebViewExtWindows};

use crate::app::UserEvent;
use crate::browser::print::{Orientation, PdfExportSettings};
use crate::browser::{TabId, WindowId};

/// Kick off a headless PDF export of `webview`'s current page to
/// `destination` using `settings`. Returns as soon as the COM call is
/// *dispatched*; the actual result (success/failure, from WebView2's own
/// async completion handler) arrives later as
/// [`UserEvent::PdfExportFinished`] — this function's `Err` return is only
/// for a failure to even start (an interface cast or settings-object
/// creation failing), which the caller (`ui::window::BrowserWindow::
/// export_tab_as_pdf`) turns into an immediate, synchronous error instead
/// of waiting for an event that will never come.
pub fn export_as_pdf(
    webview: &WebView,
    settings: &PdfExportSettings,
    destination: &Path,
    proxy: EventLoopProxy<UserEvent>,
    window_id: WindowId,
    tab_id: TabId,
) -> windows::core::Result<()> {
    let core = webview.webview();
    let env = webview.environment();

    // SAFETY: `core`/`env` are live COM references obtained through wry's
    // own `WebViewExtWindows` (see this module's doc comment) for as long
    // as `webview` (borrowed by the caller) is alive. `.cast::<T>()` is a
    // plain `QueryInterface` — it fails cleanly (`Err`, propagated by `?`)
    // rather than producing an invalid object when the runtime is too old
    // to support `T`; every setter below is a plain COM property setter on
    // an object this function itself just created and holds the only
    // reference to.
    let print_settings = unsafe {
        let print_settings = env
            .cast::<ICoreWebView2Environment6>()?
            .CreatePrintSettings()?;
        apply_settings(&print_settings, settings)?;
        print_settings
    };

    let destination = destination.to_path_buf();
    let path_arg = HSTRING::from(destination.to_string_lossy().as_ref());
    let handler_destination = destination.clone();

    // SAFETY: same COM-reference reasoning as above. The completion
    // handler closure only touches values it owns (`proxy`/`window_id`/
    // `tab_id`/`handler_destination`, all moved in) plus the two arguments
    // WebView2 hands it for this one callback — never a pointer whose
    // validity this function cannot account for.
    unsafe {
        core.cast::<ICoreWebView2_7>()?.PrintToPdf(
            &path_arg,
            &print_settings,
            &PrintToPdfCompletedHandler::create(Box::new(move |result, succeeded| {
                let error = match &result {
                    Ok(()) if succeeded => None,
                    Ok(()) => Some("PDF の生成に失敗しました".to_owned()),
                    Err(err) => Some(err.to_string()),
                };
                let success = result.is_ok() && succeeded;
                let _ = proxy.send_event(UserEvent::PdfExportFinished {
                    window_id,
                    tab_id,
                    destination: handler_destination.clone(),
                    success,
                    error,
                });
                Ok(())
            })),
        )
    }
}

/// # Safety
/// `target` must be a live `ICoreWebView2PrintSettings` this function holds
/// the only reference to (freshly created by [`export_as_pdf`]); every call
/// here is a plain COM property setter.
unsafe fn apply_settings(
    target: &ICoreWebView2PrintSettings,
    settings: &PdfExportSettings,
) -> windows::core::Result<()> {
    let (width, height) = settings.page_dimensions_in();
    target.SetOrientation(orientation_to_native(settings.orientation))?;
    target.SetScaleFactor(settings.scale)?;
    target.SetPageWidth(width)?;
    target.SetPageHeight(height)?;
    target.SetMarginTop(settings.margins.top)?;
    target.SetMarginBottom(settings.margins.bottom)?;
    target.SetMarginLeft(settings.margins.left)?;
    target.SetMarginRight(settings.margins.right)?;
    target.SetShouldPrintBackgrounds(settings.print_backgrounds)?;
    Ok(())
}

fn orientation_to_native(orientation: Orientation) -> COREWEBVIEW2_PRINT_ORIENTATION {
    match orientation {
        Orientation::Portrait => COREWEBVIEW2_PRINT_ORIENTATION_PORTRAIT,
        Orientation::Landscape => COREWEBVIEW2_PRINT_ORIENTATION_LANDSCAPE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are the one piece of this file that is plain data mapping
    // rather than a COM call, so — like `ui::webview2_blocking`'s own
    // `resource_type_from_context` tests (D59) — worth unit-testing
    // directly even though they can only ever run on an actual Windows
    // checkout (this whole module is `#[cfg(windows)]`, see `ui::mod`).

    #[test]
    fn portrait_maps_to_native_portrait() {
        assert_eq!(
            orientation_to_native(Orientation::Portrait),
            COREWEBVIEW2_PRINT_ORIENTATION_PORTRAIT
        );
    }

    #[test]
    fn landscape_maps_to_native_landscape() {
        assert_eq!(
            orientation_to_native(Orientation::Landscape),
            COREWEBVIEW2_PRINT_ORIENTATION_LANDSCAPE
        );
    }
}
