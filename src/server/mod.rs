//! The two HTTP servers.
//!
//! They are deliberately separate processes' worth of surface on two ports:
//! [`public`] is what the tunnel exposes, [`dashboard`] binds to loopback and
//! carries the settings, keys and logs. Nothing routes between them.

pub mod dashboard;
pub mod openrouter;
pub mod public;

use axum::http::{HeaderMap, HeaderValue};
use std::net::SocketAddr;

/// The caller's address, honouring proxy headers only when configured to.
///
/// Behind the tunnel, `CF-Connecting-IP` is the real client; without the
/// tunnel, trusting these headers would let anyone spoof their address, so the
/// setting defaults on but is worth turning off on a LAN-only relay.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
        {
            return ip.to_string();
        }
        if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = forwarded.split(',').next().map(str::trim) {
                if !first.is_empty() {
                    return first.to_string();
                }
            }
        }
    }
    peer.ip().to_string()
}

pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        // Some clients send the key bare, without the scheme.
        .or(Some(raw.trim()).filter(|s| !s.is_empty() && !s.contains(' ')))
}

/// Apply the configured CORS policy to a response.
pub fn cors_headers(
    origins: &[String],
    request_origin: Option<&str>,
) -> Vec<(&'static str, HeaderValue)> {
    let allow = if origins.iter().any(|o| o == "*") {
        Some("*".to_string())
    } else {
        request_origin
            .filter(|o| origins.iter().any(|allowed| allowed == o))
            .map(str::to_string)
    };

    let mut out = vec![
        (
            "access-control-allow-headers",
            HeaderValue::from_static("authorization, content-type, x-api-key, anthropic-version"),
        ),
        (
            "access-control-allow-methods",
            HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
        ),
        ("access-control-max-age", HeaderValue::from_static("86400")),
    ];
    if let Some(allow) = allow.and_then(|a| HeaderValue::from_str(&a).ok()) {
        out.push(("access-control-allow-origin", allow));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4000)
    }

    #[test]
    fn proxy_headers_are_honoured_only_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.9"));
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.7, 10.1.1.1"),
        );

        assert_eq!(client_ip(&headers, peer(), true), "203.0.113.9");
        assert_eq!(client_ip(&headers, peer(), false), "10.0.0.1");

        headers.remove("cf-connecting-ip");
        assert_eq!(client_ip(&headers, peer(), true), "198.51.100.7");
    }

    #[test]
    fn the_bearer_scheme_is_optional_because_clients_disagree() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer sk-abc"));
        assert_eq!(bearer_token(&headers), Some("sk-abc"));

        headers.insert("authorization", HeaderValue::from_static("sk-bare"));
        assert_eq!(bearer_token(&headers), Some("sk-bare"));

        headers.remove("authorization");
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn a_specific_origin_list_does_not_echo_arbitrary_origins() {
        let origins = vec!["https://app.example.com".to_string()];
        let allowed = cors_headers(&origins, Some("https://app.example.com"));
        assert!(allowed
            .iter()
            .any(|(k, v)| *k == "access-control-allow-origin" && v == "https://app.example.com"));

        let denied = cors_headers(&origins, Some("https://evil.example.com"));
        assert!(!denied
            .iter()
            .any(|(k, _)| *k == "access-control-allow-origin"));
    }
}
