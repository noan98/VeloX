//! Minimal EasyList-style filter list parsing and domain matching.
//!
//! This is not a full adblock engine: it understands only the subset of the
//! EasyList/EasyPrivacy syntax needed for domain-level blocking, which is all
//! VeloX can currently act on (see docs/decisions.md D8 — wry 0.56 exposes a
//! main-frame navigation hook but no subresource request hook).
//!
//! Supported syntax:
//! - `||domain^` — block requests whose host is `domain` or a subdomain of it
//! - `@@||domain^` — exception: never block `domain` (or its subdomains),
//!   even when a block rule also matches
//! - lines starting with `!` — comments, ignored
//! - lines containing `##` or `#@#` (element hiding / cosmetic rules) —
//!   ignored, since VeloX does no DOM-level filtering
//! - blank lines and anything else that isn't a `||domain^` rule are ignored
//!   rather than rejected, so a real EasyList-format file can be dropped in
//!   as a custom list without VeloX choking on it (it will just only obey
//!   the subset above)

use url::Url;

/// VeloX's built-in minimal block list. See `default_blocklist.txt` for the
/// rules and where they came from.
const DEFAULT_LIST: &str = include_str!("default_blocklist.txt");

/// One domain-anchor rule: matches `domain` itself and any subdomain of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DomainRule {
    domain: String,
}

impl DomainRule {
    fn matches(&self, host: &str) -> bool {
        host == self.domain || host.ends_with(&format!(".{}", self.domain))
    }
}

/// A parsed filter list: domain-anchor block rules plus their exceptions.
#[derive(Debug, Clone, Default)]
pub struct FilterList {
    blocks: Vec<DomainRule>,
    exceptions: Vec<DomainRule>,
}

/// One filter-list line, after classification.
enum ParsedLine {
    Block(String),
    Exception(String),
    Ignored,
}

fn parse_line(line: &str) -> ParsedLine {
    let line = line.trim();
    if line.is_empty() || line.starts_with('!') {
        return ParsedLine::Ignored;
    }
    // Cosmetic / element-hiding rules (`example.com##.ad`, `##.ad`,
    // `example.com#@#.ad`) target the DOM, not requests; VeloX has no
    // renderer-level hook for those, so skip them rather than misparsing.
    if line.contains("##") || line.contains("#@#") {
        return ParsedLine::Ignored;
    }

    if let Some(rest) = line.strip_prefix("@@") {
        return match parse_domain_anchor(rest) {
            Some(domain) => ParsedLine::Exception(domain),
            None => ParsedLine::Ignored,
        };
    }

    match parse_domain_anchor(line) {
        Some(domain) => ParsedLine::Block(domain),
        None => ParsedLine::Ignored,
    }
}

/// Parse a `||domain^...` domain-anchor rule, returning the bare domain.
///
/// Anything from the first `^`, `$` or `/` onward (path segments, filter
/// options like `$third-party`) is dropped: VeloX only ever matches on the
/// request host, never on path or per-option scope.
fn parse_domain_anchor(rule: &str) -> Option<String> {
    let rest = rule.strip_prefix("||")?;
    let domain_part = rest.split(['^', '$', '/']).next().unwrap_or("");
    let domain = domain_part.trim().to_ascii_lowercase();
    if domain.is_empty() || domain.contains(char::is_whitespace) {
        return None;
    }
    Some(domain)
}

impl FilterList {
    /// An empty list: nothing gets blocked.
    pub fn empty() -> Self {
        Self::default()
    }

    /// VeloX's built-in minimal list (ad networks + trackers).
    pub fn built_in() -> Self {
        Self::parse(DEFAULT_LIST)
    }

    /// Parse `text` (one rule per line) into a new list.
    pub fn parse(text: &str) -> Self {
        let mut list = Self::empty();
        list.merge(text);
        list
    }

    /// Parse `text` and merge its rules into this list, on top of whatever
    /// it already contains.
    pub fn merge(&mut self, text: &str) {
        for line in text.lines() {
            match parse_line(line) {
                ParsedLine::Block(domain) => self.blocks.push(DomainRule { domain }),
                ParsedLine::Exception(domain) => self.exceptions.push(DomainRule { domain }),
                ParsedLine::Ignored => {}
            }
        }
    }

    /// Number of active block rules (diagnostics/tests).
    pub fn block_rule_count(&self) -> usize {
        self.blocks.len()
    }

    /// Whether `url` should be blocked: some block rule matches its host and
    /// no exception rule also matches it. Malformed URLs or ones without a
    /// host (e.g. `about:blank`, `data:...`) are never blocked.
    pub fn is_blocked(&self, url: &str) -> bool {
        let Some(host) = host_of(url) else {
            return false;
        };
        if self.exceptions.iter().any(|rule| rule.matches(&host)) {
            return false;
        }
        self.blocks.iter().any(|rule| rule.matches(&host))
    }
}

fn host_of(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_exact_domain_and_subdomains() {
        let list = FilterList::parse("||doubleclick.net^");
        assert!(list.is_blocked("https://doubleclick.net/x"));
        assert!(list.is_blocked("https://ad.doubleclick.net/x"));
        assert!(list.is_blocked("https://a.b.doubleclick.net/x"));
    }

    #[test]
    fn does_not_block_unrelated_or_lookalike_domains() {
        let list = FilterList::parse("||doubleclick.net^");
        assert!(!list.is_blocked("https://example.com/"));
        // "notdoubleclick.net" must not match via a naive substring check.
        assert!(!list.is_blocked("https://notdoubleclick.net/"));
    }

    #[test]
    fn exception_overrides_a_matching_block_rule() {
        let list = FilterList::parse("||ads.example.com^\n@@||ads.example.com^");
        assert!(!list.is_blocked("https://ads.example.com/banner.js"));
    }

    #[test]
    fn exception_on_parent_domain_also_covers_subdomains() {
        let list = FilterList::parse("||ads.example.com^\n@@||example.com^");
        assert!(!list.is_blocked("https://ads.example.com/banner.js"));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let list = FilterList::parse("! a comment\n\n   \n||tracker.example^");
        assert_eq!(list.block_rule_count(), 1);
        assert!(list.is_blocked("https://tracker.example/"));
    }

    #[test]
    fn ignores_element_hiding_rules() {
        let list = FilterList::parse("##.ad-banner\nexample.com##.ad\nexample.com#@#.ad");
        assert_eq!(list.block_rule_count(), 0);
    }

    #[test]
    fn ignores_rules_outside_the_supported_subset() {
        // Plain substring rules, regex rules, and other non `||domain^`
        // syntax are silently skipped rather than mis-parsed.
        let list = FilterList::parse("/banner/*\n|http://example.com/ad\nadserver");
        assert_eq!(list.block_rule_count(), 0);
    }

    #[test]
    fn strips_path_and_options_from_domain_anchor_rules() {
        let list = FilterList::parse("||ads.example.com/banner^$third-party");
        assert!(list.is_blocked("https://ads.example.com/anything"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let list = FilterList::parse("||Example.COM^");
        assert!(list.is_blocked("https://EXAMPLE.com/"));
    }

    #[test]
    fn urls_without_a_host_are_never_blocked() {
        let list = FilterList::parse("||example.com^");
        assert!(!list.is_blocked("about:blank"));
        assert!(!list.is_blocked("not a url"));
        assert!(!list.is_blocked("data:text/plain,hi"));
    }

    #[test]
    fn merge_adds_rules_on_top_of_an_existing_list() {
        let mut list = FilterList::parse("||a.example^");
        list.merge("||b.example^");
        assert_eq!(list.block_rule_count(), 2);
        assert!(list.is_blocked("https://a.example/"));
        assert!(list.is_blocked("https://b.example/"));
    }

    #[test]
    fn built_in_list_parses_and_blocks_known_ad_domains() {
        let list = FilterList::built_in();
        assert!(list.block_rule_count() >= 40);
        assert!(list.is_blocked("https://doubleclick.net/"));
        assert!(list.is_blocked("https://www.google-analytics.com/collect"));
        assert!(!list.is_blocked("https://example.com/"));
    }
}
