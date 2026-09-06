//! EasyList/EasyPrivacy-style filter list parsing and matching (Issue #23,
//! building on the domain-anchor-only parser from Issue #21/#51).
//!
//! This is still not a full adblock engine — no cosmetic/element-hiding
//! filtering, no content-modifying options, no rule-priority system — but it
//! now understands the subset of the Adblock Plus filter syntax that
//! EasyList/EasyPrivacy actually use for **network-level** blocking, which is
//! all VeloX can currently act on (main-frame navigation, D17; Windows-only
//! subresource requests, D59). See docs/decisions.md D64 for the full
//! rationale (why a hand-written parser instead of the `adblock` crate, the
//! license reasoning behind not bundling real list data, and the exact
//! syntax coverage decisions below).
//!
//! ## Supported syntax
//!
//! - `||domain^` — domain-anchor block: matches `domain` or any subdomain of
//!   it, regardless of path.
//! - Generic patterns (anything else that isn't a `||...` rule): a sequence
//!   of literal text, `*` (wildcard, matches any run of characters) and `^`
//!   (separator placeholder — matches one "not a letter/digit/`.`/`-`/`_`/`%`"
//!   character, or the end of the URL), optionally anchored with a leading
//!   `|` (must match at the very start of the URL) and/or a trailing `|`
//!   (must match through to the very end). Matched against the full request
//!   URL, case-insensitively. This is the same core algorithm Adblock Plus
//!   itself defines for non-regex filters.
//! - `@@` prefix — exception: never block a URL a `@@` rule (of either form
//!   above) matches, even when a block rule also matches.
//! - `$` options on either a block or an exception rule:
//!   - Resource-type keywords (`script`, `image`, `stylesheet`, `object`,
//!     `xmlhttprequest`/`xhr`, `subdocument`/`frame`, `font`, `media`,
//!     `websocket`, `ping`, `popup`, `document`, `other`) restrict the rule
//!     to matching requests of one of those types — see [`RuleResourceType`]
//!     and [`MatchContext`]. Comma-separates multiple types
//!     (`$script,image`).
//!   - `domain=a.com|~b.com` scopes the rule to (or away from) specific page
//!     hosts — see [`MatchContext::page_host`].
//!   - `third-party`/`first-party` (and their `3p`/`1p`/`~`-negated forms),
//!     `important`, `match-case`, `all`, `empty`, `mp4` are recognized and
//!     accepted but have **no effect on matching** — see "What's
//!     intentionally not implemented" below.
//!   - A **negated resource-type** keyword (`$~script`) or any option
//!     keyword outside the list above (`$csp=...`, `$redirect=...`,
//!     `$rewrite=...`, `$badfilter`, `$genericblock`, `$elemhide`, ...) is
//!     not faithfully representable by this matcher; rather than guess, the
//!     **whole rule** is dropped (treated as [`ParsedLine::Ignored`]). This
//!     mirrors the "malformed rule -> skip it" policy below: an option we
//!     don't understand is exactly as unsafe to half-apply as a rule we
//!     can't parse at all.
//! - `!` comment lines, `[Adblock Plus 2.0]`-style header/config lines
//!   (`[...]`), and element-hiding / scriptlet lines (containing `##`,
//!   `#@#`, `#$#`, `#%#` or `#?#`) are ignored — VeloX does no DOM-level
//!   filtering.
//! - A filter written as `/regex/` (the *entire* pattern, options aside,
//!   starts and ends with `/`) is Adblock Plus's syntax for a full regular
//!   expression. VeloX implements no regex engine (deliberately — see D64),
//!   so these are recognized and safely skipped rather than mis-parsed as a
//!   literal pattern.
//! - Blank lines, lines over [`MAX_RULE_LINE_LEN`] bytes, and anything else
//!   that doesn't parse as one of the above are ignored rather than
//!   rejected, so a real EasyList/EasyPrivacy file can be dropped in as a
//!   custom list (`Config::extra_blocklist_path`) without VeloX choking on
//!   it — it will simply only obey the subset described here. A parsed list
//!   is also capped at [`MAX_RULES`] total rules as a defense against an
//!   unreasonably huge file consuming unbounded memory.
//!
//! ## What's intentionally not implemented
//!
//! - **Accurate third-party classification.** Telling first-party from
//!   third-party requests correctly needs a public-suffix computation
//!   (`example.co.uk` vs `example.com`); adding that is exactly the kind of
//!   dependency D6 asks to avoid for a single narrow feature, and a naive
//!   "last two labels" heuristic would misclassify enough real domains to be
//!   worse than not implementing it. `third-party`/`first-party` options are
//!   parsed (so the rule isn't dropped) but never gate a match.
//! - **`$important`** rule-priority (a `$important` block should override
//!   even a later-matching exception) and **`$badfilter`** (rule
//!   cancellation) — both need a two-pass or priority-aware matching model
//!   this module doesn't have. Recognized, not enforced/applied
//!   (`$badfilter` specifically drops the whole rule rather than risk
//!   applying it as an ordinary block, since its entire purpose is
//!   cancellation, not blocking).
//! - **Regex filters**, **cosmetic/element-hiding/scriptlet filters**, and
//!   any option that modifies response content (`$csp=`, `$redirect=`,
//!   `$rewrite=`, `$replace=`, ...) — out of scope for a request-level
//!   allow/deny matcher with no DOM or response-body access.

use url::Url;

/// VeloX's built-in minimal block list. See `default_blocklist.txt` for the
/// rules and where they came from.
const DEFAULT_LIST: &str = include_str!("default_blocklist.txt");

/// A line longer than this is skipped without being parsed at all. Real
/// EasyList/EasyPrivacy rules are at most a few hundred bytes; this exists to
/// bound the cost of parsing a pathological (accidental or malicious)
/// multi-kilobyte single line in a user-supplied list.
const MAX_RULE_LINE_LEN: usize = 8192;

/// Hard cap on the total number of rules (block + exception, domain +
/// pattern) a single [`FilterList`] will hold. EasyList + EasyPrivacy
/// combined run to roughly 150k rules today, so this leaves generous
/// headroom while still bounding memory use against an unreasonably huge or
/// adversarially crafted list file. Rules beyond the cap are simply not
/// added; parsing does not error out.
const MAX_RULES: usize = 300_000;

/// A resource-type keyword from a rule's `$...` options, restricted to the
/// vocabulary EasyList/EasyPrivacy actually use. Deliberately independent of
/// `browser::subresource::ResourceType` (that type mirrors WebView2's
/// `ResourceContext` values one-for-one; this one mirrors the filter-syntax
/// keyword set one-for-one, and the two vocabularies don't line up exactly —
/// e.g. `popup`/`document` are meaningful filter options with no equivalent
/// WebView2 subresource context). A caller that has both, such as a future
/// `browser::subresource` extension, is expected to translate between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleResourceType {
    Script,
    Image,
    Stylesheet,
    Object,
    XmlHttpRequest,
    SubDocument,
    Font,
    Media,
    WebSocket,
    Ping,
    Popup,
    Document,
    Other,
}

impl RuleResourceType {
    fn from_keyword(keyword: &str) -> Option<Self> {
        Some(match keyword {
            "script" => Self::Script,
            "image" => Self::Image,
            "stylesheet" | "css" => Self::Stylesheet,
            "object" | "object-subrequest" => Self::Object,
            "xmlhttprequest" | "xhr" => Self::XmlHttpRequest,
            "subdocument" | "frame" => Self::SubDocument,
            "font" => Self::Font,
            "media" => Self::Media,
            "websocket" => Self::WebSocket,
            "ping" | "beacon" => Self::Ping,
            "popup" => Self::Popup,
            "document" | "doc" => Self::Document,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

/// The context a request is being matched in — everything a caller may know
/// beyond the URL itself. Every field is optional; `None` means "unknown",
/// and an unknown value never causes a rule to be excluded (see
/// [`options_apply`]). This makes [`FilterList::is_blocked`] exactly
/// [`FilterList::is_blocked_with_context`] with an empty context: no context
/// means every option-based condition trivially passes, which is the same
/// "ignore options" behavior the original domain-anchor-only matcher always
/// had.
///
/// No call site in VeloX constructs a non-default `MatchContext` yet —
/// `src/ui/window.rs`'s main-frame navigation check has no resource type to
/// report (navigation isn't a "resource"), and `browser::subresource`
/// (Issue #22/D59, frozen for this issue — see its module docs) calls only
/// `FilterList::is_blocked(url)`. This type exists so resource-type/domain
/// scoping is parsed and testable now, ready for a follow-up to plumb
/// `browser::subresource::ResourceType`/page host into it.
#[derive(Debug, Clone, Copy, Default)]
pub struct MatchContext<'a> {
    pub resource_type: Option<RuleResourceType>,
    pub page_host: Option<&'a str>,
}

/// `$domain=a.com|~b.com` — a rule scoped to (or away from) specific page
/// hosts. Matching is domain-anchor style (exact host or subdomain of a
/// listed domain), same as [`DomainRule`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DomainScope {
    included: Vec<String>,
    excluded: Vec<String>,
}

/// The `$...` options attached to a rule that this matcher actually acts on.
/// Everything else recognized in the option string (`third-party`,
/// `important`, ...) is accepted for parsing purposes but has no field here
/// because it never changes a match decision — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RuleOptions {
    /// `Some(types)` restricts the rule to those resource types; `None` is
    /// unrestricted (matches every type, including an unknown one).
    resource_types: Option<Vec<RuleResourceType>>,
    domain_scope: Option<DomainScope>,
}

/// Whether `options` lets a rule apply under `ctx`. Missing context
/// information (a `None` field) never excludes the rule — see
/// [`MatchContext`]'s docs for why.
fn options_apply(options: &RuleOptions, ctx: &MatchContext) -> bool {
    if let Some(types) = &options.resource_types {
        if let Some(rt) = ctx.resource_type {
            if !types.contains(&rt) {
                return false;
            }
        }
    }
    if let Some(scope) = &options.domain_scope {
        if let Some(host) = ctx.page_host {
            let host = host.trim().to_ascii_lowercase();
            if !scope.included.is_empty()
                && !scope.included.iter().any(|d| domain_suffix_match(d, &host))
            {
                return false;
            }
            if scope.excluded.iter().any(|d| domain_suffix_match(d, &host)) {
                return false;
            }
        }
    }
    true
}

/// Domain-anchor suffix match: `host` equals `domain` or is a subdomain of
/// it. Shared by [`DomainRule`] and `$domain=` scoping — they're the same
/// relationship applied to two different hosts (the request host vs. the
/// page host).
fn domain_suffix_match(domain: &str, host: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Only ASCII letters/digits/`.`/`-`/`_` — used to validate the domain text
/// pulled out of a `||domain^` rule or a `$domain=` entry, so a malformed or
/// adversarial rule (stray punctuation, injected option syntax that a
/// looser check would let through) is rejected rather than silently
/// producing a nonsense "domain".
fn is_valid_domain_str(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// One domain-anchor rule: matches `domain` itself and any subdomain of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DomainRule {
    domain: String,
    options: RuleOptions,
}

impl DomainRule {
    fn matches(&self, host: &str) -> bool {
        domain_suffix_match(&self.domain, host)
    }
}

/// One token of a tokenized generic pattern (see [`PatternRule`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(String),
    /// `*` — matches any run of characters (including none).
    Wildcard,
    /// `^` — matches one "separator" character (anything that is not a
    /// letter, digit, `.`, `-`, `_` or `%`), or the end of the URL.
    Separator,
}

fn is_separator_char(c: char) -> bool {
    !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '%'))
}

/// A generic (non-`||domain^`) network rule: literal text, `*` and `^`
/// tokens, optionally anchored at the start and/or end of the URL. Matching
/// is a single left-to-right pass with no backtracking — if a literal's
/// first occurrence doesn't satisfy a following `^`, later occurrences of
/// that literal are not tried. This can under-match (miss a block) in rare
/// cases with a repeated literal; it can never over-match, which is the
/// safer failure direction for a content blocker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PatternRule {
    tokens: Vec<Token>,
    anchor_start: bool,
    anchor_end: bool,
    options: RuleOptions,
}

impl PatternRule {
    fn matches(&self, url_lower: &str) -> bool {
        let mut pos = 0usize;
        for (i, tok) in self.tokens.iter().enumerate() {
            match tok {
                Token::Literal(lit) => {
                    if i == 0 && self.anchor_start {
                        if url_lower[pos..].starts_with(lit.as_str()) {
                            pos += lit.len();
                        } else {
                            return false;
                        }
                    } else {
                        match url_lower[pos..].find(lit.as_str()) {
                            Some(offset) => pos += offset + lit.len(),
                            None => return false,
                        }
                    }
                }
                Token::Wildcard => {}
                Token::Separator => match url_lower[pos..].chars().next() {
                    None => {}
                    Some(c) if is_separator_char(c) => pos += c.len_utf8(),
                    Some(_) => return false,
                },
            }
        }
        if self.anchor_end && pos != url_lower.len() {
            return false;
        }
        true
    }
}

/// A parsed filter list: domain-anchor and generic-pattern block rules, plus
/// their exceptions.
#[derive(Debug, Clone, Default)]
pub struct FilterList {
    domain_blocks: Vec<DomainRule>,
    domain_exceptions: Vec<DomainRule>,
    pattern_blocks: Vec<PatternRule>,
    pattern_exceptions: Vec<PatternRule>,
}

/// One filter-list line, after classification.
enum ParsedLine {
    DomainBlock(DomainRule),
    DomainException(DomainRule),
    PatternBlock(PatternRule),
    PatternException(PatternRule),
    Ignored,
}

/// Cosmetic/element-hiding/scriptlet markers (`example.com##.ad`,
/// `##.ad`, `example.com#@#.ad`, extended-syntax `#$#`/`#%#`/`#?#`
/// variants). These target the DOM/response, not the request; VeloX has no
/// renderer-level hook for them, so lines containing one are skipped
/// entirely rather than misparsed as a network rule.
fn is_cosmetic_rule(line: &str) -> bool {
    const MARKERS: [&str; 5] = ["##", "#@#", "#$#", "#%#", "#?#"];
    MARKERS.iter().any(|m| line.contains(m))
}

/// A filter is a full regular expression, per Adblock Plus syntax, exactly
/// when its entire pattern (options aside) starts and ends with `/`. VeloX
/// implements no regex engine, so these are recognized here and skipped.
fn is_regex_pattern(pattern: &str) -> bool {
    pattern.len() >= 2 && pattern.starts_with('/') && pattern.ends_with('/')
}

/// Split a rule (with any leading `@@` already removed) into its pattern and
/// an optional raw options string. Options are introduced by the *last* `$`
/// in the line, but only when what follows actually looks like an option
/// list (`looks_like_options`) — a `$` that's just part of the URL pattern
/// itself (query strings can legally contain one) is left alone.
fn split_options(rule: &str) -> (&str, Option<&str>) {
    if let Some(idx) = rule.rfind('$') {
        let head = &rule[..idx];
        let tail = &rule[idx + 1..];
        if !tail.is_empty() && looks_like_options(tail) {
            return (head, Some(tail));
        }
    }
    (rule, None)
}

/// Whether `s` (the text after the last `$`) reads as an options list: a
/// comma-separated sequence where every token is a bare keyword or a
/// `keyword=value` pair (the value itself is not validated here — a CSP
/// directive or redirect target can contain almost anything, including
/// spaces and quotes; only the keyword before `=` needs to look like an
/// identifier). This intentionally does not try to validate option *values*
/// — `parse_options` decides per-keyword whether the rule can be
/// represented at all.
fn looks_like_options(s: &str) -> bool {
    s.split(',')
        .all(|token| token.is_empty() || token_looks_like_option(token))
}

fn token_looks_like_option(token: &str) -> bool {
    let token = token.strip_prefix('~').unwrap_or(token);
    let mut chars = token.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in chars.by_ref() {
        if c == '=' {
            // Everything after '=' is a free-form value; stop validating.
            return true;
        }
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return false;
        }
    }
    true
}

/// Parse a raw `$a,b,~c,domain=...` options string. Returns `None` if any
/// token is not one this matcher can faithfully represent (see the module
/// docs' "What's intentionally not implemented") — the caller drops the
/// whole rule in that case rather than half-apply it.
fn parse_options(raw: &str) -> Option<RuleOptions> {
    let mut options = RuleOptions::default();
    let mut resource_types: Vec<RuleResourceType> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some(domains) = token.strip_prefix("domain=") {
            if options.domain_scope.is_some() {
                continue; // duplicate domain= — keep the first, ignore the rest
            }
            options.domain_scope = Some(parse_domain_scope(domains)?);
            continue;
        }
        let (negated, keyword) = match token.strip_prefix('~') {
            Some(k) => (true, k),
            None => (false, token),
        };
        // Recognized but never gate a match — see the module docs.
        if matches!(
            keyword,
            "third-party"
                | "3p"
                | "first-party"
                | "1p"
                | "important"
                | "match-case"
                | "all"
                | "empty"
                | "mp4"
        ) {
            continue;
        }
        match RuleResourceType::from_keyword(keyword) {
            Some(rt) if !negated => resource_types.push(rt),
            // A negated resource type (`$~script`) means "every type except
            // this one", which needs an exclusion-set semantics we don't
            // implement; drop the whole rule rather than mis-apply it.
            _ => return None,
        }
    }
    if !resource_types.is_empty() {
        options.resource_types = Some(resource_types);
    }
    Some(options)
}

fn parse_domain_scope(raw: &str) -> Option<DomainScope> {
    let mut scope = DomainScope::default();
    for part in raw.split('|') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (excluded, domain) = match part.strip_prefix('~') {
            Some(d) => (true, d),
            None => (false, part),
        };
        if !is_valid_domain_str(domain) {
            return None;
        }
        let domain = domain.to_ascii_lowercase();
        if excluded {
            scope.excluded.push(domain);
        } else {
            scope.included.push(domain);
        }
    }
    if scope.included.is_empty() && scope.excluded.is_empty() {
        return None;
    }
    Some(scope)
}

/// Parse a `||domain[...]` body (the text after `||`, with `$options`
/// already stripped) down to the bare domain, dropping anything from the
/// first `^` or `/` onward (path segments). Returns `None` for anything that
/// doesn't reduce to a plausible domain string.
fn parse_domain_anchor_body(rest: &str) -> Option<String> {
    let domain_part = rest.split(['^', '/']).next().unwrap_or("");
    let domain = domain_part.trim().to_ascii_lowercase();
    is_valid_domain_str(&domain).then_some(domain)
}

/// Tokenize a generic (non-`||domain^`) pattern into `*`/`^`/literal tokens
/// plus start/end anchors. Returns `None` for a pattern that has nothing
/// left to match (empty, or — as a deliberate safety guard — reduces to
/// wildcards only, which would otherwise silently become a "block
/// everything" rule).
fn tokenize_pattern(pattern: &str) -> Option<(Vec<Token>, bool, bool)> {
    let mut body = pattern;
    let mut anchor_start = false;
    let mut anchor_end = false;
    if let Some(rest) = body.strip_prefix('|') {
        anchor_start = true;
        body = rest;
    }
    if let Some(rest) = body.strip_suffix('|') {
        anchor_end = true;
        body = rest;
    }
    if body.is_empty() {
        return None;
    }

    let mut tokens = Vec::new();
    let mut buf = String::new();
    for c in body.to_ascii_lowercase().chars() {
        match c {
            '*' => {
                if !buf.is_empty() {
                    tokens.push(Token::Literal(std::mem::take(&mut buf)));
                }
                if !matches!(tokens.last(), Some(Token::Wildcard)) {
                    tokens.push(Token::Wildcard);
                }
            }
            '^' => {
                if !buf.is_empty() {
                    tokens.push(Token::Literal(std::mem::take(&mut buf)));
                }
                tokens.push(Token::Separator);
            }
            other => buf.push(other),
        }
    }
    if !buf.is_empty() {
        tokens.push(Token::Literal(buf));
    }
    if tokens.is_empty() || tokens.iter().all(|t| matches!(t, Token::Wildcard)) {
        return None;
    }
    Some((tokens, anchor_start, anchor_end))
}

fn parse_line(line: &str) -> ParsedLine {
    if line.len() > MAX_RULE_LINE_LEN {
        return ParsedLine::Ignored;
    }
    let line = line.trim();
    if line.is_empty() || line.starts_with('!') {
        return ParsedLine::Ignored;
    }
    // `[Adblock Plus 2.0]`-style header/config directive lines.
    if line.starts_with('[') && line.ends_with(']') {
        return ParsedLine::Ignored;
    }
    if is_cosmetic_rule(line) {
        return ParsedLine::Ignored;
    }

    let (is_exception, rest) = match line.strip_prefix("@@") {
        Some(r) => (true, r),
        None => (false, line),
    };
    if rest.is_empty() {
        return ParsedLine::Ignored;
    }

    let (pattern_text, options_text) = split_options(rest);
    let options = match options_text {
        Some(raw) => match parse_options(raw) {
            Some(o) => o,
            None => return ParsedLine::Ignored,
        },
        None => RuleOptions::default(),
    };
    if pattern_text.is_empty() || is_regex_pattern(pattern_text) {
        return ParsedLine::Ignored;
    }

    if let Some(body) = pattern_text.strip_prefix("||") {
        return match parse_domain_anchor_body(body) {
            Some(domain) => {
                let rule = DomainRule { domain, options };
                if is_exception {
                    ParsedLine::DomainException(rule)
                } else {
                    ParsedLine::DomainBlock(rule)
                }
            }
            // A malformed `||...` body is not re-interpreted as a generic
            // pattern — `||` has a specific host-anchoring meaning that a
            // best-effort fallback would risk getting subtly wrong.
            None => ParsedLine::Ignored,
        };
    }

    match tokenize_pattern(pattern_text) {
        Some((tokens, anchor_start, anchor_end)) => {
            let rule = PatternRule {
                tokens,
                anchor_start,
                anchor_end,
                options,
            };
            if is_exception {
                ParsedLine::PatternException(rule)
            } else {
                ParsedLine::PatternBlock(rule)
            }
        }
        None => ParsedLine::Ignored,
    }
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
    /// it already contains. Stops adding further rules once the list holds
    /// [`MAX_RULES`] total, without treating that as an error — the rest of
    /// `text` is simply not applied.
    pub fn merge(&mut self, text: &str) {
        for line in text.lines() {
            if self.total_rule_count() >= MAX_RULES {
                break;
            }
            match parse_line(line) {
                ParsedLine::DomainBlock(rule) => self.domain_blocks.push(rule),
                ParsedLine::DomainException(rule) => self.domain_exceptions.push(rule),
                ParsedLine::PatternBlock(rule) => self.pattern_blocks.push(rule),
                ParsedLine::PatternException(rule) => self.pattern_exceptions.push(rule),
                ParsedLine::Ignored => {}
            }
        }
    }

    fn total_rule_count(&self) -> usize {
        self.domain_blocks.len()
            + self.domain_exceptions.len()
            + self.pattern_blocks.len()
            + self.pattern_exceptions.len()
    }

    /// Number of active block rules — domain-anchor and generic-pattern
    /// combined, exceptions not counted (diagnostics/tests).
    pub fn block_rule_count(&self) -> usize {
        self.domain_blocks.len() + self.pattern_blocks.len()
    }

    /// Whether `url` should be blocked, ignoring every rule's `$...`
    /// options (equivalent to [`Self::is_blocked_with_context`] with an
    /// empty [`MatchContext`]). Malformed URLs or ones without a host (e.g.
    /// `about:blank`, `data:...`) are never blocked.
    pub fn is_blocked(&self, url: &str) -> bool {
        self.is_blocked_with_context(url, &MatchContext::default())
    }

    /// Whether `url` should be blocked under `ctx`: some block rule matches
    /// and applies under `ctx` (see [`MatchContext`]), and no exception rule
    /// also matches and applies under `ctx`.
    pub fn is_blocked_with_context(&self, url: &str, ctx: &MatchContext) -> bool {
        let Some(host) = host_of(url) else {
            return false;
        };
        let url_lower = url.to_ascii_lowercase();

        let exempted = self
            .domain_exceptions
            .iter()
            .any(|r| r.matches(&host) && options_apply(&r.options, ctx))
            || self
                .pattern_exceptions
                .iter()
                .any(|r| r.matches(&url_lower) && options_apply(&r.options, ctx));
        if exempted {
            return false;
        }

        self.domain_blocks
            .iter()
            .any(|r| r.matches(&host) && options_apply(&r.options, ctx))
            || self
                .pattern_blocks
                .iter()
                .any(|r| r.matches(&url_lower) && options_apply(&r.options, ctx))
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

    // -- Domain-anchor rules (pre-existing behavior, unchanged) --------

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
        let list = FilterList::parse(
            "##.ad-banner\nexample.com##.ad\nexample.com#@#.ad\nexample.com#$#abort\nexample.com#%#snippet\nexample.com#?#.ad:has(x)",
        );
        assert_eq!(list.block_rule_count(), 0);
    }

    #[test]
    fn ignores_header_lines() {
        let list = FilterList::parse("[Adblock Plus 2.0]\n||tracker.example^");
        assert_eq!(list.block_rule_count(), 1);
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

    // -- Generic pattern rules (new in Issue #23) -----------------------

    #[test]
    fn plain_substring_pattern_blocks_a_matching_url() {
        let list = FilterList::parse("-ads-");
        assert!(list.is_blocked("https://cdn.example/img/-ads-/banner.png"));
        assert!(!list.is_blocked("https://cdn.example/img/clean/banner.png"));
    }

    #[test]
    fn wildcard_pattern_matches_anything_after_the_literal() {
        let list = FilterList::parse("/ads/*");
        assert!(list.is_blocked("https://example.com/ads/banner.png"));
        assert!(list.is_blocked("https://example.com/ads/"));
        assert!(!list.is_blocked("https://example.com/other/"));
    }

    #[test]
    fn leading_pipe_anchors_the_pattern_to_the_start_of_the_url() {
        let list = FilterList::parse("|http://example.com/ad");
        assert!(list.is_blocked("http://example.com/ad/banner.js"));
        // Same text, but not at the very start of the URL: must not match.
        assert!(!list.is_blocked("http://other.example/http://example.com/ad"));
    }

    #[test]
    fn trailing_pipe_anchors_the_pattern_to_the_end_of_the_url() {
        let list = FilterList::parse("ads.js|");
        assert!(list.is_blocked("https://cdn.example/lib/ads.js"));
        assert!(!list.is_blocked("https://cdn.example/lib/ads.js.map"));
    }

    #[test]
    fn caret_requires_a_separator_character_or_end_of_url() {
        let list = FilterList::parse("ads^");
        // "/" after "ads" is a separator character -> matches.
        assert!(list.is_blocked("https://example.com/ads/banner"));
        // "e" after "ads" (as in "adsense") is not a separator -> no match.
        assert!(!list.is_blocked("https://example.com/adsense.js"));
    }

    #[test]
    fn caret_matches_end_of_url_as_a_separator() {
        let list = FilterList::parse("tracker.example/pixel^");
        assert!(list.is_blocked("https://tracker.example/pixel"));
    }

    #[test]
    fn pattern_exception_overrides_a_matching_pattern_block() {
        let list = FilterList::parse("-ads-\n@@-ads-");
        assert!(!list.is_blocked("https://cdn.example/-ads-/x"));
    }

    #[test]
    fn a_lone_wildcard_pattern_is_rejected_rather_than_blocking_everything() {
        let list = FilterList::parse("*");
        assert_eq!(list.block_rule_count(), 0);
        assert!(!list.is_blocked("https://example.com/"));
    }

    // -- Regex filters: recognized and safely skipped --------------------

    #[test]
    fn regex_delimited_filters_are_not_supported_and_are_skipped() {
        let list = FilterList::parse(r"/^https?:\/\/ads\./");
        assert_eq!(list.block_rule_count(), 0);
    }

    // -- `$` options: resource type -------------------------------------

    #[test]
    fn resource_type_option_is_parsed_but_ignored_by_plain_is_blocked() {
        let list = FilterList::parse("||ads.example^$script,image");
        // No context supplied -> option can't exclude the match; same
        // behavior as before options existed at all.
        assert!(list.is_blocked("https://ads.example/x"));
    }

    #[test]
    fn resource_type_option_gates_is_blocked_with_context() {
        let list = FilterList::parse("||ads.example^$script,image");
        let script_ctx = MatchContext {
            resource_type: Some(RuleResourceType::Script),
            page_host: None,
        };
        let stylesheet_ctx = MatchContext {
            resource_type: Some(RuleResourceType::Stylesheet),
            page_host: None,
        };
        assert!(list.is_blocked_with_context("https://ads.example/x", &script_ctx));
        assert!(!list.is_blocked_with_context("https://ads.example/x", &stylesheet_ctx));
        // Unknown resource type never excludes the match.
        assert!(list.is_blocked_with_context("https://ads.example/x", &MatchContext::default()));
    }

    #[test]
    fn negated_resource_type_option_drops_the_whole_rule() {
        let list = FilterList::parse("||ads.example^$~script");
        assert_eq!(list.block_rule_count(), 0);
        assert!(!list.is_blocked("https://ads.example/x"));
    }

    // -- `$domain=` scoping -----------------------------------------------

    #[test]
    fn domain_scope_restricts_the_rule_to_included_page_hosts() {
        let list = FilterList::parse("||ads.example^$domain=news.example");
        let news_ctx = MatchContext {
            resource_type: None,
            page_host: Some("news.example"),
        };
        let other_ctx = MatchContext {
            resource_type: None,
            page_host: Some("other.example"),
        };
        assert!(list.is_blocked_with_context("https://ads.example/x", &news_ctx));
        assert!(!list.is_blocked_with_context("https://ads.example/x", &other_ctx));
        // Unknown page host never excludes the match.
        assert!(list.is_blocked("https://ads.example/x"));
    }

    #[test]
    fn domain_scope_excludes_negated_page_hosts() {
        let list = FilterList::parse("||ads.example^$domain=~good.example");
        let good_ctx = MatchContext {
            resource_type: None,
            page_host: Some("good.example"),
        };
        let other_ctx = MatchContext {
            resource_type: None,
            page_host: Some("other.example"),
        };
        assert!(!list.is_blocked_with_context("https://ads.example/x", &good_ctx));
        assert!(list.is_blocked_with_context("https://ads.example/x", &other_ctx));
    }

    #[test]
    fn malformed_domain_scope_value_drops_the_whole_rule() {
        let list = FilterList::parse("||ads.example^$domain=has space");
        assert_eq!(list.block_rule_count(), 0);
    }

    // -- Unsupported options: whole rule dropped, no panic ---------------

    #[test]
    fn unrecognized_option_keyword_drops_the_whole_rule() {
        let list = FilterList::parse("||ads.example^$badfilter");
        assert_eq!(list.block_rule_count(), 0);
        assert!(!list.is_blocked("https://ads.example/x"));
    }

    #[test]
    fn content_modifying_options_drop_the_whole_rule() {
        let list = FilterList::parse(
            "||ads.example^$csp=script-src 'none'\n||ads.example^$redirect=noopjs\n||ads.example^$genericblock",
        );
        assert_eq!(list.block_rule_count(), 0);
    }

    #[test]
    fn benign_options_are_accepted_and_do_not_change_matching() {
        let list =
            FilterList::parse("||ads.example^$third-party,important,match-case,all,empty,mp4");
        assert_eq!(list.block_rule_count(), 1);
        assert!(list.is_blocked("https://ads.example/x"));
    }

    // -- Robustness: malformed / adversarial input never panics ----------

    #[test]
    fn extremely_long_line_is_skipped_without_parsing() {
        let huge_line = "a".repeat(MAX_RULE_LINE_LEN + 1);
        let list = FilterList::parse(&huge_line);
        assert_eq!(list.block_rule_count(), 0);
    }

    #[test]
    fn a_line_at_exactly_the_length_cap_still_parses() {
        // One under the line-length check's boundary, valid pattern syntax.
        let line = "a".repeat(MAX_RULE_LINE_LEN);
        let list = FilterList::parse(&line);
        assert_eq!(list.block_rule_count(), 1);
    }

    #[test]
    fn does_not_panic_on_a_large_garbage_file() {
        // A mix of malformed/edge-case lines, repeated many times. The point
        // of this test is that none of it panics the parser, however many
        // rules some of these edge cases happen to produce — so it checks
        // only coarse sanity bounds plus one known-good rule surviving amid
        // the noise, rather than an exact, fragile rule count.
        let mut text = String::new();
        for i in 0..5_000 {
            match i % 9 {
                0 => text.push_str("! just a comment\n"),
                1 => text.push_str("##.cosmetic-only\n"),
                2 => text.push_str(&format!("||weird$$$domain{i}^$csp=whatever\n")),
                3 => text.push_str(&format!("$domain=only-options-no-pattern{i}\n")),
                4 => text.push_str("****^^^^****\n"),
                5 => text.push_str(&format!("/regex-looking-{i}/\n")),
                6 => text.push_str(&format!("||valid{i}.example^\n")),
                7 => text.push_str(&format!("plain-pattern-{i}^*\n")),
                _ => text.push_str("@@\n"),
            }
        }
        let list = FilterList::parse(&text);
        assert!(list.block_rule_count() > 0);
        assert!(list.block_rule_count() <= 5_000);
        assert!(list.is_blocked("https://valid6.example/"));
    }

    #[test]
    fn rule_count_is_capped_against_an_unreasonably_huge_list() {
        let mut text = String::new();
        for i in 0..(MAX_RULES + 500) {
            text.push_str(&format!("||domain{i}.example^\n"));
        }
        let list = FilterList::parse(&text);
        assert_eq!(list.block_rule_count(), MAX_RULES);
    }

    #[test]
    fn a_bare_dollar_sign_in_a_query_string_is_not_mistaken_for_options() {
        // "$" that doesn't introduce a recognizable option list is left as
        // part of the pattern, not silently swallowed.
        let list = FilterList::parse("example.com/pay?amount=$5");
        assert_eq!(list.block_rule_count(), 1);
        assert!(list.is_blocked("https://example.com/pay?amount=$5"));
    }
}
