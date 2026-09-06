//! Site permissions: a pure, in-memory collection of per-origin decisions
//! for camera/microphone/geolocation/notifications/clipboard requests
//! (Issue #24).
//!
//! This is deliberately shaped like [`crate::browser::bookmarks`] /
//! [`crate::browser::history`]: plain data and pure functions, no `wry` or
//! any other UI/engine dependency, so it is unit-testable without a window
//! and without a webview. File IO lives in
//! [`crate::browser::persistence`]; the actual `wry::PermissionKind` /
//! `wry::PermissionResponse` mapping and the `with_permission_handler`
//! wiring live in `src/ui/window.rs`, which is the only place allowed to
//! know `wry` exists (see the crate-level architecture doc comment).
//!
//! See docs/decisions.md D60 for why wry 0.56's permission hook is used the
//! way it is here (an origin-keyed allow/block override, safe-by-default
//! otherwise) rather than a full custom "Allow / Block" prompt UI.
//!
//! ## What is and is not modeled here
//!
//! - **"今後も許可" / "今後も拒否"** (always allow / always block) are the
//!   only two states [`PermissionDecision`] can hold, and the only ones
//!   this store ever persists to disk — see [`SitePermissionStore::set`].
//! - **"一度だけ許可"** (allow once) is deliberately *not* a variant of
//!   [`PermissionDecision`]: "once" means exactly "do not call
//!   [`SitePermissionStore::set`] at all", so a one-time grant never
//!   touches this store and is gone the moment the session ends. Callers
//!   that want session-only allow/block behavior track it themselves
//!   (e.g. alongside a tab's other session-only state) instead of this
//!   module growing a third, harder-to-reason-about persisted state.
//! - **Unknown permission kinds** ([`PermissionKind::Other`]) always
//!   resolve to [`Resolution::Block`], unconditionally, before any record
//!   lookup happens — see [`SitePermissionStore::resolve`]. A future `wry`
//!   version, or a platform backend, exposing a permission kind this
//!   version of VeloX has never heard of must never be able to fall through
//!   to an allow.

use serde::{Deserialize, Serialize};

/// A kind of permission a site can request, independent of how any
/// particular platform/engine spells it (that mapping lives in
/// `src/ui/window.rs`).
///
/// Deliberately a small, closed set matching the issue's list (camera,
/// microphone, geolocation, notifications, clipboard) plus a catch-all —
/// see the module doc comment for why [`Self::Other`] always loses.
/// `#[serde(other)]` on `Other` means a `site_permissions.json` written by
/// a future VeloX version with a permission kind this version does not
/// know about still deserializes cleanly (as `Other`, i.e. always denied)
/// instead of failing to parse the whole file — the same
/// forward-compatibility need `persistence`'s corrupt-file handling
/// addresses at the file level, just one level down at the enum level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionKind {
    Camera,
    Microphone,
    Geolocation,
    Notifications,
    ClipboardRead,
    /// Anything else — see the module doc comment.
    #[serde(other)]
    Other,
}

/// A stored, persistent decision for one `(origin, PermissionKind)` pair.
/// There is no "ask"/"undecided" variant here — the *absence* of a record
/// is what "ask" means; see [`SitePermissionStore::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    Allow,
    Block,
}

/// What [`SitePermissionStore::resolve`] answers for a given request: the
/// three states the issue's acceptance criteria describe (明示 needed /
/// 許可 / 拒否).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// A stored "always allow" record matched.
    Allow,
    /// A stored "always block" record matched, or the kind is unknown
    /// ([`PermissionKind::Other`]) — see the module doc comment.
    Block,
    /// No stored decision for this `(origin, kind)` pair. The caller
    /// decides what "ask" means for its platform (e.g. defer to the
    /// engine's own native prompt, or deny outright) — this module only
    /// ever reports the *absence* of a VeloX-level decision, it never
    /// itself guesses one.
    Ask,
}

/// One persisted `(origin, kind) -> decision` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRecord {
    /// `scheme://host[:port]`, as produced by [`origin_of`] — never a full
    /// URL (no path/query/fragment), so two pages on the same site share
    /// one record regardless of which page triggered the request.
    pub origin: String,
    pub kind: PermissionKind,
    pub decision: PermissionDecision,
    /// Unix timestamp (seconds) this record was last set.
    pub updated_at: u64,
}

/// An unordered collection of [`PermissionRecord`]s, at most one per
/// `(origin, kind)` pair (enforced by [`Self::set`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SitePermissionStore {
    /// `#[serde(default)]` so a `site_permissions.json` missing this key
    /// entirely still deserializes to an empty store rather than failing.
    #[serde(default)]
    records: Vec<PermissionRecord>,
}

impl SitePermissionStore {
    /// A new, empty store — every request resolves to [`Resolution::Ask`]
    /// (or [`Resolution::Block`] for an unknown kind) until [`Self::set`]
    /// is called.
    pub fn new() -> Self {
        Self::default()
    }

    /// All stored records, in no particular order — the settings UI
    /// ("現在サイトの権限状態表示") groups/sorts these itself.
    pub fn records(&self) -> &[PermissionRecord] {
        &self.records
    }

    /// Records for one `origin` only, e.g. for "this site's permissions"
    /// display.
    pub fn records_for<'a>(
        &'a self,
        origin: &'a str,
    ) -> impl Iterator<Item = &'a PermissionRecord> {
        self.records
            .iter()
            .filter(move |record| record.origin == origin)
    }

    /// Decide how a permission request for `(origin, kind)` should be
    /// handled. This is the single choke point every caller (the settings
    /// UI's "current state" display, and `src/ui/window.rs`'s
    /// `with_permission_handler` wiring) must go through — see
    /// docs/decisions.md D60.
    ///
    /// Safe-by-default: an unknown kind always blocks, before any record
    /// is even looked at (see the module doc comment); a known kind with
    /// no stored record asks rather than silently allowing.
    pub fn resolve(&self, origin: &str, kind: PermissionKind) -> Resolution {
        if kind == PermissionKind::Other {
            return Resolution::Block;
        }
        match self
            .records
            .iter()
            .find(|record| record.origin == origin && record.kind == kind)
        {
            Some(record) => match record.decision {
                PermissionDecision::Allow => Resolution::Allow,
                PermissionDecision::Block => Resolution::Block,
            },
            None => Resolution::Ask,
        }
    }

    /// Store an always-allow/always-block decision for `(origin, kind)`,
    /// replacing any previous decision for that exact pair. `now` is a
    /// Unix timestamp in seconds (caller-supplied, same convention as
    /// [`crate::browser::bookmarks::BookmarkStore::add`]'s `created_at`).
    pub fn set(
        &mut self,
        origin: impl Into<String>,
        kind: PermissionKind,
        decision: PermissionDecision,
        now: u64,
    ) {
        let origin = origin.into();
        match self
            .records
            .iter_mut()
            .find(|record| record.origin == origin && record.kind == kind)
        {
            Some(record) => {
                record.decision = decision;
                record.updated_at = now;
            }
            None => self.records.push(PermissionRecord {
                origin,
                kind,
                decision,
                updated_at: now,
            }),
        }
    }

    /// Remove the stored decision for `(origin, kind)`, reverting future
    /// requests to [`Resolution::Ask`]. Returns `true` when a record was
    /// removed.
    pub fn clear(&mut self, origin: &str, kind: PermissionKind) -> bool {
        let before = self.records.len();
        self.records
            .retain(|record| !(record.origin == origin && record.kind == kind));
        self.records.len() != before
    }

    /// Remove every stored decision for `origin` (all kinds). Returns
    /// `true` when at least one record was removed.
    pub fn clear_origin(&mut self, origin: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|record| record.origin != origin);
        self.records.len() != before
    }
}

/// Reduce a URL to the origin a permission decision is scoped to:
/// `scheme://host` or `scheme://host:port`, with no path/query/fragment —
/// so `https://example.com/a` and `https://example.com/b` share a
/// decision, but `https://example.com` and `https://sub.example.com` (or
/// `:8443`) do not.
///
/// Only `http`/`https` have a meaningful, request-worthy origin for this
/// module's purposes; every other scheme (`file`, `about`, `data`, or an
/// unparseable URL) yields `None`, and callers must treat that the same as
/// [`PermissionKind::Other`] — i.e. deny (see the module doc comment and
/// docs/decisions.md D60). This mirrors `browser::navigation`'s own
/// `http`/`https`-first posture without duplicating its scheme allow-list
/// (that stays the one place scheme *acceptance for navigation* is
/// decided; this function only ever narrows further, for permissions).
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{}://{host}:{port}", parsed.scheme())),
        None => Some(format!("{}://{host}", parsed.scheme())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- resolve / set / clear ---

    #[test]
    fn new_store_asks_for_everything() {
        let store = SitePermissionStore::new();
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Camera),
            Resolution::Ask
        );
    }

    #[test]
    fn unknown_kind_always_blocks_even_with_no_records_at_all() {
        let store = SitePermissionStore::new();
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Other),
            Resolution::Block
        );
    }

    #[test]
    fn unknown_kind_blocks_even_when_the_origin_has_other_allows() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            1,
        );
        // An allow for Camera must never leak into Other.
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Other),
            Resolution::Block
        );
    }

    #[test]
    fn set_allow_then_resolve_allow() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Microphone,
            PermissionDecision::Allow,
            1,
        );
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Microphone),
            Resolution::Allow
        );
    }

    #[test]
    fn set_block_then_resolve_block() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Geolocation,
            PermissionDecision::Block,
            1,
        );
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Geolocation),
            Resolution::Block
        );
    }

    #[test]
    fn setting_again_overwrites_the_previous_decision() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            1,
        );
        store.set(
            "https://example.com",
            PermissionKind::Camera,
            PermissionDecision::Block,
            2,
        );
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Camera),
            Resolution::Block
        );
        assert_eq!(store.records().len(), 1);
        assert_eq!(store.records()[0].updated_at, 2);
    }

    #[test]
    fn decisions_are_scoped_per_origin() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://a.example",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            1,
        );
        assert_eq!(
            store.resolve("https://b.example", PermissionKind::Camera),
            Resolution::Ask
        );
    }

    #[test]
    fn decisions_are_scoped_per_kind() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            1,
        );
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Microphone),
            Resolution::Ask
        );
    }

    #[test]
    fn clear_reverts_to_ask() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://example.com",
            PermissionKind::Notifications,
            PermissionDecision::Allow,
            1,
        );
        assert!(store.clear("https://example.com", PermissionKind::Notifications));
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Notifications),
            Resolution::Ask
        );
    }

    #[test]
    fn clearing_an_unknown_record_is_a_noop() {
        let mut store = SitePermissionStore::new();
        assert!(!store.clear("https://example.com", PermissionKind::Camera));
    }

    #[test]
    fn clear_origin_removes_only_that_origins_records() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://a.example",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            1,
        );
        store.set(
            "https://a.example",
            PermissionKind::Microphone,
            PermissionDecision::Allow,
            1,
        );
        store.set(
            "https://b.example",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            1,
        );
        assert!(store.clear_origin("https://a.example"));
        assert_eq!(store.records().len(), 1);
        assert_eq!(
            store.resolve("https://b.example", PermissionKind::Camera),
            Resolution::Allow
        );
    }

    #[test]
    fn clear_origin_of_an_unknown_origin_is_a_noop() {
        let mut store = SitePermissionStore::new();
        assert!(!store.clear_origin("https://example.com"));
    }

    #[test]
    fn records_for_filters_by_origin() {
        let mut store = SitePermissionStore::new();
        store.set(
            "https://a.example",
            PermissionKind::Camera,
            PermissionDecision::Allow,
            1,
        );
        store.set(
            "https://b.example",
            PermissionKind::Camera,
            PermissionDecision::Block,
            1,
        );
        let a_records: Vec<_> = store.records_for("https://a.example").collect();
        assert_eq!(a_records.len(), 1);
        assert_eq!(a_records[0].decision, PermissionDecision::Allow);
    }

    // --- origin_of ---

    #[test]
    fn origin_of_strips_path_query_and_fragment() {
        assert_eq!(
            origin_of("https://example.com/a/b?x=1#y"),
            Some("https://example.com".to_owned())
        );
    }

    #[test]
    fn origin_of_keeps_a_non_default_port() {
        assert_eq!(
            origin_of("https://example.com:8443/"),
            Some("https://example.com:8443".to_owned())
        );
    }

    #[test]
    fn origin_of_distinguishes_http_and_https() {
        assert_ne!(
            origin_of("http://example.com/").unwrap(),
            origin_of("https://example.com/").unwrap()
        );
    }

    #[test]
    fn origin_of_distinguishes_subdomains() {
        assert_ne!(
            origin_of("https://example.com/").unwrap(),
            origin_of("https://sub.example.com/").unwrap()
        );
    }

    #[test]
    fn origin_of_rejects_non_http_schemes() {
        assert_eq!(origin_of("file:///etc/passwd"), None);
        assert_eq!(origin_of("about:blank"), None);
        assert_eq!(origin_of("data:text/plain,hi"), None);
    }

    #[test]
    fn origin_of_rejects_unparseable_input() {
        assert_eq!(origin_of("not a url"), None);
        assert_eq!(origin_of(""), None);
    }

    // --- Forward compatibility: unknown persisted `PermissionKind` ---

    #[test]
    fn deserializing_an_unrecognized_kind_string_falls_back_to_other() {
        let json = r#"{"records":[
            {"origin":"https://example.com","kind":"SomeFutureKind","decision":"Allow","updated_at":1}
        ]}"#;
        let store: SitePermissionStore =
            serde_json::from_str(json).expect("unknown kind must not fail the whole file");
        assert_eq!(store.records()[0].kind, PermissionKind::Other);
        // And per the module's safety rule, that still blocks regardless of
        // the persisted "Allow".
        assert_eq!(
            store.resolve("https://example.com", PermissionKind::Other),
            Resolution::Block
        );
    }

    #[test]
    fn missing_records_field_deserializes_to_an_empty_store() {
        let store: SitePermissionStore = serde_json::from_str("{}").unwrap();
        assert!(store.records().is_empty());
    }
}
