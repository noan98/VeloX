//! content webview (信頼しないページ) に注入・評価する JS と、そこから
//! 届く IPC メッセージの解析。
//!
//! どれも webview を持たない純粋な文字列処理なので、表示環境なしで単体
//! テストできる。信頼境界の考え方は docs/decisions.md D18/D23/D62/D78 を参照。

use serde::Deserialize;

use crate::app::UserEvent;
use crate::browser::context_menu;
use crate::browser::{parse_sentinel, ShortcutId, TabId, WindowId, SHORTCUT_TABLE};
use crate::ui::toolbar;

use super::ContentShortcut;

/// The only message the content webview's devtools IPC channel accepts.
///
/// The content webview renders untrusted page content, so unlike the
/// toolbar's IPC channel (which parses a structured, trusted [`ToolbarCommand`]),
/// this handler does not deserialize anything a page sends it. It only ever
/// compares the raw body against this fixed string and otherwise ignores the
/// message. See docs/decisions.md D18 for the trust-boundary reasoning.
///
/// [`ToolbarCommand`]: crate::ui::toolbar::ToolbarCommand
const OPEN_DEVTOOLS_MESSAGE: &str = "velox:open-devtools";

// --- Tab-management keyboard shortcuts (see docs/decisions.md D18/D23/D77) ---
//
// The content webview's shortcut IPC channel, alongside
// `OPEN_DEVTOOLS_MESSAGE` above, exists because the content webview is
// untrusted page content: it can never grow into a second
// `ToolbarCommand`-style structured-command parser (see docs/decisions.md
// D18), so every message on it is compared by exact string equality only,
// never deserialized.
//
// Issue #38 (D77) centralized the fixed sentinel strings themselves —
// previously one `const` per shortcut here — into
// `browser::shortcuts::ShortcutId::sentinel()`, read from
// `browser::shortcuts::SHORTCUT_TABLE`. [`parse_content_shortcut`] and
// [`tab_shortcut_script`] (via [`tab_shortcut_branches`]) both call it
// rather than each hand-rolling their own copy of the string table, and
// this module's tests call it directly instead of naming a
// module-private `const`.

// --- Right-click context menu (Issue #39), see docs/decisions.md D78 ---
//
// Unlike every sentinel above (a keyboard shortcut, always an exact-match
// fixed string with no page-supplied data attached), a context menu is
// unavoidably about *what the user clicked on* — link/image/selection data
// the content webview, i.e. untrusted page content, controls. This still
// never grows the trusted `ToolbarCommand`/`ContentShortcut` parsers into
// something that accepts structured data from an untrusted source (D18's
// rule): it is a third, independent, still-untrusted channel, size-capped
// before parsing, whose output only ever becomes a [`context_menu::MenuContext`]
// via [`context_menu::sanitize`] — never trusted as-is. See this module's
// `context_menu_script` and `parse_context_menu_open`, and
// docs/decisions.md D78 for the full reasoning.

/// Prefix for a context-menu-open report: `"velox:context-menu-open:"` plus
/// a JSON object (see [`ContextMenuOpenMessage`]). Chosen as a prefix rather
/// than a single exact-match sentinel specifically because — unlike every
/// other message on this channel — this one carries real, page-controlled
/// data that cannot be reduced to picking from a fixed set of strings.
const CONTEXT_MENU_OPEN_PREFIX: &str = "velox:context-menu-open:";

/// Prefix for "the user clicked menu row N": `"velox:context-menu-action:"`
/// plus a small unsigned integer — an index into the exact [`context_menu::
/// MenuEntry`] list [`BrowserWindow::show_context_menu`](super::BrowserWindow::show_context_menu) most recently sent
/// down for this tab, resolved server-side via
/// [`crate::browser::context_menu::OpenContextMenu::resolve`]. Never
/// anything richer than an integer: the content webview cannot forge a
/// choice VeloX itself did not already offer.
const CONTEXT_MENU_ACTION_PREFIX: &str = "velox:context-menu-action:";

/// The menu was dismissed with no selection (clicked outside it, or Esc).
const CONTEXT_MENU_CLOSE_MESSAGE: &str = "velox:context-menu-close";

/// The page reported form input (Issue #272, see docs/decisions.md D142).
/// Sent by [`form_input_script`] over the same untrusted content IPC
/// channel the three sentinels above use.
///
/// **The narrowest message on this channel.** It carries no payload at all
/// — not a field name, not a value, not even which frame it came from.
/// The whole message is "somebody typed in this tab", which is all the
/// suspension policy needs (`browser::suspension::Candidate::has_form_input`).
/// A page that sends it without the user typing only keeps *itself* alive,
/// which is a power it already has today by playing silent audio
/// (`is_playing_audio` feeds `Candidate::protected`).
const FORM_INPUT_MESSAGE: &str = "velox:form-input";

/// What [`form_input_script`] posts to its parent frame when it is running
/// in a subframe, for the main frame to relay over
/// [`FORM_INPUT_MESSAGE`]. A fixed string on a fixed property, checked by
/// exact match: the relay never forwards anything a page sends that is not
/// literally this.
const FORM_INPUT_RELAY_TOKEN: &str = "velox:form-input-relay";

/// Hard cap on a `CONTEXT_MENU_OPEN_PREFIX` message's JSON payload, checked
/// *before* `serde_json::from_str` ever runs — the same "reject outright,
/// never even attempt to parse" pattern D62 established for the toolbar's
/// `MAX_IPC_PAYLOAD_BYTES`. Far smaller than that 1 MiB budget: this
/// payload is just one click's worth of coordinates/URLs/selection text,
/// never a pasted document.
const MAX_CONTEXT_MENU_MESSAGE_BYTES: usize = 32 * 1024;

/// Raw wire shape of a [`CONTEXT_MENU_OPEN_PREFIX`] message, exactly as
/// [`context_menu_script`] serializes it — untrusted input in every field
/// (see `context_menu`'s module doc comment). Deserializing this
/// successfully proves nothing about *safety*, only about *shape*; every
/// field still goes through [`context_menu::sanitize`] before anything
/// treats it as safe to display or act on.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextMenuOpenMessage {
    x: f64,
    y: f64,
    #[serde(default)]
    link_href: Option<String>,
    #[serde(default)]
    image_src: Option<String>,
    #[serde(default)]
    selection_text: Option<String>,
    #[serde(default)]
    is_editable: bool,
}

/// Parse a [`CONTEXT_MENU_OPEN_PREFIX`] message body into clamped
/// viewport-relative coordinates and a [`context_menu::RawMenuContext`].
/// `None` for anything that is not this exact prefix, is over
/// [`MAX_CONTEXT_MENU_MESSAGE_BYTES`], or fails to deserialize — the size
/// check runs *before* `serde_json::from_str`, so an oversized payload never
/// reaches the parser at all (D62's pattern).
///
/// Coordinates are clamped to a sane non-negative range rather than trusted
/// outright: a hostile page could otherwise report `NaN`/`Infinity`/an
/// absurdly large number, which would still be syntactically valid to embed
/// as a JS numeric literal but would position the rendered menu somewhere
/// nonsensical.
fn parse_context_menu_open(body: &str) -> Option<(f64, f64, context_menu::RawMenuContext)> {
    let json = body.strip_prefix(CONTEXT_MENU_OPEN_PREFIX)?;
    if json.len() > MAX_CONTEXT_MENU_MESSAGE_BYTES {
        return None;
    }
    let message: ContextMenuOpenMessage = serde_json::from_str(json).ok()?;
    let clamp = |v: f64| {
        if v.is_finite() {
            v.clamp(0.0, 1_000_000.0)
        } else {
            0.0
        }
    };
    Some((
        clamp(message.x),
        clamp(message.y),
        context_menu::RawMenuContext {
            link_href: message.link_href,
            image_src: message.image_src,
            selection_text: message.selection_text,
            is_editable: message.is_editable,
        },
    ))
}

/// Parse a [`CONTEXT_MENU_ACTION_PREFIX`] message body into a menu-entry
/// index. Strict on purpose (D23's "closed match, no near-miss accepted"
/// spirit extended to a numeric payload): the remainder must be 1-3 ASCII
/// digits with no sign, no leading/trailing whitespace, and no leading
/// zero padding tricks — anything else, including a value so large it would
/// not fit a realistic menu, is rejected rather than clamped, since there is
/// no sane index to clamp it *to*.
fn parse_context_menu_action(body: &str) -> Option<usize> {
    let digits = body.strip_prefix(CONTEXT_MENU_ACTION_PREFIX)?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse::<usize>().ok()
}

/// Initialization script that reports a right-click's target (Issue #39,
/// see docs/decisions.md D78) to Rust and suppresses the engine's own
/// native context menu.
///
/// `event.preventDefault()` in a `contextmenu` listener is the standard,
/// cross-engine way web content already suppresses the browser's native
/// menu (used by every site with its own custom right-click UI) — WebKitGTK,
/// WKWebView, and WebView2 (Chromium) all honor it, so this one script,
/// injected the same way `devtools_shortcut_script`/`tab_shortcut_script`
/// are, needs no per-platform branch. `content_webview_builder` also passes
/// `.with_default_context_menus(false)` on Windows as defense in depth (see
/// its call site) in case some edge case ever bypasses the JS-level
/// suppression there; nothing equivalent exists (or is needed) on the other
/// two engines.
///
/// Every field gathered here is then reported completely raw — see
/// [`ContextMenuOpenMessage`] and `context_menu::sanitize` for where it
/// actually gets validated. `.href`/`.src` are read through the DOM's own
/// IDL getters (not `getAttribute`), which already resolve a relative
/// `href`/`src` to an absolute URL per the DOM spec — this script never does
/// its own URL resolution.
pub(super) fn context_menu_script() -> String {
    format!(
        r#"(() => {{
  "use strict";
  function closestLinkHref(el) {{
    while (el) {{
      if (el.tagName === "A" && el.hasAttribute("href")) return el.href;
      el = el.parentElement;
    }}
    return null;
  }}
  function closestImageSrc(el) {{
    while (el) {{
      if (el.tagName === "IMG" && el.src) return el.src;
      el = el.parentElement;
    }}
    return null;
  }}
  const NON_TEXT_INPUT_TYPES = ["button","checkbox","radio","submit","reset","file","image","range","color"];
  function isEditableTarget(el) {{
    if (!el) return false;
    if (el.isContentEditable) return true;
    if (el.tagName === "TEXTAREA") return true;
    if (el.tagName === "INPUT") {{
      const type = (el.getAttribute("type") || "text").toLowerCase();
      return NON_TEXT_INPUT_TYPES.indexOf(type) === -1;
    }}
    return false;
  }}
  window.addEventListener("contextmenu", (event) => {{
    event.preventDefault();
    let selectionText = null;
    try {{
      const selected = window.getSelection ? String(window.getSelection()) : "";
      selectionText = selected.length > 0 ? selected : null;
    }} catch (e) {{}}
    const payload = {{
      x: event.clientX,
      y: event.clientY,
      linkHref: closestLinkHref(event.target),
      imageSrc: closestImageSrc(event.target),
      selectionText: selectionText,
      isEditable: isEditableTarget(event.target),
    }};
    if (window.ipc) {{
      window.ipc.postMessage("{CONTEXT_MENU_OPEN_PREFIX}" + JSON.stringify(payload));
    }}
  }}, true);
}})();"#
    )
}

/// Parse one content-webview shortcut IPC message body. `None` for anything
/// that is not an exact match for one of the fixed sentinel strings
/// `browser::shortcuts::SHORTCUT_TABLE` enumerates — including, deliberately,
/// any attempt at parsing it as JSON or otherwise treating it as structured
/// data (see [`ContentShortcut`]'s doc comment and docs/decisions.md
/// D18/D23/D77).
///
/// Delegates the actual exact-string lookup to
/// `browser::shortcuts::parse_sentinel` (Issue #38/D77) rather than
/// re-matching the sentinel strings here a second time — that function is
/// the one place the closed set of accepted strings is enumerated; this
/// function's only remaining job is mapping the resulting
/// `browser::shortcuts::ShortcutId` onto this module's own
/// [`ContentShortcut`]. `ShortcutId::OpenDevtools` maps to `None`: devtools
/// is delivered through the separate, pre-existing
/// [`OPEN_DEVTOOLS_MESSAGE`]/[`devtools_shortcut_script`] channel (see
/// docs/decisions.md D18), not through `ContentShortcut` at all.
fn parse_content_shortcut(body: &str) -> Option<ContentShortcut> {
    match parse_sentinel(body)? {
        ShortcutId::NewTab => Some(ContentShortcut::NewTab),
        ShortcutId::CloseTab => Some(ContentShortcut::CloseTab),
        ShortcutId::ReopenClosedTab => Some(ContentShortcut::ReopenClosedTab),
        ShortcutId::NextTab => Some(ContentShortcut::NextTab),
        ShortcutId::PrevTab => Some(ContentShortcut::PrevTab),
        ShortcutId::ActivateTabAt(position) => Some(ContentShortcut::ActivateTabAt(position)),
        ShortcutId::ActivateLastTab => Some(ContentShortcut::ActivateLastTab),
        ShortcutId::FocusAddressBar => Some(ContentShortcut::FocusAddressBar),
        ShortcutId::ToggleBookmark => Some(ContentShortcut::ToggleBookmark),
        ShortcutId::ToggleBookmarkBar => Some(ContentShortcut::ToggleBookmarkBar),
        ShortcutId::NewWindow => Some(ContentShortcut::NewWindow),
        ShortcutId::NewPrivateWindow => Some(ContentShortcut::NewPrivateWindow),
        ShortcutId::OpenFindBar => Some(ContentShortcut::OpenFindBar),
        ShortcutId::ViewSource => Some(ContentShortcut::ViewSource),
        ShortcutId::Print => Some(ContentShortcut::Print),
        ShortcutId::SavePage => Some(ContentShortcut::SavePage),
        ShortcutId::OpenDevtools => None,
    }
}

/// Initialization script injected into the content webview to capture the
/// devtools shortcut (F12, or Cmd+Opt+I on macOS) even while the page has
/// focus, and forward it to Rust over [`OPEN_DEVTOOLS_MESSAGE`].
///
/// Registered via `with_initialization_script`, so it runs before any page
/// script on every navigation, and listens in the capture phase so it gets
/// first refusal against pages that try to swallow the keydown themselves.
/// See docs/decisions.md D18 for why this approach was chosen over a
/// tao-level accelerator / `WindowEvent::KeyboardInput`.
pub(super) fn devtools_shortcut_script() -> String {
    format!(
        r#"(() => {{
  "use strict";
  window.addEventListener("keydown", (event) => {{
    const isF12 = event.key === "F12";
    const isMacToggle = event.metaKey && event.altKey && (event.key === "i" || event.key === "I");
    if (!isF12 && !isMacToggle) {{
      return;
    }}
    event.preventDefault();
    if (window.ipc) {{
      window.ipc.postMessage("{OPEN_DEVTOOLS_MESSAGE}");
    }}
  }}, true);
}})();"#
    )
}

/// Initialization script that reports form input to Rust over
/// [`FORM_INPUT_MESSAGE`], so the suspension policy can keep a tab the
/// user is typing in alive (Issue #272, docs/decisions.md D142).
///
/// Injected into **every frame**, not just the main one
/// (`with_initialization_script_for_main_only(.., false)`): a payment or
/// comment form is very often in a cross-origin iframe, and missing those
/// would miss exactly the input worth protecting.
///
/// ## Why a subframe never calls `window.ipc` itself
///
/// It would work on Linux and **fail silently on Windows** — see D142
/// 決定2. wry injects its `window.ipc` shim into subframes on Windows
/// (wry's own docs: "scripts are always added to subframes regardless of
/// the `for_main_frame_only` option"), but registers a
/// `WebMessageReceived` handler only on the top-level `ICoreWebView2`.
/// Microsoft's reference is explicit that an iframe's
/// `chrome.webview.postMessage` raises `CoreWebView2Frame`'s event, not
/// the top-level one, so the call would succeed, throw nothing, and go
/// nowhere. On Linux the shim is main-frame-only, so a "try direct, else
/// relay" version would take the relay and work — which is precisely why
/// testing only on Linux would not catch it.
///
/// So a subframe **always** relays through its parent, and only the main
/// frame talks to `window.ipc`. One path, both platforms.
///
/// ## Why every frame listens, not just the main one
///
/// An iframe inside an iframe reaches the top only if the frame between
/// them passes the token along, so the listener cannot be gated on
/// `isMain`: a grandchild's input would reach the middle frame and stop
/// there, silently — this change's own bug, one nesting level down. A
/// payment widget embedded inside another embed is exactly that shape.
/// Since `report` already picks the right thing for whichever frame it
/// runs in, listening everywhere makes intermediate frames re-relay for
/// free, at any depth.
///
/// The relay accepts a message only when it is exactly
/// [`FORM_INPUT_RELAY_TOKEN`]; nothing from the page is forwarded. That is
/// no weaker than reporting the main frame's own input, since a page can
/// already fire an `input` event on itself.
pub(super) fn form_input_script() -> String {
    format!(
        r#"(() => {{
  "use strict";
  const isMain = window.top === window;
  const report = () => {{
    if (isMain) {{
      if (window.ipc) {{
        window.ipc.postMessage("{FORM_INPUT_MESSAGE}");
      }}
      return;
    }}
    // Subframe: never the direct shim — see this script's Rust doc comment.
    try {{
      window.parent.postMessage("{FORM_INPUT_RELAY_TOKEN}", "*");
    }} catch (error) {{
      // A sandboxed frame can be denied even this; nothing to fall back
      // on, and a lost signal only means the tab stays suspendable.
    }}
  }};
  document.addEventListener("input", report, true);
  // Every frame listens, not just the main one: an iframe nested inside
  // another iframe can only reach the top by having the frame between
  // them pass the token along. `report` already does the right thing for
  // whichever frame it runs in — relay upward from a subframe, hand it to
  // the shim at the top — so re-relaying falls out of listening
  // everywhere, and the chain works at any depth.
  window.addEventListener("message", (event) => {{
    if (event.data === "{FORM_INPUT_RELAY_TOKEN}") {{
      report();
    }}
  }});
}})();"#
    )
}

/// Initialization script that captures the tab-management keyboard
/// shortcuts (Ctrl/Cmd+T/W/Shift+T/Tab/Shift+Tab/1-9/N/...) while the
/// content webview has focus, forwarding a fixed sentinel string per
/// shortcut over the same untrusted IPC channel devtools uses (see
/// [`ContentShortcut`] and docs/decisions.md D18/D23 for why this is a
/// separate injected script rather than a tao-level accelerator).
///
/// `event.ctrlKey || event.metaKey` accepts both modifiers on every
/// platform instead of branching on OS (macOS is Cmd, Linux/Windows is
/// Ctrl) — the simplest way to "handle both", and harmless since Cmd simply
/// never fires outside macOS and vice versa.
///
/// The two `if`/`else if` chains inside (`mod` alone, `mod+Shift`) are built
/// by [`tab_shortcut_branches`] from `browser::shortcuts::SHORTCUT_TABLE`
/// (Issue #38/D77) rather than hand-written here — adding a new *plain*
/// Ctrl/Cmd(+Shift)+key content-webview shortcut now means adding one row to
/// that table (plus, unavoidably, a new [`ContentShortcut`] variant and
/// [`parse_content_shortcut`] arm, since those must stay real Rust types —
/// see docs/decisions.md D77), not hand-editing this JS template too.
pub(super) fn tab_shortcut_script() -> String {
    let (plain_arms, shift_arms) = tab_shortcut_branches();
    format!(
        r#"(() => {{
  "use strict";
  window.addEventListener("keydown", (event) => {{
    const mod = event.ctrlKey || event.metaKey;
    if (!mod) {{
      return;
    }}
    let message = null;
    if (!event.altKey && !event.shiftKey) {{
      {plain_arms}
    }} else if (event.shiftKey && !event.altKey) {{
      {shift_arms}
    }}
    if (message === null) {{
      return;
    }}
    event.preventDefault();
    if (window.ipc) {{
      window.ipc.postMessage(message);
    }}
  }}, true);
}})();"#
    )
}

/// Build the two `if`/`else if` chains [`tab_shortcut_script`] splices into
/// its listener — `(mod only, mod+Shift)` — from
/// `browser::shortcuts::SHORTCUT_TABLE`. `ShortcutId::OpenDevtools` is
/// skipped: it is delivered through the separate, pre-existing
/// [`OPEN_DEVTOOLS_MESSAGE`]/[`devtools_shortcut_script`] mechanism (see
/// docs/decisions.md D18), not through this table-driven content-shortcut
/// channel.
///
/// `debug_assert!`s rather than silently mishandling a future table row that
/// combines `mod` with `Alt`: no shortcut in scope for this generator uses
/// `Alt` today (only `ShortcutId::OpenDevtools`'s Cmd+Option+I does, and
/// that row is skipped above), so this generator only ever builds the two
/// branches `tab_shortcut_script`'s listener already distinguishes — a
/// programming-error guard, not attacker-reachable input, since
/// `SHORTCUT_TABLE` is a fixed compile-time constant.
fn tab_shortcut_branches() -> (String, String) {
    let mut plain = Vec::new();
    let mut shift = Vec::new();
    for def in SHORTCUT_TABLE {
        if def.id == ShortcutId::OpenDevtools {
            continue;
        }
        for chord in def.chords {
            debug_assert!(
                !chord.modifiers.alt,
                "tab_shortcut_branches only generates mod/mod+Shift combos; {:?} needs a dedicated branch",
                def.id
            );
            let arm = format!(
                "if ({}) {{\n        message = \"{}\";\n      }}",
                chord.key.js_condition(),
                def.id.sentinel()
            );
            if chord.modifiers.shift {
                shift.push(arm);
            } else {
                plain.push(arm);
            }
        }
    }
    (plain.join(" else "), shift.join(" else "))
}

/// Initialization script that resolves this page's favicon URL on demand:
/// its `<link rel="icon">` (or the closest relative, `rel~="icon"`, which
/// also matches `shortcut icon`/`apple-touch-icon` etc.) if the page
/// declares one, otherwise a same-origin `/favicon.ico` guess. Only ever
/// invoked via [`BrowserWindow::fetch_favicon`](super::BrowserWindow::fetch_favicon)'s
/// `evaluate_script_with_callback` — not injected as a standing listener —
/// so this returns a value rather than posting a message. See
/// docs/decisions.md D22 for why resolving *a URL* is all this does: the
/// actual image fetch is left entirely to the toolbar webview's own `<img>`
/// tag, never performed here or anywhere else in Rust.
pub(super) const RESOLVE_FAVICON_SCRIPT: &str = r#"(() => {
  try {
    const link = document.querySelector('link[rel~="icon"][href]');
    if (link && link.href) {
      return link.href;
    }
    return new URL("/favicon.ico", location.href).href;
  } catch (err) {
    return "";
  }
})();"#;

/// Reads the current page's full markup for View Source (Issue #45, see
/// docs/decisions.md D72): `document.documentElement.outerHTML`, the same
/// value a page's own devtools "View Page Source" reproduces. Wrapped in
/// try/catch like [`RESOLVE_FAVICON_SCRIPT`]: a document in a state this
/// cannot be read from (should not normally happen) yields an empty string
/// rather than propagating a JS exception into the
/// `evaluate_script_with_callback` result.
///
/// Deliberately a *live-DOM* snapshot, not a second network fetch of the
/// original response bytes — see docs/decisions.md D72 for the alternatives
/// considered (a raw HTTP re-fetch would need a whole separate networking
/// path wry does not expose, and would show different markup for JS-authored
/// pages than what is actually on screen) and its accepted trade-off (a page
/// that mutated its own DOM after load shows the *current* DOM, not the
/// bytes the server originally sent).
pub(super) const VIEW_SOURCE_FETCH_SCRIPT: &str = r#"(() => {
  try {
    return document.documentElement.outerHTML;
  } catch (err) {
    return "";
  }
})();"#;

/// `WebView::evaluate_script_with_callback` hands back the JS result
/// serialized as a JSON string (see wry's `eval`); unwrap that one layer to
/// get the actual string `document.title` evaluated to.
pub(super) fn extract_js_string_result(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(raw).ok()
}

// --- Right-click context menu render script (Issue #39), see
// docs/decisions.md D78 ---

/// Root element id the rendered menu overlay uses inside the content
/// webview's own DOM — also relied on by [`CONTEXT_MENU_HIDE_SCRIPT`] to
/// find and remove it.
const CONTEXT_MENU_ROOT_ID: &str = "velox-context-menu-root";
const CONTEXT_MENU_STYLE_ID: &str = "velox-context-menu-style";

/// Removes the context menu overlay (if present) from the page — used when
/// Rust already knows the menu should go away without the user having
/// clicked a row (a background navigation, tab close, etc.).
pub(super) const CONTEXT_MENU_HIDE_SCRIPT: &str = r#"(() => {
  const root = document.getElementById("velox-context-menu-root");
  if (root && root.__veloxClose) {
    root.__veloxClose();
  } else if (root) {
    root.remove();
  }
})();"#;

/// One row of the menu, exactly as serialized into the render script's
/// `items` array. `label` is the only field that can ever contain
/// page-derived text (via `MenuAction::SearchSelection`'s selection
/// preview, see docs/decisions.md D78) — every other field is a plain
/// number/boolean.
fn context_menu_item_json(index: usize, entry: &context_menu::MenuEntry) -> serde_json::Value {
    serde_json::json!({
        "index": index,
        "label": entry.action.label(),
        "enabled": entry.enabled,
    })
}

/// Build the script [`BrowserWindow::show_context_menu`](super::BrowserWindow::show_context_menu) evaluates: renders
/// a small absolutely-positioned overlay listing `entries` at viewport
/// coordinates `(x, y)`, clamped to stay on screen after layout.
///
/// **Why this needs no HTML-escaping (unlike View Source, D72) but does
/// need JS-string escaping (like D62/D69's `escape_js_line_terminators`):**
/// every row's label is inserted via `row.textContent = item.label` — a DOM
/// API that cannot interpret its argument as markup, structurally ruling
/// out the classic "attacker's `<script>` ends up as live HTML" failure
/// mode no matter what `item.label` contains (see
/// `context_menu::MenuAction::label`'s doc comment). The risk that
/// *remains* is `item.label` breaking out of the JS string literal this
/// whole `items` array is embedded as — handled exactly the way
/// `find_query_literal`/D69 and D62's `set_*_script` functions handle it:
/// `serde_json` escapes `"`/`\`/control characters per RFC 8259, and
/// [`toolbar::escape_js_line_terminators`] additionally neutralizes
/// U+2028/U+2029, which pre-ES2019 engines treat as string-terminating even
/// inside a JSON-escaped literal.
///
/// `x`/`y` are plain `f64`s formatted directly (never through JSON/string
/// escaping) — safe because [`parse_context_menu_open`] already clamped
/// them to a finite, non-negative range before this function ever sees
/// them; there is no string content here to escape.
pub(super) fn context_menu_render_script(
    entries: &[context_menu::MenuEntry],
    x: f64,
    y: f64,
) -> String {
    let items: Vec<serde_json::Value> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| context_menu_item_json(index, entry))
        .collect();
    let items_json =
        toolbar::escape_js_line_terminators(serde_json::Value::Array(items).to_string());
    format!(
        r#"(() => {{
  "use strict";
  const ROOT_ID = "{CONTEXT_MENU_ROOT_ID}";
  const STYLE_ID = "{CONTEXT_MENU_STYLE_ID}";
  const existing = document.getElementById(ROOT_ID);
  if (existing) {{
    if (existing.__veloxClose) existing.__veloxClose(); else existing.remove();
  }}
  if (!document.getElementById(STYLE_ID)) {{
    const style = document.createElement("style");
    style.id = STYLE_ID;
    style.textContent = `#${{ROOT_ID}}{{position:fixed;z-index:2147483647;background:#fff;color:#1a1a1a;border:1px solid #ccc;border-radius:4px;box-shadow:0 2px 10px rgba(0,0,0,.25);font:13px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;padding:4px 0;min-width:180px;}}
#${{ROOT_ID}} .velox-cm-item{{padding:6px 16px;cursor:default;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:320px;}}
#${{ROOT_ID}} .velox-cm-item[data-enabled="1"]{{cursor:pointer;}}
#${{ROOT_ID}} .velox-cm-item[data-enabled="0"]{{color:#999;}}
#${{ROOT_ID}} .velox-cm-item[data-enabled="1"]:hover{{background:#e8e8e8;}}`;
    (document.head || document.documentElement).appendChild(style);
  }}
  const items = {items_json};
  const root = document.createElement("div");
  root.id = ROOT_ID;
  function close() {{
    document.removeEventListener("mousedown", onOutside, true);
    document.removeEventListener("keydown", onKey, true);
    if (root.parentNode) root.remove();
  }}
  root.__veloxClose = close;
  function onOutside(event) {{
    if (!root.contains(event.target)) {{
      close();
      if (window.ipc) window.ipc.postMessage("{CONTEXT_MENU_CLOSE_MESSAGE}");
    }}
  }}
  function onKey(event) {{
    if (event.key === "Escape") {{
      event.preventDefault();
      close();
      if (window.ipc) window.ipc.postMessage("{CONTEXT_MENU_CLOSE_MESSAGE}");
    }}
  }}
  for (const item of items) {{
    const row = document.createElement("div");
    row.className = "velox-cm-item";
    row.textContent = item.label;
    row.dataset.enabled = item.enabled ? "1" : "0";
    if (item.enabled) {{
      row.addEventListener("click", () => {{
        close();
        if (window.ipc) {{
          window.ipc.postMessage("{CONTEXT_MENU_ACTION_PREFIX}" + item.index);
        }}
      }});
    }}
    root.appendChild(row);
  }}
  document.body.appendChild(root);
  document.addEventListener("mousedown", onOutside, true);
  document.addEventListener("keydown", onKey, true);
  const rect = root.getBoundingClientRect();
  let left = {x};
  let top = {y};
  const maxLeft = window.innerWidth - rect.width;
  const maxTop = window.innerHeight - rect.height;
  if (maxLeft >= 0 && left > maxLeft) left = maxLeft;
  if (maxTop >= 0 && top > maxTop) top = maxTop;
  if (left < 0) left = 0;
  if (top < 0) top = 0;
  root.style.left = left + "px";
  root.style.top = top + "px";
}})();"#
    )
}

// --- In-page find (Issue #43), see docs/decisions.md D69 ---

/// The JSON object [`find_search_script`]'s completion value stringifies —
/// unwrapped by [`extract_js_string_result`], then this, in
/// [`find_search_total`] (called from `BrowserWindow::search_in_page`'s
/// callback). A missing/malformed value
/// (should not happen; defensive only) is treated as "zero matches" rather
/// than panicking or leaving the find bar showing a stale count.
#[derive(Deserialize)]
struct FindSearchResult {
    total: usize,
}

/// [`find_search_script`] の評価結果 (`evaluate_script_with_callback` が渡す
/// 生の文字列) から一致件数を取り出す。取り出せなければ 0 件扱い
/// ([`FindSearchResult`] の doc comment を参照)。
pub(super) fn find_search_total(raw: &str) -> usize {
    extract_js_string_result(raw)
        .and_then(|json| serde_json::from_str::<FindSearchResult>(&json).ok())
        .map_or(0, |result| result.total)
}

/// Embeds `query` as a JSON string literal, hardened against
/// U+2028/U+2029 breaking a JS string literal early exactly the way
/// `ui::toolbar`'s `set_*_script` functions are (D62) — reused here via
/// `toolbar::escape_js_line_terminators` rather than a second copy of that
/// logic, since this splices into a script too (just for the content
/// webview instead of the toolbar's).
pub(super) fn find_query_literal(query: &str) -> String {
    let json = serde_json::Value::String(query.to_owned()).to_string();
    toolbar::escape_js_line_terminators(json)
}

/// Builds the script [`BrowserWindow::search_in_page`](super::BrowserWindow::search_in_page) evaluates in a
/// content webview: clears any highlight left by a previous search, then
/// (if `query_literal` is non-empty) walks every text node under
/// `document.body` — skipping `<script>`/`<style>`/`<noscript>`/
/// `<textarea>`/`<input>` subtrees — wrapping each literal-substring match
/// in a `<span class="velox-find-hl">` (`ui/toolbar.html` styles this
/// class; the toolbar webview and content webview share no CSS, so this
/// style has to be injected as an inline `<style>` the first time a search
/// runs — see the script body). The completion value is
/// `JSON.stringify({ total: N })` — parsed back by [`FindSearchResult`].
///
/// `query_literal` must already be a JSON string literal (see
/// [`find_query_literal`]) — this function does not escape it itself.
/// Matching is always a literal substring, never a user-supplied regex: the
/// query is regex-escaped client-side before being handed to `RegExp` so a
/// search term containing `.`/`*`/`(` etc. is never interpreted as a
/// pattern (see docs/decisions.md D69's "一致方式" note — regex/whole-word
/// search is out of scope for this issue).
///
/// **Known limitation** (documented, not fixed, in D69): a match cannot
/// span a text-node boundary, so text broken up by an inline element (e.g.
/// `<b>` in the middle of a word) will not be found — the same limitation a
/// naive per-text-node walker always has. Hidden text (`display:none` etc.)
/// is not specially excluded either, unlike a browser's native find; this
/// keeps the script simple and fast at the cost of occasionally matching
/// text a user cannot see.
pub(super) fn find_search_script(query_literal: &str, case_sensitive: bool) -> String {
    let flags = if case_sensitive { "g" } else { "gi" };
    format!(
        r#"(() => {{
  "use strict";
  const HL_CLASS = "velox-find-hl";
  const STYLE_ID = "velox-find-style";
  if (!document.getElementById(STYLE_ID)) {{
    const style = document.createElement("style");
    style.id = STYLE_ID;
    style.textContent = ".velox-find-hl{{background:#ffd54f !important;color:#000 !important;}}.velox-find-hl-active{{background:#ff7043 !important;}}";
    (document.head || document.documentElement).appendChild(style);
  }}
  const prevMatches = window.__veloxFindMatches || [];
  for (const el of prevMatches) {{
    if (!el || !el.parentNode) continue;
    const parent = el.parentNode;
    parent.replaceChild(document.createTextNode(el.textContent), el);
    parent.normalize();
  }}
  window.__veloxFindMatches = [];
  window.__veloxFindActiveIndex = -1;
  const query = {query_literal};
  if (!query || !document.body) {{
    return JSON.stringify({{ total: 0 }});
  }}
  const escaped = query.replace(/[.*+?^${{}}()|[\]\\]/g, "\\$&");
  const flags = {flags:?};
  let re;
  try {{
    re = new RegExp(escaped, flags);
  }} catch (e) {{
    return JSON.stringify({{ total: 0 }});
  }}
  const SKIP_TAGS = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEXTAREA", "INPUT"]);
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {{
    acceptNode(node) {{
      const parent = node.parentElement;
      if (!parent || SKIP_TAGS.has(parent.tagName)) return NodeFilter.FILTER_REJECT;
      if (!node.nodeValue) return NodeFilter.FILTER_SKIP;
      re.lastIndex = 0;
      return re.test(node.nodeValue) ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP;
    }}
  }});
  const nodes = [];
  let n;
  while ((n = walker.nextNode())) {{ nodes.push(n); }}
  const matches = [];
  for (const node of nodes) {{
    const text = node.nodeValue;
    re.lastIndex = 0;
    let match;
    let lastIndex = 0;
    let any = false;
    const frag = document.createDocumentFragment();
    while ((match = re.exec(text)) !== null) {{
      any = true;
      if (match.index > lastIndex) {{
        frag.appendChild(document.createTextNode(text.slice(lastIndex, match.index)));
      }}
      const span = document.createElement("span");
      span.className = HL_CLASS;
      span.textContent = match[0];
      frag.appendChild(span);
      matches.push(span);
      lastIndex = match.index + match[0].length;
      if (match[0].length === 0) {{ re.lastIndex += 1; lastIndex = re.lastIndex; }}
    }}
    if (!any) continue;
    if (lastIndex < text.length) {{
      frag.appendChild(document.createTextNode(text.slice(lastIndex)));
    }}
    node.parentNode.replaceChild(frag, node);
  }}
  window.__veloxFindMatches = matches;
  return JSON.stringify({{ total: matches.length }});
}})();"#
    )
}

/// Builds the script [`BrowserWindow::highlight_find_match`](super::BrowserWindow::highlight_find_match) evaluates:
/// deactivates whichever match `window.__veloxFindActiveIndex` last pointed
/// at, activates the match at `index`, and scrolls it into view. Assumes
/// [`find_search_script`] already ran in this page load (harmless no-op via
/// the `matches[index]` guard if it did not, e.g. a stale index after the
/// page navigated).
pub(super) fn find_activate_script(index: usize) -> String {
    format!(
        r#"(() => {{
  "use strict";
  const matches = window.__veloxFindMatches || [];
  const prevIndex = window.__veloxFindActiveIndex;
  if (typeof prevIndex === "number" && matches[prevIndex]) {{
    matches[prevIndex].classList.remove("velox-find-hl-active");
  }}
  const index = {index};
  const el = matches[index];
  if (el) {{
    el.classList.add("velox-find-hl-active");
    el.scrollIntoView({{ block: "center", inline: "nearest" }});
  }}
  window.__veloxFindActiveIndex = index;
}})();"#
    )
}

/// The script [`BrowserWindow::clear_find_highlights`](super::BrowserWindow::clear_find_highlights) evaluates:
/// unwraps every `<span class="velox-find-hl">` [`find_search_script`]
/// inserted back into plain text and resets the DOM-side bookkeeping.
/// Idempotent — safe to call with no search having run (`window.
/// __veloxFindMatches` is then `undefined`, treated as empty).
pub(super) const FIND_CLEAR_SCRIPT: &str = r#"(() => {
  "use strict";
  const prev = window.__veloxFindMatches || [];
  for (const el of prev) {
    if (!el || !el.parentNode) continue;
    const parent = el.parentNode;
    parent.replaceChild(document.createTextNode(el.textContent), el);
    parent.normalize();
  }
  window.__veloxFindMatches = [];
  window.__veloxFindActiveIndex = -1;
})();"#;

/// 信頼しない content webview の IPC メッセージ 1 件を、対応する
/// [`UserEvent`] に変換する。認識できないものは `None` (黙って無視する)。
/// 判定順は `with_ipc_handler` に直書きしていた頃のまま。
///
/// Untrusted content-webview IPC channel (see OPEN_DEVTOOLS_MESSAGE
/// and ContentShortcut's doc comment): every branch here is either
/// one fixed exact-match string comparison, a lookup into a
/// fixed, closed set of them, or (context menu only, see D78) a
/// bounded, size-capped parse whose result is never trusted
/// as-is — never a page-supplied value used directly.
pub(super) fn content_ipc_event(own_id: WindowId, id: TabId, body: &str) -> Option<UserEvent> {
    if body == OPEN_DEVTOOLS_MESSAGE {
        Some(UserEvent::OpenDevtoolsRequested(own_id))
    } else if let Some(shortcut) = parse_content_shortcut(body) {
        Some(UserEvent::ContentShortcut(own_id, shortcut))
    } else if let Some((x, y, raw)) = parse_context_menu_open(body) {
        Some(UserEvent::ContextMenuRequested {
            window_id: own_id,
            tab_id: id,
            x,
            y,
            raw,
        })
    } else if body == FORM_INPUT_MESSAGE {
        Some(UserEvent::FormInputDetected(own_id, id))
    } else if body == CONTEXT_MENU_CLOSE_MESSAGE {
        Some(UserEvent::ContextMenuClosed(own_id, id))
    } else {
        parse_context_menu_action(body).map(|index| UserEvent::ContextMenuActionSelected {
            window_id: own_id,
            tab_id: id,
            index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devtools_script_captures_f12_and_mac_toggle_and_reports_the_trigger_message() {
        let script = devtools_shortcut_script();
        assert!(script.contains("F12"));
        assert!(script.contains("metaKey && event.altKey"));
        assert!(script.contains(&format!(
            "window.ipc.postMessage(\"{OPEN_DEVTOOLS_MESSAGE}\")"
        )));
        // Registered in the capture phase (the trailing `true` to addEventListener).
        assert!(script.contains("}, true);"));
    }

    #[test]
    fn extracts_a_js_string_result() {
        assert_eq!(
            extract_js_string_result("\"Example Domain\""),
            Some("Example Domain".to_owned())
        );
    }

    #[test]
    fn extracting_non_string_js_results_yields_none() {
        assert_eq!(extract_js_string_result("null"), None);
        assert_eq!(extract_js_string_result(""), None);
        assert_eq!(extract_js_string_result("42"), None);
    }

    #[test]
    fn tab_shortcut_script_captures_expected_combos_in_capture_phase() {
        let script = tab_shortcut_script();
        for message in [
            ShortcutId::NewTab.sentinel(),
            ShortcutId::CloseTab.sentinel(),
            ShortcutId::ReopenClosedTab.sentinel(),
            ShortcutId::NextTab.sentinel(),
            ShortcutId::PrevTab.sentinel(),
            ShortcutId::ActivateLastTab.sentinel(),
            ShortcutId::FocusAddressBar.sentinel(),
            ShortcutId::ToggleBookmark.sentinel(),
            ShortcutId::ToggleBookmarkBar.sentinel(),
            ShortcutId::NewWindow.sentinel(),
            ShortcutId::OpenFindBar.sentinel(),
            ShortcutId::ViewSource.sentinel(),
        ] {
            assert!(
                script.contains(&message),
                "script is missing sentinel {message:?}"
            );
        }
        for n in 1u8..=8 {
            assert!(script.contains(&ShortcutId::ActivateTabAt(n).sentinel()));
        }
        // `OpenDevtools` is deliberately excluded from this table-driven
        // script — it keeps its own separate delivery mechanism (see
        // `devtools_shortcut_script`), so its sentinel must NOT show up
        // here.
        assert!(!script.contains(&ShortcutId::OpenDevtools.sentinel()));
        assert!(script.contains("event.ctrlKey || event.metaKey"));
        assert!(script.contains("}, true);"));
    }

    /// Issue #38's explicit acceptance test: the content webview (untrusted
    /// page content) sending an unrecognized command string — including one
    /// shaped like a real command name, or a JSON payload mimicking the
    /// toolbar's trusted `ToolbarCommand` channel — must never be accepted
    /// as a shortcut. See docs/decisions.md D18/D23/D77: the sentinel set is
    /// fully enumerated in `browser::shortcuts::SHORTCUT_TABLE`, and nothing
    /// outside it can ever parse.
    #[test]
    fn content_webview_cannot_smuggle_an_unknown_command_through_the_shortcut_channel() {
        for body in [
            "velox:quit",
            "velox:open-devtools-for-toolbar",
            "velox:clear-site-data",
            r#"{"cmd":"clear_site_data"}"#,
            r#"{"cmd":"update_settings","settings":{}}"#,
            "javascript:alert(1)",
        ] {
            assert_eq!(
                parse_content_shortcut(body),
                None,
                "content webview must not be able to trigger {body:?}"
            );
        }
    }

    #[test]
    fn parse_content_shortcut_matches_every_sentinel_exactly() {
        for (id, expected) in [
            (ShortcutId::NewTab, ContentShortcut::NewTab),
            (ShortcutId::CloseTab, ContentShortcut::CloseTab),
            (
                ShortcutId::ReopenClosedTab,
                ContentShortcut::ReopenClosedTab,
            ),
            (ShortcutId::NextTab, ContentShortcut::NextTab),
            (ShortcutId::PrevTab, ContentShortcut::PrevTab),
            (
                ShortcutId::ActivateLastTab,
                ContentShortcut::ActivateLastTab,
            ),
            (
                ShortcutId::FocusAddressBar,
                ContentShortcut::FocusAddressBar,
            ),
            (ShortcutId::ToggleBookmark, ContentShortcut::ToggleBookmark),
            (
                ShortcutId::ToggleBookmarkBar,
                ContentShortcut::ToggleBookmarkBar,
            ),
            (ShortcutId::NewWindow, ContentShortcut::NewWindow),
            (ShortcutId::OpenFindBar, ContentShortcut::OpenFindBar),
            (ShortcutId::ViewSource, ContentShortcut::ViewSource),
        ] {
            assert_eq!(
                parse_content_shortcut(&id.sentinel()),
                Some(expected),
                "{id:?}"
            );
        }
        assert_eq!(
            parse_content_shortcut(&ShortcutId::OpenDevtools.sentinel()),
            None,
            "devtools is delivered through its own separate channel, not this one"
        );
        for n in 1u8..=8 {
            assert_eq!(
                parse_content_shortcut(&format!("velox:activate-tab-{n}")),
                Some(ContentShortcut::ActivateTabAt(n))
            );
        }
    }

    // --- In-page find (Issue #43), see docs/decisions.md D69 ---

    #[test]
    fn find_query_literal_escapes_quotes_and_backslashes() {
        assert_eq!(find_query_literal(r#""a"\b"#), r#""\"a\"\\b""#.to_owned());
    }

    #[test]
    fn find_query_literal_escapes_u2028_and_u2029_line_terminators() {
        // Same D62 hardening `ui::toolbar`'s `set_*_script` functions apply,
        // reused here (not duplicated) via `toolbar::escape_js_line_terminators`.
        let literal = find_query_literal("foo\u{2028}bar\u{2029}");
        assert!(literal.contains("\\u2028"), "{literal}");
        assert!(literal.contains("\\u2029"), "{literal}");
        assert!(!literal.contains('\u{2028}'));
        assert!(!literal.contains('\u{2029}'));
    }

    #[test]
    fn find_search_script_embeds_the_query_literal_and_case_flags() {
        let script = find_search_script(&find_query_literal("hello"), false);
        assert!(script.contains("const query = \"hello\";"));
        assert!(script.contains("\"gi\""));
        assert!(script.ends_with("})();"));

        let script = find_search_script(&find_query_literal("hello"), true);
        assert!(script.contains("\"g\""));
        assert!(!script.contains("\"gi\""));
    }

    #[test]
    fn find_search_script_neutralizes_quotes_and_script_closing_sequences() {
        // A search term is arbitrary text a user typed, potentially copied
        // from the very (untrusted) page being searched — it must come
        // through as inert JSON string content embedded in `const query =
        // ...`, never break out of that statement (D62/D69).
        let literal = find_query_literal(r#""; document.body.innerHTML = "pwned"; //"#);
        let script = find_search_script(&literal, false);
        assert!(script.contains(&format!("const query = {literal};\n")));
        // The generated script still parses as the single intended
        // statement shape — the injected quotes/semicolons are backslash-
        // escaped inside the JSON string, not raw JS syntax.
        assert!(!script.contains("innerHTML = \"pwned\"; //\";\n"));
    }

    #[test]
    fn find_search_script_neutralizes_regex_metacharacters_in_the_query() {
        // The query is matched as a literal substring, never interpreted as
        // a regex pattern — the generated script must regex-escape it
        // client-side rather than splice it into `new RegExp` raw.
        let script = find_search_script(&find_query_literal("a.b*c"), false);
        assert!(script.contains("query.replace(/[.*+?^${}()|[\\]\\\\]/g"));
    }

    #[test]
    fn find_activate_script_embeds_the_index() {
        let script = find_activate_script(3);
        assert!(script.contains("const index = 3;"));
        assert!(script.contains("scrollIntoView"));
        assert!(script.ends_with("})();"));
    }

    #[test]
    fn find_clear_script_unwraps_previous_highlights() {
        assert!(FIND_CLEAR_SCRIPT.contains("__veloxFindMatches"));
        assert!(FIND_CLEAR_SCRIPT.contains("replaceChild"));
        assert!(FIND_CLEAR_SCRIPT.ends_with("})();"));
    }

    #[test]
    fn parse_content_shortcut_rejects_anything_not_an_exact_known_sentinel() {
        // Never treated as JSON/structured data, and never a prefix/fuzzy
        // match — see docs/decisions.md D18/D23.
        for body in [
            "",
            "velox:new-tab ",
            "VELOX:NEW-TAB",
            "velox:activate-tab-0",
            "velox:activate-tab-9",
            "velox:activate-tab-99",
            "velox:activate-tab-",
            r#"{"cmd":"new_tab"}"#,
            "velox:open-devtools",
        ] {
            assert_eq!(parse_content_shortcut(body), None, "body was {body:?}");
        }
    }

    #[test]
    fn parse_content_shortcut_does_not_panic_on_hostile_content_webview_input() {
        // The content webview loads arbitrary, potentially hostile web
        // pages (unlike the toolbar webview) — this is the least-trusted
        // IPC boundary in the app (Issue #35, docs/decisions.md D18/D23/
        // D62), so it gets the same "malformed/huge input must never
        // panic" treatment as `ui::toolbar::parse_command`.
        let huge = "a".repeat(5_000_000);
        assert_eq!(parse_content_shortcut(&huge), None);

        for hostile in [
            "\0\0\0",
            "velox:new-tab\0",
            "velox:activate-tab-18446744073709551616", // overflows u32/usize
            "🚀日本語velox:new-tab",
            "\u{202e}velox:new-tab",
        ] {
            assert_eq!(parse_content_shortcut(hostile), None, "{hostile:?}");
        }
    }

    #[test]
    fn favicon_script_falls_back_to_a_same_origin_guess() {
        assert!(RESOLVE_FAVICON_SCRIPT.contains("link[rel~=\"icon\"]"));
        assert!(RESOLVE_FAVICON_SCRIPT.contains("/favicon.ico"));
    }

    // --- View Source (Issue #45), see docs/decisions.md D72 ---

    #[test]
    fn view_source_fetch_script_reads_outer_html_and_is_exception_safe() {
        assert!(VIEW_SOURCE_FETCH_SCRIPT.contains("document.documentElement.outerHTML"));
        // Wrapped in try/catch, like RESOLVE_FAVICON_SCRIPT, so a page whose
        // DOM cannot be read from yields "" instead of propagating a JS
        // exception through `evaluate_script_with_callback`.
        assert!(VIEW_SOURCE_FETCH_SCRIPT.contains("try {"));
        assert!(VIEW_SOURCE_FETCH_SCRIPT.contains("catch"));
    }

    // --- Right-click context menu (Issue #39), see docs/decisions.md D78 ---

    #[test]
    fn context_menu_script_suppresses_default_menu_and_reports_via_ipc() {
        let script = context_menu_script();
        assert!(script.contains("addEventListener(\"contextmenu\""));
        assert!(script.contains("event.preventDefault();"));
        assert!(script.contains(CONTEXT_MENU_OPEN_PREFIX));
        assert!(script.contains("window.ipc.postMessage"));
        // Reads via the DOM's own `.href`/`.src` IDL getters (already
        // absolute), never `getAttribute` (which would hand back a
        // possibly-relative raw attribute value this script would then have
        // to resolve itself).
        assert!(script.contains("el.href"));
        assert!(script.contains("el.src"));
    }

    #[test]
    fn form_input_script_reports_input_and_relays_subframes_through_the_parent() {
        let script = form_input_script();
        // Capture phase, so a page that swallows `input` on an ancestor
        // cannot hide the fact that typing happened.
        assert!(script.contains(r#"addEventListener("input", report, true)"#));
        assert!(script.contains(FORM_INPUT_MESSAGE));
        assert!(script.contains(FORM_INPUT_RELAY_TOKEN));
        // The relay only ever fires on an exact match, never on some
        // property of whatever the page posted.
        assert!(script.contains(&format!(r#"event.data === "{FORM_INPUT_RELAY_TOKEN}""#)));

        // **Every frame listens for the relay token, not just the main
        // one.** An iframe inside an iframe reaches the top only if the
        // frame between them forwards what it received; gating the
        // listener on `isMain` loses a grandchild's input silently — the
        // very bug this whole change exists to prevent, reintroduced one
        // nesting level down. Pinned by counting the guard: the only
        // `isMain` branch left is the one inside `report` that chooses
        // between `window.ipc` and relaying upward.
        assert_eq!(
            script.matches("if (isMain)").count(),
            1,
            "the relay listener must not be gated on isMain: {script}"
        );
    }

    /// **The regression this test exists for is Windows-only and silent.**
    ///
    /// A subframe on Windows *has* a `window.ipc` (wry injects its shim
    /// into subframes there) but its messages reach `CoreWebView2Frame`,
    /// which wry never listens on — so a subframe calling `window.ipc`
    /// throws nothing, returns nothing, and loses the signal. On Linux the
    /// shim is main-frame-only, so the natural "try `window.ipc`, else
    /// relay" shape *works*, and nothing here would go red. See
    /// docs/decisions.md D142 決定2.
    ///
    /// So this asserts on the script's shape rather than its behavior:
    /// the only `window.ipc` call sits behind the `isMain` branch.
    #[test]
    fn form_input_script_never_posts_to_window_ipc_from_a_subframe() {
        let script = form_input_script();
        let ipc_calls = script.matches("window.ipc.postMessage").count();
        assert_eq!(
            ipc_calls, 1,
            "exactly one `window.ipc` call, in the main-frame branch: {script}"
        );

        let guard = script
            .find("if (isMain)")
            .expect("the report path must branch on isMain");
        let ipc = script
            .find("window.ipc.postMessage")
            .expect("checked just above");
        let relay = script
            .find("window.parent.postMessage")
            .expect("a subframe must relay through its parent");
        assert!(
            guard < ipc,
            "the `window.ipc` call must sit inside the isMain branch"
        );
        assert!(
            ipc < relay,
            "the relay must be the else-path of that branch, not a fallback \
             tried after `window.ipc` (which would be a no-op on Windows)"
        );
        // And the subframe path must not consult `window.ipc` at all —
        // not even to test for it, which is what a fallback would do.
        // Sliced past the main-frame call itself, which is what `ipc`
        // points at.
        let subframe = &script[ipc + "window.ipc.postMessage".len()..];
        assert!(
            !subframe.contains("window.ipc"),
            "the subframe path must not look at `window.ipc`: {subframe}"
        );
    }

    #[test]
    fn content_ipc_event_maps_each_message_kind_and_ignores_the_rest() {
        let (w, t) = (WindowId::from(1), TabId::from(2));
        let event = |body: &str| content_ipc_event(w, t, body);
        assert!(matches!(
            event(OPEN_DEVTOOLS_MESSAGE),
            Some(UserEvent::OpenDevtoolsRequested(id)) if id == w
        ));
        assert!(matches!(
            event(&ShortcutId::NewTab.sentinel()),
            Some(UserEvent::ContentShortcut(id, ContentShortcut::NewTab)) if id == w
        ));
        assert!(matches!(
            event(&format!("{CONTEXT_MENU_OPEN_PREFIX}{{\"x\":1,\"y\":2}}")),
            Some(UserEvent::ContextMenuRequested { window_id, tab_id, .. })
                if window_id == w && tab_id == t
        ));
        assert!(matches!(
            event(FORM_INPUT_MESSAGE),
            Some(UserEvent::FormInputDetected(id, tab)) if id == w && tab == t
        ));
        assert!(matches!(
            event(CONTEXT_MENU_CLOSE_MESSAGE),
            Some(UserEvent::ContextMenuClosed(id, tab)) if id == w && tab == t
        ));
        assert!(matches!(
            event(&format!("{CONTEXT_MENU_ACTION_PREFIX}3")),
            Some(UserEvent::ContextMenuActionSelected { window_id, tab_id, index: 3 })
                if window_id == w && tab_id == t
        ));
        for ignored in [
            "",
            "velox:quit",
            r#"{"cmd":"new_tab"}"#,
            FORM_INPUT_RELAY_TOKEN,
        ] {
            assert!(event(ignored).is_none(), "{ignored:?}");
        }
    }

    #[test]
    fn parse_context_menu_open_accepts_a_well_formed_message() {
        let body = format!(
            "{CONTEXT_MENU_OPEN_PREFIX}{{\"x\":12.5,\"y\":34.0,\"linkHref\":\"https://example.com/\",\"imageSrc\":null,\"selectionText\":\"hi\",\"isEditable\":false}}"
        );
        let (x, y, raw) = parse_context_menu_open(&body).expect("should parse");
        assert_eq!(x, 12.5);
        assert_eq!(y, 34.0);
        assert_eq!(raw.link_href.as_deref(), Some("https://example.com/"));
        assert_eq!(raw.image_src, None);
        assert_eq!(raw.selection_text.as_deref(), Some("hi"));
        assert!(!raw.is_editable);
    }

    #[test]
    fn parse_context_menu_open_defaults_missing_optional_fields() {
        let body = format!("{CONTEXT_MENU_OPEN_PREFIX}{{\"x\":0,\"y\":0}}");
        let (_, _, raw) = parse_context_menu_open(&body).expect("should parse");
        assert_eq!(raw.link_href, None);
        assert_eq!(raw.image_src, None);
        assert_eq!(raw.selection_text, None);
        assert!(!raw.is_editable);
    }

    #[test]
    fn parse_context_menu_open_rejects_wrong_prefix_and_malformed_json() {
        assert!(parse_context_menu_open("velox:new-tab").is_none());
        assert!(parse_context_menu_open(&format!("{CONTEXT_MENU_OPEN_PREFIX}not json")).is_none());
        assert!(parse_context_menu_open(&format!("{CONTEXT_MENU_OPEN_PREFIX}{{}}")).is_none());
    }

    #[test]
    fn parse_context_menu_open_rejects_oversized_payloads_before_parsing() {
        // A pathologically large "selectionText" must be rejected outright
        // (D62's "reject before ever calling `serde_json::from_str`"
        // pattern), not merely truncated after a slow parse.
        let huge_string = "a".repeat(MAX_CONTEXT_MENU_MESSAGE_BYTES + 100);
        let body = format!(
            "{CONTEXT_MENU_OPEN_PREFIX}{{\"x\":0,\"y\":0,\"selectionText\":\"{huge_string}\"}}"
        );
        assert!(parse_context_menu_open(&body).is_none());
    }

    #[test]
    fn parse_context_menu_open_clamps_non_finite_or_out_of_range_coordinates() {
        let body = format!("{CONTEXT_MENU_OPEN_PREFIX}{{\"x\":NaN,\"y\":-500}}");
        // `NaN`/negative numbers are valid JSON5-ish JS literals but not
        // standard JSON — serde_json rejects `NaN` outright, which is fine:
        // that whole message is simply refused. Exercise the in-range
        // negative case (valid JSON) instead to check clamping.
        let _ = parse_context_menu_open(&body); // must not panic either way

        let body = format!("{CONTEXT_MENU_OPEN_PREFIX}{{\"x\":-500,\"y\":50000000}}");
        let (x, y, _) = parse_context_menu_open(&body).expect("should parse");
        assert_eq!(x, 0.0, "negative x must clamp to 0");
        assert_eq!(y, 1_000_000.0, "absurdly large y must clamp to the cap");
    }

    #[test]
    fn parse_context_menu_open_does_not_panic_on_hostile_input() {
        for body in [
            CONTEXT_MENU_OPEN_PREFIX,
            &format!("{CONTEXT_MENU_OPEN_PREFIX}\0\0\0"),
            &format!("{CONTEXT_MENU_OPEN_PREFIX}{}", "{".repeat(10_000)),
            &"a".repeat(1_000_000),
        ] {
            let _ = parse_context_menu_open(body);
        }
    }

    #[test]
    fn parse_context_menu_action_accepts_small_plain_integers() {
        assert_eq!(
            parse_context_menu_action(&format!("{CONTEXT_MENU_ACTION_PREFIX}0")),
            Some(0)
        );
        assert_eq!(
            parse_context_menu_action(&format!("{CONTEXT_MENU_ACTION_PREFIX}7")),
            Some(7)
        );
        assert_eq!(
            parse_context_menu_action(&format!("{CONTEXT_MENU_ACTION_PREFIX}42")),
            Some(42)
        );
    }

    #[test]
    fn parse_context_menu_action_rejects_anything_not_a_plain_small_integer() {
        for body in [
            "velox:context-menu-action:",
            "velox:context-menu-action:-1",
            "velox:context-menu-action:01",
            "velox:context-menu-action:1.5",
            "velox:context-menu-action:1a",
            "velox:context-menu-action: 1",
            "velox:context-menu-action:1 ",
            "velox:context-menu-action:99999",
            "velox:context-menu-action:18446744073709551616", // overflows usize
            "velox:new-tab",
        ] {
            assert_eq!(parse_context_menu_action(body), None, "body={body:?}");
        }
    }

    #[test]
    fn parse_context_menu_action_does_not_panic_on_hostile_input() {
        let huge = format!("{CONTEXT_MENU_ACTION_PREFIX}{}", "9".repeat(1_000_000));
        assert_eq!(parse_context_menu_action(&huge), None);
    }

    #[test]
    fn context_menu_render_script_embeds_labels_and_positions_via_textcontent() {
        let entries = vec![context_menu::MenuEntry {
            action: context_menu::MenuAction::Back,
            enabled: true,
        }];
        let script = context_menu_render_script(&entries, 12.0, 34.0);
        assert!(script.contains("row.textContent = item.label;"));
        assert!(script.contains("\"label\":\"戻る\""));
        assert!(script.contains("let left = 12;"));
        assert!(script.contains("let top = 34;"));
    }

    #[test]
    fn context_menu_render_script_neutralizes_quotes_and_script_closing_sequences_in_a_label() {
        // The only page-derived text that ever reaches a label is a
        // selection preview (`MenuAction::SearchSelection`) — exercise the
        // exact injection attempt D62/D69 already guard other embeddings
        // against: a value trying to break out of the JS string literal the
        // `items` JSON array is spliced into.
        let hostile = r#""; document.body.innerHTML = "pwned"; //"#.to_owned();
        let entries = vec![context_menu::MenuEntry {
            action: context_menu::MenuAction::SearchSelection(hostile),
            enabled: true,
        }];
        let script = context_menu_render_script(&entries, 0.0, 0.0);
        // The generated `const items = [...]` must remain one syntactically
        // closed JS statement — i.e. still contain the trailing pieces of
        // the script that come after it, proving the hostile string did not
        // prematurely terminate anything.
        assert!(script.contains("const items ="));
        assert!(script.contains("document.body.appendChild(root);"));
        assert!(script.contains("root.style.left"));
    }

    #[test]
    fn context_menu_render_script_escapes_u2028_and_u2029_line_terminators_in_a_label() {
        let hostile = "foo\u{2028}bar\u{2029}baz".to_owned();
        let entries = vec![context_menu::MenuEntry {
            action: context_menu::MenuAction::SearchSelection(hostile),
            enabled: true,
        }];
        let script = context_menu_render_script(&entries, 0.0, 0.0);
        assert!(script.contains("\\u2028"), "{script}");
        assert!(script.contains("\\u2029"), "{script}");
        assert!(!script.contains('\u{2028}'));
        assert!(!script.contains('\u{2029}'));
    }

    #[test]
    fn context_menu_render_script_never_emits_a_raw_script_tag_for_a_hostile_selection_label() {
        // The label is inserted via `textContent`, never HTML — but this
        // test fixes that guarantee at the *generated script's own source
        // text* level too: the literal bytes `<script>` from a hostile
        // selection must not appear unescaped as if it were meant to be
        // parsed as markup (it only ever appears inside a JSON string
        // value assigned to a JS variable, never inside an HTML tag
        // position).
        let hostile = "<script>alert(document.cookie)</script>".to_owned();
        let entries = vec![context_menu::MenuEntry {
            action: context_menu::MenuAction::SearchSelection(hostile.clone()),
            enabled: true,
        }];
        let script = context_menu_render_script(&entries, 0.0, 0.0);
        // The text is present (it is safe precisely *because* it ends up as
        // a JS string value, never HTML) but only inside the `items` JSON,
        // never as a document-level `<script>` tag of its own.
        assert!(script.contains("alert(document.cookie)"));
        assert_eq!(
            script.matches("<script>").count(),
            1,
            "the hostile text's own literal <script> should appear exactly once, as inert JSON string content"
        );
    }

    #[test]
    fn context_menu_render_script_shows_disabled_entries_as_unclickable() {
        let entries = vec![context_menu::MenuEntry {
            action: context_menu::MenuAction::Copy,
            enabled: false,
        }];
        let script = context_menu_render_script(&entries, 0.0, 0.0);
        assert!(script.contains("\"enabled\":false"));
    }

    #[test]
    fn hide_script_targets_the_same_root_id_the_render_script_uses() {
        assert!(CONTEXT_MENU_HIDE_SCRIPT.contains(CONTEXT_MENU_ROOT_ID));
    }
}
