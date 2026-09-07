//! The public, tunnel-facing server.
//!
//! It speaks the OpenAI HTTP API so any existing client works unchanged, and
//! exposes nothing about the real backend.

use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::relay::error_response;
use crate::relay::handler::{self, Auth};
use crate::state::AppState;
use crate::store::RequestRecord;
use crate::util::{day_key, hour_key, new_id, now_ms, round, truncate};

use super::{bearer_token, client_ip, cors_headers};

pub fn router(state: Arc<AppState>) -> Router {
    let limit = state.config.current().server.max_body_bytes;
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(model_by_id))
        .route("/v1/chat/completions", post(chat))
        .route("/chat/completions", post(chat))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

/// Wrap a response in the configured CORS headers.
fn with_cors(state: &AppState, headers: &HeaderMap, mut response: Response) -> Response {
    let origins = &state.config.current().security.cors_origins;
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    for (name, value) in cors_headers(origins, origin) {
        if let Ok(name) = axum::http::HeaderName::try_from(name) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

async fn health(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config.current();
    Json(serde_json::json!({
        "status": "ok",
        "service": "chtting-relay",
        "version": env!("CARGO_PKG_VERSION"),
        "models": cfg.models.iter().filter(|m| m.enabled).count(),
        "backends": cfg.backends.iter().filter(|b| b.enabled).count(),
        "uptime_s": state.stats.uptime_s(),
        "in_flight": state.stats.in_flight.load(std::sync::atomic::Ordering::Relaxed),
    }))
    .into_response()
}

async fn not_found() -> Response {
    error_response(404, "no route for that path", "not_found", None)
}

/// Authenticate, or turn the refusal into the response to send.
///
/// The refusal is boxed: it is the rare path, and an unboxed `Response` in the
/// error variant would make every successful authentication carry its weight.
fn require_key(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::config::ClientKey, Box<Response>> {
    let cfg = state.config.current();
    match handler::authenticate(&cfg, bearer_token(headers)) {
        Auth::Ok(key) => Ok(key),
        Auth::Denied { status, message } => Err(Box::new(error_response(
            status,
            &message,
            "invalid_request_error",
            Some("invalid_api_key"),
        ))),
    }
}

fn blocked(state: &AppState, ip: &str) -> Option<Response> {
    state
        .config
        .current()
        .security
        .blocked_ips
        .iter()
        .any(|b| b == ip)
        .then(|| error_response(403, "blocked", "forbidden", None))
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let response = match require_key(&state, &headers) {
        Err(denied) => *denied,
        Ok(_) => Json(handler::list_models(&state.config.current())).into_response(),
    };
    with_cors(&state, &headers, response)
}

async fn model_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let response = match require_key(&state, &headers) {
        Err(denied) => *denied,
        Ok(_) => {
            let listed = handler::list_models(&state.config.current());
            let found = listed["data"]
                .as_array()
                .and_then(|a| a.iter().find(|m| m["id"] == id.as_str()))
                .cloned();
            match found {
                Some(model) => Json(model).into_response(),
                None => error_response(
                    404,
                    &format!("model \"{id}\" not found"),
                    "invalid_request_error",
                    Some("model_not_found"),
                ),
            }
        }
    };
    with_cors(&state, &headers, response)
}

async fn chat(
    state: State<Arc<AppState>>,
    connect: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Json<Value>,
) -> Response {
    chat_inner(state, connect, headers, body, "v1/chat/completions").await
}

async fn completions(
    state: State<Arc<AppState>>,
    connect: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Json<Value>,
) -> Response {
    chat_inner(state, connect, headers, body, "v1/completions").await
}

async fn chat_inner(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
    endpoint: &str,
) -> Response {
    let cfg = state.config.current();
    let ip = client_ip(&headers, peer, cfg.security.trust_proxy_headers);
    if let Some(refused) = blocked(&state, &ip) {
        return with_cors(&state, &headers, refused);
    }

    let key = match require_key(&state, &headers) {
        Ok(key) => key,
        Err(denied) => return with_cors(&state, &headers, *denied),
    };

    // Rate limit before doing any work, so a hammering key costs nothing.
    if let Err(retry_after) = state.limiter.check(&key.id, key.quota.requests_per_minute) {
        let message = format!("rate limit reached ({}/min)", key.quota.requests_per_minute);
        let response = (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", retry_after.to_string())],
            Json(serde_json::json!({
                "error": {"message": message, "type": "invalid_request_error", "code": "rate_limit_exceeded"}
            })),
        )
            .into_response();
        return with_cors(&state, &headers, response);
    }

    if let Some(response) = daily_quota_refusal(&state, &key) {
        return with_cors(&state, &headers, response);
    }

    if body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty()
    {
        return with_cors(
            &state,
            &headers,
            error_response(
                400,
                "the \"model\" field is required",
                "invalid_request_error",
                None,
            ),
        );
    }

    let response = handler::handle_chat(state.clone(), key, ip, &headers, endpoint, body).await;
    with_cors(&state, &headers, response)
}

/// Daily quotas are answered from memory, not from a query per request.
fn daily_quota_refusal(state: &Arc<AppState>, key: &crate::config::ClientKey) -> Option<Response> {
    let quota = &key.quota;
    if quota.requests_per_day == 0 && quota.tokens_per_day == 0 {
        return None;
    }
    let today = state.today();
    let used = state.quotas.get(&key.id, &today);

    let message = if quota.requests_per_day > 0 && used.requests >= quota.requests_per_day {
        format!("daily request quota reached ({})", quota.requests_per_day)
    } else if quota.tokens_per_day > 0 && used.tokens >= quota.tokens_per_day {
        format!("daily token quota reached ({})", quota.tokens_per_day)
    } else {
        return None;
    };

    Some(error_response(
        429,
        &message,
        "invalid_request_error",
        Some("rate_limit_exceeded"),
    ))
}

/// Embeddings get the same alias translation and token accounting as chat, but
/// there is no stream and no reshaping beyond the model name.
async fn embeddings(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let cfg = state.config.current();
    let ip = client_ip(&headers, peer, cfg.security.trust_proxy_headers);
    let key = match require_key(&state, &headers) {
        Ok(key) => key,
        Err(denied) => return with_cors(&state, &headers, *denied),
    };

    let asked_for = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let Some(route) = cfg.find_model(asked_for).filter(|m| m.enabled).cloned() else {
        return with_cors(
            &state,
            &headers,
            error_response(
                404,
                &format!("model \"{asked_for}\" is not available on this relay"),
                "invalid_request_error",
                Some("model_not_found"),
            ),
        );
    };
    if let Err(message) = handler::check_access(&key, &route.id) {
        return with_cors(
            &state,
            &headers,
            error_response(
                403,
                &message,
                "invalid_request_error",
                Some("model_forbidden"),
            ),
        );
    }
    let Some(backend) = cfg
        .find_backend(&route.backend)
        .filter(|b| b.enabled)
        .cloned()
    else {
        return with_cors(
            &state,
            &headers,
            error_response(502, "backend unavailable", "upstream_error", None),
        );
    };

    let started = std::time::Instant::now();
    let started_wall = now_ms();
    let tz = cfg.tz();

    let inputs: Vec<String> = match body.get("input") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![String::new()],
    };

    let resolved = state.counter.resolve(
        &cfg,
        &route.upstream_model,
        &route.tokenizer,
        &route.chat_profile,
    );
    let mut local_prompt = 0usize;
    let mut exact = true;
    let mut tokenizer_name = String::new();
    for text in &inputs {
        let (n, is_exact, name) = state.counter.count_text(text, &resolved).await;
        local_prompt += n;
        exact &= is_exact;
        tokenizer_name = name;
    }

    let mut upstream_body = body.clone();
    if let Some(map) = upstream_body.as_object_mut() {
        map.insert("model".into(), Value::String(route.upstream_model.clone()));
    }

    let (status, error, payload) = match state
        .upstream
        .send(&backend, "v1/embeddings", &upstream_body, false)
        .await
    {
        Ok((res, _)) => {
            let status = res.status().as_u16();
            let payload: Option<Value> = res.json().await.ok();
            let error = if (200..300).contains(&status) {
                String::new()
            } else {
                payload
                    .as_ref()
                    .and_then(|p| p.get("error").and_then(|e| e.get("message")))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("backend returned {status}"))
            };
            (status, error, payload)
        }
        Err(err) => (err.status, err.message, None),
    };

    let upstream_prompt = payload
        .as_ref()
        .and_then(|p| p.get("usage"))
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let prompt_tokens = if upstream_prompt > 0 {
        upstream_prompt as i64
    } else {
        local_prompt as i64
    };

    state.store.insert(RequestRecord {
        id: new_id("emb"),
        ts: started_wall,
        day: day_key(started_wall, &tz),
        hour: hour_key(started_wall, &tz),
        key_id: key.id.clone(),
        key_label: if key.label.is_empty() {
            key.id.clone()
        } else {
            key.label.clone()
        },
        ip,
        user_agent: truncate(
            headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            200,
        ),
        endpoint: "v1/embeddings".into(),
        public_model: route.id.clone(),
        backend_id: backend.id.clone(),
        upstream_model: route.upstream_model.clone(),
        status: i64::from(status),
        error: truncate(&error, 500),
        total_ms: round(started.elapsed().as_secs_f64() * 1000.0, 1),
        prompt_tokens,
        total_tokens: prompt_tokens,
        local_prompt: local_prompt as i64,
        drift_prompt: local_prompt as i64 - prompt_tokens,
        usage_source: if upstream_prompt > 0 {
            "upstream".into()
        } else {
            "local".into()
        },
        tokenizer: tokenizer_name,
        exact: i64::from(exact),
        ..Default::default()
    });

    let response = if !error.is_empty() {
        state
            .logger
            .warn(format!("embeddings failed for {}: {error}", route.id));
        error_response(
            if status >= 400 { status } else { 502 },
            &error,
            "upstream_error",
            None,
        )
    } else {
        let mut out = payload.unwrap_or(Value::Null);
        if let Some(map) = out.as_object_mut() {
            // The alias goes back out; the backend's own name never does.
            map.insert("model".into(), Value::String(route.id.clone()));
        }
        Json(out).into_response()
    };
    with_cors(&state, &headers, response)
}
