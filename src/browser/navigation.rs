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
}
