/// How address-bar input resolves into a load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// An absolute URL to navigate to.
    Url(String),
    /// Text to run through the search template.
    Search(String),
}

/// Template used when no `browser_search_template` setting is configured.
pub const DEFAULT_SEARCH_TEMPLATE: &str = "https://duckduckgo.com/?q={query}";

/// Schemes recognized without a `//` after the colon. Everything else needs
/// `://` so inputs like `localhost:3000` stay hosts, not schemes.
const SCHEME_PREFIXES: &[&str] = &["about:", "blob:", "data:", "file:", "view-source:"];

/// Turn address-bar input into either a URL to load or a search query.
/// Returns `None` for empty input.
pub fn resolve_address(raw: &str) -> Option<Resolved> {
    let input = raw.trim();
    if input.is_empty() {
        return None;
    }
    if input.chars().any(|c| c.is_whitespace()) {
        return Some(Resolved::Search(input.to_owned()));
    }
    if has_explicit_scheme(input) {
        return Some(Resolved::Url(input.to_owned()));
    }
    let authority = input.split(['/', '?', '#']).next().unwrap_or("");
    if looks_like_host(authority) {
        return Some(Resolved::Url(format!("https://{input}")));
    }
    Some(Resolved::Search(input.to_owned()))
}

/// Substitute the encoded query into a search template. A template without a
/// `{query}` placeholder is treated as a prefix the query is appended to.
pub fn search_url(template: &str, query: &str) -> String {
    let encoded = encode_query(query);
    if template.contains("{query}") {
        template.replace("{query}", &encoded)
    } else {
        format!("{template}{encoded}")
    }
}

fn has_explicit_scheme(input: &str) -> bool {
    input.contains("://")
        || SCHEME_PREFIXES.iter().any(|prefix| {
            input
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        })
}

/// Does this authority (host[:port], no path) look like something to navigate
/// to rather than search for?
fn looks_like_host(authority: &str) -> bool {
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        Some(_) => return false,
        None => authority,
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") || is_ipv4(host) {
        return true;
    }
    if !host.is_ascii() || !host.contains('.') {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.iter().any(|label| {
        label.is_empty()
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) {
        return false;
    }
    let tld = labels[labels.len() - 1];
    tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 3
                && part.bytes().all(|b| b.is_ascii_digit())
                && part.parse::<u8>().is_ok()
        })
}

/// Percent-encode everything outside the unreserved set.
fn encode_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Option<Resolved> {
        Some(Resolved::Url(input.to_owned()))
    }

    fn search(input: &str) -> Option<Resolved> {
        Some(Resolved::Search(input.to_owned()))
    }

    #[test]
    fn empty_input_does_not_resolve() {
        assert_eq!(resolve_address(""), None);
        assert_eq!(resolve_address("   \t "), None);
    }

    #[test]
    fn absolute_urls_pass_through() {
        assert_eq!(
            resolve_address("https://example.com/a?b=c#d"),
            url("https://example.com/a?b=c#d")
        );
        assert_eq!(
            resolve_address("HTTP://EXAMPLE.COM"),
            url("HTTP://EXAMPLE.COM")
        );
        assert_eq!(resolve_address("about:blank"), url("about:blank"));
        assert_eq!(
            resolve_address("file:///tmp/x.html"),
            url("file:///tmp/x.html")
        );
        assert_eq!(
            resolve_address("data:text/html,<h1>hi</h1>"),
            url("data:text/html,<h1>hi</h1>")
        );
    }

    #[test]
    fn host_like_input_gets_https() {
        assert_eq!(resolve_address("example.com"), url("https://example.com"));
        assert_eq!(
            resolve_address("example.com/path?q=1"),
            url("https://example.com/path?q=1")
        );
        assert_eq!(
            resolve_address("sub.domain.co.uk:8080/x"),
            url("https://sub.domain.co.uk:8080/x")
        );
        assert_eq!(resolve_address("EXAMPLE.com."), url("https://EXAMPLE.com."));
    }

    #[test]
    fn localhost_and_ipv4_navigate() {
        assert_eq!(resolve_address("localhost"), url("https://localhost"));
        assert_eq!(
            resolve_address("localhost:3000/api"),
            url("https://localhost:3000/api")
        );
        assert_eq!(
            resolve_address("127.0.0.1:8080"),
            url("https://127.0.0.1:8080")
        );
        assert_eq!(resolve_address("192.168.1.1"), url("https://192.168.1.1"));
    }

    #[test]
    fn anything_else_is_a_search() {
        assert_eq!(resolve_address("rust lang"), search("rust lang"));
        assert_eq!(
            resolve_address("how do I use example.com"),
            search("how do I use example.com")
        );
        assert_eq!(resolve_address("wikipedia"), search("wikipedia"));
        assert_eq!(resolve_address("foo..bar"), search("foo..bar"));
        assert_eq!(resolve_address("exa_mple.com"), search("exa_mple.com"));
        assert_eq!(resolve_address("1.2.3"), search("1.2.3"));
        assert_eq!(resolve_address("tëst.com"), search("tëst.com"));
        assert_eq!(resolve_address("example.com:"), search("example.com:"));
    }

    #[test]
    fn search_urls_encode_the_query() {
        assert_eq!(
            search_url(DEFAULT_SEARCH_TEMPLATE, "c++ tutorial"),
            "https://duckduckgo.com/?q=c%2B%20tutorial"
        );
        assert_eq!(
            search_url("https://lite.duckduckgo.com/lite/?q=", "a b"),
            "https://lite.duckduckgo.com/lite/?q=a%20b"
        );
        assert_eq!(
            search_url("https://search.example/?x=1&q={query}", "héllo&bye"),
            "https://search.example/?x=1&q=h%C3%A9llo%26bye"
        );
    }
}
