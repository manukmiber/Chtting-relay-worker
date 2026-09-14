//! The two HTTP servers.
//!
//! They are deliberately separate processes' worth of surface on two ports:
//! [`public`] is what the tunnel exposes, [`dashboard`] binds to loopback and
//! carries the settings, keys and logs. Nothing routes between them.

pub mod catalog;
pub mod dashboard;
pub mod openrouter;
pub mod public;

use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;
use std::sync::Arc;

/// Answer a handler that panicked, instead of dropping the connection.
///
/// `panic = "unwind"` is set in `Cargo.toml` for exactly this: one request's
/// bug must not take down the few hundred callers sharing the process. But
/// unwinding on its own only carries the panic up to hyper's connection task,
/// which then dies having sent nothing — the caller sees a reset socket, with
/// no status to act on, and a client that retries network errors retries
/// straight back into the same panic.
///
/// So it is caught at the one boundary that can still answer. What the panic
/// actually said goes to the log, where the operator needs it; the caller gets
/// a plain 500, because a panic message names internals and is no more use to
/// them than the reset was.
pub async fn catch_panics(
    axum::extract::State(logger): axum::extract::State<Arc<crate::logging::Logger>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use futures_util::FutureExt;

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    match std::panic::AssertUnwindSafe(next.run(request))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(panic) => {
            let said = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic that carried no message".into());
            logger.error(format!("panic while handling {method} {path}: {said}"));
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": {
                        "message": "the relay failed to handle that request",
                        "type": "server_error",
                        "code": "internal_error",
                    }
                })),
            )
                .into_response()
        }
    }
}

/// The caller's address, honouring proxy headers only when configured to *and*
/// only when the connection came from this machine.
///
/// Behind the tunnel, `CF-Connecting-IP` is the real client and the connection
/// arrives from cloudflared on loopback, so the header is worth believing. From
/// anywhere else it is a string the caller typed. Believing it there would let
/// anyone walk straight through `blockedIps` — the block list is checked
/// against whatever this returns — and write any address they like into the
/// request log at the same time.
///
/// So the peer decides. `trustProxyHeaders` stays as the switch it always was;
/// this is the condition it was always missing.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy && peer.ip().is_loopback() {
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
    let raw = headers.get("authorization")?.to_str().ok()?.trim();
    // A scheme that is present decides the answer, even when what follows it is
    // empty. Falling through to the bare-key branch on `Bearer ` used to hand
    // back the word "Bearer" as though it were the key, which turns "no key was
    // sent" into "this key is wrong" and writes a scheme name into the logs
    // where a secret's shape belongs.
    if let Some(rest) = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
    {
        return Some(rest.trim()).filter(|s| !s.is_empty());
    }
    if raw.eq_ignore_ascii_case("bearer") {
        return None;
    }
    // Some clients send the key bare, without the scheme.
    Some(raw).filter(|s| !s.is_empty() && !s.contains(' '))
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
            // Only what a caller actually sends. `x-api-key` and
            // `anthropic-version` used to be listed here and nothing inbound
            // has ever read either — a client sending them in place of a
            // bearer token gets a 401 regardless — so all they did was name a
            // vendor to every browser that asked what this API accepts.
            "access-control-allow-headers",
            HeaderValue::from_static("authorization, content-type, x-user-id"),
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

    fn loopback() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4000)
    }

    #[test]
    fn proxy_headers_are_honoured_only_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.9"));
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.7, 10.1.1.1"),
        );

        assert_eq!(client_ip(&headers, loopback(), true), "203.0.113.9");
        assert_eq!(client_ip(&headers, loopback(), false), "127.0.0.1");

        headers.remove("cf-connecting-ip");
        assert_eq!(client_ip(&headers, loopback(), true), "198.51.100.7");
    }

    /// cloudflared runs on the phone and connects over loopback, so a
    /// forwarded-for header that did not arrive that way did not come from it.
    /// Believing one that came off the network would make `blockedIps` a
    /// suggestion — anyone refused could pick another address and try again.
    #[test]
    fn a_forwarded_address_from_a_remote_peer_is_not_believed() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.9"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));

        assert_eq!(
            client_ip(&headers, peer(), true),
            "10.0.0.1",
            "the address the packets actually came from wins"
        );
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

    /// A scheme with nothing after it is a missing key, not a key called
    /// "Bearer". Reading it as one turned "you sent no key" into "your key is
    /// wrong" and put a scheme name everywhere a secret's shape is recorded.
    #[test]
    fn an_empty_bearer_is_no_key_rather_than_the_word_bearer() {
        let mut headers = HeaderMap::new();
        for raw in ["Bearer ", "Bearer", "bearer   ", "   ", ""] {
            headers.insert("authorization", HeaderValue::from_str(raw).unwrap());
            assert_eq!(bearer_token(&headers), None, "{raw:?}");
        }

        // Surrounding whitespace is still just whitespace.
        headers.insert(
            "authorization",
            HeaderValue::from_static("  Bearer sk-abc "),
        );
        assert_eq!(bearer_token(&headers), Some("sk-abc"));
    }

    /// A handler that panics has to produce an answer. Before this, unwinding
    /// reached hyper's connection task and killed it silently: the caller got a
    /// reset socket with no status, which a retrying client treats as a network
    /// blip and walks straight back into.
    ///
    /// This test panics on purpose, so one "thread panicked at" line on stderr
    /// is the test working rather than the test failing.
    #[tokio::test]
    async fn a_panicking_handler_is_answered_rather_than_dropped() {
        use axum::routing::get;

        let logger = crate::logging::Logger::console(crate::logging::Level::Silent);
        let app = axum::Router::new()
            .route(
                "/boom",
                get(|| async {
                    panic!("a bug in a handler");
                    #[allow(unreachable_code)]
                    ""
                }),
            )
            .route("/fine", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                logger.clone(),
                catch_panics,
            ))
            .with_state(());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let boom = client
            .get(format!("http://{addr}/boom"))
            .send()
            .await
            .expect("the connection survives the panic");
        assert_eq!(boom.status(), 500);

        let body = boom.text().await.unwrap();
        assert!(body.contains("failed to handle"), "{body}");
        assert!(
            !body.contains("a bug in a handler"),
            "the panic's own words must not travel to the caller: {body}"
        );

        // And the process is still serving afterwards.
        let fine = client
            .get(format!("http://{addr}/fine"))
            .send()
            .await
            .expect("still listening");
        assert_eq!(fine.status(), 200);
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
