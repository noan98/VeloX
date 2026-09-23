//! タブの content webview の組み立て (`content_webview_builder`) と、
//! そこに取り付けるサイト権限ハンドラの判定ロジック。

use std::sync::{Arc, Mutex};

use tao::event_loop::EventLoopProxy;
use wry::{
    PageLoadEvent, PermissionKind as WryPermissionKind, PermissionResponse, WebContext, WebView,
    WebViewBuilder,
};

use crate::app::UserEvent;
use crate::browser::site_permissions::{self, PermissionKind, Resolution, SitePermissionStore};
use crate::browser::{FilterList, TabId, WindowId};

use super::content_scripts::{
    content_ipc_event, context_menu_script, devtools_shortcut_script, form_input_script,
    tab_shortcut_script,
};
use super::download_handlers::{
    download_handler_host, with_download_handlers, DownloadHandlerHost,
    DOWNLOAD_HANDLERS_PER_CONTEXT,
};
use super::engine::{
    disable_default_context_menus, new_webview_builder, with_related_content_view,
};
use super::{to_bounds, LogicalRect};

/// A webview's private-browsing/`WebContext` isolation settings, bundled so
/// [`content_webview_builder`] stays under clippy's argument-count lint
/// (docs/decisions.md D49 added the `context` field; `private` moved in
/// alongside it since the two are directly related — see this struct's
/// field docs).
pub(super) struct WebviewIsolation<'a> {
    /// This window's own private-browsing flag (see docs/decisions.md D14,
    /// D74).
    pub(super) private: bool,
    /// The `WebContext` to build this webview against when `private` is
    /// `false` (docs/decisions.md D49). Ignored — not even read — when
    /// `private` is `true`: `.with_incognito(true)` makes `wry`'s WebKitGTK
    /// backend build a fresh ephemeral context per webview regardless of
    /// what is passed here (docs/decisions.md D15), so a private webview's
    /// builder is constructed with `context: None` in the first place (see
    /// `BrowserWindow::context`'s doc comment) rather than relying on that
    /// downstream behavior to discard a real one.
    pub(super) context: Option<&'a mut WebContext>,
    /// An already-alive content webview whose `WebKitWebProcess` this one
    /// should join (docs/decisions.md D54, see [`with_related_content_view`]).
    /// `None` for the very first content webview (there is nothing to join
    /// yet) and always `None` when `private` is `true`: a private webview
    /// goes through wry's `.with_incognito(true)` path, which builds its
    /// own ephemeral `WebContext` per webview (D15) — relating it to
    /// another view would make WebKitGTK take the *related* view's context
    /// instead, silently changing what "private" isolates, so the private
    /// process layout stays exactly as it was before D54 (see
    /// `docs/memory-analysis.md` §9.4/§10).
    pub(super) related: Option<&'a WebView>,
}

/// Site-scoped policy every content webview is built with: the ad/tracker
/// blocklist (docs/decisions.md D17) and the site permission store
/// (docs/decisions.md D60), bundled for the same reason
/// [`WebviewIsolation`] exists — keeping [`content_webview_builder`] under
/// clippy's argument-count lint as the set of cross-cutting, per-tab
/// policies grows.
pub(super) struct ContentPolicy {
    /// Ad/tracker filter rules content blocking matches against.
    pub(super) blocklist: Arc<FilterList>,
    /// Whether content blocking is currently active.
    pub(super) content_blocking_enabled: bool,
    /// Per-origin camera/microphone/geolocation/notifications/clipboard
    /// decisions (Issue #24, docs/decisions.md D60). Loaded once at startup
    /// and shared read-only across every tab's webview — nothing in this
    /// iteration mutates it at runtime, so a plain `Arc` (no lock) is
    /// enough; see the doc comment on [`content_webview_builder`]'s
    /// `with_permission_handler` call for why.
    pub(super) site_permissions: Arc<SitePermissionStore>,
    /// `Config::download_dir_override` (Issue #30, docs/decisions.md D67),
    /// threaded through to this tab's `with_download_handlers` call.
    pub(super) download_dir_override: Option<String>,
}

/// Build the `WebViewBuilder` for a tab's content webview: bounds, initial
/// URL, and navigation/page-load handlers that tag their `UserEvent`s with
/// `id` so `app.rs` knows which tab they belong to.
///
/// Every content webview — the first tab built in [`BrowserWindow::new`](super::BrowserWindow::new), a
/// tab opened later via [`BrowserWindow::open_tab`](super::BrowserWindow::open_tab), and a tab rebuilt on
/// resume from suspension (which reuses `open_tab`) — is built through this
/// one function, which is what makes content blocking apply uniformly
/// regardless of when or how a tab's webview comes into existence: the
/// navigation handler checks `blocklist` before firing
/// [`UserEvent::NavigationStarted`], refusing the navigation (returning
/// `false`) and reporting [`UserEvent::NavigationBlocked`] instead when
/// `content_blocking_enabled` is set and the URL matches (see
/// docs/decisions.md D17). The same uniformity is why the permission
/// handler below (docs/decisions.md D60) lives here too, rather than being
/// bolted on only where a tab happens to be created first.
pub(super) fn content_webview_builder<'a>(
    own_id: WindowId,
    id: TabId,
    url: &str,
    content_rect: LogicalRect,
    proxy: &EventLoopProxy<UserEvent>,
    isolation: WebviewIsolation<'a>,
    policy: ContentPolicy,
) -> WebViewBuilder<'a> {
    let WebviewIsolation {
        private,
        context,
        related,
    } = isolation;
    let ContentPolicy {
        blocklist,
        content_blocking_enabled,
        site_permissions,
        download_dir_override,
    } = policy;
    // Never relate a private webview (see `WebviewIsolation::related`).
    let related = if private { None } else { related };
    let nav_proxy = proxy.clone();
    let block_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let ipc_proxy = proxy.clone();
    let new_window_proxy = proxy.clone();
    // Current origin of this tab, for the permission handler below
    // (docs/decisions.md D60): `with_permission_handler`'s callback
    // receives only a `PermissionKind`, no URL/origin (see the vendored
    // `wry` source cited in D60), so this webview's navigation handler —
    // the one place in this builder that *does* see every URL it loads —
    // keeps it up to date. Starts from the tab's initial `url` so a
    // permission requested before the first `NavigationStarted` (unlikely,
    // but not impossible) still resolves against a real origin rather than
    // `None`. A `Mutex` (not a `Cell`, which is not `Sync`) because
    // `with_permission_handler` requires `Send + Sync` — unlike the rest of
    // this file's state, which is never read outside the main thread's
    // `UserEvent` dispatch (see the crate-level architecture doc comment),
    // this closure may run off it, since the trait bound is the same across
    // every `wry` platform backend regardless of whether a given one
    // actually needs it.
    let current_origin = Arc::new(Mutex::new(site_permissions::origin_of(url)));
    let permission_origin = Arc::clone(&current_origin);
    let builder = with_related_content_view(new_webview_builder(context), related)
        .with_bounds(to_bounds(content_rect))
        .with_url(url)
        // Ephemeral (non-persistent) cookies/storage/cache for the page
        // content itself; see docs/decisions.md D15 for the per-platform
        // backing (WebKitGTK ephemeral WebContext / WKWebsiteDataStore
        // nonPersistentDataStore / WebView2 private-mode controller option).
        // The toolbar webview now gets the same treatment for the same
        // reason (see the `with_incognito` call on `toolbar_builder` in
        // `BrowserWindow::new`) since #11's favicon rendering gave it its
        // own path to page-controlled URLs. Every tab's webview goes through
        // this builder, so tabs opened later - and suspended tabs rebuilt on
        // resume - stay ephemeral too.
        .with_incognito(private)
        // DevTools (see docs/decisions.md D18): every content webview built
        // through this one function — the initial tab, a newly opened tab,
        // and a suspended tab rebuilt on resume — gets the inspector enabled,
        // the F12/Cmd+Opt+I capture script injected, and its own untrusted
        // devtools IPC channel, so the shortcut keeps working no matter when
        // or how the webview came to exist.
        .with_devtools(true)
        .with_initialization_script(devtools_shortcut_script())
        // Tab-management keyboard shortcuts (see docs/decisions.md D23):
        // same treatment, same trust boundary, same untrusted IPC channel
        // below — just a second injected script and a second fixed set of
        // sentinel strings, rather than growing the devtools one to mean two
        // different things.
        .with_initialization_script(tab_shortcut_script())
        // Right-click context menu (Issue #39, see docs/decisions.md D78):
        // a third injected script, same treatment as the two above —
        // captures the click target and suppresses the engine's native
        // menu (`event.preventDefault()`, honored cross-engine — see the
        // script's own doc comment) so VeloX's own menu can replace it.
        .with_initialization_script(context_menu_script())
        // Form-input detection (Issue #272, see docs/decisions.md D142):
        // a fourth injected script, same channel and the same fixed
        // sentinel treatment — but the only one injected into *subframes*
        // too (`for_main_frame_only = false`), since the forms most worth
        // protecting are routinely in a cross-origin iframe. See
        // `form_input_script`'s own doc comment for why a subframe relays
        // through its parent instead of calling `window.ipc` itself.
        .with_initialization_script_for_main_only(form_input_script(), false);
    // Defense in depth, Windows only: `with_default_context_menus(false)`
    // is a WebView2-specific setting (`wry::WebViewBuilderExtWindows`, only
    // compiled `#[cfg(windows)]` in wry itself) that disables its native
    // context menu at the engine level. `context_menu_script`'s
    // `event.preventDefault()` is expected to already suppress it on every
    // platform per the standard DOM contract, so this is redundant in the
    // common case — but costs nothing to also set on the one platform
    // CLAUDE.md prioritizes, in case some edge case (e.g. a WebView2
    // version quirk) ever lets the native menu through despite
    // `preventDefault()`. No equivalent builder option exists for
    // WebKitGTK/WKWebView in wry 0.56, so nothing is done there beyond the
    // JS-level suppression every platform already gets.
    let builder = disable_default_context_menus(builder);
    let builder = builder
        .with_navigation_handler(move |url| {
            if content_blocking_enabled && blocklist.is_blocked(&url) {
                let _ = block_proxy.send_event(UserEvent::NavigationBlocked(own_id, id, url));
                return false;
            }
            // Keep the permission handler's notion of "current origin" in
            // sync with what this tab is actually about to show — see
            // `current_origin`'s doc comment above and docs/decisions.md
            // D60. Only updated for a navigation that is actually allowed
            // to proceed (this function already returned above for a
            // blocked one), so a denied navigation never overwrites the
            // origin of the page still on screen. `if let Ok` rather than
            // `unwrap`/`expect`: a poisoned mutex (only possible if some
            // other holder of this `Arc` panicked while holding the lock)
            // must not crash page navigation — see CLAUDE.md's
            // "`unwrap()`/`expect()` の乱用を避ける" rule; worst case the
            // permission handler below keeps resolving against a stale
            // origin instead.
            if let Ok(mut origin) = current_origin.lock() {
                *origin = site_permissions::origin_of(&url);
            }
            let _ = nav_proxy.send_event(UserEvent::NavigationStarted(own_id, id, url));
            true
        })
        .with_on_page_load_handler(move |event, url| {
            let event = match event {
                PageLoadEvent::Started => UserEvent::LoadStarted(own_id, id, url),
                PageLoadEvent::Finished => UserEvent::LoadFinished(own_id, id, url),
            };
            let _ = load_proxy.send_event(event);
        })
        .with_ipc_handler(move |request| {
            if let Some(event) = content_ipc_event(own_id, id, request.body()) {
                let _ = ipc_proxy.send_event(event);
            }
        })
        // `target="_blank"` links and `window.open()` (see docs/decisions.md
        // D25): wry's `with_new_window_req_handler` fires synchronously with
        // the requested URL on every backend (WebKitGTK's `create` signal,
        // WebView2's `NewWindowRequested`, WKWebView's
        // `createWebViewWithConfiguration:...`). We always `Deny` — never
        // `Allow` (a bare native window outside VeloX's tab model) or
        // `Create` (would need a platform-specific webview sharing the
        // opener's configuration; see D25) — and instead open the URL as a
        // new VeloX tab ourselves, the same way `ToolbarCommand::NewTab`
        // does but at the requested URL instead of the homepage.
        .with_new_window_req_handler(move |url, _features| {
            let _ = new_window_proxy.send_event(UserEvent::NewTabRequested(own_id, url));
            wry::NewWindowResponse::Deny
        })
        // Site permissions (Issue #24, docs/decisions.md D60): `wry` 0.56
        // does have a cross-platform permission-request hook — unlike D17's
        // content-blocking investigation, this one is not a dead end — but
        // it hands the handler only a `PermissionKind`, synchronously, with
        // no origin and no way to suspend the decision for a custom prompt
        // (see D60 for the exact source citations). `resolve_permission`
        // below is the actual decision logic (unit-tested directly, no
        // `wry` involved); this closure is just wiring: look up the
        // request's origin from `permission_origin`, ask the store, and
        // translate the answer into what `wry` expects.
        .with_permission_handler(move |kind| {
            let origin = permission_origin.lock().ok().and_then(|o| o.clone());
            resolve_permission(&site_permissions, origin.as_deref(), kind)
        });
    // Downloads (Issue #16, docs/decisions.md D28 / D53): only where wry
    // scopes download handlers to the webview itself. On WebKitGTK in
    // non-private mode they belong to the shared `WebContext` and are
    // registered once, on the toolbar webview in `BrowserWindow::new` —
    // see `download_handler_host`.
    match download_handler_host(private, DOWNLOAD_HANDLERS_PER_CONTEXT) {
        DownloadHandlerHost::EachContentWebview => {
            with_download_handlers(builder, own_id, proxy, download_dir_override)
        }
        DownloadHandlerHost::SharedContext => builder,
    }
}

/// Map `wry`'s permission-kind enum onto VeloX's own
/// [`crate::browser::site_permissions::PermissionKind`] (Issue #24,
/// docs/decisions.md D60). `wry::PermissionKind` is `#[non_exhaustive]` and
/// covers far more kinds than VeloX's own list tracks (window management,
/// MIDI, local fonts, ...) — every one of those, and anything a future
/// `wry` version adds, falls into the wildcard arm and maps to
/// [`PermissionKind::Other`], which [`SitePermissionStore::resolve`] always
/// blocks regardless of any stored decision. This is the browser-agnostic
/// enum staying decoupled from `wry` (see `site_permissions`' module doc
/// comment) while still degrading safely for anything it does not
/// explicitly know about.
fn map_permission_kind(kind: WryPermissionKind) -> PermissionKind {
    match kind {
        WryPermissionKind::Camera => PermissionKind::Camera,
        WryPermissionKind::Microphone => PermissionKind::Microphone,
        WryPermissionKind::Geolocation => PermissionKind::Geolocation,
        WryPermissionKind::Notifications => PermissionKind::Notifications,
        WryPermissionKind::ClipboardRead => PermissionKind::ClipboardRead,
        _ => PermissionKind::Other,
    }
}

/// Decide how to answer one `wry` permission request (docs/decisions.md
/// D60): the actual logic behind [`content_webview_builder`]'s
/// `with_permission_handler` closure, pulled out as a free function so it
/// is unit-testable without building a real webview.
///
/// `origin` is `None` for a request with no known/meaningful origin (the
/// tab has not navigated anywhere with an `http`/`https` origin yet, or
/// [`crate::browser::site_permissions::origin_of`] rejected its URL, e.g.
/// `file://`/`about:`) — treated the same as an unmapped
/// [`PermissionKind::Other`]: always [`PermissionResponse::Deny`], never
/// looked up in `store`, since a permission decision with nothing to scope
/// it to cannot be "site-scoped" at all.
///
/// For a known origin and a kind VeloX tracks, an explicit stored decision
/// ([`Resolution::Allow`]/[`Resolution::Block`]) is applied directly — no
/// `wry`-native prompt runs in that case. With no stored decision
/// ([`Resolution::Ask`]) this returns [`PermissionResponse::Default`],
/// which — per the doc comment on `wry`'s `with_permission_handler` (see
/// D60) — hands the request to the platform's own default behavior:
/// WebView2 and WKWebView show their native permission prompt (so the user
/// still sees an explicit Allow/Block UI, just not a VeloX-drawn one) while
/// WebKitGTK denies. VeloX has no way to observe what the user chose in
/// that native prompt (see D60), so it is never written back into `store`
/// — only an explicit VeloX-level decision (not implemented in this
/// iteration; see D60's "future work") ever is.
fn resolve_permission(
    store: &SitePermissionStore,
    origin: Option<&str>,
    kind: WryPermissionKind,
) -> PermissionResponse {
    let Some(origin) = origin else {
        return PermissionResponse::Deny;
    };
    match store.resolve(origin, map_permission_kind(kind)) {
        Resolution::Allow => PermissionResponse::Allow,
        Resolution::Block => PermissionResponse::Deny,
        Resolution::Ask => PermissionResponse::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Site permissions (Issue #24, docs/decisions.md D60) ---

    use crate::browser::site_permissions::PermissionDecision;

    /// `origin` の `kind` に `decision` を 1 件だけ保存したストア。
    fn store_with(
        origin: &str,
        kind: PermissionKind,
        decision: PermissionDecision,
    ) -> SitePermissionStore {
        let mut store = SitePermissionStore::new();
        store.set(origin, kind, decision, 1);
        store
    }

    #[test]
    fn known_wry_kinds_map_onto_velox_kinds() {
        for (wry_kind, expected) in [
            (WryPermissionKind::Camera, PermissionKind::Camera),
            (WryPermissionKind::Microphone, PermissionKind::Microphone),
            (WryPermissionKind::Geolocation, PermissionKind::Geolocation),
            (
                WryPermissionKind::Notifications,
                PermissionKind::Notifications,
            ),
            (
                WryPermissionKind::ClipboardRead,
                PermissionKind::ClipboardRead,
            ),
        ] {
            assert_eq!(map_permission_kind(wry_kind), expected, "{wry_kind:?}");
        }
    }

    #[test]
    fn unmapped_wry_kinds_fall_back_to_other() {
        // A representative sample of the kinds VeloX does not track —
        // every one of these must land on `Other`, which always denies.
        for wry_kind in [
            WryPermissionKind::Midi,
            WryPermissionKind::WindowManagement,
            WryPermissionKind::DisplayCapture,
            WryPermissionKind::Other,
        ] {
            assert_eq!(
                map_permission_kind(wry_kind),
                PermissionKind::Other,
                "{wry_kind:?}"
            );
        }
    }

    #[test]
    fn resolve_permission_denies_when_there_is_no_origin() {
        let store = SitePermissionStore::new();
        assert_eq!(
            resolve_permission(&store, None, WryPermissionKind::Camera),
            PermissionResponse::Deny
        );
    }

    #[test]
    fn resolve_permission_denies_an_unmapped_kind_even_with_a_known_origin() {
        let store = SitePermissionStore::new();
        assert_eq!(
            resolve_permission(&store, Some("https://example.com"), WryPermissionKind::Midi),
            PermissionResponse::Deny
        );
    }

    #[test]
    fn resolve_permission_defers_to_the_platform_default_when_undecided() {
        let store = SitePermissionStore::new();
        assert_eq!(
            resolve_permission(
                &store,
                Some("https://example.com"),
                WryPermissionKind::Camera
            ),
            PermissionResponse::Default
        );
    }

    #[test]
    fn resolve_permission_applies_a_stored_allow() {
        let store = store_with(
            "https://example.com",
            PermissionKind::Camera,
            PermissionDecision::Allow,
        );
        assert_eq!(
            resolve_permission(
                &store,
                Some("https://example.com"),
                WryPermissionKind::Camera
            ),
            PermissionResponse::Allow
        );
    }

    #[test]
    fn resolve_permission_applies_a_stored_block() {
        let store = store_with(
            "https://example.com",
            PermissionKind::Microphone,
            PermissionDecision::Block,
        );
        assert_eq!(
            resolve_permission(
                &store,
                Some("https://example.com"),
                WryPermissionKind::Microphone
            ),
            PermissionResponse::Deny
        );
    }

    #[test]
    fn resolve_permission_is_scoped_to_the_given_origin() {
        let store = store_with(
            "https://a.example",
            PermissionKind::Camera,
            PermissionDecision::Allow,
        );
        assert_eq!(
            resolve_permission(&store, Some("https://b.example"), WryPermissionKind::Camera),
            PermissionResponse::Default
        );
    }
}
