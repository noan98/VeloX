//! Classification of background-tab network activity (Issue #65).
//!
//! **Why this module has no wiring into `src/ui/` yet.** Blocking or
//! throttling a subresource request needs a per-request hook, and
//! `docs/decisions.md` D17/D59 already established that wry 0.56 exposes
//! one on exactly one platform: `ICoreWebView2::WebResourceRequested` on
//! Windows (`src/ui/webview2_blocking.rs`), reached through the same
//! `WebViewExtWindows::webview()` escape hatch this crate already uses for
//! ad/tracker blocking. WebKitGTK (Linux) and WKWebView (macOS) have no
//! equivalent through wry. Actually suppressing background-tab traffic on
//! Windows through that hook is future work this issue does not attempt —
//! see `docs/decisions.md` D80 for why: this project's only development and
//! CI environment is Linux, so a Windows-only behavior change with no way
//! to run it even once would violate Epic #57's rule 1 (no optimization
//! without a benchmark) in the worst way, an *unverifiable* one. What is
//! implemented here is the reusable, engine-independent piece that any
//! future Windows-side implementation (or a future wry release that closes
//! the Linux/macOS gap) would need on day one: which resource types are
//! safe to ever throttle, and a pure statistical test for "this looks like
//! polling" from a sequence of observed request timestamps — both fully
//! unit-tested without any engine dependency, unlike the platform glue that
//! would consume them.
//!
//! **Classification is per resource type, not per URL or per site.** VeloX
//! has no way to know what a request is *for* beyond what
//! `subresource::ResourceType` already captures (see that module's own doc
//! comment on why: it is fed by WebView2's `ResourceContext`, an enum, not
//! request/response bodies). That is enough to separate what must never be
//! touched (an open `WebSocket`, a `Document`/iframe load, a `Media`
//! stream — see [`NetworkActivityClass::Protected`]) from what merely
//! *could* be throttled if a future implementation ever wants to
//! ([`NetworkActivityClass::Throttleable`]) — see [`classify`]'s doc
//! comment for the full reasoning per variant. It cannot, on its own, tell
//! a background tab's essential `fetch()` (e.g. a chat app's own message
//! poll) from a wasteful one; [`is_polling`] narrows that further using
//! *timing*, the one signal available regardless of what the request is
//! for.

use std::time::Duration;

use super::subresource::ResourceType;

/// Whether a resource type may ever be a candidate for background-tab
/// network suppression. See [`classify`] for the reasoning behind each
/// request type's assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkActivityClass {
    /// Must never be throttled or blocked while backgrounded, regardless of
    /// how it behaves: doing so would break the exact legitimate patterns
    /// Issue #65 calls out by name (WebSocket, WebRTC, audio, notifications).
    Protected,
    /// May be a candidate for throttling *if* it also looks wasteful (see
    /// [`is_polling`]) — being `Throttleable` alone is not a verdict, only
    /// "not on the protected list".
    Throttleable,
}

/// Classify a resource type for background-tab network suppression
/// purposes (Issue #65).
///
/// - [`ResourceType::WebSocket`] is always [`NetworkActivityClass::Protected`]:
///   the issue names WebSocket explicitly as something that must not break,
///   and a socket is a single long-lived connection, not a repeatable
///   request — there is nothing to "throttle" about it without closing it,
///   which is exactly the breakage to avoid.
/// - [`ResourceType::Media`] is `Protected` for the same reason as
///   `WebSocket`: an `<audio>`/`<video>` element's own network stream is
///   exactly the "music playback" case the issue calls out, and
///   `browser::suspension` already special-cases audio for the same reason
///   (its `Candidate::protected` flag) — this module stays consistent with
///   that existing protection rather than inventing a second, different
///   audio rule.
/// - [`ResourceType::Document`] is `Protected`: identical reasoning to
///   `subresource::is_blocked_resource`, which never blocks a document load
///   either (see that function's doc comment) — a background tab's own
///   navigation (e.g. a redirect a site scheduled while hidden) is a
///   legitimate page-level event, not "background chatter".
/// - Everything else ([`ResourceType::XhrOrFetch`], `Image`, `Script`,
///   `Stylesheet`, `Font`, `Other`) is `Throttleable`: these are exactly the
///   categories Issue #65's own "対象" list names (`polling` and `periodic
///   fetch/XHR` are `XhrOrFetch`; `prefetch` and `background resource
///   loading` show up as `Other`/`Image`/`Script` depending on what is being
///   prefetched). None of them is inherently protected — a single
///   `Throttleable` request is not itself "unnecessary"; see [`is_polling`]
///   for the piece that actually tells wasteful repetition apart from a
///   normal one-off load.
pub fn classify(resource_type: ResourceType) -> NetworkActivityClass {
    match resource_type {
        ResourceType::WebSocket | ResourceType::Media | ResourceType::Document => {
            NetworkActivityClass::Protected
        }
        ResourceType::XhrOrFetch
        | ResourceType::Image
        | ResourceType::Script
        | ResourceType::Stylesheet
        | ResourceType::Font
        | ResourceType::Other => NetworkActivityClass::Throttleable,
    }
}

/// Minimum number of observed requests before [`is_polling`] will call
/// anything polling. Two requests define one interval, which is not enough
/// to distinguish "polling" from "two unrelated loads that happened to land
/// close together" — three requests (two intervals) is the smallest sample
/// that lets the second interval corroborate the first.
pub const MIN_SAMPLES_FOR_POLLING: usize = 3;

/// Whether a sequence of request timestamps (to the same URL, from the same
/// tab — the caller's job to group them that way; see the module doc
/// comment) looks like periodic polling rather than a handful of unrelated
/// loads.
///
/// `timestamps` must already be sorted ascending (ordinary request-arrival
/// order — nothing here re-sorts, so a caller feeding it out of order gets
/// a meaningless answer, not a panic). Fewer than
/// [`MIN_SAMPLES_FOR_POLLING`] timestamps is never polling — there is not
/// enough data to say so.
///
/// The test is a coefficient of variation on the inter-arrival intervals:
/// `stddev / mean <= tolerance`. Polling from a `setInterval`/`setTimeout`
/// loop produces near-constant intervals (this project's own
/// `scripts/bench/pages/network_activity.html` fixture does exactly that,
/// see `docs/performance-targets.md` §15) even once a browser's timer
/// throttling stretches the *period*, because throttling scales every
/// interval by roughly the same factor rather than making them irregular
/// (`docs/decisions.md` D58 measured this for CPU timers: a 500ms interval
/// became ~1000ms in the background, not "sometimes 400ms, sometimes
/// 2000ms"). A one-off burst of unrelated loads (e.g. a page's own
/// synchronous asset fetches) has no such regularity and a high
/// coefficient of variation.
///
/// `tolerance` is the caller's own choice (no built-in default): what
/// counts as "close enough to regular" depends on which resource type is
/// under test and how strict the caller wants to be before ever acting on
/// the answer. `0.15` (15% of the mean) is a reasonable starting point —
/// wide enough to absorb ordinary scheduling jitter without also nodding
/// through timestamps that are not periodic at all — see this module's own
/// tests for concrete pass/fail examples at that value.
pub fn is_polling(timestamps: &[Duration], tolerance: f64) -> bool {
    if timestamps.len() < MIN_SAMPLES_FOR_POLLING {
        return false;
    }
    let intervals: Vec<f64> = timestamps
        .windows(2)
        .map(|pair| (pair[1].as_secs_f64() - pair[0].as_secs_f64()).abs())
        .collect();
    let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
    if mean <= 0.0 {
        // Every timestamp identical (or intervals summed to zero some other
        // way): not meaningful as "periodic", and dividing by `mean` below
        // would be a division by zero.
        return false;
    }
    let variance =
        intervals.iter().map(|i| (i - mean).powi(2)).sum::<f64>() / intervals.len() as f64;
    let coefficient_of_variation = variance.sqrt() / mean;
    coefficient_of_variation <= tolerance
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- classify ------------------------------------------------------

    #[test]
    fn websocket_media_and_document_are_protected() {
        for resource_type in [
            ResourceType::WebSocket,
            ResourceType::Media,
            ResourceType::Document,
        ] {
            assert_eq!(
                classify(resource_type),
                NetworkActivityClass::Protected,
                "{resource_type:?} should be protected"
            );
        }
    }

    #[test]
    fn everything_else_is_throttleable() {
        for resource_type in [
            ResourceType::XhrOrFetch,
            ResourceType::Image,
            ResourceType::Script,
            ResourceType::Stylesheet,
            ResourceType::Font,
            ResourceType::Other,
        ] {
            assert_eq!(
                classify(resource_type),
                NetworkActivityClass::Throttleable,
                "{resource_type:?} should be throttleable"
            );
        }
    }

    // -- is_polling ------------------------------------------------------

    fn secs(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|s| Duration::from_secs(*s)).collect()
    }

    #[test]
    fn fewer_than_three_samples_is_never_polling() {
        assert!(!is_polling(&secs(&[]), 0.15));
        assert!(!is_polling(&secs(&[0]), 0.15));
        assert!(!is_polling(&secs(&[0, 2]), 0.15));
    }

    #[test]
    fn perfectly_regular_intervals_are_polling() {
        // Exactly like network_activity.html's 2-second poll timer.
        assert!(is_polling(&secs(&[0, 2, 4, 6, 8]), 0.15));
    }

    #[test]
    fn a_uniformly_throttled_interval_is_still_polling() {
        // D58: background throttling roughly doubles the period, it does
        // not make it irregular. The *ratio* to the mean is what matters,
        // so a slower-but-still-regular cadence must still read as polling.
        assert!(is_polling(&secs(&[0, 4, 8, 12, 16]), 0.15));
    }

    #[test]
    fn irregular_intervals_are_not_polling() {
        // 2s, then 20s, then 3s: no consistent period.
        assert!(!is_polling(&secs(&[0, 2, 22, 25]), 0.15));
    }

    #[test]
    fn small_jitter_within_tolerance_is_still_polling() {
        // A real setInterval loop is never exactly on the millisecond;
        // +/-1 out of a 10s period is comfortably inside a 15% tolerance.
        let jittered = [
            Duration::from_millis(0),
            Duration::from_millis(9_800),
            Duration::from_millis(20_100),
            Duration::from_millis(29_900),
        ];
        assert!(is_polling(&jittered, 0.15));
    }

    #[test]
    fn a_stricter_tolerance_can_reject_the_same_jitter() {
        let jittered = [
            Duration::from_millis(0),
            Duration::from_millis(9_800),
            Duration::from_millis(20_100),
            Duration::from_millis(29_900),
        ];
        // Same data as the test above, but a caller asking for near-exact
        // regularity (1%) should not get a pass.
        assert!(!is_polling(&jittered, 0.01));
    }

    #[test]
    fn identical_timestamps_do_not_panic_and_are_not_polling() {
        assert!(!is_polling(&secs(&[5, 5, 5, 5]), 0.15));
    }
}
