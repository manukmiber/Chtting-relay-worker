//! The relay proper.

pub mod gate;
pub mod handler;
pub mod sse;
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
