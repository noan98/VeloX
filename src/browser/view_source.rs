//! Pure logic for "View Source" (Issue #45): turning a page's raw HTML text
//! into a **plain-text-escaped**, line-numbered document safe to render in a
//! webview, plus the small `data:` URL encoder that carries it there.
//!
//! **Security is the entire point of this module.** The input here is a
//! page's own markup — which may itself contain `<script>` tags, event
//! handler attributes, or anything else a hostile page cares to put in its
//! HTML. If that text were ever inserted into a new page *as markup*, the
//! source viewer would simply re-run the very page it was supposed to be
//! inspecting, on VeloX's own trusted browser chrome/tab. Every byte of page
//! source that reaches [`build_view_source_document`] is therefore escaped
//! with [`escape_html`] before being written out — it can only ever render
//! as inert text inside a `<pre>` block, never as markup. See
//! docs/decisions.md D72 for the full design rationale and the unit tests
//! below (in particular the ones with `<script>` and a source cut off
//! mid-tag) for what this guarantees.
//!
//! This module knows nothing about `wry`/tabs/webviews — see
//! docs/architecture.md's four-layer split. `ui::window::BrowserWindow`
//! fetches the raw source (`document.documentElement.outerHTML`) and
//! `app.rs` calls into here to turn it into something displayable.

/// Maximum number of UTF-8 bytes of raw page source rendered before
/// truncating (see [`truncate_source_utf8`]).
///
/// Deliberately much smaller than "as much as the engine can fetch": the
/// resulting `data:` URL becomes that view-source tab's
/// `browser::tab::Tab::current_url` for as long as the tab stays open, and
/// `current_url` is not a one-time payload — it is re-serialized into the
/// tab strip (`app::sync_tab_strip`'s `ToolbarCommand`/`TabSummary` JSON,
/// pushed to the toolbar webview via `evaluate_script` on nearly every
/// tab-affecting event) and into `session.json` on every
/// `app::persist_session` call. A multi-megabyte source would turn "switch
/// tabs" or "close an unrelated tab" into a multi-megabyte `evaluate_script`
/// call and a multi-megabyte disk write, repeatedly, for as long as the view
/// source tab stays open. 300 KB keeps the encoded `data:` URL in the
/// hundreds-of-KB range (see [`to_data_url`]'s doc comment for the encoding
/// overhead) — enough to read the markup of the large majority of real
/// pages (minified bundles live in `<script src>`/external files, not in the
/// HTML document itself) without that cost, while pages beyond it are still
/// viewable, just truncated with a visible notice.
pub const MAX_SOURCE_BYTES: usize = 300_000;

/// Escape `input` for safe embedding as HTML **text content** (never inside
/// an attribute value or a `<script>`/`<style>` block — this module never
/// puts page source in either). Escapes the five characters that matter for
/// preventing a browser's HTML parser from ever treating any part of
/// `input` as a tag, entity, or attribute boundary: `&`, `<`, `>`, `"`,
/// `'`.
///
/// This alone is what keeps [`build_view_source_document`] safe: no matter
/// what `input` contains — a complete `<script>...</script>` block, a
/// stray `<`, a source string cut off mid-tag — the escaped result can only
/// ever be parsed as literal text, never as markup.
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Truncate `source` to at most `max_bytes` UTF-8 bytes, on a `char`
/// boundary (never splitting a multi-byte character, which would otherwise
/// panic on the slice below), returning the possibly-shortened source and
/// whether truncation actually happened (`false` when `source` already fit).
pub fn truncate_source_utf8(source: &str, max_bytes: usize) -> (&str, bool) {
    if source.len() <= max_bytes {
        return (source, false);
    }
    let mut end = max_bytes;
    while end > 0 && !source.is_char_boundary(end) {
        end -= 1;
    }
    (&source[..end], true)
}

/// Build the full HTML document View Source shows: a header naming
/// `page_url`, then `source` — truncated to `max_bytes` (see
/// [`truncate_source_utf8`], with a visible notice when that happened),
/// escaped ([`escape_html`]) and numbered one `<span>` pair per line inside
/// one `<pre>` block.
///
/// `page_url` is also escaped before being embedded (in both `<title>` and
/// the header) — defense in depth: every real caller only ever passes an
/// already-`browser::navigation::normalize_input`-validated URL, which
/// cannot contain a literal `<`/`>`/`"` unescaped, but this function does
/// not rely on that being true to stay safe.
///
/// The returned document contains no `<script>` element at all — nothing
/// here ever needs to run script — so there is no JS string/regex context
/// for `source`/`page_url` to break out of in the first place (contrast
/// `ui::window`'s `find_query_literal`/D62's `escape_js_line_terminators`,
/// which escape content destined for exactly such a context). Standard HTML
/// text escaping is the entire defense this document needs, and it is
/// applied to every byte of untrusted content that reaches the output.
pub fn build_view_source_document(page_url: &str, source: &str, max_bytes: usize) -> String {
    let (visible, truncated) = truncate_source_utf8(source, max_bytes);
    let mut body = String::with_capacity(visible.len() + visible.len() / 4 + 64);
    let mut line_count: u64 = 0;
    for line in visible.lines() {
        line_count += 1;
        body.push_str("<span class=\"ln\">");
        body.push_str(&line_count.to_string());
        body.push_str("</span><span class=\"src\">");
        body.push_str(&escape_html(line));
        body.push_str("</span>\n");
    }
    // An empty page (or a page whose source is exactly one blank line) still
    // gets one visible (empty) row rather than a completely blank `<pre>`,
    // matching what `str::lines` would otherwise silently drop.
    if line_count == 0 {
        body.push_str("<span class=\"ln\">1</span><span class=\"src\"></span>\n");
    }
    let notice = if truncated {
        "<p class=\"velox-notice\">\
         ソースが大きいため、先頭の一部のみを表示しています。\
         </p>"
    } else {
        ""
    };
    let escaped_url = escape_html(page_url);
    format!(
        "<!doctype html>\n\
<html>\n\
<head>\n\
<meta charset=\"utf-8\">\n\
<title>ソースを表示: {escaped_url}</title>\n\
<style>\n\
  :root {{ color-scheme: light dark; }}\n\
  body {{ margin: 0; font-family: system-ui, sans-serif; }}\n\
  header {{\n\
    position: sticky; top: 0; padding: 8px 12px;\n\
    background: #eee; border-bottom: 1px solid #ccc;\n\
    font-size: 13px; word-break: break-all;\n\
  }}\n\
  .velox-notice {{ margin: 0; padding: 6px 12px; color: #a30000; font-size: 13px; }}\n\
  pre {{\n\
    margin: 0; padding: 8px 0 32px; font-size: 13px; line-height: 1.5;\n\
    font-family: ui-monospace, Menlo, Consolas, monospace;\n\
    white-space: pre-wrap; word-break: break-word;\n\
  }}\n\
  .ln {{\n\
    display: inline-block; width: 4em; padding-right: 1em;\n\
    text-align: right; color: #888; user-select: none;\n\
  }}\n\
  @media (prefers-color-scheme: dark) {{\n\
    header {{ background: #2a2a2a; border-color: #444; color: #eee; }}\n\
    .velox-notice {{ color: #ff8a8a; }}\n\
    .ln {{ color: #888; }}\n\
  }}\n\
</style>\n\
</head>\n\
<body>\n\
<header>ソースを表示: {escaped_url}</header>\n\
{notice}\n\
<pre>{body}</pre>\n\
</body>\n\
</html>\n",
    )
}

/// The 64-character RFC 4648 base64 alphabet (standard, `+`/`/`, `=`
/// padding) — the same alphabet every mainstream `data:` URL uses.
const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A small, dependency-free RFC 4648 base64 encoder (no crate added for
/// this — see docs/decisions.md D6/D72 — this is the one place VeloX needs
/// it, and the algorithm is a fixed ~20-line lookup-table transform with no
/// security-sensitive properties of its own to get subtly wrong; the unit
/// tests below check it against the RFC's own test vectors).
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(BASE64_TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(BASE64_TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_TABLE[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_TABLE[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Wrap `html` (assumed to already be a complete, safe-to-render document —
/// i.e. the output of [`build_view_source_document`]) as a base64-encoded
/// `data:text/html` URL, loadable by `wry::WebView::load_url` like any other
/// URL (`data:` is already in `browser::navigation::ALLOWED_SCHEMES`).
///
/// Base64, not percent-encoding: percent-encoding every non-alphanumeric
/// byte of an HTML document (every space, newline, `<`/`>`/`"` that
/// [`escape_html`] just turned into multi-character entities, ...) would
/// expand many common bytes to `%XX` (3x), and — unlike inside a `<pre>` —
/// characters such as `#`/`?`/`%` are not just visual noise here, they are
/// structural to a URL and would need escaping anyway to avoid truncating
/// or corrupting the `data:` URL itself at the first stray `#`/`?`. Base64 is
/// a fixed, content-independent ~33% expansion with no such special
/// characters in its output alphabet.
pub fn to_data_url(html: &str) -> String {
    format!(
        "data:text/html;charset=utf-8;base64,{}",
        base64_encode(html.as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- escape_html ---

    #[test]
    fn escape_html_escapes_all_five_special_characters() {
        assert_eq!(escape_html("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
    }

    #[test]
    fn escape_html_leaves_ordinary_and_unicode_text_untouched() {
        assert_eq!(escape_html("hello, world"), "hello, world");
        assert_eq!(escape_html("こんにちは 世界"), "こんにちは 世界");
    }

    #[test]
    fn escape_html_neutralizes_a_complete_script_tag() {
        let escaped = escape_html("<script>alert(document.cookie)</script>");
        assert!(!escaped.contains("<script>"));
        assert!(!escaped.contains("</script>"));
        assert_eq!(
            escaped,
            "&lt;script&gt;alert(document.cookie)&lt;/script&gt;"
        );
    }

    // --- truncate_source_utf8 ---

    #[test]
    fn truncate_source_utf8_returns_input_unchanged_when_under_the_limit() {
        let (out, truncated) = truncate_source_utf8("hello", 100);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_source_utf8_returns_input_unchanged_when_exactly_at_the_limit() {
        let (out, truncated) = truncate_source_utf8("hello", 5);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_source_utf8_cuts_at_the_byte_limit_for_ascii() {
        let (out, truncated) = truncate_source_utf8("hello world", 5);
        assert_eq!(out, "hello");
        assert!(truncated);
    }

    #[test]
    fn truncate_source_utf8_never_splits_a_multibyte_character() {
        // "あ" is 3 bytes in UTF-8; a byte limit landing inside it must back
        // off to the previous character boundary instead of panicking.
        let source = "aaあbb";
        for max_bytes in 0..source.len() {
            let (out, _) = truncate_source_utf8(source, max_bytes);
            // Must not panic (the slice above would panic on a non-boundary
            // index) and must always be valid UTF-8 on its own.
            assert!(out.len() <= max_bytes, "max_bytes={max_bytes} out={out:?}");
            let _ = out.chars().count(); // would panic on invalid UTF-8
        }
    }

    #[test]
    fn truncate_source_utf8_handles_zero_max_bytes() {
        let (out, truncated) = truncate_source_utf8("hello", 0);
        assert_eq!(out, "");
        assert!(truncated);
    }

    #[test]
    fn truncate_source_utf8_handles_empty_source() {
        let (out, truncated) = truncate_source_utf8("", 10);
        assert_eq!(out, "");
        assert!(!truncated);
    }

    // --- build_view_source_document: the security-critical tests ---

    #[test]
    fn build_view_source_document_never_reproduces_a_live_script_tag() {
        let source = "<html><body><script>alert(document.cookie)</script></body></html>";
        let doc = build_view_source_document("https://example.com/", source, MAX_SOURCE_BYTES);
        assert!(
            !doc.contains("<script>alert"),
            "the page's own <script> tag must never survive as live markup: {doc}"
        );
        // The escaped form must still be present as visible text.
        assert!(doc.contains("&lt;script&gt;alert(document.cookie)&lt;/script&gt;"));
    }

    #[test]
    fn build_view_source_document_is_safe_when_source_is_cut_off_mid_tag() {
        // A source string that ends in the middle of an opening tag (as a
        // truncated fetch, or simply a malformed page, could produce).
        let source = "some text <scr";
        let doc = build_view_source_document("https://example.com/", source, MAX_SOURCE_BYTES);
        // The stray "<" must be escaped, never left as a literal "<" that
        // could be reinterpreted as the start of a tag by the surrounding
        // document (e.g. if a later chunk of the *document template* itself
        // happened to follow it).
        assert!(!doc.contains("some text <scr"));
        assert!(doc.contains("some text &lt;scr"));
    }

    #[test]
    fn build_view_source_document_escapes_an_attempted_pre_closing_tag() {
        // Source that tries to close our own wrapping `<pre>` early and
        // inject a sibling element.
        let source = "</pre><img src=x onerror=alert(1)>";
        let doc = build_view_source_document("https://example.com/", source, MAX_SOURCE_BYTES);
        assert!(!doc.contains("</pre><img"));
        assert!(doc.contains("&lt;/pre&gt;&lt;img src=x onerror=alert(1)&gt;"));
        // Exactly one real `<pre>...</pre>` pair remains — ours.
        assert_eq!(doc.matches("<pre>").count(), 1);
        assert_eq!(doc.matches("</pre>").count(), 1);
    }

    #[test]
    fn build_view_source_document_escapes_the_page_url_in_title_and_header() {
        let hostile_url = "https://example.com/\"><script>alert(1)</script>";
        let doc = build_view_source_document(hostile_url, "<p>hi</p>", MAX_SOURCE_BYTES);
        assert!(!doc.contains("<script>alert(1)</script>"));
        assert!(doc.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[test]
    fn build_view_source_document_numbers_lines_starting_at_one() {
        let doc = build_view_source_document(
            "https://example.com/",
            "line one\nline two\nline three",
            10_000,
        );
        assert!(doc.contains(">1</span><span class=\"src\">line one<"));
        assert!(doc.contains(">2</span><span class=\"src\">line two<"));
        assert!(doc.contains(">3</span><span class=\"src\">line three<"));
    }

    #[test]
    fn build_view_source_document_renders_one_empty_row_for_empty_source() {
        let doc = build_view_source_document("https://example.com/", "", MAX_SOURCE_BYTES);
        assert!(doc.contains(">1</span><span class=\"src\"></span>"));
    }

    #[test]
    fn build_view_source_document_adds_a_notice_only_when_actually_truncated() {
        // Checked against the actual notice *paragraph*, not just the
        // substring "velox-notice" — that also appears in this document's
        // (always-present) `<style>` rule for it, which must not make this
        // test pass regardless of whether truncation happened.
        let notice_tag = "<p class=\"velox-notice\">";

        let short = build_view_source_document("https://example.com/", "hello", 10_000);
        assert!(!short.contains(notice_tag), "{short}");

        let long = build_view_source_document("https://example.com/", "hello world", 5);
        assert!(long.contains(notice_tag), "{long}");
    }

    #[test]
    fn build_view_source_document_respects_the_max_bytes_cap() {
        let source = "q".repeat(1_000);
        let doc = build_view_source_document("https://example.com/", &source, 10);
        // Extract exactly what landed inside the one `<span class="src">`
        // row (there is only one line, since the un-truncated source has no
        // newlines) rather than counting a repeated character anywhere in
        // the whole document — the surrounding template's own CSS/HTML is
        // free to contain that character too (e.g. "font-size" or "8px").
        let open_tag = "<span class=\"src\">";
        let start = doc.find(open_tag).expect("missing src span") + open_tag.len();
        let end = start + doc[start..].find("</span>").expect("missing closing span");
        assert_eq!(&doc[start..end], "q".repeat(10), "{doc}");
    }

    // --- base64_encode: RFC 4648 test vectors ---

    #[test]
    fn base64_encode_matches_rfc4648_test_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_output_length_is_a_multiple_of_four() {
        for len in 0..20 {
            let bytes = vec![0x41u8; len];
            assert_eq!(base64_encode(&bytes).len() % 4, 0, "len={len}");
        }
    }

    // --- to_data_url ---

    #[test]
    fn to_data_url_has_the_expected_prefix_and_a_valid_base64_payload() {
        let url = to_data_url("<!doctype html><p>hi</p>");
        let payload = url
            .strip_prefix("data:text/html;charset=utf-8;base64,")
            .expect("missing expected data: URL prefix");
        assert!(!payload.is_empty());
        assert!(payload
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')));
    }

    #[test]
    fn to_data_url_round_trips_through_the_encoder_deterministically() {
        // Same input always produces the same data: URL (no randomness, no
        // timestamps) — a basic sanity property callers rely on implicitly.
        let a = to_data_url("hello");
        let b = to_data_url("hello");
        assert_eq!(a, b);
    }
}
