//! Address bar input handling: turning what the user typed into a URL the
//! engine can load.

use url::Url;

/// Schemes VeloX will pass to the web engine.
///
/// Deliberately small for the MVP; anything else is rejected instead of being
/// handed to the engine unchecked.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "file", "about", "data"];

/// Normalize address bar input into a loadable URL.
///
/// Rules:
/// - surrounding whitespace is ignored
/// - input with an allowed explicit scheme is used as-is (after parsing)
/// - schemeless input (`example.com`, `rust-lang.org/learn`) is retried as
///   `https://<input>`; loopback hosts (`localhost:8080`, `127.0.0.1`) get
///   `http://`, since local dev servers rarely speak TLS
/// - empty input, unsupported schemes and unparseable input yield `None`
///
/// Search-engine fallback is intentionally not implemented yet.
pub fn normalize_input(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    if let Ok(parsed) = Url::parse(input) {
        if ALLOWED_SCHEMES.contains(&parsed.scheme()) {
            return Some(parsed.into());
        }
        // `localhost:8080` parses as scheme "localhost"; only treat input as
        // truly having a scheme when it spells one out with "://" (or is a
        // non-authority scheme like `about:`/`data:`, handled above).
        if input.contains("://") {
            return None;
        }
    }

    let with_https = format!("https://{input}");
    match Url::parse(&with_https) {
        Ok(mut parsed) => {
            if is_loopback_host(&parsed) {
                // Both http and https are "special" schemes, so this cannot
                // fail; keep https if it somehow does.
                let _ = parsed.set_scheme("http");
            }
            Some(parsed.into())
        }
        Err(_) => None,
    }
}

/// True for hosts that almost certainly serve plain HTTP during development.
fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost" || domain.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

// --- Omnibox: URL-vs-search classification (Issue #15) ---
//
// `normalize_input` above stays exactly as it was (and is still the only
// place URL parsing/scheme rejection happens); everything below only ever
// calls it, never duplicates it. See docs/decisions.md D26.

/// What the omnibox should do with a piece of address-bar input: load it as
/// a URL, or send it to the configured search engine as a query. See
/// [`classify_input`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Load this URL — already normalized via [`normalize_input`], never the
    /// raw input.
    Url(String),
    /// Search for this text — trimmed, still raw (percent-encoding happens
    /// in [`build_search_url`], not here).
    Search(String),
}

/// Decide whether address-bar/omnibox input should be loaded as a URL or
/// sent to the search engine as a query, and normalize it for whichever
/// applies.
///
/// Rules, in order:
/// - empty (or whitespace-only) input yields `None` — nothing to do
/// - a leading `?` always forces a search, e.g. `?rust` searches for
///   `rust`, regardless of what follows; `?` with nothing left after it
///   (once trimmed) yields `None`
/// - input containing more than one whitespace-separated word is always a
///   search (`rust ownership`) — a real URL a user types by hand never
///   contains an unencoded space, so there is nothing to gain by attempting
///   [`normalize_input`] on it first
/// - a single "word" is attempted as a URL only when it looks like one — an
///   explicit scheme, a dotted host (`example.com`), or a `:`-qualified
///   host (`localhost:3000`, `about:blank`); [`looks_url_like`] decides.
///   [`normalize_input`] is the *only* place that parses/normalizes/rejects
///   it, so scheme allow-listing lives in exactly one function. A rejected
///   URL (unsupported scheme, unparseable) yields `None` here too — it is
///   *not* silently retried as a search, since that would blur "this input
///   was refused" into "this input was resolved to something else" for
///   whatever made `normalize_input` say no (see docs/decisions.md D26)
/// - a single word that does not look like a URL at all (`rust`, with no
///   dot and no colon) is treated as a search query — typing a bare word
///   and hitting enter is almost always meant as a search, not a domain
///   with an assumed TLD
pub fn classify_input(input: &str) -> Option<Intent> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix('?') {
        let rest = rest.trim();
        return if rest.is_empty() {
            None
        } else {
            Some(Intent::Search(rest.to_owned()))
        };
    }

    if trimmed.split_whitespace().count() > 1 {
        return Some(Intent::Search(trimmed.to_owned()));
    }

    if looks_url_like(trimmed) {
        return normalize_input(trimmed).map(Intent::Url);
    }

    Some(Intent::Search(trimmed.to_owned()))
}

/// Heuristic for whether a single "word" of input (no whitespace, already
/// checked by [`classify_input`]) is shaped like something meant as a URL
/// rather than a search term: it names an explicit scheme (`https://…`,
/// `about:blank`) or a dotted/colon-qualified host (`example.com`,
/// `localhost:3000`). This only gates whether [`normalize_input`] is even
/// attempted — it never itself decides a URL is valid or safe; that stays
/// entirely inside [`normalize_input`].
fn looks_url_like(input: &str) -> bool {
    input.contains(':') || input.contains('.')
}

/// Build a search-engine URL from a query template and raw query text.
///
/// `template` must contain the literal placeholder `{}` exactly once (e.g.
/// `https://duckduckgo.com/?q={}`); `query` is percent-encoded as
/// `application/x-www-form-urlencoded` (spaces become `+`, reserved
/// characters are escaped) via [`url::form_urlencoded`] — never
/// hand-rolled — before being substituted in. The result is parsed with
/// [`Url::parse`] and only handed back if it comes out as an `http`/`https`
/// URL, so a misconfigured template (missing `{}`, or a non-URL template)
/// cannot produce something the engine would be asked to load.
pub fn build_search_url(template: &str, query: &str) -> Option<String> {
    if !template.contains("{}") {
        return None;
    }
    let encoded: String = url::form_urlencoded::byte_serialize(query.trim().as_bytes()).collect();
    let candidate = template.replacen("{}", &encoded, 1);
    let parsed = Url::parse(&candidate).ok()?;
    if matches!(parsed.scheme(), "http" | "https") {
        Some(parsed.into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_explicit_http_and_https() {
        assert_eq!(
            normalize_input("https://example.com"),
            Some("https://example.com/".to_owned())
        );
        assert_eq!(
            normalize_input("http://example.com/a?b=c"),
            Some("http://example.com/a?b=c".to_owned())
        );
    }

    #[test]
    fn prepends_https_to_schemeless_input() {
        assert_eq!(
            normalize_input("example.com"),
            Some("https://example.com/".to_owned())
        );
        assert_eq!(
            normalize_input("rust-lang.org/learn"),
            Some("https://rust-lang.org/learn".to_owned())
        );
    }

    #[test]
    fn loopback_hosts_default_to_http() {
        assert_eq!(
            normalize_input("localhost:8080"),
            Some("http://localhost:8080/".to_owned())
        );
        assert_eq!(
            normalize_input("127.0.0.1:8741/index.html"),
            Some("http://127.0.0.1:8741/index.html".to_owned())
        );
    }

    #[test]
    fn explicit_https_to_loopback_is_kept() {
        assert_eq!(
            normalize_input("https://localhost:8443"),
            Some("https://localhost:8443/".to_owned())
        );
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            normalize_input("  example.com  "),
            Some("https://example.com/".to_owned())
        );
    }

    #[test]
    fn keeps_about_urls() {
        assert_eq!(
            normalize_input("about:blank"),
            Some("about:blank".to_owned())
        );
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(normalize_input(""), None);
        assert_eq!(normalize_input("   "), None);
    }

    #[test]
    fn rejects_unsupported_schemes() {
        assert_eq!(normalize_input("ftp://example.com"), None);
        assert_eq!(normalize_input("javascript://alert(1)"), None);
    }

    #[test]
    fn rejects_unparseable_input() {
        assert_eq!(normalize_input("not a url"), None);
    }

    // --- Robustness against hostile/malformed input (Issue #35): every
    // case here only needs to not panic and (where there is an obviously
    // correct answer) resolve to the expected `Some`/`None` — never a
    // scheme VeloX did not already intend to allow. See docs/decisions.md
    // D62. ---

    #[test]
    fn rejects_dangerous_schemes_regardless_of_case_or_whitespace_tricks() {
        for hostile in [
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "JavaScript:alert(1)",
            "  javascript:alert(1)",
            "javascript:alert(1)  ",
            "\tjavascript:alert(1)",
            "vbscript:msgbox(1)",
            "livescript:alert(1)",
            "javascript:alert(1)//innocuous-looking-comment",
        ] {
            assert_eq!(
                normalize_input(hostile),
                None,
                "{hostile:?} should have been rejected"
            );
        }
    }

    #[test]
    fn a_control_character_embedded_in_the_scheme_defeats_parsing_rather_than_smuggling_it_through()
    {
        // A literal tab/newline spliced into "javascript:" so the scheme no
        // longer spells the word out — must not become a way to sneak the
        // scheme past `ALLOWED_SCHEMES`. Either `Url::parse` itself refuses
        // this (most likely, since a raw control character inside what
        // would be the scheme is not valid per the URL spec) or it parses
        // into some other scheme name that is not in `ALLOWED_SCHEMES` —
        // either way the result must be `None`, never a loadable URL.
        for hostile in ["java\tscript:alert(1)", "java\nscript:alert(1)"] {
            assert_eq!(
                normalize_input(hostile),
                None,
                "{hostile:?} should have been rejected"
            );
        }
    }

    #[test]
    fn does_not_panic_on_an_extremely_long_input() {
        // A multi-megabyte paste into the address bar must never panic —
        // this is the one guarantee this test pins. A bare huge "word" with
        // no dots/scheme of its own is schemeless input, so it takes the
        // same `https://<input>` path any bare word does (a real browser
        // does not reject an absurdly long hostname at the input layer
        // either — DNS resolution is where that eventually fails, not URL
        // normalization), so this legitimately comes back `Some`, just a
        // very long one.
        let huge = "a".repeat(5_000_000);
        let result = normalize_input(&huge);
        assert!(result.is_some());
        assert!(result.unwrap().starts_with("https://"));

        // A huge *scheme* name followed by `://` is not one of
        // `ALLOWED_SCHEMES` no matter how long it is — rejected the same
        // way `ftp://` is.
        let huge_scheme = format!("{}://evil.example/", "a".repeat(2_000_000));
        assert_eq!(normalize_input(&huge_scheme), None);

        // A syntactically valid, merely huge, https URL must still work —
        // the size cap that matters for IPC (`toolbar::MAX_IPC_PAYLOAD_BYTES`)
        // is a different boundary; `normalize_input` itself has no reason to
        // second-guess an enormous but well-formed query string.
        let huge_query = format!("https://example.com/?q={}", "a".repeat(2_000_000));
        assert_eq!(
            normalize_input(&huge_query),
            Some(huge_query),
            "a huge but well-formed URL must still normalize"
        );
    }

    #[test]
    fn does_not_panic_on_embedded_nul_or_other_control_characters() {
        for hostile in [
            "http://example.com/\0",
            "https://example.com/a\0b",
            "https://example.com/\x01\x02\x03",
            "https://example.com/a\rb\nc",
        ] {
            // No expectation on the exact `Some`/`None` outcome (the `url`
            // crate's own control-character handling per the WHATWG URL
            // spec governs that) — only that this never panics.
            let _ = normalize_input(hostile);
        }
    }

    #[test]
    fn keeps_legitimate_unicode_and_idn_domains() {
        // Non-ASCII/internationalized domains and paths are normal input —
        // hardening against attacks must never regress these (see this
        // module's CLAUDE.md-driven "don't break legitimate input" rule).
        // The exact Punycode (`xn--...`) the `idna`/`url` crates produce is
        // an implementation detail this test does not pin — only that an
        // IDN host is accepted and normalized to *some* ASCII-compatible
        // form, and a non-ASCII path is percent-encoded rather than
        // rejected.
        let idn = normalize_input("https://例え.テスト/").expect("IDN host must be accepted");
        assert!(idn.starts_with("https://xn--"), "{idn}");
        assert!(idn.is_ascii(), "{idn}");

        let schemeless_idn =
            normalize_input("日本語.example/パス").expect("schemeless IDN host must be accepted");
        assert!(schemeless_idn.starts_with("https://"));
        assert!(
            schemeless_idn.contains("%E3%83%91%E3%82%B9"),
            "{schemeless_idn}"
        );
    }

    #[test]
    fn does_not_panic_on_bidi_override_characters_in_a_url() {
        // A right-to-left override character can make a URL's path *look*
        // like it points somewhere else when rendered (a known browser
        // spoofing vector, tracked separately — see docs/decisions.md D62's
        // "what's out of scope" note); VeloX does not attempt to strip or
        // reject it today, since doing so blindly could mangle legitimate
        // RTL-script paths. This only pins "must not panic" and "the
        // scheme/host are unaffected by what the path contains".
        let hostile = "https://example.com/\u{202e}gpj.exe";
        let result = normalize_input(hostile);
        assert!(result.is_some());
        assert!(result.unwrap().starts_with("https://example.com/"));
    }

    #[test]
    fn userinfo_before_the_host_does_not_change_the_actual_host() {
        // `https://google.com@evil.com/` parses (per the URL spec) with
        // `evil.com` as the *host* and `google.com` as discarded userinfo —
        // a well-known browser-address-bar spoofing pattern. VeloX does not
        // special-case or strip this today (see D62); this test only pins
        // that the crate's parsing behaves the way every caller of
        // `normalize_input` already assumes, and that it never panics.
        let result = normalize_input("https://google.com@evil.com/");
        assert_eq!(result, Some("https://google.com@evil.com/".to_owned()));
    }

    #[test]
    fn backslashes_are_treated_like_forward_slashes_for_special_schemes() {
        // Per the WHATWG URL spec (which the `url` crate implements),
        // backslashes act as path separators for "special" schemes
        // (http/https/...) — a legacy IE quirk browsers still emulate so a
        // URL cannot be misinterpreted differently by VeloX than by the
        // engine actually loading it. Pinned here so a future crate upgrade
        // that changed this would be caught by a test, not discovered as a
        // security regression.
        assert_eq!(
            normalize_input(r"https:\\evil.example\path"),
            Some("https://evil.example/path".to_owned())
        );
    }

    #[test]
    fn rejects_an_unknown_explicit_scheme_even_when_it_looks_url_shaped() {
        for hostile in [
            "not-a-real-scheme://host/",
            "chrome://settings/",
            "about:config", // only `about:blank` is meaningful, but the
            // scheme itself is allowed — this documents that
            // `normalize_input` does not special-case the
            // path/opaque-data half of an `about:` URL.
            "file:///etc/passwd", // `file:` is in ALLOWED_SCHEMES (D-nothing,
                                  // predates decision numbering) — kept
                                  // exactly as-is, not newly rejected here.
        ] {
            let _ = normalize_input(hostile); // must not panic either way
        }
        assert_eq!(normalize_input("not-a-real-scheme://host/"), None);
        assert_eq!(normalize_input("chrome://settings/"), None);
    }

    #[test]
    fn incomplete_percent_encoding_does_not_panic() {
        for hostile in [
            "https://example.com/%",
            "https://example.com/%2",
            "https://example.com/%zz",
            "https://example.com/%%%%",
        ] {
            let _ = normalize_input(hostile);
        }
    }

    #[test]
    fn a_huge_data_url_is_accepted_without_panicking() {
        // `data:` is a deliberately allowed scheme (ALLOWED_SCHEMES); this
        // only confirms a large payload inside one does not cause excessive
        // work or a panic, not that the scheme itself is newly considered
        // safe (unchanged behavior — see docs/decisions.md D62).
        let huge_data_url = format!("data:text/plain;base64,{}", "QQ==".repeat(500_000));
        let result = normalize_input(&huge_data_url);
        assert!(result.is_some());
        assert!(result.unwrap().starts_with("data:"));
    }

    // --- classify_input ---

    #[test]
    fn classifies_dotted_host_as_url() {
        assert_eq!(
            classify_input("example.com"),
            Some(Intent::Url("https://example.com/".to_owned()))
        );
    }

    #[test]
    fn classifies_loopback_with_port_as_url() {
        assert_eq!(
            classify_input("localhost:3000"),
            Some(Intent::Url("http://localhost:3000/".to_owned()))
        );
    }

    #[test]
    fn classifies_explicit_scheme_as_url() {
        assert_eq!(
            classify_input("http://example.com/a?b=c"),
            Some(Intent::Url("http://example.com/a?b=c".to_owned()))
        );
        assert_eq!(
            classify_input("about:blank"),
            Some(Intent::Url("about:blank".to_owned()))
        );
    }

    #[test]
    fn classifies_multi_word_input_as_search() {
        assert_eq!(
            classify_input("rust ownership"),
            Some(Intent::Search("rust ownership".to_owned()))
        );
    }

    #[test]
    fn classifies_bare_word_with_no_dot_or_colon_as_search() {
        assert_eq!(
            classify_input("rust"),
            Some(Intent::Search("rust".to_owned()))
        );
    }

    #[test]
    fn leading_question_mark_forces_a_search() {
        assert_eq!(
            classify_input("?rust"),
            Some(Intent::Search("rust".to_owned()))
        );
        // Even something that would otherwise look like a URL is still a
        // forced search once the sentinel prefix is stripped.
        assert_eq!(
            classify_input("?example.com"),
            Some(Intent::Search("example.com".to_owned()))
        );
    }

    #[test]
    fn bare_question_mark_is_rejected() {
        assert_eq!(classify_input("?"), None);
        assert_eq!(classify_input("?   "), None);
    }

    #[test]
    fn classify_rejects_empty_and_whitespace_only_input() {
        assert_eq!(classify_input(""), None);
        assert_eq!(classify_input("   "), None);
    }

    #[test]
    fn classify_rejects_dangerous_schemes_instead_of_falling_back_to_search() {
        // Mirrors normalize_input's own rejections (see
        // rejects_unsupported_schemes above) — a refused URL is refused
        // outright here too, never silently reinterpreted as a search.
        assert_eq!(classify_input("javascript://alert(1)"), None);
        assert_eq!(classify_input("ftp://example.com"), None);
    }

    #[test]
    fn classify_trims_surrounding_whitespace() {
        assert_eq!(
            classify_input("  example.com  "),
            Some(Intent::Url("https://example.com/".to_owned()))
        );
        assert_eq!(
            classify_input("  rust  "),
            Some(Intent::Search("rust".to_owned()))
        );
    }

    // --- build_search_url ---

    #[test]
    fn build_search_url_substitutes_and_percent_encodes_the_query() {
        assert_eq!(
            build_search_url("https://duckduckgo.com/?q={}", "rust ownership"),
            Some("https://duckduckgo.com/?q=rust+ownership".to_owned())
        );
    }

    #[test]
    fn build_search_url_encodes_reserved_and_non_ascii_characters() {
        let url = build_search_url("https://duckduckgo.com/?q={}", "a&b=c/d").unwrap();
        assert_eq!(url, "https://duckduckgo.com/?q=a%26b%3Dc%2Fd");
    }

    #[test]
    fn build_search_url_trims_the_query() {
        assert_eq!(
            build_search_url("https://duckduckgo.com/?q={}", "  rust  "),
            Some("https://duckduckgo.com/?q=rust".to_owned())
        );
    }

    #[test]
    fn build_search_url_rejects_a_template_without_the_placeholder() {
        assert_eq!(
            build_search_url("https://duckduckgo.com/?q=fixed", "rust"),
            None
        );
    }

    #[test]
    fn build_search_url_rejects_a_non_http_template() {
        assert_eq!(build_search_url("javascript:alert({})", "x"), None);
    }
}
