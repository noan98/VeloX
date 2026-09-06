//! Resource-type-aware subresource request matching (Issue #22).
//!
//! This module is pure, engine-agnostic logic: given a request URL, the kind
//! of resource it is for, and the host of the page that made the request, it
//! decides whether [`FilterList`] should block it. It knows nothing about
//! WebView2, WebKitGTK or WKWebView — the platform glue that actually
//! intercepts requests and calls into this module lives in `src/ui/` (see
//! `ui::webview2_blocking` on Windows) and does not exist at all on
//! macOS/Linux yet (docs/decisions.md D59; the wry 0.56 gap for those two
//! platforms is unchanged from D17).
//!
//! Kept separate from [`crate::browser::blocklist`] rather than folded into
//! `FilterList` because the two axes are independent: `FilterList` only ever
//! answers "does this URL's host match a rule", while this module adds two
//! concerns specific to subresource requests that main-frame navigation
//! blocking (D17) never needed — resource-type gating (never touch the
//! top-level document) and a site-level allow list.

use super::blocklist::FilterList;
use std::collections::HashSet;

/// The kind of thing a subresource request is for, independent of any
/// platform's own enum (see `ui::webview2_blocking::resource_type_from_context`
/// for the WebView2 mapping on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceType {
    /// A (sub-)document load: the page itself, or an `<iframe>`'s document.
    /// Always exempt from subresource blocking here — see
    /// [`is_blocked_resource`]'s doc comment for why.
    Document,
    Stylesheet,
    Image,
    Font,
    Script,
    /// `XMLHttpRequest` and `fetch()` calls — WebView2 reports these as two
    /// distinct resource contexts, but nothing in VeloX's matching needs to
    /// tell them apart, so they collapse to one variant here.
    XhrOrFetch,
    Media,
    WebSocket,
    /// Everything else (manifests, pings, event sources, ...).
    Other,
}

/// A set of hostnames for which content blocking is switched off entirely —
/// the "サイト単位の例外" (per-site exception) the issue asks for. Matching is
/// exact-host (no subdomain expansion, unlike [`FilterList`]'s domain-anchor
/// rules): a user who allow-lists `example.com` is exempting the site they
/// are looking at, not asking to also exempt every subdomain of it.
#[derive(Debug, Clone, Default)]
pub struct SiteExceptions {
    hosts: HashSet<String>,
}

impl SiteExceptions {
    /// No sites excepted — every host is still subject to `FilterList`.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from a list of hostnames (case-insensitive, surrounding
    /// whitespace trimmed, blanks dropped) — the shape
    /// `Config::content_blocking_site_exceptions` is parsed into.
    pub fn from_hosts<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = Self::empty();
        for host in hosts {
            set.add(host.as_ref());
        }
        set
    }

    /// Except `host` from content blocking. A no-op for an empty string.
    pub fn add(&mut self, host: &str) {
        let host = host.trim().to_ascii_lowercase();
        if !host.is_empty() {
            self.hosts.insert(host);
        }
    }

    /// Re-enable content blocking for `host`.
    pub fn remove(&mut self, host: &str) {
        self.hosts.remove(&host.trim().to_ascii_lowercase());
    }

    /// Whether `host` is on the exception list.
    pub fn contains(&self, host: &str) -> bool {
        self.hosts.contains(&host.trim().to_ascii_lowercase())
    }

    /// Number of excepted sites (diagnostics/tests).
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

/// Decide whether a subresource request for `url` should be blocked.
///
/// - `resource_type == Document` is **never** blocked here, regardless of
///   `list`/`exceptions`: WebView2's `WebResourceRequested` fires for the
///   top-level navigation too (it has no separate "this is the main frame"
///   event), and that decision already belongs to the navigation handler
///   (D17) — re-deciding it here without a reliable way to tell a top-level
///   navigation from an `<iframe>`'s own document load (see docs/decisions.md
///   D59) risks either double-handling the same block or, far worse, silently
///   blocking the page the user asked to open. Leaving iframe document loads
///   unblocked is a deliberate, documented gap (D59's revisit condition),
///   not an oversight.
/// - `page_host` (the host of the top-level page making the request) is
///   checked against `exceptions` next: an excepted site is never blocked,
///   no matter what `list` says.
/// - Otherwise the decision is exactly `list.is_blocked(url)`, reusing the
///   same domain-anchor matching main-frame navigation blocking already
///   uses, so a single filter list has one consistent meaning everywhere.
pub fn is_blocked_resource(
    list: &FilterList,
    exceptions: &SiteExceptions,
    page_host: Option<&str>,
    resource_type: ResourceType,
    url: &str,
) -> bool {
    if resource_type == ResourceType::Document {
        return false;
    }
    if let Some(host) = page_host {
        if exceptions.contains(host) {
            return false;
        }
    }
    list.is_blocked(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_blocking(rule: &str) -> FilterList {
        FilterList::parse(rule)
    }

    #[test]
    fn blocks_a_matching_script_request() {
        let list = list_blocking("||ads.example^");
        let exceptions = SiteExceptions::empty();
        assert!(is_blocked_resource(
            &list,
            &exceptions,
            Some("news.example"),
            ResourceType::Script,
            "https://ads.example/banner.js",
        ));
    }

    #[test]
    fn blocks_matching_image_xhr_and_fetch_requests() {
        let list = list_blocking("||tracker.example^");
        let exceptions = SiteExceptions::empty();
        for resource_type in [
            ResourceType::Image,
            ResourceType::XhrOrFetch,
            ResourceType::Stylesheet,
            ResourceType::Media,
            ResourceType::Font,
            ResourceType::WebSocket,
            ResourceType::Other,
        ] {
            assert!(
                is_blocked_resource(
                    &list,
                    &exceptions,
                    None,
                    resource_type,
                    "https://tracker.example/x",
                ),
                "{resource_type:?} should be blocked"
            );
        }
    }

    #[test]
    fn never_blocks_document_requests_even_when_the_host_matches() {
        let list = list_blocking("||ads.example^");
        let exceptions = SiteExceptions::empty();
        assert!(!is_blocked_resource(
            &list,
            &exceptions,
            None,
            ResourceType::Document,
            "https://ads.example/",
        ));
    }

    #[test]
    fn does_not_block_unrelated_hosts() {
        let list = list_blocking("||ads.example^");
        let exceptions = SiteExceptions::empty();
        assert!(!is_blocked_resource(
            &list,
            &exceptions,
            Some("news.example"),
            ResourceType::Script,
            "https://cdn.example/app.js",
        ));
    }

    #[test]
    fn site_exception_overrides_a_matching_block_rule() {
        let list = list_blocking("||ads.example^");
        let mut exceptions = SiteExceptions::empty();
        exceptions.add("news.example");
        assert!(!is_blocked_resource(
            &list,
            &exceptions,
            Some("news.example"),
            ResourceType::Script,
            "https://ads.example/banner.js",
        ));
    }

    #[test]
    fn site_exception_is_scoped_to_the_page_host_not_the_request_host() {
        let list = list_blocking("||ads.example^");
        let mut exceptions = SiteExceptions::empty();
        // Excepting the ad host itself must not be confused with excepting
        // the page that embeds it.
        exceptions.add("ads.example");
        assert!(is_blocked_resource(
            &list,
            &exceptions,
            Some("news.example"),
            ResourceType::Script,
            "https://ads.example/banner.js",
        ));
    }

    #[test]
    fn site_exception_matching_is_case_insensitive_and_trims_whitespace() {
        let mut exceptions = SiteExceptions::empty();
        exceptions.add("  News.Example  ");
        assert!(exceptions.contains("news.example"));
        assert!(exceptions.contains("NEWS.EXAMPLE"));
        assert_eq!(exceptions.len(), 1);
    }

    #[test]
    fn empty_host_is_never_added_as_an_exception() {
        let mut exceptions = SiteExceptions::empty();
        exceptions.add("   ");
        assert!(exceptions.is_empty());
    }

    #[test]
    fn from_hosts_builds_the_same_set_as_repeated_add() {
        let exceptions = SiteExceptions::from_hosts(["News.Example", " other.example "]);
        assert_eq!(exceptions.len(), 2);
        assert!(exceptions.contains("news.example"));
        assert!(exceptions.contains("other.example"));
    }

    #[test]
    fn remove_re_enables_blocking_for_a_previously_excepted_site() {
        let list = list_blocking("||ads.example^");
        let mut exceptions = SiteExceptions::empty();
        exceptions.add("news.example");
        exceptions.remove("news.example");
        assert!(is_blocked_resource(
            &list,
            &exceptions,
            Some("news.example"),
            ResourceType::Script,
            "https://ads.example/banner.js",
        ));
    }

    #[test]
    fn no_page_host_still_applies_the_filter_list() {
        // Not every request has a known page host (e.g. a top-level
        // navigation's own preload requests); absence of a host must not be
        // treated as an implicit exception.
        let list = list_blocking("||ads.example^");
        let exceptions = SiteExceptions::empty();
        assert!(is_blocked_resource(
            &list,
            &exceptions,
            None,
            ResourceType::Image,
            "https://ads.example/pixel.gif",
        ));
    }
}
