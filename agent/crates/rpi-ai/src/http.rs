//! HTTP client construction: system-proxy resolution and pooled-connection
//! idle timeout.
//!
//! Port of upstream's `packages/ai/src/utils/node-http-proxy.ts` and
//! `packages/coding-agent/src/core/http-dispatcher.ts`. Upstream routes every
//! provider request through a shared dispatcher that:
//!
//! * resolves `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` (with
//!   wildcard and `host:port` matching), rejecting SOCKS/PAC URLs; and
//! * closes idle pooled connections after a configurable timeout.
//!
//! `reqwest` honours proxy env vars on its own, but the explicit resolution
//! here gives the same `NO_PROXY` semantics as pi and lets the CLI expose the
//! idle-timeout setting.

use std::collections::HashMap;

pub use reqwest::Url;

/// Native `DEFAULT_HTTP_IDLE_TIMEOUT_MS`.
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: i64 = 300_000;

/// Native `HTTP_IDLE_TIMEOUT_CHOICES` (label, timeoutMs). `0` = disabled.
pub const HTTP_IDLE_TIMEOUT_CHOICES: &[(&str, i64)] = &[
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];

/// Native `UNSUPPORTED_PROXY_PROTOCOL_MESSAGE`.
pub const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str =
    "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

/// The User-Agent rpi sends on provider requests (native `getPiUserAgent`).
pub fn user_agent() -> String {
    format!(
        "rpi/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Insert a default `user-agent` header unless one is already present
/// (case-insensitively). Returns true when a header was added.
pub fn ensure_user_agent(headers: &mut std::collections::BTreeMap<String, String>) -> bool {
    if headers.keys().any(|k| k.eq_ignore_ascii_case("user-agent")) {
        return false;
    }
    headers.insert("user-agent".to_string(), user_agent());
    true
}

fn default_proxy_port(protocol: &str) -> Option<u16> {
    match protocol {
        "ftp" => Some(21),
        "gopher" => Some(70),
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

/// Parse the `httpIdleTimeout` setting value. Mirrors native
/// `parseHttpIdleTimeoutMs`: a number is ms; `"disabled"` → 0; empty/unknown →
/// `None` (leave the builder default).
pub fn parse_http_idle_timeout_ms(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().filter(|v| *v >= 0),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            trimmed.parse::<i64>().ok().filter(|v| *v >= 0)
        }
        _ => None,
    }
}

fn env_get(env: Option<&HashMap<String, String>>, key: &str) -> String {
    let lower = key.to_lowercase();
    let upper = key.to_uppercase();
    if let Some(map) = env {
        if let Some(v) = map.get(&lower).or_else(|| map.get(&upper)) {
            return v.clone();
        }
    }
    std::env::var(&lower)
        .or_else(|_| std::env::var(&upper))
        .unwrap_or_default()
}

fn get_proxy_env(env: Option<&HashMap<String, String>>, key: &str) -> String {
    env_get(env, key)
}

/// `NO_PROXY` matching. Returns true when the host should be proxied. Mirrors
/// native `shouldProxyHostname`: `*` disables proxying entirely; entries may be
/// `host`, `host:port`, `.suffix`, or `*suffix`.
pub fn should_proxy_hostname(
    hostname: &str,
    port: u16,
    env: Option<&HashMap<String, String>>,
) -> bool {
    let no_proxy = get_proxy_env(env, "no_proxy").to_lowercase();
    if no_proxy.is_empty() {
        return true;
    }
    if no_proxy == "*" {
        return false;
    }

    no_proxy.split([',', ' ', '\t', '\n']).all(|proxy| {
        let proxy = proxy.trim();
        if proxy.is_empty() {
            return true;
        }
        let (mut proxy_hostname, proxy_port) = match proxy.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), p.parse::<u16>().unwrap_or(0))
            }
            _ => (proxy.to_string(), 0),
        };
        if proxy_port != 0 && proxy_port != port {
            return true;
        }
        if !proxy_hostname.starts_with('.') && !proxy_hostname.starts_with('*') {
            return hostname != proxy_hostname;
        }
        if let Some(stripped) = proxy_hostname.strip_prefix('*') {
            proxy_hostname = stripped.to_string();
        }
        !hostname.ends_with(&proxy_hostname)
    })
}

/// Resolve the proxy URL to use for `target_url` from the environment, or
/// `None` when it should be contacted directly. Errors on an invalid or
/// unsupported (SOCKS/PAC) proxy URL.
pub fn resolve_http_proxy_url_for_target(
    target_url: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<Option<Url>, String> {
    let parsed =
        Url::parse(target_url).map_err(|e| format!("invalid target URL {target_url:?}: {e}"))?;
    let protocol = parsed.scheme();
    if protocol.is_empty() || parsed.host_str().is_none() {
        return Ok(None);
    }
    let hostname = parsed.host_str().unwrap_or("").to_lowercase();
    let port = parsed
        .port()
        .or_else(|| default_proxy_port(protocol))
        .unwrap_or(0);
    if !should_proxy_hostname(&hostname, port, env) {
        return Ok(None);
    }

    let mut proxy = get_proxy_env(env, &format!("{protocol}_proxy"));
    if proxy.is_empty() {
        proxy = get_proxy_env(env, "all_proxy");
    }
    if proxy.is_empty() {
        return Ok(None);
    }
    if !proxy.contains("://") {
        proxy = format!("{protocol}://{proxy}");
    }

    let proxy_url = Url::parse(&proxy).map_err(|e| format!("invalid proxy URL {proxy:?}: {e}"))?;
    match proxy_url.scheme() {
        "http" | "https" => Ok(Some(proxy_url)),
        other => Err(format!("{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {other}:")),
    }
}

/// Build a `reqwest::ClientBuilder` with the system proxy for `target_url` and
/// the pooled-connection idle timeout applied. `idle_timeout_ms == 0` disables
/// idle expiry (native "disabled").
pub fn client_builder_for(
    target_url: &str,
    idle_timeout_ms: Option<i64>,
    env: Option<&HashMap<String, String>>,
) -> Result<reqwest::ClientBuilder, String> {
    // A provider request that cannot establish a connection should fail fast
    // rather than consuming the whole response deadline (10 minutes by default).
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .tcp_nodelay(true)
        .pool_max_idle_per_host(8);
    if let Some(proxy_url) = resolve_http_proxy_url_for_target(target_url, env)? {
        let proxy = reqwest::Proxy::all(proxy_url.as_str()).map_err(|e| e.to_string())?;
        builder = builder.proxy(proxy);
    }
    let idle = idle_timeout_ms.unwrap_or(DEFAULT_HTTP_IDLE_TIMEOUT_MS);
    if idle > 0 {
        builder = builder.pool_idle_timeout(std::time::Duration::from_millis(idle as u64));
    } else {
        builder = builder.pool_idle_timeout(None);
    }
    Ok(builder)
}

/// Convenience: build the client (see [`client_builder_for`]).
pub fn build_client(
    target_url: &str,
    idle_timeout_ms: Option<i64>,
    env: Option<&HashMap<String, String>>,
) -> Result<reqwest::Client, String> {
    client_builder_for(target_url, idle_timeout_ms, env)?
        .build()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resolves_https_proxy() {
        let e = env(&[("https_proxy", "http://proxy.local:8080")]);
        let url = resolve_http_proxy_url_for_target("https://api.anthropic.com/v1", Some(&e))
            .unwrap()
            .unwrap();
        assert_eq!(url.as_str(), "http://proxy.local:8080/");
    }

    #[test]
    fn no_proxy_star_disables() {
        let e = env(&[("https_proxy", "http://proxy:8080"), ("no_proxy", "*")]);
        assert!(
            resolve_http_proxy_url_for_target("https://x.test", Some(&e))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn no_proxy_host_match() {
        let e = env(&[("https_proxy", "http://proxy:8080"), ("no_proxy", "x.test")]);
        assert!(
            resolve_http_proxy_url_for_target("https://x.test", Some(&e))
                .unwrap()
                .is_none()
        );
        assert!(
            resolve_http_proxy_url_for_target("https://y.test", Some(&e))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn rejects_socks() {
        let e = env(&[("https_proxy", "socks5://proxy:1080")]);
        let err = resolve_http_proxy_url_for_target("https://x.test", Some(&e)).unwrap_err();
        assert!(err.contains("SOCKS"));
    }

    #[test]
    fn bare_host_port_gets_scheme() {
        let e = env(&[("http_proxy", "proxy.local:3128")]);
        let url = resolve_http_proxy_url_for_target("http://x.test", Some(&e))
            .unwrap()
            .unwrap();
        assert_eq!(url.scheme(), "http");
    }

    #[test]
    fn parse_idle_timeout() {
        use serde_json::json;
        assert_eq!(parse_http_idle_timeout_ms(&json!(60_000)), Some(60_000));
        assert_eq!(parse_http_idle_timeout_ms(&json!("disabled")), Some(0));
        assert_eq!(parse_http_idle_timeout_ms(&json!("")), None);
        assert_eq!(parse_http_idle_timeout_ms(&json!("nope")), None);
    }
}
