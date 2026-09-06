//! Right-click context menu (Issue #39) — the UI/engine-independent half.
//!
//! `ui::window` captures the raw `contextmenu` DOM event in the content
//! webview and reports it to Rust (see its module doc comment for the wire
//! format and docs/decisions.md D78 for why a JS-rendered menu was chosen
//! over a native WebView2/WKWebView/WebKitGTK context menu). Everything that
//! can be expressed as plain data — validating what the click actually
//! landed on, and deciding which menu items apply and whether each is
//! enabled — belongs here per `docs/architecture.md`'s four-layer split, so
//! it is unit-tested without a webview.
//!
//! **Trust boundary (critical — see docs/decisions.md D18/D23/D78).** Every
//! [`RawMenuContext`] field is attacker-controlled: the content webview
//! renders arbitrary, untrusted page content, and that page — not VeloX —
//! decides what is under the cursor (a `<a href>`, an `<img src>`, a text
//! selection). [`sanitize`] is the *only* place a [`RawMenuContext`] is
//! turned into a [`MenuContext`], and every field it produces has already
//! been validated (length-capped, scheme-checked for URLs) before anything
//! downstream — [`build_menu`], `ui::window`'s renderer, or `app.rs`'s
//! action dispatch — is allowed to treat it as safe to act on or display.

use super::navigation;
use super::TabId;

/// Hard cap on a raw link/image URL's length, checked before it is even
/// parsed. Defense in depth against a pathological page feeding a
/// multi-megabyte `href`/`src` string through the IPC channel and into the
/// URL parser for no legitimate reason.
pub const MAX_URL_LEN: usize = 8 * 1024;

/// Hard cap on the selected text kept for the "selection を検索" action
/// itself (the *label* preview shown in the menu is capped far shorter, see
/// [`MenuAction::label`]). Generous enough for any selection a user would
/// plausibly want to search for; not a UI limit, just a sanity bound before
/// the text is handed to `navigation::build_search_url`.
pub const MAX_SELECTION_LEN: usize = 4_000;

/// How many characters of a text selection are shown, verbatim, inside the
/// "「…」を検索" menu label before an ellipsis is appended.
const SELECTION_PREVIEW_CHARS: usize = 30;

/// Raw click-time context exactly as reported by the content webview's
/// `contextmenu` handler (`ui::window`'s `context_menu_script`) — untrusted
/// input, see this module's doc comment. `None` means "nothing of that kind
/// under the cursor", not "unknown"; the content script always reports a
/// definite answer for every field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawMenuContext {
    /// The resolved (already made absolute by the DOM's own `.href` IDL
    /// getter) `href` of the nearest ancestor `<a>` with an `href`
    /// attribute, if any.
    pub link_href: Option<String>,
    /// The resolved `src` of the nearest ancestor `<img>`, if any.
    pub image_src: Option<String>,
    /// `window.getSelection().toString()` at click time, if non-empty.
    pub selection_text: Option<String>,
    /// Whether the click landed on an editable target (a text `<input>`,
    /// `<textarea>`, or an element with `isContentEditable`).
    pub is_editable: bool,
}

/// The sanitized, safe-to-act-on click target — the only thing
/// [`build_menu`] or `app.rs`'s action dispatch ever sees. See [`sanitize`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuContext {
    /// `Some` only when the raw `link_href` both parsed as a URL and used an
    /// `http`/`https` scheme — see [`sanitize_menu_url`]'s doc comment for
    /// why this is deliberately narrower than the address bar's own allowed
    /// schemes.
    pub link_url: Option<String>,
    /// Same validation as `link_url`, for `image_src`.
    pub image_url: Option<String>,
    /// Trimmed, control-character-stripped, length-capped selection text.
    /// `Some` only when the raw selection was non-empty after cleanup.
    pub selection_text: Option<String>,
    pub is_editable: bool,
}

/// Strip ASCII/Unicode control characters other than plain whitespace
/// (space, tab, newline) from untrusted text before it is stored or later
/// embedded anywhere. Not an HTML/JS escape (see [`MenuAction::label`]'s doc
/// comment for why none is needed here) — just hygiene against control
/// characters (e.g. bidi overrides, U+0000) a page could plant in a
/// selection.
fn strip_control_chars(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == ' ' || *c == '\t' || *c == '\n')
        .collect()
}

/// Truncate `text` to at most `max_chars` `char`s (never splitting a
/// multi-byte character), returning the possibly-shortened string.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        text.chars().take(max_chars).collect()
    }
}

/// Validate a raw link/image URL reported by the content webview.
///
/// Deliberately **narrower** than `navigation::ALLOWED_SCHEMES` (which also
/// allows `file`/`about`/`data` for address-bar input a *user* typed):
/// here, the URL was never typed by the user at all — it is whatever a page
/// chose to put in an `<a href>`/`<img src>` attribute that the user merely
/// right-clicked. Only `http`/`https` are accepted, so a page can never use
/// VeloX's own "open link in a new tab/window" action as a vector to make
/// the browser navigate to a `javascript:`/`data:`/`file:` target the user
/// never asked for. See docs/decisions.md D78.
fn sanitize_menu_url(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    if raw.is_empty() || raw.len() > MAX_URL_LEN {
        return None;
    }
    let normalized = navigation::normalize_input(raw)?;
    let scheme_is_http = normalized.starts_with("http://") || normalized.starts_with("https://");
    scheme_is_http.then_some(normalized)
}

/// The one place a [`RawMenuContext`] becomes a [`MenuContext`] — see this
/// module's doc comment for why every field must go through here before
/// anything treats it as safe.
pub fn sanitize(raw: RawMenuContext) -> MenuContext {
    let selection_text = raw
        .selection_text
        .as_deref()
        .map(strip_control_chars)
        .map(|text| truncate_chars(text.trim(), MAX_SELECTION_LEN))
        .filter(|text| !text.is_empty());
    MenuContext {
        link_url: sanitize_menu_url(raw.link_href.as_deref()),
        image_url: sanitize_menu_url(raw.image_src.as_deref()),
        selection_text,
        is_editable: raw.is_editable,
    }
}

/// One executable context-menu action, already carrying whatever
/// already-sanitized data it needs (a validated absolute `http`/`https` URL,
/// or length-capped selection text) so nothing downstream ever has to
/// re-derive or re-trust anything from the original click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    Back,
    Forward,
    Reload,
    Copy,
    Paste,
    /// Search the web for this (already sanitized, capped) selection text.
    SearchSelection(String),
    OpenLinkInNewTab(String),
    OpenLinkInNewWindow(String),
    OpenImageInNewTab(String),
    /// Open DevTools for the tab the menu was opened on (Issue #39's
    /// "DevTools導線と統合できる" acceptance condition) — reuses the exact
    /// same `BrowserWindow::open_devtools` path F12 already uses (D18).
    Inspect,
}

impl MenuAction {
    /// The menu label shown for this action — a fixed, Rust-authored
    /// Japanese string for every variant except [`Self::SearchSelection`],
    /// which includes a short, literal preview of the selected text.
    ///
    /// **Why no HTML/JS escaping is needed here, unlike View Source (D72):**
    /// `ui::window` renders every label through a DOM `element.textContent =
    /// …` assignment, never by building an HTML string — `textContent`
    /// cannot interpret its argument as markup no matter what characters it
    /// contains, so the classic HTML-injection concern structurally does
    /// not apply to this string. The only real risk is a *different* one —
    /// this string later being spliced into a JS string literal inside the
    /// script `ui::window` generates — and that is handled once, uniformly,
    /// for every label (not just this one) via `serde_json` +
    /// `escape_js_line_terminators` at the point the whole item list is
    /// serialized (see `ui::window::context_menu_render_script`). See
    /// docs/decisions.md D78.
    pub fn label(&self) -> String {
        match self {
            MenuAction::Back => "戻る".to_owned(),
            MenuAction::Forward => "進む".to_owned(),
            MenuAction::Reload => "再読み込み".to_owned(),
            MenuAction::Copy => "コピー".to_owned(),
            MenuAction::Paste => "貼り付け".to_owned(),
            MenuAction::SearchSelection(text) => {
                let preview = truncate_chars(text, SELECTION_PREVIEW_CHARS);
                let truncated = preview.chars().count() < text.chars().count();
                let ellipsis = if truncated { "…" } else { "" };
                format!("「{preview}{ellipsis}」を検索")
            }
            MenuAction::OpenLinkInNewTab(_) => "リンクを新しいタブで開く".to_owned(),
            MenuAction::OpenLinkInNewWindow(_) => "リンクを新しいウィンドウで開く".to_owned(),
            MenuAction::OpenImageInNewTab(_) => "画像を新しいタブで開く".to_owned(),
            MenuAction::Inspect => "検証".to_owned(),
        }
    }
}

/// One row [`build_menu`] decided should appear, and whether it is
/// currently usable. A disabled entry (e.g. "コピー" with nothing selected)
/// is still included — `ui::window` renders it greyed out and unclickable —
/// rather than omitted, matching how Copy/Paste behave in mainstream
/// browsers. Link/image/search-selection entries are the opposite: they are
/// only ever *included* when relevant (there is nothing sensible to show a
/// disabled "リンクを新しいタブで開く" for a click with no link).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub action: MenuAction,
    pub enabled: bool,
}

/// **The single table this issue's design centers on.** Every context-menu
/// item VeloX can show, and the one place that decides which of them apply
/// to a given click and whether each is enabled. Adding a new item — e.g.
/// once #46 (名前を付けて保存) or #27 (プライベートウィンドウ) lands — means
/// adding one line here (and one `MenuAction` arm in `ui::window`'s
/// renderer/`app.rs`'s dispatch) and nothing else; see docs/decisions.md
/// D78's "拡張性" section.
///
/// **Known limitation** (see docs/decisions.md D20/D78): VeloX does not
/// duplicate the web engine's own session history, so there is no
/// "can go back"/"can go forward" signal available to disable Back/Forward
/// against — they are always enabled here, exactly like the toolbar's own
/// Back/Forward buttons already are.
pub fn build_menu(context: &MenuContext) -> Vec<MenuEntry> {
    let mut entries = vec![
        MenuEntry {
            action: MenuAction::Back,
            enabled: true,
        },
        MenuEntry {
            action: MenuAction::Forward,
            enabled: true,
        },
        MenuEntry {
            action: MenuAction::Reload,
            enabled: true,
        },
    ];

    if let Some(url) = &context.link_url {
        entries.push(MenuEntry {
            action: MenuAction::OpenLinkInNewTab(url.clone()),
            enabled: true,
        });
        entries.push(MenuEntry {
            action: MenuAction::OpenLinkInNewWindow(url.clone()),
            enabled: true,
        });
    }
    if let Some(url) = &context.image_url {
        entries.push(MenuEntry {
            action: MenuAction::OpenImageInNewTab(url.clone()),
            enabled: true,
        });
    }

    entries.push(MenuEntry {
        action: MenuAction::Copy,
        enabled: context.selection_text.is_some(),
    });
    entries.push(MenuEntry {
        action: MenuAction::Paste,
        enabled: context.is_editable,
    });
    if let Some(text) = &context.selection_text {
        entries.push(MenuEntry {
            action: MenuAction::SearchSelection(text.clone()),
            enabled: true,
        });
    }

    entries.push(MenuEntry {
        action: MenuAction::Inspect,
        enabled: true,
    });

    entries
}

/// A currently-open context menu in one window: which tab it was opened for
/// and the exact, already-decided [`MenuEntry`] list it was rendered with.
///
/// Mirrors `browser::find::FindState`'s shape deliberately (`tab_id` kept
/// alongside the session's own data, one session per window — see
/// `browser::windows::WindowEntry::context_menu`): when a click on entry
/// `N` comes back from the content webview (see `ui::window`'s
/// `CONTEXT_MENU_ACTION_PREFIX`), `app.rs` looks it up here rather than
/// re-running [`build_menu`] against a freshly (and separately) received
/// click — the exact list the user was actually shown, including which
/// rows were disabled, is what an index into it must resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenContextMenu {
    tab_id: TabId,
    entries: Vec<MenuEntry>,
}

impl OpenContextMenu {
    pub fn new(tab_id: TabId, entries: Vec<MenuEntry>) -> Self {
        Self { tab_id, entries }
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn entries(&self) -> &[MenuEntry] {
        &self.entries
    }

    /// Resolve `index` (as reported by the content webview's click handler)
    /// against this menu's own entry list — `None` for an out-of-range
    /// index (including an index sent for a menu that has since been
    /// replaced by a different one) or a disabled entry (a hostile page
    /// dispatching a fake click event on a row it could see was greyed out
    /// must not be able to run the action anyway).
    pub fn resolve(self, index: usize) -> Option<MenuAction> {
        let entry = self.entries.into_iter().nth(index)?;
        entry.enabled.then_some(entry.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RawMenuContext {
        RawMenuContext::default()
    }

    // --- sanitize: URL scheme validation (the core threat model) ---

    #[test]
    fn sanitize_accepts_http_and_https_links() {
        let out = sanitize(RawMenuContext {
            link_href: Some("https://example.com/page".to_owned()),
            ..ctx()
        });
        assert_eq!(out.link_url.as_deref(), Some("https://example.com/page"));

        let out = sanitize(RawMenuContext {
            link_href: Some("http://example.com/page".to_owned()),
            ..ctx()
        });
        assert_eq!(out.link_url.as_deref(), Some("http://example.com/page"));
    }

    #[test]
    fn sanitize_rejects_javascript_scheme_link() {
        let out = sanitize(RawMenuContext {
            link_href: Some("javascript:alert(document.cookie)".to_owned()),
            ..ctx()
        });
        assert_eq!(out.link_url, None);
    }

    #[test]
    fn sanitize_rejects_javascript_scheme_regardless_of_case_or_whitespace() {
        for raw in [
            "JavaScript:alert(1)",
            "  javascript:alert(1)",
            "javascript:alert(1)//",
            "\tJAVASCRIPT:alert(1)",
        ] {
            let out = sanitize(RawMenuContext {
                link_href: Some(raw.to_owned()),
                ..ctx()
            });
            assert_eq!(out.link_url, None, "should reject {raw:?}");
        }
    }

    #[test]
    fn sanitize_rejects_data_and_file_scheme_links() {
        let out = sanitize(RawMenuContext {
            link_href: Some("data:text/html,<script>alert(1)</script>".to_owned()),
            ..ctx()
        });
        assert_eq!(out.link_url, None);

        let out = sanitize(RawMenuContext {
            link_href: Some("file:///etc/passwd".to_owned()),
            ..ctx()
        });
        assert_eq!(out.link_url, None);
    }

    #[test]
    fn sanitize_rejects_vbscript_scheme_link() {
        let out = sanitize(RawMenuContext {
            link_href: Some("vbscript:msgbox(1)".to_owned()),
            ..ctx()
        });
        assert_eq!(out.link_url, None);
    }

    #[test]
    fn sanitize_rejects_javascript_scheme_image_src_too() {
        let out = sanitize(RawMenuContext {
            image_src: Some("javascript:alert(1)".to_owned()),
            ..ctx()
        });
        assert_eq!(out.image_url, None);
    }

    #[test]
    fn sanitize_rejects_empty_or_oversized_urls() {
        let out = sanitize(RawMenuContext {
            link_href: Some(String::new()),
            ..ctx()
        });
        assert_eq!(out.link_url, None);

        let huge = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN + 1));
        let out = sanitize(RawMenuContext {
            link_href: Some(huge),
            ..ctx()
        });
        assert_eq!(out.link_url, None);
    }

    #[test]
    fn sanitize_does_not_panic_on_hostile_link_input() {
        for raw in [
            "\0\0\0",
            "https://",
            "\u{202e}https://evil.example/",
            "://not-a-url",
            "https:// space in host/",
        ] {
            let _ = sanitize(RawMenuContext {
                link_href: Some(raw.to_owned()),
                image_src: Some(raw.to_owned()),
                ..ctx()
            });
        }
    }

    // --- sanitize: selection text ---

    #[test]
    fn sanitize_trims_and_keeps_non_empty_selection() {
        let out = sanitize(RawMenuContext {
            selection_text: Some("  hello world  ".to_owned()),
            ..ctx()
        });
        assert_eq!(out.selection_text.as_deref(), Some("hello world"));
    }

    #[test]
    fn sanitize_drops_whitespace_only_selection() {
        let out = sanitize(RawMenuContext {
            selection_text: Some("   \n\t  ".to_owned()),
            ..ctx()
        });
        assert_eq!(out.selection_text, None);
    }

    #[test]
    fn sanitize_strips_control_characters_from_selection_but_keeps_ordinary_text() {
        let out = sanitize(RawMenuContext {
            selection_text: Some("a\u{0}b\u{7}c<script>d".to_owned()),
            ..ctx()
        });
        // Control characters are gone, but ordinary punctuation/markup-like
        // text (which is just *text*, never interpreted as HTML — see
        // `MenuAction::label`'s doc comment) is preserved verbatim.
        assert_eq!(out.selection_text.as_deref(), Some("abc<script>d"));
    }

    #[test]
    fn sanitize_caps_selection_length() {
        let long = "a".repeat(MAX_SELECTION_LEN + 500);
        let out = sanitize(RawMenuContext {
            selection_text: Some(long),
            ..ctx()
        });
        assert_eq!(
            out.selection_text.unwrap().chars().count(),
            MAX_SELECTION_LEN
        );
    }

    #[test]
    fn sanitize_never_splits_a_multibyte_character_when_truncating_selection() {
        let long = "あ".repeat(MAX_SELECTION_LEN + 10);
        let out = sanitize(RawMenuContext {
            selection_text: Some(long),
            ..ctx()
        });
        let text = out.selection_text.unwrap();
        assert_eq!(text.chars().count(), MAX_SELECTION_LEN);
        // Would panic on a byte-boundary split; getting here at all proves
        // truncation was char-aware, not byte-aware.
        assert!(text.chars().all(|c| c == 'あ'));
    }

    // --- build_menu: the decision table ---

    #[test]
    fn plain_page_click_has_no_link_or_image_items_and_disabled_copy_paste() {
        let entries = build_menu(&MenuContext::default());
        let actions: Vec<_> = entries.iter().map(|e| &e.action).collect();
        assert!(!actions
            .iter()
            .any(|a| matches!(a, MenuAction::OpenLinkInNewTab(_))));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, MenuAction::OpenLinkInNewWindow(_))));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, MenuAction::OpenImageInNewTab(_))));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, MenuAction::SearchSelection(_))));

        let copy = entries
            .iter()
            .find(|e| e.action == MenuAction::Copy)
            .unwrap();
        assert!(!copy.enabled);
        let paste = entries
            .iter()
            .find(|e| e.action == MenuAction::Paste)
            .unwrap();
        assert!(!paste.enabled);

        // Always present regardless of target.
        for action in [
            MenuAction::Back,
            MenuAction::Forward,
            MenuAction::Reload,
            MenuAction::Inspect,
        ] {
            assert!(entries.iter().any(|e| e.action == action));
        }
    }

    #[test]
    fn link_click_adds_open_in_new_tab_and_new_window_enabled() {
        let context = MenuContext {
            link_url: Some("https://example.com/".to_owned()),
            ..MenuContext::default()
        };
        let entries = build_menu(&context);
        let new_tab = entries
            .iter()
            .find(|e| matches!(e.action, MenuAction::OpenLinkInNewTab(_)))
            .expect("open in new tab item");
        assert!(new_tab.enabled);
        assert_eq!(
            new_tab.action,
            MenuAction::OpenLinkInNewTab("https://example.com/".to_owned())
        );
        let new_window = entries
            .iter()
            .find(|e| matches!(e.action, MenuAction::OpenLinkInNewWindow(_)))
            .expect("open in new window item");
        assert!(new_window.enabled);
    }

    #[test]
    fn image_click_adds_open_image_in_new_tab_enabled() {
        let context = MenuContext {
            image_url: Some("https://example.com/cat.png".to_owned()),
            ..MenuContext::default()
        };
        let entries = build_menu(&context);
        let item = entries
            .iter()
            .find(|e| matches!(e.action, MenuAction::OpenImageInNewTab(_)))
            .expect("open image item");
        assert!(item.enabled);
    }

    #[test]
    fn selection_enables_copy_and_adds_search_selection() {
        let context = MenuContext {
            selection_text: Some("hello".to_owned()),
            ..MenuContext::default()
        };
        let entries = build_menu(&context);
        assert!(
            entries
                .iter()
                .find(|e| e.action == MenuAction::Copy)
                .unwrap()
                .enabled
        );
        let search = entries
            .iter()
            .find(|e| matches!(e.action, MenuAction::SearchSelection(_)))
            .expect("search selection item");
        assert!(search.enabled);
        assert_eq!(
            search.action,
            MenuAction::SearchSelection("hello".to_owned())
        );
    }

    #[test]
    fn editable_target_enables_paste_only() {
        let context = MenuContext {
            is_editable: true,
            ..MenuContext::default()
        };
        let entries = build_menu(&context);
        assert!(
            entries
                .iter()
                .find(|e| e.action == MenuAction::Paste)
                .unwrap()
                .enabled
        );
        assert!(
            !entries
                .iter()
                .find(|e| e.action == MenuAction::Copy)
                .unwrap()
                .enabled
        );
    }

    // --- MenuAction::label: preview truncation, no dependence on escaping ---

    #[test]
    fn search_selection_label_includes_a_short_preview() {
        let action = MenuAction::SearchSelection("hello world".to_owned());
        assert_eq!(action.label(), "「hello world」を検索");
    }

    #[test]
    fn search_selection_label_truncates_long_selection_with_ellipsis() {
        let long = "a".repeat(SELECTION_PREVIEW_CHARS + 20);
        let action = MenuAction::SearchSelection(long);
        let label = action.label();
        assert!(label.contains('…'), "{label}");
        // Preview portion (excluding the wrapping quotes/ellipsis/suffix)
        // must not exceed the configured cap.
        let preview_len = label
            .trim_start_matches('「')
            .trim_end_matches("…」を検索")
            .chars()
            .count();
        assert_eq!(preview_len, SELECTION_PREVIEW_CHARS);
    }

    #[test]
    fn search_selection_label_keeps_html_like_text_as_literal_characters() {
        // The label is inserted via `textContent`, never as an HTML string
        // (see `MenuAction::label`'s doc comment) — so preserving these
        // characters verbatim here is correct, not a bug; the safety
        // property is enforced downstream by `ui::window`, exercised by its
        // own script-escaping tests.
        let action = MenuAction::SearchSelection("<script>alert(1)</script>".to_owned());
        assert!(action.label().contains("<script>alert(1)</script>"));
    }

    // --- OpenContextMenu::resolve ---

    #[test]
    fn resolve_returns_the_action_at_a_valid_enabled_index() {
        let context = MenuContext {
            link_url: Some("https://example.com/".to_owned()),
            ..MenuContext::default()
        };
        let entries = build_menu(&context);
        let index = entries
            .iter()
            .position(|e| matches!(e.action, MenuAction::OpenLinkInNewTab(_)))
            .unwrap();
        let menu = OpenContextMenu::new(TabId::from(0), entries);
        assert_eq!(
            menu.resolve(index),
            Some(MenuAction::OpenLinkInNewTab(
                "https://example.com/".to_owned()
            ))
        );
    }

    #[test]
    fn resolve_rejects_a_disabled_entry() {
        // A hostile page dispatching a fake click on a greyed-out row (e.g.
        // "コピー" with nothing selected) must not be able to run it.
        let entries = build_menu(&MenuContext::default());
        let index = entries
            .iter()
            .position(|e| e.action == MenuAction::Copy)
            .unwrap();
        assert!(!entries[index].enabled);
        let menu = OpenContextMenu::new(TabId::from(0), entries);
        assert_eq!(menu.resolve(index), None);
    }

    #[test]
    fn resolve_rejects_an_out_of_range_index() {
        let entries = build_menu(&MenuContext::default());
        let len = entries.len();
        let menu = OpenContextMenu::new(TabId::from(0), entries.clone());
        assert_eq!(menu.resolve(len), None);
        let menu = OpenContextMenu::new(TabId::from(0), entries);
        assert_eq!(menu.resolve(usize::MAX), None);
    }

    #[test]
    fn every_other_label_is_stable_and_non_empty() {
        for action in [
            MenuAction::Back,
            MenuAction::Forward,
            MenuAction::Reload,
            MenuAction::Copy,
            MenuAction::Paste,
            MenuAction::OpenLinkInNewTab("https://example.com/".to_owned()),
            MenuAction::OpenLinkInNewWindow("https://example.com/".to_owned()),
            MenuAction::OpenImageInNewTab("https://example.com/x.png".to_owned()),
            MenuAction::Inspect,
        ] {
            assert!(!action.label().is_empty());
        }
    }
}
