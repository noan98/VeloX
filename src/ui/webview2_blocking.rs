//! Windows-only (WebView2) subresource content blocking (Issue #22).
//!
//! See `docs/decisions.md` D59 for the full re-investigation this module is
//! built on (D17 only checked `wry::WebViewBuilder`'s own `with_*` hooks and
//! concluded no platform exposed a subresource hook through wry; it missed
//! that `wry::WebViewExtWindows` — a *post-build* extension trait on the
//! already-constructed `wry::WebView`, not a builder method — hands back the
//! raw platform webview object directly). In short:
//!
//! - [`wry::WebViewExtWindows::webview`] (stable, safe, public API) returns
//!   the `ICoreWebView2` COM object wry's own webview2 backend already owns
//!   internally.
//! - `ICoreWebView2::AddWebResourceRequestedFilter` +
//!   `add_WebResourceRequested` is WebView2's native per-request
//!   interception hook (`webview2-com-sys` binds it; wry itself calls this
//!   exact pair internally, but only to serve its own `with_custom_protocol`
//!   feature — see `wry::webview2::InnerWebView::attach_custom_protocol_handler`
//!   in the vendored source). Nothing stops a second, independent listener
//!   from being registered from outside the crate, which is what this file
//!   does.
//!
//! Matching itself — URL/resource-type/site-exception decision — is pure,
//! OS-independent logic in `browser::subresource`, unit-tested there without
//! any of the COM machinery below. Everything in this file is the glue
//! needed to feed that function real WebView2 requests and act on its
//! answer. It has not been exercised on an actual Windows machine — this
//! project's development environment is Linux-only, so verification stopped
//! at `cargo check --target x86_64-pc-windows-msvc` (type-checks, does not
//! link or run); see D59 for the details of what was and was not verified.

use std::sync::Arc;

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Environment, ICoreWebView2WebResourceRequestedEventArgs,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
};
use webview2_com::{take_pwstr, WebResourceRequestedEventHandler};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::System::Com::IStream;
use wry::{WebView, WebViewExtWindows};

use crate::app::UserEvent;
use crate::browser::{is_blocked_resource, FilterList, ResourceType, SiteExceptions, TabId};

/// HTTP status VeloX answers a blocked subresource request with. Any value
/// works from WebView2's point of view (the request simply never reaches
/// the network and the page sees this response instead) — 403 was picked
/// because it is the closest standard meaning ("this was refused on
/// purpose", as opposed to e.g. 404 "not found").
const BLOCKED_STATUS: i32 = 403;

/// Attach WebView2's `WebResourceRequested` listener to `webview` so every
/// subresource request it makes is matched against `blocklist`/`exceptions`
/// (`browser::subresource::is_blocked_resource`) and answered with an empty
/// [`BLOCKED_STATUS`] response instead of being allowed to reach the network
/// when it matches. A no-op when `enabled` is `false`, mirroring the same
/// flag `content_webview_builder`'s navigation handler checks for
/// main-frame blocking (docs/decisions.md D17) — nothing is registered at
/// all, so a browser started with content blocking off pays no per-request
/// overhead here.
///
/// Registration failures (both `AddWebResourceRequestedFilter` and
/// `add_WebResourceRequested` return `windows::core::Result`) are logged to
/// stderr and otherwise ignored, never propagated as a hard error: the same
/// "a UI/engine integration failure must not crash the browser" rule
/// `app.rs`'s `log_failure` follows for every other webview call. Losing
/// subresource blocking for one tab is a degraded experience, not a reason
/// to take the whole window down.
pub fn attach(
    webview: &WebView,
    id: TabId,
    blocklist: Arc<FilterList>,
    exceptions: Arc<SiteExceptions>,
    enabled: bool,
    proxy: EventLoopProxy<UserEvent>,
) {
    if !enabled {
        return;
    }

    let core = webview.webview();
    let env = webview.environment();
    // A second, independent COM reference to the same `ICoreWebView2` (COM
    // objects are reference-counted; `.clone()` is an `AddRef`, not a deep
    // copy) so the request handler below can query the page's current URL
    // via `ICoreWebView2::Source` without needing a borrow of `core` itself,
    // which is moved into the `add_WebResourceRequested` call further down.
    let core_for_source = core.clone();

    // SAFETY: `core` is a live `ICoreWebView2` obtained through wry's own
    // `WebViewExtWindows::webview()`. `windows-rs` marks every COM method
    // call `unsafe` because the compiler cannot verify a foreign vtable
    // call succeeds or that the pointer behind it is still valid; here it
    // is always valid because it is the same object wry keeps alive for the
    // entire lifetime of `webview` (`&WebView`, still borrowed by the
    // caller), and this function does not retain `core`/`core_for_source`
    // past the request handler's own lifetime. `AddWebResourceRequestedFilter`
    // with a `"*"` filter and `COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL` is the
    // same "match everything, let the handler decide" pattern wry's own
    // custom-protocol code uses internally.
    let filter_result = unsafe {
        core.AddWebResourceRequestedFilter(
            &HSTRING::from("*"),
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
        )
    };
    if let Err(err) = filter_result {
        eprintln!("velox: WebResourceRequestedFilter の登録に失敗しました: {err}");
        return;
    }

    let mut token: i64 = 0;
    // SAFETY: same COM-call reasoning as above. The handler closure itself
    // only touches `args` (an event-args object WebView2 hands us for the
    // duration of this one callback) and `core_for_source`/`env` (COM
    // references we hold for as long as this listener is registered, i.e.
    // for the tab's whole lifetime) — never a pointer whose validity we
    // cannot account for.
    let register_result = unsafe {
        core.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_sender, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                handle_request(
                    &args,
                    &core_for_source,
                    &env,
                    &blocklist,
                    &exceptions,
                    id,
                    &proxy,
                )
            })),
            &mut token,
        )
    };
    if let Err(err) = register_result {
        eprintln!("velox: WebResourceRequested の登録に失敗しました: {err}");
    }
}

/// Decide one request and, if it matches, answer it with a blocked
/// response. Returns whatever the failing COM call returned so WebView2
/// sees a proper `HRESULT` failure if a getter/setter itself errors — never
/// a value VeloX invents (the "should this be blocked" decision below is
/// plain Rust and never itself fails).
fn handle_request(
    args: &ICoreWebView2WebResourceRequestedEventArgs,
    core: &ICoreWebView2,
    env: &ICoreWebView2Environment,
    blocklist: &FilterList,
    exceptions: &SiteExceptions,
    id: TabId,
    proxy: &EventLoopProxy<UserEvent>,
) -> windows::core::Result<()> {
    // SAFETY: `args`/`core`/`env` are live COM references for the duration
    // of this call (see `attach`'s doc comment); every call below is a
    // plain COM getter/setter.
    let (url, resource_type) = unsafe {
        let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
        args.ResourceContext(&mut context)?;

        let request = args.Request()?;
        let mut uri = PWSTR::null();
        request.Uri(&mut uri)?;

        (take_pwstr(uri), resource_type_from_context(context))
    };

    // SAFETY: same as above — `ICoreWebView2::Source` is a plain COM
    // getter.
    let page_host = unsafe { current_page_host(core) };

    if !is_blocked_resource(
        blocklist,
        exceptions,
        page_host.as_deref(),
        resource_type,
        &url,
    ) {
        return Ok(());
    }

    // SAFETY: `env.CreateWebResourceResponse` and `args.SetResponse` are
    // plain COM calls on live references, same as every call above.
    unsafe {
        let response = env.CreateWebResourceResponse(
            None::<&IStream>,
            BLOCKED_STATUS,
            &HSTRING::from("Blocked by VeloX"),
            &HSTRING::from(""),
        )?;
        args.SetResponse(&response)?;
    }
    let _ = proxy.send_event(UserEvent::SubresourceBlocked(id, url));
    Ok(())
}

/// The host of the page currently loaded in `core`, if any. Used only to
/// check the per-site exception list (`browser::SiteExceptions`) — VeloX
/// does not do first-party/third-party request classification here.
///
/// # Safety
/// `core` must be a live `ICoreWebView2`; `Source` is a plain COM getter.
unsafe fn current_page_host(core: &ICoreWebView2) -> Option<String> {
    let mut source = PWSTR::null();
    core.Source(&mut source).ok()?;
    let source = take_pwstr(source);
    url::Url::parse(&source)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// Map WebView2's resource-context enum to VeloX's engine-agnostic
/// [`ResourceType`]. `XmlHttpRequest` and `Fetch` collapse to one variant
/// (see `ResourceType::XhrOrFetch`'s doc comment); every context this browser
/// has no dedicated variant for (manifests, pings, event sources, web
/// sockets' handshake, ...) falls back to `ResourceType::Other`, which is
/// still fully subject to `FilterList` — only `Document` is special-cased.
fn resource_type_from_context(context: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> ResourceType {
    if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT {
        ResourceType::Document
    } else if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET {
        ResourceType::Stylesheet
    } else if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE {
        ResourceType::Image
    } else if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT {
        ResourceType::Font
    } else if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT {
        ResourceType::Script
    } else if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST
        || context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH
    {
        ResourceType::XhrOrFetch
    } else if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA {
        ResourceType::Media
    } else if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET {
        ResourceType::WebSocket
    } else {
        ResourceType::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `resource_type_from_context` is the one piece of this file that is
    // plain data mapping rather than a COM call, so it is worth unit-testing
    // directly even though the rest of the module cannot run outside a real
    // WebView2 host.

    #[test]
    fn document_context_maps_to_document() {
        assert_eq!(
            resource_type_from_context(COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT),
            ResourceType::Document
        );
    }

    #[test]
    fn xhr_and_fetch_both_map_to_xhr_or_fetch() {
        assert_eq!(
            resource_type_from_context(COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST),
            ResourceType::XhrOrFetch
        );
        assert_eq!(
            resource_type_from_context(COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH),
            ResourceType::XhrOrFetch
        );
    }

    #[test]
    fn unrecognized_context_maps_to_other() {
        assert_eq!(
            resource_type_from_context(COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL),
            ResourceType::Other
        );
    }
}
