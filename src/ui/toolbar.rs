//! Toolbar (browser chrome) definition and its IPC protocol.
//!
//! The toolbar is rendered by a dedicated webview from [`TOOLBAR_HTML`].
//! JS -> Rust: `window.ipc.postMessage` with a JSON [`ToolbarCommand`].
//! Rust -> JS: `evaluate_script` with the snippets built by
//! [`set_url_script`] / [`set_loading_script`] / [`set_tabs_script`] and
//! friends below.
//!
//! Tabs are identified to the toolbar by a plain `u64` (the toolbar's JS has
//! no notion of `browser::TabId`); `app.rs` converts between the two at the
//! boundary.

use serde::{Deserialize, Serialize};

use crate::browser::{BookmarkEntry, HistoryEntry};

/// The static HTML/CSS/JS that renders the toolbar.
pub const TOOLBAR_HTML: &str = include_str!("toolbar.html");

/// Which dropdown panel (if any) the toolbar is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Panel {
    History,
    Bookmarks,
}

/// A command sent from the toolbar UI to the browser.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolbarCommand {
    /// The user submitted the address bar; `input` is the raw typed text.
    Navigate {
        input: String,
    },
    Back,
    Forward,
    Reload,
    /// The "+" button was clicked: open a new tab.
    NewTab,
    /// A tab's close button was clicked.
    CloseTab {
        id: u64,
    },
    /// A tab in the strip was clicked: make it the active tab. Resumes the
    /// tab first (rebuilding its content webview) if it was suspended.
    ActivateTab {
        id: u64,
    },
    /// A tab's suspend button was clicked: drop its content webview to
    /// reclaim memory while keeping its URL, to be reloaded on the next
    /// `ActivateTab`. A no-op for the active tab or an already-suspended
    /// tab (see `browser::tabs::Tabs::suspend`).
    SuspendTab {
        id: u64,
    },
    /// The toolbar document finished loading and wants the current state
    /// (the content page may have started loading before the toolbar was
    /// ready to display it).
    Ready,
    /// Open the content webview's DevTools (Web Inspector).
    OpenDevtools,
    /// The star button was clicked: bookmark the current page, or remove
    /// its bookmark if it already has one.
    ToggleBookmark,
    /// Open or close a history/bookmarks panel; clicking the button for the
    /// panel that is already open closes it.
    TogglePanel {
        panel: Panel,
    },
    /// Remove one entry from the history list (the "x" next to a row).
    DeleteHistoryEntry {
        id: u64,
    },
    /// Remove every history entry ("clear history" in the panel).
    ClearHistory,
    /// Remove one bookmark (the "x" next to a row in the bookmarks panel).
    RemoveBookmark {
        id: u64,
    },
}

/// One row of the tab strip, as sent to the toolbar JS by [`set_tabs_script`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabSummary {
    pub id: u64,
    pub url: String,
    pub loading: bool,
    pub active: bool,
    /// Whether the tab is suspended (its content webview has been dropped
    /// to reclaim memory — see docs/decisions.md D9). The tab strip shows
    /// this distinctly so the user can tell a dormant tab from a live one.
    pub suspended: bool,
}

/// Parse a raw IPC message body into a [`ToolbarCommand`].
pub fn parse_command(body: &str) -> Result<ToolbarCommand, serde_json::Error> {
    serde_json::from_str(body)
}

/// JS snippet that updates the address bar text.
///
/// The URL is embedded as a JSON string literal, so arbitrary URLs cannot
/// break out of the script.
pub fn set_url_script(url: &str) -> String {
    format!(
        "veloxSetUrl({});",
        serde_json::Value::String(url.to_owned())
    )
}

/// JS snippet that toggles the loading indicator.
pub fn set_loading_script(loading: bool) -> String {
    format!("veloxSetLoading({loading});")
}

/// JS snippet that re-renders the tab strip from scratch.
///
/// `tabs` is embedded as a JSON array, so no tab title/URL can break out of
/// the script. Serialization only fails for types that cannot occur here
/// (e.g. non-string map keys), so a failure falls back to an empty tab strip
/// rather than panicking.
pub fn set_tabs_script(tabs: &[TabSummary]) -> String {
    let json = serde_json::to_string(tabs).unwrap_or_else(|_| "[]".to_owned());
    format!("veloxSetTabs({json});")
}

/// JS snippet that toggles the bookmark ("star") button's active state.
pub fn set_bookmark_active_script(active: bool) -> String {
    format!("veloxSetBookmarkActive({active});")
}

/// JS snippet that toggles the always-visible private-browsing indicator
/// (see docs/decisions.md D14). Pushed once, from the toolbar's `ready`
/// handler, since whole-app private mode never changes for the life of the
/// process.
pub fn set_private_script(private: bool) -> String {
    format!("veloxSetPrivate({private});")
}

/// JS snippet that opens the given panel, or closes whichever panel is open
/// when `panel` is `None`.
pub fn set_panel_script(panel: Option<Panel>) -> String {
    let arg = match panel {
        Some(Panel::History) => "\"history\"",
        Some(Panel::Bookmarks) => "\"bookmarks\"",
        None => "null",
    };
    format!("veloxSetPanel({arg});")
}

/// JS snippet that replaces the history panel's contents.
///
/// Entries are serialized as JSON, which embeds as-is inside `evaluate_script`
/// (an array/object JSON literal is valid JS syntax); URLs and titles never
/// need separate escaping the way [`set_url_script`]'s bare string does.
pub fn set_history_script(entries: &[&HistoryEntry]) -> String {
    format!("veloxSetHistory({});", entries_to_json(entries))
}

/// JS snippet that replaces the bookmarks panel's contents.
pub fn set_bookmarks_script(entries: &[&BookmarkEntry]) -> String {
    format!("veloxSetBookmarks({});", entries_to_json(entries))
}

fn entries_to_json<T: Serialize>(entries: &[T]) -> serde_json::Value {
    serde_json::to_value(entries).unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_navigate_command() {
        let cmd = parse_command(r#"{"cmd":"navigate","input":"example.com"}"#).unwrap();
        assert_eq!(
            cmd,
            ToolbarCommand::Navigate {
                input: "example.com".to_owned()
            }
        );
    }

    #[test]
    fn parses_plain_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"back"}"#).unwrap(),
            ToolbarCommand::Back
        );
        assert_eq!(
            parse_command(r#"{"cmd":"forward"}"#).unwrap(),
            ToolbarCommand::Forward
        );
        assert_eq!(
            parse_command(r#"{"cmd":"reload"}"#).unwrap(),
            ToolbarCommand::Reload
        );
        assert_eq!(
            parse_command(r#"{"cmd":"new_tab"}"#).unwrap(),
            ToolbarCommand::NewTab
        );
        assert_eq!(
            parse_command(r#"{"cmd":"ready"}"#).unwrap(),
            ToolbarCommand::Ready
        );
        assert_eq!(
            parse_command(r#"{"cmd":"open_devtools"}"#).unwrap(),
            ToolbarCommand::OpenDevtools
        );
    }

    #[test]
    fn parses_tab_commands_with_ids() {
        assert_eq!(
            parse_command(r#"{"cmd":"close_tab","id":3}"#).unwrap(),
            ToolbarCommand::CloseTab { id: 3 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"activate_tab","id":42}"#).unwrap(),
            ToolbarCommand::ActivateTab { id: 42 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"suspend_tab","id":7}"#).unwrap(),
            ToolbarCommand::SuspendTab { id: 7 }
        );
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(parse_command(r#"{"cmd":"self_destruct"}"#).is_err());
        assert!(parse_command("not json").is_err());
    }

    #[test]
    fn parses_bookmark_and_panel_commands() {
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_bookmark"}"#).unwrap(),
            ToolbarCommand::ToggleBookmark
        );
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_panel","panel":"history"}"#).unwrap(),
            ToolbarCommand::TogglePanel {
                panel: Panel::History
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"toggle_panel","panel":"bookmarks"}"#).unwrap(),
            ToolbarCommand::TogglePanel {
                panel: Panel::Bookmarks
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"delete_history_entry","id":42}"#).unwrap(),
            ToolbarCommand::DeleteHistoryEntry { id: 42 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"clear_history"}"#).unwrap(),
            ToolbarCommand::ClearHistory
        );
        assert_eq!(
            parse_command(r#"{"cmd":"remove_bookmark","id":7}"#).unwrap(),
            ToolbarCommand::RemoveBookmark { id: 7 }
        );
    }

    #[test]
    fn url_script_escapes_quotes_and_backslashes() {
        let script = set_url_script(r#"https://example.com/?q="a"\b"#);
        assert_eq!(script, r#"veloxSetUrl("https://example.com/?q=\"a\"\\b");"#);
    }

    #[test]
    fn loading_script_is_a_bool_literal() {
        assert_eq!(set_loading_script(true), "veloxSetLoading(true);");
        assert_eq!(set_loading_script(false), "veloxSetLoading(false);");
    }

    #[test]
    fn tabs_script_embeds_a_json_array() {
        let tabs = vec![
            TabSummary {
                id: 1,
                url: "https://a.example/".to_owned(),
                loading: false,
                active: true,
                suspended: false,
            },
            TabSummary {
                id: 2,
                url: "https://b.example/?q=\"x\"".to_owned(),
                loading: true,
                active: false,
                suspended: false,
            },
            TabSummary {
                id: 3,
                url: "https://c.example/".to_owned(),
                loading: false,
                active: false,
                suspended: true,
            },
        ];
        let script = set_tabs_script(&tabs);
        assert!(script.starts_with("veloxSetTabs(["));
        assert!(script.ends_with("]);"));
        // Quotes inside a URL must be escaped, not break out of the array.
        assert!(script.contains(r#"\"x\""#));
        assert!(script.contains(r#""suspended":true"#));

        let empty = set_tabs_script(&[]);
        assert_eq!(empty, "veloxSetTabs([]);");
    }

    #[test]
    fn bookmark_active_script_is_a_bool_literal() {
        assert_eq!(
            set_bookmark_active_script(true),
            "veloxSetBookmarkActive(true);"
        );
        assert_eq!(
            set_bookmark_active_script(false),
            "veloxSetBookmarkActive(false);"
        );
    }

    #[test]
    fn private_script_is_a_bool_literal() {
        assert_eq!(set_private_script(true), "veloxSetPrivate(true);");
        assert_eq!(set_private_script(false), "veloxSetPrivate(false);");
    }

    #[test]
    fn panel_script_names_the_open_panel_or_null() {
        assert_eq!(
            set_panel_script(Some(Panel::History)),
            "veloxSetPanel(\"history\");"
        );
        assert_eq!(
            set_panel_script(Some(Panel::Bookmarks)),
            "veloxSetPanel(\"bookmarks\");"
        );
        assert_eq!(set_panel_script(None), "veloxSetPanel(null);");
    }

    #[test]
    fn history_script_embeds_entries_as_json() {
        let entry = HistoryEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: Some("Example".to_owned()),
            visited_at: 100,
        };
        let script = set_history_script(&[&entry]);
        assert!(script.starts_with("veloxSetHistory("));
        assert!(script.contains(r#""url":"https://example.com/""#));
        assert!(script.contains(r#""title":"Example""#));
    }

    #[test]
    fn history_script_escapes_untrusted_title_and_url_content() {
        let entry = HistoryEntry {
            id: 1,
            url: r#"https://example.com/?q="a"</script>"#.to_owned(),
            title: Some(r#"a"b\c"#.to_owned()),
            visited_at: 1,
        };
        let script = set_history_script(&[&entry]);
        // A double quote inside a JSON string value must be escaped, so the
        // string never terminates early.
        assert!(script.contains(r#"\"a\""#));
        assert!(script.contains(r#"a\"b\\c"#));
    }

    #[test]
    fn bookmarks_script_embeds_entries_as_json() {
        let entry = BookmarkEntry {
            id: 1,
            url: "https://example.com/".to_owned(),
            title: None,
            created_at: 100,
        };
        let script = set_bookmarks_script(&[&entry]);
        assert!(script.starts_with("veloxSetBookmarks("));
        assert!(script.contains(r#""url":"https://example.com/""#));
        assert!(script.contains(r#""title":null"#));
    }

    #[test]
    fn empty_entry_list_serializes_to_an_empty_array() {
        assert_eq!(set_history_script(&[]), "veloxSetHistory([]);".to_owned());
        assert_eq!(
            set_bookmarks_script(&[]),
            "veloxSetBookmarks([]);".to_owned()
        );
    }

    #[test]
    fn toolbar_html_declares_expected_hooks() {
        assert!(TOOLBAR_HTML.contains("veloxSetUrl"));
        assert!(TOOLBAR_HTML.contains("veloxSetLoading"));
        assert!(TOOLBAR_HTML.contains("veloxSetTabs"));
        assert!(TOOLBAR_HTML.contains("veloxSetBookmarkActive"));
        assert!(TOOLBAR_HTML.contains("veloxSetPrivate"));
        assert!(TOOLBAR_HTML.contains("veloxSetPanel"));
        assert!(TOOLBAR_HTML.contains("veloxSetHistory"));
        assert!(TOOLBAR_HTML.contains("veloxSetBookmarks"));
        assert!(TOOLBAR_HTML.contains("ipc.postMessage"));
        assert!(TOOLBAR_HTML.contains("new_tab"));
        assert!(TOOLBAR_HTML.contains("close_tab"));
        assert!(TOOLBAR_HTML.contains("activate_tab"));
        assert!(TOOLBAR_HTML.contains("suspend_tab"));
        assert!(TOOLBAR_HTML.contains("toggle_bookmark"));
        assert!(TOOLBAR_HTML.contains("toggle_panel"));
        assert!(TOOLBAR_HTML.contains("delete_history_entry"));
        assert!(TOOLBAR_HTML.contains("clear_history"));
        assert!(TOOLBAR_HTML.contains("remove_bookmark"));
    }
}
