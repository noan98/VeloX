//! Pure logic for "名前を付けて保存" (Save Page, Issue #46): deriving a safe
//! suggested file name from a page's title/URL, wrapping a raw
//! `outerHTML` snapshot as a loadable document (the non-Windows fallback
//! format), and parsing the small JSON payload WebView2's
//! `Page.captureSnapshot` DevTools Protocol call hands back (the
//! Windows/MHTML path). See docs/decisions.md D76 for the full design
//! rationale behind both save formats.
//!
//! This module knows nothing about `wry`/tabs/webviews/COM — see
//! docs/architecture.md's four-layer split. `ui::window` (and, on Windows,
//! `ui::save_dialog_windows`) call into here; this file only ever sees
//! plain strings, never a webview handle.
//!
//! **Security note**: a page's title and URL are exactly as untrusted as any
//! other page-supplied content (see docs/decisions.md D62) — a hostile page
//! can set `document.title` to anything at all, including path-traversal
//! sequences, a Windows-reserved device name, or characters illegal in a
//! Windows file name. [`suggested_file_name`] is the single place that
//! turns such a title into a file name VeloX will actually write to, and is
//! exercised heavily by the tests below for exactly that reason — mirroring
//! `browser::downloads::sanitize_filename`'s own doc comment/tests, which
//! this module reuses rather than re-implementing.

use crate::browser::downloads;

/// Extension used for the Windows/WebView2 MHTML save path (D76): a single
/// file that also carries the page's images/CSS/subframes inline, unlike
/// [`HTML_EXTENSION`] below.
pub const MHTML_EXTENSION: &str = "mhtml";

/// Extension used for the non-Windows fallback save path: a plain
/// `document.documentElement.outerHTML` snapshot written out verbatim (see
/// [`wrap_outer_html_as_document`]). Deliberately **not** the same document
/// shape as `browser::view_source`'s escaped/line-numbered output (Issue
/// #45) — that one is built to be displayed as text, this one to be
/// reloaded as real markup. Images/external CSS/etc. referenced by URL are
/// *not* fetched or embedded — see this module's doc comment and D76.
pub const HTML_EXTENSION: &str = "html";

/// Windows file-name characters this module additionally guards against, on
/// top of what [`downloads::sanitize_filename`] already handles.
///
/// `sanitize_filename` was built for *download* suggested names (usually a
/// bare file name off a `Content-Disposition` header, or a URL's last path
/// segment) — its traversal defense is "keep only the last `/`/`\`-delimited
/// segment", which is exactly right when the input might look like a path.
/// A page **title** is ordinary free text and routinely contains
/// `:`/`/`/`|`/`?` as plain punctuation ("Breaking: Top Story", "Tips &
/// Tricks (Q&A)") rather than as a path separator — collapsing everything
/// before the last `/` would silently throw away most of a title like that.
/// So [`replace_forbidden_filename_chars`] replaces every character Windows
/// forbids in a file name with `_` *before* the result ever reaches
/// `sanitize_filename`, preserving as much of the title as possible while
/// still guaranteeing no `/`/`\` survives (so there is nothing left for a
/// resulting path to "traverse" with) — see docs/decisions.md D76.
const FORBIDDEN_FILENAME_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

fn replace_forbidden_filename_chars(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if FORBIDDEN_FILENAME_CHARS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// The base name (no extension) to suggest for `title`/`url`: the page
/// title, trimmed, if it has a non-blank one; otherwise the URL's host;
/// otherwise a fixed fallback. Not itself guaranteed to be a safe file
/// name — see [`suggested_file_name`], which every real caller uses.
fn default_file_stem(title: Option<&str>, url: &str) -> String {
    if let Some(title) = title {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            if !host.is_empty() {
                return host.to_owned();
            }
        }
    }
    "page".to_owned()
}

/// A safe, bare (no directory component) file name to suggest for saving
/// the page at `url` titled `title`, with `extension` appended (no leading
/// dot — pass [`MHTML_EXTENSION`] or [`HTML_EXTENSION`]).
///
/// Pipeline: [`default_file_stem`] (title, or the URL's host, or a fixed
/// fallback) -> [`replace_forbidden_filename_chars`] (Windows-illegal
/// characters, so nothing here is later misread as a path separator) ->
/// [`downloads::sanitize_filename`] (control characters, Windows-reserved
/// device names, trailing dot/space, length, and the final not-empty
/// fallback) -> `.{extension}` appended last, deliberately *after*
/// `sanitize_filename`'s own truncation so the extension this function was
/// asked for is never itself the part that gets cut off.
///
/// On Windows this is only ever the name *pre-filled* into the native
/// Save-As dialog (`ui::save_dialog_windows::show_save_dialog`) — the user
/// can still edit it there, and whatever the dialog finally returns is a
/// path the OS itself already validated, not something this function needs
/// to re-check. On macOS/Linux (no dialog, see docs/decisions.md D76) this
/// *is* the actual file name written to disk, collision-avoided the same
/// way a download's suggested name is (`downloads::build_destination`).
pub fn suggested_file_name(title: Option<&str>, url: &str, extension: &str) -> String {
    let stem = default_file_stem(title, url);
    let stem = replace_forbidden_filename_chars(&stem);
    let stem = downloads::sanitize_filename(&stem);
    format!("{stem}.{extension}")
}

/// Wrap a page's raw `document.documentElement.outerHTML` as a standalone,
/// loadable HTML document (the non-Windows fallback save format — see this
/// module's doc comment and docs/decisions.md D76). `outer_html` is written
/// back out **verbatim** — unlike `browser::view_source::escape_html`, this
/// document is meant to be reloaded as real markup, not displayed as text,
/// so no HTML-escaping is applied here.
pub fn wrap_outer_html_as_document(outer_html: &str) -> String {
    format!("<!doctype html>\n{outer_html}")
}

// --- WebView2 `Page.captureSnapshot` DevTools Protocol call (Windows/MHTML
// path, D76) ---

/// The DevTools Protocol method `ui::save_dialog_windows` invokes via
/// `ICoreWebView2::CallDevToolsProtocolMethod` to capture the page as MHTML.
/// Part of the base `ICoreWebView2` interface (present since WebView2's
/// first stable release, no `ICoreWebView2_NN`-generation gate needed —
/// unlike the native `ShowSaveAsUI`/`SaveAsUIShowing` APIs, which only exist
/// from `ICoreWebView2_25` onward and were rejected as too new to rely on;
/// see D76).
pub const CAPTURE_SNAPSHOT_METHOD: &str = "Page.captureSnapshot";

/// `CallDevToolsProtocolMethod`'s `parametersAsJson` argument for
/// [`CAPTURE_SNAPSHOT_METHOD`]: `"mhtml"` is the only format Chromium's
/// `Page.captureSnapshot` accepts — spelled out explicitly here rather than
/// relying on it also being the (undocumented) default.
pub const CAPTURE_SNAPSHOT_PARAMS: &str = r#"{"format":"mhtml"}"#;

#[derive(serde::Deserialize)]
struct CaptureSnapshotResult {
    data: String,
}

/// Parse `Page.captureSnapshot`'s completion JSON (`{"data": "...mhtml
/// text..."}`) into the raw MHTML text to write to disk. Pure and
/// unit-tested here since it cannot be exercised without a live WebView2
/// host — the same limitation docs/decisions.md D59 already documents for
/// this project's Linux-only CI. `ui::save_dialog_windows` only ever calls
/// this, never re-implements the parsing itself.
pub fn extract_mhtml(result_json: &str) -> Result<String, String> {
    serde_json::from_str::<CaptureSnapshotResult>(result_json)
        .map(|result| result.data)
        .map_err(|err| format!("MHTML の取得結果を解析できませんでした: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- default_file_stem (exercised indirectly through suggested_file_name
    // below too, but tested directly here for the title/url fallback chain
    // itself) ---

    #[test]
    fn uses_the_trimmed_title_when_present() {
        assert_eq!(
            default_file_stem(Some("  Example Page  "), "https://example.com/"),
            "Example Page"
        );
    }

    #[test]
    fn falls_back_to_the_url_host_when_title_is_absent() {
        assert_eq!(
            default_file_stem(None, "https://example.com/path"),
            "example.com"
        );
    }

    #[test]
    fn falls_back_to_the_url_host_when_title_is_blank() {
        assert_eq!(
            default_file_stem(Some("   "), "https://example.com/"),
            "example.com"
        );
    }

    #[test]
    fn falls_back_to_a_fixed_name_when_the_url_has_no_host() {
        assert_eq!(default_file_stem(None, "about:blank"), "page");
        assert_eq!(default_file_stem(None, "not a url"), "page");
    }

    // --- suggested_file_name: the security-critical path (D76) ---

    #[test]
    fn appends_the_requested_extension() {
        assert_eq!(
            suggested_file_name(Some("Example"), "https://example.com/", MHTML_EXTENSION),
            "Example.mhtml"
        );
        assert_eq!(
            suggested_file_name(Some("Example"), "https://example.com/", HTML_EXTENSION),
            "Example.html"
        );
    }

    #[test]
    fn replaces_windows_forbidden_characters_in_the_title() {
        assert_eq!(
            suggested_file_name(
                Some("Breaking: Top Story"),
                "https://example.com/",
                HTML_EXTENSION
            ),
            "Breaking_ Top Story.html"
        );
        assert_eq!(
            suggested_file_name(Some("A/B Testing"), "https://example.com/", HTML_EXTENSION),
            "A_B Testing.html"
        );
        assert_eq!(
            suggested_file_name(
                Some("Weird \"Title\" <here> | *?"),
                "https://example.com/",
                HTML_EXTENSION
            ),
            "Weird _Title_ _here_ _ __.html"
        );
    }

    #[test]
    fn neutralizes_path_traversal_in_the_title() {
        let name = suggested_file_name(
            Some("../../etc/passwd"),
            "https://example.com/",
            HTML_EXTENSION,
        );
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        // No directory component survives at all - this is always a single
        // flat file name, so there is nothing left to "traverse" with.
        assert_eq!(std::path::Path::new(&name).components().count(), 1);
    }

    #[test]
    fn neutralizes_an_absolute_windows_path_in_the_title() {
        let name = suggested_file_name(
            Some(r"C:\Windows\System32\evil.exe"),
            "https://example.com/",
            HTML_EXTENSION,
        );
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert!(!name.contains(':'));
        assert_eq!(std::path::Path::new(&name).components().count(), 1);
    }

    #[test]
    fn escapes_a_windows_reserved_device_name_title() {
        assert_eq!(
            suggested_file_name(Some("CON"), "https://example.com/", HTML_EXTENSION),
            "_CON.html"
        );
        assert_eq!(
            suggested_file_name(Some("con"), "https://example.com/", MHTML_EXTENSION),
            "_con.mhtml"
        );
        assert_eq!(
            suggested_file_name(Some("LPT9"), "https://example.com/", HTML_EXTENSION),
            "_LPT9.html"
        );
    }

    #[test]
    fn does_not_flag_a_title_that_merely_starts_with_a_reserved_prefix() {
        assert_eq!(
            suggested_file_name(Some("CONSTITUTION"), "https://example.com/", HTML_EXTENSION),
            "CONSTITUTION.html"
        );
    }

    #[test]
    fn trims_trailing_dots_and_spaces_from_the_title() {
        assert_eq!(
            suggested_file_name(Some("Example..."), "https://example.com/", HTML_EXTENSION),
            "Example.html"
        );
        assert_eq!(
            suggested_file_name(Some("Example   "), "https://example.com/", HTML_EXTENSION),
            "Example.html"
        );
    }

    #[test]
    fn strips_control_characters_from_the_title() {
        assert_eq!(
            suggested_file_name(Some("evil\0title"), "https://example.com/", HTML_EXTENSION),
            "eviltitle.html"
        );
        assert_eq!(
            suggested_file_name(Some("a\nb\tc"), "https://example.com/", HTML_EXTENSION),
            "abc.html"
        );
    }

    #[test]
    fn falls_back_to_a_safe_name_when_the_title_is_only_dot_or_dotdot() {
        assert_eq!(
            suggested_file_name(Some("."), "https://example.com/", HTML_EXTENSION),
            "download.html"
        );
        assert_eq!(
            suggested_file_name(Some(".."), "https://example.com/", HTML_EXTENSION),
            "download.html"
        );
    }

    #[test]
    fn stays_non_empty_when_the_title_is_entirely_forbidden_characters() {
        // Every character replaced with `_`, none of it dot/space/control,
        // so this must *not* hit `sanitize_filename`'s empty-name fallback -
        // it should stay a valid (if ugly) name rather than silently
        // becoming "download.html" and risking collision with an unrelated
        // save.
        assert_eq!(
            suggested_file_name(Some(":::"), "https://example.com/", HTML_EXTENSION),
            "___.html"
        );
    }

    #[test]
    fn is_never_empty_before_the_extension_even_with_no_title_and_an_invalid_url() {
        let name = suggested_file_name(Some(""), "not a url", HTML_EXTENSION);
        assert!(!name.is_empty());
        assert!(name.ends_with(".html"));
        assert_eq!(name, "page.html");
    }

    #[test]
    fn truncates_an_extremely_long_title() {
        let long_title = "a".repeat(500);
        let name = suggested_file_name(Some(&long_title), "https://example.com/", HTML_EXTENSION);
        // Generous ceiling: `sanitize_filename`'s own 200-byte cap plus the
        // ".html" this function appends afterwards.
        assert!(name.len() <= 210);
        assert!(name.ends_with(".html"));
    }

    #[test]
    fn keeps_unicode_titles_intact() {
        assert_eq!(
            suggested_file_name(
                Some("日本語のタイトル"),
                "https://example.com/",
                HTML_EXTENSION
            ),
            "日本語のタイトル.html"
        );
    }

    // --- wrap_outer_html_as_document ---

    #[test]
    fn wraps_outer_html_with_a_doctype_prefix() {
        let doc = wrap_outer_html_as_document("<html><body>hi</body></html>");
        assert!(doc.starts_with("<!doctype html>\n"));
        assert!(doc.ends_with("<html><body>hi</body></html>"));
    }

    #[test]
    fn does_not_alter_the_outer_html_itself() {
        // Unlike `view_source::escape_html`, this must never escape or
        // otherwise transform the markup - it has to remain loadable as
        // real HTML.
        let doc = wrap_outer_html_as_document("<html><body><script>x()</script></body></html>");
        assert!(doc.contains("<script>x()</script>"));
    }

    // --- extract_mhtml ---

    #[test]
    fn extract_mhtml_parses_the_data_field() {
        let json = r#"{"data":"MIME-Version: 1.0\r\nContent-Type: multipart/related\r\n"}"#;
        assert_eq!(
            extract_mhtml(json).unwrap(),
            "MIME-Version: 1.0\r\nContent-Type: multipart/related\r\n"
        );
    }

    #[test]
    fn extract_mhtml_rejects_malformed_or_unexpected_json() {
        assert!(extract_mhtml("not json").is_err());
        assert!(extract_mhtml(r#"{"nope": true}"#).is_err());
        assert!(extract_mhtml("").is_err());
    }
}
