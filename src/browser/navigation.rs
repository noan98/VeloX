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
