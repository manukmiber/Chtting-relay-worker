//! The relay proper.

pub mod gate;
pub mod handler;
pub mod pace;
pub mod sse;
pub mod trace;
pub mod transform;
pub mod upstream;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// An OpenAI-shaped error body, so existing clients parse it without changes.
pub fn error_response(status: u16, message: &str, kind: &str, code: Option<&str>) -> Response {
    let mut error = serde_json::json!({
        "message": message,
        "type": kind,
    });
    if let Some(code) = code {
        error["code"] = serde_json::Value::String(code.to_string());
    }
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

/// What the caller is told when the backend fails.
///
/// Requirement 14, on the one path that had been missing it: an upstream
/// failure used to travel outwards verbatim — the backend's own wording, its
/// name, and on a connection error the host it lives at. A relay whose whole
/// point is that callers cannot see past it must not describe its backend in
/// the one response most likely to be pasted into a bug report.
///
/// So the status is mapped by class and the text is ours. The backend's real
/// answer still goes to the log and the request row, where the operator needs
/// it and nobody else can read it.
///
/// The mapping keeps the statuses a caller can act on and collapses the rest
/// into 502: a 401 from the backend means *our* credentials failed, and
/// passing it on would tell the caller their own key was rejected, which is
/// both false and unactionable.
pub fn upstream_failure(status: u16) -> Response {
    let (status, message, code) = match status {
        400 | 422 => (
            400,
            "that request was not accepted for this model",
            "invalid_request",
        ),
        413 => (413, "the request is too large for this model", "too_large"),
        408 | 504 => (504, "the model took too long to answer", "timeout"),
        429 => (
            429,
            "the model is busy — try again shortly",
            "rate_limit_exceeded",
        ),
        _ => (
            502,
            "the model is unavailable right now",
            "model_unavailable",
        ),
    };
    error_response(status, message, "server_error", Some(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_upstream_failure_never_carries_the_backends_own_words() {
        // The shapes that used to travel: the backend's name, its error text,
        // and the host behind it.
        for status in [400, 401, 402, 403, 404, 408, 413, 429, 500, 502, 503, 504] {
            let response = upstream_failure(status);
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("a small body");
            let text = String::from_utf8_lossy(&body).to_lowercase();
            for leak in [
                "deepseek",
                "openrouter",
                "api key",
                "authentication",
                "backend",
                // Not just in the prose: `type` and `code` are part of the
                // body a caller pastes into an issue, and "upstream_error"
                // says there is something upstream as plainly as a sentence
                // would.
                "upstream",
                "relay",
                "provider",
                "http://",
                "https://",
            ] {
                assert!(
                    !text.contains(leak),
                    "status {status} leaked {leak:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn a_caller_is_never_told_their_own_key_was_refused_for_ours() {
        // 401 and 403 upstream are about the relay's credentials, not the
        // caller's, so they must not come back as the caller's problem.
        for status in [401, 402, 403, 404, 500, 503] {
            let response = upstream_failure(status);
            assert_eq!(
                response.status().as_u16(),
                502,
                "upstream {status} should read as unavailable"
            );
        }
        // The ones a caller can actually do something about keep their meaning.
        assert_eq!(upstream_failure(400).status().as_u16(), 400);
        assert_eq!(upstream_failure(413).status().as_u16(), 413);
        assert_eq!(upstream_failure(429).status().as_u16(), 429);
        assert_eq!(upstream_failure(408).status().as_u16(), 504);
    }
}
