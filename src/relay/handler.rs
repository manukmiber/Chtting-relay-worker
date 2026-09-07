//! Authenticate, translate the model name, inject the system prompt, call the
//! backend, reshape what comes back, and record what it cost.
//!
//! Timing vocabulary used throughout:
//!
//! ```text
//!    sent                 first token                  last token
//!      │◄──── ttft_ms ────►│◄──────── gen_ms ──────────►│
//!      │◄───────────────── total_ms ───────────────────►│
//!    tokens_per_sec = completion_tokens / gen_ms
//! ```

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::{ClientKey, Config, Model, RequestTransform, ResolvedResponseTransform, ResponseTransform};
use crate::relay::sse::{self, SseParser, StreamRewriter};
use crate::relay::transform::{
    collect_tool_calls, compile_text_rules, rules_lookbehind, transform_chunk, transform_request,
    transform_response, ReasoningState,
};
use crate::relay::upstream::Sent;
use crate::relay::error_response;
use crate::state::AppState;
use crate::store::RequestRecord;
use crate::tokenizer::chat::flatten_content;
use crate::tokenizer::{reconcile_usage, LocalCount, Resolved, Usage};
use crate::util::{day_key, hour_key, new_id, now_ms, round, truncate};

/// The outcome of an authentication attempt.
pub enum Auth {
    Ok(ClientKey),
    Denied { status: u16, message: String },
}

pub fn authenticate(cfg: &Config, secret: Option<&str>) -> Auth {
    if !cfg.security.require_client_key {
        return Auth::Ok(ClientKey {
            id: "anonymous".into(),
            label: "anonymous".into(),
            models: vec!["*".into()],
            ..Default::default()
        });
    }
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return Auth::Denied {
            status: 401,
            message: "missing API key: send Authorization: Bearer <key>".into(),
        };
    };
    match cfg.find_key_by_secret(secret) {
        None => Auth::Denied {
            status: 401,
            message: "invalid API key".into(),
        },
        Some(key) if !key.enabled => Auth::Denied {
            status: 403,
            message: format!(
                "key \"{}\" is disabled",
                if key.label.is_empty() { &key.id } else { &key.label }
            ),
        },
        Some(key) => Auth::Ok(key.clone()),
    }
}

pub fn check_access(key: &ClientKey, model_id: &str) -> Result<(), String> {
    if key.models.iter().any(|m| m == "*" || m == model_id) {
        return Ok(());
    }
    Err(format!(
        "key \"{}\" may not use model \"{model_id}\"",
        if key.label.is_empty() { &key.id } else { &key.label }
    ))
}

/// The public model list. The backend's real name is deliberately absent.
pub fn list_models(cfg: &Config) -> Value {
    let data: Vec<Value> = cfg
        .models
        .iter()
        .filter(|m| m.enabled)
        .map(|m| {
            let mut entry = serde_json::json!({
                "id": m.id,
                "object": "model",
                "created": m.created_at / 1000,
                "owned_by": "chtting-relay",
                "display_name": if m.display_name.is_empty() { &m.id } else { &m.display_name },
            });
            let map = entry.as_object_mut().expect("just built an object");
            if !m.description.is_empty() {
                map.insert("description".into(), Value::String(m.description.clone()));
            }
            if m.context_length > 0 {
                map.insert("context_length".into(), Value::from(m.context_length));
            }
            entry
        })
        .collect();
    serde_json::json!({ "object": "list", "data": data })
}

/* ------------------------------------------------------------- context -- */

struct Ctx {
    state: Arc<AppState>,
    cfg: Arc<Config>,
    route: Model,
    resolved: Resolved,
    transform: ResolvedResponseTransform,
    record: RequestRecord,
    started: Instant,
    client_wants_stream: bool,
}

/* ------------------------------------------------------------ dispatch -- */

pub async fn handle_chat(
    state: Arc<AppState>,
    key: ClientKey,
    ip: String,
    headers: &HeaderMap,
    endpoint: &str,
    body: Value,
) -> Response {
    let cfg = state.config.current();
    let started = Instant::now();
    let started_wall = now_ms();
    let tz = cfg.tz();
    let id = new_id("req");

    let mut record = RequestRecord {
        id: id.clone(),
        ts: started_wall,
        day: day_key(started_wall, &tz),
        hour: hour_key(started_wall, &tz),
        key_id: key.id.clone(),
        key_label: if key.label.is_empty() { key.id.clone() } else { key.label.clone() },
        ip,
        user_agent: truncate(
            headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            200,
        ),
        endpoint: endpoint.to_string(),
        public_model: body.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        exact: 1,
        ..Default::default()
    };

    let asked_for = record.public_model.clone();
    let Some(route) = cfg.find_model(&asked_for).filter(|m| m.enabled).cloned() else {
        let message = format!("model \"{asked_for}\" is not available on this relay");
        finish(&state, record, 404, &message, None, None, None, None, "", "");
        return error_response(404, &message, "invalid_request_error", Some("model_not_found"));
    };

    if let Err(message) = check_access(&key, &route.id) {
        finish(&state, record, 403, &message, None, None, None, None, "", "");
        return error_response(403, &message, "invalid_request_error", Some("model_forbidden"));
    }

    record.backend_id = route.backend.clone();
    record.upstream_model = route.upstream_model.clone();

    // Requirement 2: count what goes in, with the tokenizer of the *backend*
    // model, because that is the one that bills.
    let rt = RequestTransform::merged(&cfg.defaults.request_transform, &route.request_transform);
    let mut upstream_body = transform_request(&body, &route, &cfg, &rt);
    let resolved = state
        .counter
        .resolve(&cfg, &route.upstream_model, &route.tokenizer, &route.chat_profile);
    let input = state
        .counter
        .count_request(&upstream_body, &resolved, &cfg.tokenizer.image_defaults)
        .await;

    record.local_prompt = input.total as i64;
    record.tokenizer = input.tokenizer.clone();
    record.exact = i64::from(input.exact);

    let max_in = route.limits.max_input_tokens;
    if max_in > 0 && input.total > max_in as usize {
        let message = format!(
            "prompt is {} tokens, over this model's {max_in} token limit",
            input.total
        );
        finish(&state, record, 413, &message, None, None, None, None, "", "");
        return error_response(413, &message, "invalid_request_error", Some("context_length_exceeded"));
    }

    if cfg.logging.store_bodies != "none" {
        record.req_preview = preview_request(&upstream_body, &cfg);
    }

    let client_wants_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    // Streaming upstream is what makes TTFT and tokens/sec measurable; when the
    // caller asked for a whole response we buffer the stream back together.
    let stream_upstream = rt.force_stream.unwrap_or(client_wants_stream);
    record.stream = i64::from(client_wants_stream);

    if let Some(map) = upstream_body.as_object_mut() {
        map.insert("stream".into(), Value::Bool(stream_upstream));
        if stream_upstream {
            let wants_usage = cfg
                .find_backend(&route.backend)
                .map(|b| b.stream_options)
                .unwrap_or(true);
            if wants_usage {
                map.insert(
                    "stream_options".into(),
                    serde_json::json!({"include_usage": true}),
                );
            }
        } else {
            map.remove("stream_options");
        }
    }

    // Backpressure: refuse rather than pile work onto a phone that is already
    // saturated. A queued request that times out helps nobody.
    let Ok(permit) = state.in_flight.clone().try_acquire_owned() else {
        state.stats.rejected_overload.fetch_add(1, Ordering::Relaxed);
        let message = format!(
            "relay is at its concurrency limit ({}); retry shortly",
            cfg.server.max_concurrent_requests
        );
        finish(&state, record, 503, &message, None, None, None, None, "", "");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "1")],
            axum::Json(serde_json::json!({
                "error": {"message": message, "type": "server_error", "code": "overloaded"}
            })),
        )
            .into_response();
    };
    state.stats.in_flight.fetch_add(1, Ordering::Relaxed);

    let sent = state
        .upstream
        .send_with_fallback(&cfg, &route, endpoint, &upstream_body, stream_upstream)
        .await;

    let sent = match sent {
        Ok(sent) => sent,
        Err(err) => {
            record.retries = i64::from(err.attempts.saturating_sub(1));
            finish(&state, record, err.status as i64, &err.message, Some(started), None, None, None, "", "");
            state.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
            drop(permit);
            return error_response(err.status, &err.message, "upstream_error", None);
        }
    };

    record.backend_id = sent.backend().id.clone();
    record.retries = i64::from(sent.attempts().saturating_sub(1));

    let response = match sent {
        Sent::Failed { status, body, .. } => {
            let message = format!("backend rejected the request: {}", truncate(&body, 400));
            finish(
                &state,
                record,
                i64::from(status),
                &truncate(&body, 500),
                Some(started),
                None,
                None,
                None,
                "",
                "",
            );
            error_response(status, &message, "upstream_error", None)
        }
        Sent::Ok { response, .. } => {
            let transform =
                ResponseTransform::merged(&cfg.defaults.response_transform, &route.response_transform);
            let is_sse = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("text/event-stream"));

            let ctx = Ctx {
                state: state.clone(),
                cfg: cfg.clone(),
                route: route.clone(),
                resolved,
                transform,
                record,
                started,
                client_wants_stream,
            };

            if stream_upstream && is_sse {
                return pipe_stream(response, ctx, permit).await;
            }
            let out = pipe_buffered(response, ctx).await;
            state.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
            drop(permit);
            return out;
        }
    };

    state.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    drop(permit);
    response
}

/* ----------------------------------------------------------- streaming -- */

#[derive(Default)]
struct Pumped {
    text: String,
    reasoning: String,
    tool_calls: Vec<Value>,
    upstream_usage: Option<Value>,
    finish_reason: String,
    first_token_at: Option<Instant>,
    last_token_at: Option<Instant>,
    error: Option<String>,
    /// Set when the caller hung up, so the row is recorded as 499 rather than
    /// as an upstream failure.
    disconnected: bool,
}

/// Consume the upstream SSE stream, optionally forwarding reshaped events.
async fn pump(
    response: reqwest::Response,
    ctx: &Ctx,
    emit: Option<&mpsc::Sender<Result<Bytes, std::io::Error>>>,
) -> Pumped {
    let mut out = Pumped::default();
    let mut parser = SseParser::new();
    let rules = &ctx.transform.replace;
    let mut rewriter = StreamRewriter::new(compile_text_rules(rules), rules_lookbehind(rules));
    let mut reasoning_state = ReasoningState::default();
    let mut tool_calls: Map<String, Value> = Map::new();
    let mut prefix_sent = false;

    let mut stream = response.bytes_stream();
    // A streaming decoder, so a multi-byte character split across two network
    // chunks is reassembled rather than turning into replacement characters.
    let mut decoder = Utf8Decoder::default();

    'outer: while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(err) => {
                out.error = Some(err.to_string());
                break;
            }
        };

        let text = decoder.push(&chunk);
        for event in parser.push(&text) {
            if event.data == "[DONE]" {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                continue; // keep-alive comments and malformed frames
            };

            if let Some(usage) = parsed.get("usage").filter(|v| !v.is_null()) {
                out.upstream_usage = Some(usage.clone());
            }
            let choice = parsed
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first());
            let delta = choice.and_then(|c| c.get("delta"));

            if let Some(reason) = choice
                .and_then(|c| c.get("finish_reason"))
                .and_then(|v| v.as_str())
            {
                out.finish_reason = reason.to_string();
            }

            let content_delta = delta
                .and_then(|d| d.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let reasoning_delta = delta
                .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if let Some(calls) = delta.and_then(|d| d.get("tool_calls")).and_then(|v| v.as_array()) {
                collect_tool_calls(&mut tool_calls, calls);
            }

            if !content_delta.is_empty() || !reasoning_delta.is_empty() {
                if out.first_token_at.is_none() {
                    out.first_token_at = Some(Instant::now());
                }
                out.last_token_at = Some(Instant::now());
            }
            out.text.push_str(content_delta);
            out.reasoning.push_str(reasoning_delta);

            let mut shaped = transform_chunk(
                &parsed,
                &ctx.route.id,
                &ctx.transform,
                &mut reasoning_state,
            );

            let Some(tx) = emit else { continue };

            // Content deltas go through the rewriter, which may hold text back
            // until a pattern straddling this chunk boundary is complete.
            let shaped_content = shaped
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !shaped_content.is_empty() {
                let with_prefix = if !prefix_sent && !ctx.transform.prefix.is_empty() {
                    prefix_sent = true;
                    format!("{}{shaped_content}", ctx.transform.prefix)
                } else {
                    shaped_content
                };
                let safe = rewriter.push(&with_prefix);
                if safe.is_empty() {
                    continue; // held back; it will be flushed later
                }
                set_delta_content(&mut shaped, &safe);
            }

            if tx.send(Ok(sse::format_sse(&shaped))).await.is_err() {
                out.disconnected = true;
                break 'outer;
            }
        }
    }

    for event in parser.flush() {
        if event.data != "[DONE]" {
            if let Ok(parsed) = serde_json::from_str::<Value>(&event.data) {
                if let Some(usage) = parsed.get("usage").filter(|v| !v.is_null()) {
                    out.upstream_usage = Some(usage.clone());
                }
            }
        }
    }

    out.tool_calls = tool_calls.into_values().collect();

    // Flush whatever the rewriter was holding back, plus any suffix.
    if let Some(tx) = emit {
        if !out.disconnected {
            let tail = format!("{}{}", rewriter.flush(), ctx.transform.suffix);
            if !tail.is_empty() {
                let chunk = delta_chunk(&ctx.record.id, &ctx.route.id, &tail);
                if tx.send(Ok(sse::format_sse(&chunk))).await.is_err() {
                    out.disconnected = true;
                }
            }
        }
    }
    out
}

/// Stream to the caller. The pump runs in its own task so the HTTP response can
/// be returned immediately and the body produced as it arrives.
async fn pipe_stream(
    response: reqwest::Response,
    ctx: Ctx,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    if !ctx.client_wants_stream {
        // We streamed upstream only to measure TTFT; the caller wants JSON.
        let out = pipe_streamed_into_json(response, ctx).await;
        drop(permit);
        return out;
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let request_id = ctx.record.id.clone();

    tokio::spawn(async move {
        let state = ctx.state.clone();
        let pumped = pump(response, &ctx, Some(&tx)).await;
        let usage = finalise_usage(&ctx, &pumped, true).await;

        if !pumped.disconnected {
            if usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
                let chunk = usage_chunk(&ctx.record.id, &ctx.route.id, &usage);
                let _ = tx.send(Ok(sse::format_sse(&chunk))).await;
            }
            if let Some(err) = &pumped.error {
                let chunk = serde_json::json!({
                    "error": {"message": err, "type": "upstream_error"}
                });
                let _ = tx.send(Ok(sse::format_sse(&chunk))).await;
            }
            let _ = tx.send(Ok(Bytes::from_static(sse::DONE.as_bytes()))).await;
        }

        record_stream_outcome(&ctx, &pumped, usage);
        state.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
        drop(permit);
    });

    let mut headers = HeaderMap::new();
    for (k, v) in sse::SSE_HEADERS {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(k),
            axum::http::HeaderValue::from_str(v),
        ) {
            headers.insert(name, value);
        }
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        headers.insert("x-relay-request-id", value);
    }

    (
        StatusCode::OK,
        headers,
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

/// Rebuild a normal chat completion from a stream we consumed ourselves.
async fn pipe_streamed_into_json(response: reqwest::Response, ctx: Ctx) -> Response {
    let pumped = pump(response, &ctx, None).await;
    let usage = finalise_usage(&ctx, &pumped, false).await;

    let rules = compile_text_rules(&ctx.transform.replace);
    let body_text = match &rules {
        Some(r) => r.apply(&pumped.text),
        None => pumped.text.clone(),
    };
    let content = format!("{}{body_text}{}", ctx.transform.prefix, ctx.transform.suffix);

    let assembled = assemble_completion(
        &ctx.record.id,
        &ctx.route.id,
        &content,
        if ctx.transform.reasoning == "strip" { "" } else { &pumped.reasoning },
        &pumped.tool_calls,
        if pumped.finish_reason.is_empty() { "stop" } else { &pumped.finish_reason },
        &usage,
    );

    record_stream_outcome(&ctx, &pumped, usage);
    json_with_id(&ctx.record.id, assembled)
}

/// The path where the backend answered in one piece.
async fn pipe_buffered(response: reqwest::Response, ctx: Ctx) -> Response {
    let payload: Value = match response.json().await {
        Ok(v) => v,
        Err(err) => {
            let message = format!("backend sent a response the relay could not parse: {err}");
            finish(
                &ctx.state,
                ctx.record.clone(),
                502,
                &message,
                Some(ctx.started),
                None,
                None,
                None,
                "",
                "",
            );
            return error_response(502, &message, "upstream_error", None);
        }
    };

    let mut shaped = transform_response(&payload, &ctx.route.id, &ctx.transform);
    let choice = shaped
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);

    let content = flatten_content(
        choice
            .get("message")
            .and_then(|m| m.get("content"))
            .or_else(|| choice.get("text"))
            .unwrap_or(&Value::Null),
    );
    let reasoning = choice
        .get("message")
        .and_then(|m| m.get("reasoning_content").or_else(|| m.get("reasoning")))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool_calls: Vec<Value> = choice
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let (completion, exact, _) = ctx
        .state
        .counter
        .count_output(
            &content,
            &ctx.resolved,
            if ctx.transform.reasoning == "strip" { "" } else { &reasoning },
            tool_calls,
        )
        .await;

    let usage = reconcile_usage(
        LocalCount {
            prompt: ctx.record.local_prompt as u64,
            completion: completion as u64,
            exact: exact && ctx.record.exact == 1,
        },
        payload.get("usage"),
        ctx.cfg.tokenizer.prefer_upstream_usage,
    );

    if let Some(map) = shaped.as_object_mut() {
        map.insert("usage".into(), usage.public());
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let preview = if ctx.cfg.logging.store_bodies == "none" {
        String::new()
    } else {
        truncate(&content, ctx.cfg.logging.preview_chars)
    };

    let finished = Instant::now();
    finish(
        &ctx.state,
        ctx.record.clone(),
        200,
        "",
        Some(ctx.started),
        None,
        Some(finished),
        Some(&usage),
        &finish_reason,
        &preview,
    );

    if ctx.client_wants_stream {
        // The backend could not stream, so replay the finished answer as SSE.
        return replay_as_sse(&ctx, &content, &usage);
    }
    json_with_id(&ctx.record.id, shaped)
}

/* ------------------------------------------------------------ helpers -- */

async fn finalise_usage(ctx: &Ctx, pumped: &Pumped, streaming_to_client: bool) -> Usage {
    let counted_text = if streaming_to_client {
        pumped.text.clone()
    } else {
        let rules = compile_text_rules(&ctx.transform.replace);
        let body = match &rules {
            Some(r) => r.apply(&pumped.text),
            None => pumped.text.clone(),
        };
        format!("{}{body}{}", ctx.transform.prefix, ctx.transform.suffix)
    };

    let (completion, exact, _) = ctx
        .state
        .counter
        .count_output(
            &counted_text,
            &ctx.resolved,
            if ctx.transform.reasoning == "strip" { "" } else { &pumped.reasoning },
            pumped.tool_calls.clone(),
        )
        .await;

    reconcile_usage(
        LocalCount {
            prompt: ctx.record.local_prompt as u64,
            completion: completion as u64,
            exact: exact && ctx.record.exact == 1,
        },
        pumped.upstream_usage.as_ref(),
        ctx.cfg.tokenizer.prefer_upstream_usage,
    )
}

fn record_stream_outcome(ctx: &Ctx, pumped: &Pumped, usage: Usage) {
    let (status, error) = if pumped.disconnected {
        (499, "client disconnected mid-stream".to_string())
    } else if let Some(err) = &pumped.error {
        (502, err.clone())
    } else {
        (200, String::new())
    };

    let preview = if ctx.cfg.logging.store_bodies == "none" {
        String::new()
    } else {
        truncate(&pumped.text, ctx.cfg.logging.preview_chars)
    };

    finish(
        &ctx.state,
        ctx.record.clone(),
        status,
        &error,
        Some(ctx.started),
        pumped.first_token_at,
        pumped.last_token_at,
        Some(&usage),
        &pumped.finish_reason,
        &preview,
    );
}

/// Compute the derived metrics and hand the row to the writer.
#[allow(clippy::too_many_arguments)]
fn finish(
    state: &Arc<AppState>,
    mut record: RequestRecord,
    status: i64,
    error: &str,
    started: Option<Instant>,
    first_token_at: Option<Instant>,
    last_token_at: Option<Instant>,
    usage: Option<&Usage>,
    finish_reason: &str,
    response_preview: &str,
) {
    let end = Instant::now();
    let total_ms = started.map_or(0.0, |s| end.duration_since(s).as_secs_f64() * 1000.0);
    let ttft_ms = match (started, first_token_at) {
        (Some(s), Some(f)) => f.duration_since(s).as_secs_f64() * 1000.0,
        _ => 0.0,
    };
    let gen_ms = match (first_token_at, last_token_at) {
        (Some(f), Some(l)) if l > f => l.duration_since(f).as_secs_f64() * 1000.0,
        _ => 0.0,
    };

    let completion = usage.map_or(0, |u| u.completion_tokens);
    record.status = status;
    record.error = truncate(error, 800);
    record.finish_reason = finish_reason.to_string();
    record.total_ms = round(total_ms, 1);
    record.ttft_ms = round(ttft_ms, 1);
    record.gen_ms = round(gen_ms, 1);
    record.prompt_tokens = usage.map_or(record.local_prompt, |u| u.prompt_tokens as i64);
    record.completion_tokens = completion as i64;
    record.total_tokens = usage.map_or(record.prompt_tokens, |u| u.total_tokens as i64);
    record.cached_tokens = usage.map_or(0, |u| u.cached_tokens as i64);
    record.reasoning_tokens = usage.map_or(0, |u| u.reasoning_tokens as i64);
    record.usage_source = usage.map_or(String::new(), |u| u.source.to_string());
    record.exact = usage.map_or(record.exact, |u| i64::from(u.exact));
    record.local_prompt = usage.map_or(record.local_prompt, |u| u.local_prompt as i64);
    record.local_completion = usage.map_or(0, |u| u.local_completion as i64);
    record.drift_prompt = usage.map_or(0, |u| u.drift_prompt);
    record.drift_completion = usage.map_or(0, |u| u.drift_completion);
    // Throughput over the generation phase only, which is the number that
    // actually describes how fast the model produced tokens.
    record.tokens_per_sec = if gen_ms > 0.0 && completion > 0 {
        round(completion as f64 / gen_ms * 1000.0, 2)
    } else {
        0.0
    };
    record.res_preview = response_preview.to_string();

    let tag = if status >= 400 || status == 0 { "error" } else { "ok" };
    state.logger.info(format!(
        "{tag} {} -> {} {}in/{}out ttft={}ms total={}ms tps={} key={}{}",
        record.public_model,
        record.upstream_model,
        record.prompt_tokens,
        record.completion_tokens,
        record.ttft_ms,
        record.total_ms,
        record.tokens_per_sec,
        record.key_label,
        if error.is_empty() {
            String::new()
        } else {
            format!(" err={}", truncate(error, 160))
        },
    ));

    state
        .quotas
        .record(&record.key_id, &record.day, record.total_tokens.max(0) as u64);

    if !state.store.insert(record) {
        state
            .logger
            .warn("metrics queue is full; dropped a request row rather than blocking the relay");
    }
}

fn json_with_id(request_id: &str, body: Value) -> Response {
    let mut response = axum::Json(body).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-relay-request-id", value);
    }
    response
}

fn set_delta_content(chunk: &mut Value, content: &str) {
    if let Some(delta) = chunk
        .get_mut("choices")
        .and_then(|c| c.as_array_mut())
        .and_then(|a| a.first_mut())
        .and_then(|c| c.get_mut("delta"))
        .and_then(|d| d.as_object_mut())
    {
        delta.insert("content".into(), Value::String(content.to_string()));
    }
}

fn delta_chunk(id: &str, model: &str, content: &str) -> Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_ms() / 1000,
        "model": model,
        "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}],
    })
}

fn usage_chunk(id: &str, model: &str, usage: &Usage) -> Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_ms() / 1000,
        "model": model,
        "choices": [],
        "usage": usage.public(),
    })
}

fn assemble_completion(
    id: &str,
    model: &str,
    content: &str,
    reasoning: &str,
    tool_calls: &[Value],
    finish_reason: &str,
    usage: &Usage,
) -> Value {
    let mut message = serde_json::json!({"role": "assistant", "content": content});
    if let Some(map) = message.as_object_mut() {
        if !reasoning.is_empty() {
            map.insert("reasoning_content".into(), Value::String(reasoning.into()));
        }
        if !tool_calls.is_empty() {
            map.insert("tool_calls".into(), Value::Array(tool_calls.to_vec()));
        }
    }
    serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": now_ms() / 1000,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason, "logprobs": null}],
        "usage": usage.public(),
    })
}

/// Replay a finished answer as SSE for callers that insisted on streaming.
fn replay_as_sse(ctx: &Ctx, content: &str, usage: &Usage) -> Response {
    let id = &ctx.record.id;
    let model = &ctx.route.id;
    let created = now_ms() / 1000;
    let mut body = Vec::new();

    body.extend_from_slice(&sse::format_sse(&serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}],
    })));

    // Chunk on character boundaries, never bytes, or multi-byte text breaks.
    let chars: Vec<char> = content.chars().collect();
    for piece in chars.chunks(24) {
        let text: String = piece.iter().collect();
        body.extend_from_slice(&sse::format_sse(&serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}],
        })));
    }

    body.extend_from_slice(&sse::format_sse(&serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": usage.public(),
    })));
    body.extend_from_slice(sse::DONE.as_bytes());

    let mut headers = HeaderMap::new();
    for (k, v) in sse::SSE_HEADERS {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(k),
            axum::http::HeaderValue::from_str(v),
        ) {
            headers.insert(name, value);
        }
    }
    (StatusCode::OK, headers, Body::from(body)).into_response()
}

fn preview_request(body: &Value, cfg: &Config) -> String {
    if cfg.logging.store_bodies == "full" {
        return truncate(&body.to_string(), 20_000);
    }
    let last_user = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|msgs| {
            msgs.iter()
                .rev()
                .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        })
        .and_then(|m| m.get("content"))
        .cloned()
        .or_else(|| body.get("prompt").cloned())
        .unwrap_or(Value::Null);
    truncate(&flatten_content(&last_user), cfg.logging.preview_chars)
}

/// Reassembles UTF-8 across network chunk boundaries.
///
/// A naive `String::from_utf8_lossy` per chunk turns any character split
/// across two TCP reads into replacement characters — which is exactly what
/// happens to CJK and emoji on a slow mobile link.
#[derive(Default)]
struct Utf8Decoder {
    partial: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.partial.extend_from_slice(bytes);
        match std::str::from_utf8(&self.partial) {
            Ok(s) => {
                let out = s.to_string();
                self.partial.clear();
                out
            }
            Err(err) => {
                let valid = err.valid_up_to();
                let out = String::from_utf8_lossy(&self.partial[..valid]).into_owned();
                // Keep the incomplete tail for the next chunk. An actually
                // invalid sequence (not just a truncated one) is dropped.
                self.partial = match err.error_len() {
                    None => self.partial[valid..].to_vec(),
                    Some(len) => self.partial[valid + len..].to_vec(),
                };
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_across_chunks_is_reassembled() {
        let text = "你好世界🚀";
        let bytes = text.as_bytes();
        let mut decoder = Utf8Decoder::default();
        let mut out = String::new();
        // Feed one byte at a time: the worst case a network can produce.
        for byte in bytes {
            out.push_str(&decoder.push(&[*byte]));
        }
        assert_eq!(out, text);
        assert!(!out.contains('\u{FFFD}'));
    }

    #[test]
    fn a_key_limited_to_one_model_cannot_reach_another() {
        let key = ClientKey {
            id: "k1".into(),
            models: vec!["allowed".into()],
            ..Default::default()
        };
        assert!(check_access(&key, "allowed").is_ok());
        assert!(check_access(&key, "other").is_err());

        let wildcard = ClientKey {
            models: vec!["*".into()],
            ..Default::default()
        };
        assert!(check_access(&wildcard, "anything").is_ok());
    }

    #[test]
    fn the_model_list_never_mentions_the_backend() {
        let mut cfg = Config::default();
        cfg.models.push(Model {
            id: "manukmiberai/creative-writer".into(),
            upstream_model: "Deepseek-v4-flash-0731".into(),
            display_name: "Creative Writer".into(),
            enabled: true,
            ..Default::default()
        });
        cfg.models.push(Model {
            id: "hidden".into(),
            upstream_model: "secret-model".into(),
            enabled: false,
            ..Default::default()
        });

        let listed = list_models(&cfg);
        let text = listed.to_string();
        assert!(text.contains("manukmiberai/creative-writer"));
        assert!(!text.contains("Deepseek-v4-flash-0731"), "backend name leaked");
        assert!(!text.contains("hidden"), "a disabled model must not be listed");
    }

    #[test]
    fn authentication_distinguishes_missing_wrong_and_disabled_keys() {
        let mut cfg = Config::default();
        cfg.keys.push(ClientKey {
            id: "k1".into(),
            key: "sk-good".into(),
            enabled: true,
            ..Default::default()
        });
        cfg.keys.push(ClientKey {
            id: "k2".into(),
            key: "sk-off".into(),
            enabled: false,
            ..Default::default()
        });

        assert!(matches!(authenticate(&cfg, Some("sk-good")), Auth::Ok(_)));
        assert!(matches!(
            authenticate(&cfg, None),
            Auth::Denied { status: 401, .. }
        ));
        assert!(matches!(
            authenticate(&cfg, Some("sk-wrong")),
            Auth::Denied { status: 401, .. }
        ));
        assert!(matches!(
            authenticate(&cfg, Some("sk-off")),
            Auth::Denied { status: 403, .. }
        ));

        // With the requirement switched off, anyone gets in as "anonymous".
        cfg.security.require_client_key = false;
        assert!(matches!(authenticate(&cfg, None), Auth::Ok(k) if k.id == "anonymous"));
    }
}
