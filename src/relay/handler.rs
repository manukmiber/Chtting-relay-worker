//! Authenticate, translate the model name, inject the system prompt, call the
//! backend, reshape what comes back, and record what it cost.
//!
//! Timing vocabulary used throughout:
//!
//! ```text
//!  arrives      admitted            first token         last token
//!     │◄─queued─►│◄──── ttft_ms ────►│◄──── gen_ms ──────►│
//!                │◄───────────── total_ms ───────────────►│
//!    tokens_per_sec = completion_tokens / gen_ms
//! ```
//!
//! And two prompt figures that must not be confused:
//!
//! * **billed** — the body as sent upstream, system prompt included. What the
//!   backend charges the relay.
//! * **charged** — the relay's own count of the caller's own body, measured
//!   before anything was injected. What they are shown and accounted for,
//!   because they did not write the rest and cannot see it.
//!
//! Nothing the backend says about itself reaches the caller. Every reply is
//! rebuilt from an envelope the relay owns — its own uuid, its own timestamp,
//! its own model name — and only a named handful of fields are copied across.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::{
    ClientKey, Config, KeyIndex, Model, Pricing, RequestTransform, ResolvedResponseTransform,
    ResponseTransform,
};
use crate::pricing::{self, Effort, Priced, Shape};
use crate::relay::error_response;
use crate::relay::gate::{Admission, Ticket};
use crate::relay::pace::Pacer;
use crate::relay::sse::{self, SseParser, StreamRewriter};
use crate::relay::trace::Trace;
use crate::relay::transform::{
    collect_tool_calls, compile_text_rules, rules_lookbehind, select_system_prompt,
    transform_chunk, transform_request, transform_response, Identity, ReasoningState,
};
use crate::relay::upstream::Sent;
use crate::state::AppState;
use crate::store::{LedgerEntry, Phase, RequestRecord};
use crate::tokenizer::chat::flatten_content;
use crate::tokenizer::{reconcile_usage, CacheCredit, LocalCount, Resolved, Usage};
use crate::util::{day_key, hour_key, new_uuid_v4, now_ms, round, truncate};

/// The outcome of an authentication attempt.
pub enum Auth {
    Ok(Arc<ClientKey>),
    Denied { status: u16, message: String },
}

/// The key a relay with `requireClientKey` switched off runs every call under.
///
/// Built once rather than per request, and deliberately a company key: an open
/// relay has no idea who is calling, so the caller's own `user` field is the
/// only identity there is.
fn anonymous_key() -> Arc<ClientKey> {
    static ANONYMOUS: std::sync::OnceLock<Arc<ClientKey>> = std::sync::OnceLock::new();
    ANONYMOUS
        .get_or_init(|| {
            Arc::new(ClientKey {
                id: "anonymous".into(),
                label: "anonymous".into(),
                models: vec!["*".into()],
                ..Default::default()
            })
        })
        .clone()
}

/// Authenticate against the published key index.
///
/// One hash and one map lookup, whatever the key list looks like — see
/// [`crate::config::KeyIndex`]. The `Arc` is what the rest of the request
/// carries: cloning the key itself would copy its model list, its quota and
/// its plaintext secret into every request's context, and the secret has no
/// business being there at all.
pub fn authenticate(cfg: &Config, keys: &KeyIndex, secret: Option<&str>) -> Auth {
    if !cfg.security.require_client_key {
        return Auth::Ok(anonymous_key());
    }
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return Auth::Denied {
            status: 401,
            message: "missing API key: send Authorization: Bearer <key>".into(),
        };
    };
    match keys.get(secret) {
        None => Auth::Denied {
            status: 401,
            message: "invalid API key".into(),
        },
        Some(key) if !key.enabled => Auth::Denied {
            status: 403,
            message: format!("key \"{}\" is disabled", key.display_name()),
        },
        Some(key) => Auth::Ok(key.clone()),
    }
}

/// Did the caller ask for the reasoning trace to be left out of the reply?
///
/// Both spellings OpenRouter's chat API accepts: `reasoning.exclude` on the
/// object, and the older flat `include_reasoning`. Silence is not a request to
/// exclude — only an explicit `false` is.
fn reasoning_excluded(body: &Value) -> bool {
    if body
        .get("reasoning")
        .and_then(|r| r.get("exclude"))
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        return true;
    }
    body.get("include_reasoning").and_then(|v| v.as_bool()) == Some(false)
}

/// Who this request is on behalf of, which is not the same question for the
/// two kinds of key.
///
/// * A **company** key is one customer with many people behind it, so the id
///   the caller sends is the id that counts: it isolates the prompt cache
///   upstream, and it is what the usage breaks down by. This is what every key
///   did before there were two kinds, so nothing about an existing setup moves.
/// * A **private** key *is* one person. There is nobody else behind it to name,
///   so whatever the caller put in `user` is ignored — not merged, not
///   preferred, ignored — and the key's own identity answers instead. A private
///   caller therefore cannot claim to be somebody else's user, land in somebody
///   else's cache partition, or file their spend under another name.
///
/// What a private key's identity *looks like* upstream is
/// [`SecurityConfig::private_user_id`]. By default it is a fingerprint: a
/// truncated SHA-256 of the key, which is stable and unique per key but is not
/// the key. Sending the key itself is available and is not the default,
/// because it would write a working credential into a third party's logs.
pub fn effective_user_id(
    cfg: &Config,
    key: &ClientKey,
    body: &Value,
    headers: &HeaderMap,
) -> String {
    if !key.kind.is_private() {
        return user_id_of(body, headers);
    }
    match cfg.security.private_user_id.trim() {
        crate::config::PRIVATE_ID_SECRET => key.key.clone(),
        crate::config::PRIVATE_ID_KEY_ID => key.id.clone(),
        // Anything unrecognised is the safe one. `normalize` already rewrites a
        // misspelling in the config, so this is only reachable in a test that
        // built a `Config` by hand.
        _ => crate::util::fingerprint(&key.key),
    }
}

/// Who the caller says they are.
///
/// Requirement 22: this is the prompt-cache isolation key. OpenAI clients put
/// it in the body's `user` field; everything else sends a header. Whichever
/// arrives, the same string is recorded, logged and passed upstream, so two
/// callers behind one API key never share a cache entry.
pub fn user_id_of(body: &Value, headers: &HeaderMap) -> String {
    let from_body = body
        .get("user")
        .or_else(|| body.get("user_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(id) = from_body {
        return id.to_string();
    }
    for name in ["x-user-id", "x-user", "x-openai-user", "x-kv-user"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return value.to_string();
        }
    }
    String::new()
}

/// The request body's size on the wire.
///
/// Taken from the header rather than by re-serialising the parsed body: the
/// bytes are already gone by the time a handler runs, and re-encoding a 20 MB
/// conversation to measure it would cost more than everything else here.
fn body_bytes(headers: &HeaderMap) -> u64 {
    headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

pub fn check_access(key: &ClientKey, model_id: &str) -> Result<(), String> {
    if key.models.iter().any(|m| m == "*" || m == model_id) {
        return Ok(());
    }
    Err(format!(
        "key \"{}\" may not use model \"{model_id}\"",
        if key.label.is_empty() {
            &key.id
        } else {
            &key.label
        }
    ))
}

/* ------------------------------------------------------------- context -- */

struct Ctx {
    state: Arc<AppState>,
    cfg: Arc<Config>,
    route: Model,
    resolved: Resolved,
    transform: ResolvedResponseTransform,
    /// The envelope every reply is rebuilt into: the relay's own uuid, its own
    /// timestamp, its own model name.
    identity: Identity,
    record: RequestRecord,
    started: Instant,
    client_wants_stream: bool,
    /// The relay's own count of the caller's own body. What they are charged.
    user_local: u64,
    /// Whether the injected prefix comes off the backend's cache hit before any
    /// of it is credited to the caller.
    cache_credit: CacheCredit,
    /// This route's price list, global and per-model already merged.
    pricing: Pricing,
    effort: Effort,
    trace: Trace,
    /// What this request's input row already put in the ledger, if one was
    /// written.
    ledgered: Option<Ledgered>,
}

impl Ctx {
    /// Price the request as it stands. Called once the token counts are final,
    /// with the answer in the model's own words — before any rewriting of ours,
    /// since a refusal is priced from what the model said.
    fn price(&self, usage: &Usage, answer: &str) -> Priced {
        pricing::price(
            &self.pricing,
            &Shape {
                model_id: self.route.id.clone(),
                input_tokens: self.user_local,
                cached_input_tokens: usage.cached_tokens,
                output_tokens: usage.completion_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                effort: Some(self.effort),
                hour: self.record.local_hour,
                weekday: self.record.local_weekday,
                streamed: self.client_wants_stream,
                refused: pricing::is_refusal(answer, &self.pricing.refusal_phrases),
            },
        )
    }

    /// What the relay pays for the same request, at the backend's own rates
    /// over the body the backend actually received.
    fn backend_price(&self, billed: &Usage) -> Priced {
        pricing::price(
            &self.pricing,
            &Shape {
                model_id: self.route.id.clone(),
                input_tokens: billed.prompt_tokens,
                cached_input_tokens: billed.cached_tokens,
                output_tokens: billed.completion_tokens,
                reasoning_tokens: billed.reasoning_tokens,
                effort: Some(self.effort),
                hour: self.record.local_hour,
                weekday: self.record.local_weekday,
                streamed: self.client_wants_stream,
                // Never on this side: whether the model refused is our business
                // with the caller, and no concern of the upstream invoice.
                refused: false,
            },
        )
    }

    /// The `usage` block the caller gets, cost included.
    ///
    /// The relay's own rate card is the bill when it is switched on. When it is
    /// not, the price the model is *published* at in `/v1/models` is — because
    /// a listing that quotes a rate and a reply that quotes no cost leave the
    /// caller doing arithmetic the relay already did. Only a model nobody has
    /// priced at all comes back without a cost, which is the honest answer
    /// rather than a zero that reads like free service.
    fn usage_json(&self, charged: &Usage, answer: &str) -> Value {
        let cost = if self.pricing.enabled {
            Some(self.price(charged, answer).proxy_usd)
        } else {
            crate::server::catalog::published_cost(&self.route, &self.cfg, charged, self.record.ts)
        };
        charged.public(cost)
    }
}

/// The input figures already on the books, so the closing row can settle
/// against them instead of counting the same tokens twice.
#[derive(Debug, Clone, Copy)]
struct Ledgered {
    input: i64,
    billed: i64,
}

/// Everything [`finish`] needs that is not already on the record.
///
/// A struct rather than ten positional arguments: at the call sites it is the
/// difference between reading `status: 502` and counting commas to work out
/// which `None` is which.
#[derive(Default)]
struct Outcome<'a> {
    status: i64,
    error: &'a str,
    started: Option<Instant>,
    first_token_at: Option<Instant>,
    last_token_at: Option<Instant>,
    /// What the backend charged for.
    billed: Option<&'a Usage>,
    /// What the caller is charged for.
    charged: Option<&'a Usage>,
    finish_reason: &'a str,
    response_preview: &'a str,
    /// Set when the ledger already carries this request's input row, so the
    /// closing row settles against it instead of counting the same tokens a
    /// second time.
    ledgered: Option<Ledgered>,
    /// What this request cost and what it sold for. Absent when the relay never
    /// got far enough to know.
    priced: Option<Priced>,
    /// The backend's half of the same sum.
    backend_priced: Option<Priced>,
    /// Bytes written back to the caller.
    bytes_out: u64,
    /// Bytes read from the backend.
    bytes_upstream: u64,
    trace: Option<&'a Trace>,
}

/* ------------------------------------------------------------ dispatch -- */

pub async fn handle_chat(
    state: Arc<AppState>,
    key: Arc<ClientKey>,
    ip: String,
    headers: &HeaderMap,
    endpoint: &str,
    body: Value,
) -> Response {
    let cfg = state.config.current();
    let started = Instant::now();
    let started_wall = now_ms();
    let tz = cfg.tz();
    // Requirement 5a: a uuid, minted here, carried by every log line this
    // request writes and by every byte of the reply it produces.
    let id = new_uuid_v4();
    let local = crate::util::local_parts(started_wall, &tz);
    let trace = Trace::new(state.logger.clone(), &id, cfg.logging.verbose_requests);
    let effort = pricing::effort_of(&body);
    let user_id = effective_user_id(&cfg, &key, &body, headers);

    let mut record = RequestRecord {
        id: id.clone(),
        ts: started_wall,
        day: day_key(started_wall, &tz),
        hour: hour_key(started_wall, &tz),
        local_hour: local.hour,
        local_weekday: local.weekday,
        user_id: truncate(&user_id, 120),
        reasoning_effort: effort.as_str().to_string(),
        bytes_in: body_bytes(headers),
        key_id: key.id.clone(),
        key_label: key.display_name().to_string(),
        key_kind: key.kind,
        ip,
        user_agent: truncate(
            headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            200,
        ),
        endpoint: endpoint.to_string(),
        public_model: body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        exact: 1,
        ..Default::default()
    };

    let asked_for = record.public_model.clone();
    trace.phase(
        "in",
        format!(
            "{} model={asked_for} key={} ({}) user={} effort={} stream={} bytes={} ip={}",
            local.stamp,
            record.key_label,
            key.kind.as_str(),
            if record.user_id.is_empty() {
                "-"
            } else {
                &record.user_id
            },
            effort.as_str(),
            body.get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            record.bytes_in,
            record.ip,
        ),
    );

    let Some(route) = cfg.find_model(&asked_for).filter(|m| m.enabled).cloned() else {
        let message = format!("model \"{asked_for}\" is not available on this relay");
        finish(
            &state,
            record,
            Outcome {
                status: 404,
                error: &message,
                trace: Some(&trace),
                ..Default::default()
            },
        );
        return error_response(
            404,
            &message,
            "invalid_request_error",
            Some("model_not_found"),
        );
    };

    if let Err(message) = check_access(&key, &route.id) {
        finish(
            &state,
            record,
            Outcome {
                status: 403,
                error: &message,
                trace: Some(&trace),
                ..Default::default()
            },
        );
        return error_response(
            403,
            &message,
            "invalid_request_error",
            Some("model_forbidden"),
        );
    }

    record.backend_id = route.backend.clone();
    record.upstream_model = route.upstream_model.clone();
    // A private key is not paced. The throttle exists to keep one reseller's
    // traffic from filling the phone's uplink on everybody else's behalf; a key
    // with one holder behind it is the case that costs nobody else anything, so
    // it gets whatever the backend can produce.
    record.target_tps = if key.kind.is_private() {
        0.0
    } else {
        route.max_tokens_per_second
    };

    // Take a slot before doing any real work. Tokenizing a long conversation
    // is the most expensive thing this relay does, so letting an unbounded
    // number of callers do it at once is exactly the pile-up the limit exists
    // to prevent.
    state.gate.resize(cfg.server.max_concurrent_requests);
    let ticket = match state
        .gate
        .admit(
            cfg.server.queue_capacity,
            std::time::Duration::from_millis(cfg.server.queue_timeout_ms),
        )
        .await
    {
        Admission::Admitted(ticket) => ticket,
        Admission::QueueFull { waiting, capacity } => {
            let message = format!(
                "relay is saturated: {waiting} request(s) already waiting for one of \
                 {} slots, and the queue holds {capacity}",
                cfg.server.max_concurrent_requests
            );
            finish(
                &state,
                record,
                Outcome {
                    status: 503,
                    error: &message,
                    trace: Some(&trace),
                    ..Default::default()
                },
            );
            return overloaded(&message, 1);
        }
        Admission::TimedOut { waited_ms } => {
            let message = format!(
                "waited {:.0}ms for a free slot and gave up; the relay is running {} at a time",
                waited_ms, cfg.server.max_concurrent_requests
            );
            record.queued_ms = round(waited_ms, 1);
            finish(
                &state,
                record,
                Outcome {
                    status: 503,
                    error: &message,
                    trace: Some(&trace),
                    ..Default::default()
                },
            );
            return overloaded(&message, 2);
        }
    };
    record.queued_ms = round(ticket.queued_ms, 1);

    // Requirements 2 and 3. The caller is counted on the body they sent, and
    // the relay's system prompt goes in after — so nobody is billed for text
    // they never wrote, whatever the transform does to the request on its way
    // out. Both bodies go to the counter together because the figures come out
    // of one pass over the thread pool, not two. The tokenizer is the
    // *backend* model's, because that is the one that bills.
    //
    // Requirement 21: which system prompt goes in depends on how hard the
    // caller asked the model to think, so the choice is made from their own
    // body before anything is written into it.
    let rt = RequestTransform::merged(&cfg.defaults.request_transform, &route.request_transform);
    let (prompt_spec, prompt_rule_id) = select_system_prompt(&route, effort);
    record.prompt_id = prompt_rule_id.clone();

    let injecting = Instant::now();
    let mut upstream_body = transform_request(&body, &route, &cfg, &rt, prompt_spec);
    record.inject_ms = round(injecting.elapsed().as_secs_f64() * 1000.0, 3);
    trace.timed(
        "inj",
        record.inject_ms,
        format!(
            "rule={} mode={}",
            if prompt_rule_id.is_empty() {
                "default"
            } else {
                &prompt_rule_id
            },
            prompt_spec.mode,
        ),
    );

    let resolved = state.counter.resolve(
        &cfg,
        &route.upstream_model,
        &route.tokenizer,
        &route.chat_profile,
    );
    let tokenizing = Instant::now();
    let input = state
        .counter
        .count_prompt(
            &body,
            &upstream_body,
            &resolved,
            &cfg.tokenizer.image_defaults,
        )
        .await;
    record.tokenize_ms = round(tokenizing.elapsed().as_secs_f64() * 1000.0, 3);

    // Charging for the injected prompt means treating the whole body as the
    // caller's.
    let (user_local, cache_credit) = if cfg.tokenizer.bill_system_prompt_to_user {
        // They are charged for the injected prompt, so a cache hit over it is
        // a discount on tokens they are paying for and travels whole.
        (input.billed as u64, CacheCredit::WholeBody)
    } else {
        (input.user as u64, CacheCredit::CallersBodyOnly)
    };
    trace.timed(
        "tok",
        record.tokenize_ms,
        format!(
            "{} caller / {} upstream  {} {}",
            crate::relay::trace::grouped(user_local as i64),
            crate::relay::trace::grouped(input.billed as i64),
            input.tokenizer,
            if input.exact { "exact" } else { "estimated" },
        ),
    );
    record.local_prompt = input.billed as i64;
    record.billed_prompt_tokens = input.billed as i64;
    record.user_prompt_tokens = user_local as i64;
    record.system_prompt_tokens = input.injected;
    record.tokenizer = input.tokenizer.clone();
    record.exact = i64::from(input.exact);

    // The model's own context window is a fact about the backend, so it is
    // checked against what the backend will actually receive.
    let max_in = route.limits.max_input_tokens;
    if max_in > 0 && input.billed > max_in as usize {
        let message = format!(
            "prompt is {} tokens, over this model's {max_in} token limit",
            input.billed
        );
        finish(
            &state,
            record,
            Outcome {
                status: 413,
                error: &message,
                trace: Some(&trace),
                ..Default::default()
            },
        );
        return error_response(
            413,
            &message,
            "invalid_request_error",
            Some("context_length_exceeded"),
        );
    }

    // Requirement 22: what is kept is what the *caller* sent. Previewing the
    // upstream body instead would file the relay's own system prompt under the
    // caller's words, which is both misleading and a way to leak it.
    if cfg.logging.store_bodies != "none" {
        record.req_preview = preview_request(&body, &cfg);
    }

    let client_wants_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Streaming upstream is what makes TTFT and tokens/sec measurable; when the
    // caller asked for a whole response we buffer the stream back together.
    let stream_upstream = rt.force_stream.unwrap_or(client_wants_stream);
    record.stream = i64::from(client_wants_stream);

    let backend_cfg = cfg.find_backend(&route.backend).cloned();

    if let Some(map) = upstream_body.as_object_mut() {
        // Requirement 22: the caller's own id goes upstream, because a backend
        // that keys its prompt cache by user needs it to keep one caller's
        // cache out of another's. It is the one thing about the caller that
        // does travel, and only when the backend is set up to want it.
        //
        // Under two names, because the backends disagree on one: OpenAI's
        // `user`, and whatever `userIdField` says — `user_id` unless an
        // operator changed it. A backend reading only its own spelling would
        // otherwise pool every caller behind this relay into one cache, which
        // is the exact leak the id exists to prevent.
        //
        // For a private key `user_id` is the key's own identity rather than
        // anything the caller wrote, so writing it here is also what stops a
        // caller naming themselves as somebody else upstream.
        let forward = backend_cfg.as_ref().is_none_or(|b| b.forward_user_id);
        let field = backend_cfg
            .as_ref()
            .map_or("user_id", |b| b.user_id_field.trim());
        if forward && !user_id.is_empty() {
            map.insert("user".into(), Value::String(user_id.clone()));
            if !field.is_empty() && field != "user" {
                map.insert(field.to_string(), Value::String(user_id.clone()));
            }
        } else {
            // Either the backend does not want an id, or what the caller sent
            // was not one — whitespace, a number, an empty string. Neither is
            // worth forwarding, and leaving the caller's own value in place
            // would forward exactly the thing that failed to parse as an id.
            map.remove("user");
            if !field.is_empty() {
                map.remove(field);
            }
        }
        map.insert("stream".into(), Value::Bool(stream_upstream));
        if stream_upstream {
            let wants_usage = backend_cfg
                .as_ref()
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

    // Requirement 4: the request goes to the backend.
    let sent = state
        .upstream
        .send_with_fallback(
            &cfg,
            &route,
            endpoint,
            &upstream_body,
            stream_upstream,
            &user_id,
        )
        .await;

    let sent = match sent {
        Ok(sent) => sent,
        Err(err) => {
            // Never reached the backend, so nothing was consumed there; the
            // closing ledger row accounts for the whole request on its own.
            record.retries = i64::from(err.attempts.saturating_sub(1));
            finish(
                &state,
                record,
                Outcome {
                    status: err.status as i64,
                    error: &err.message,
                    started: Some(started),
                    trace: Some(&trace),
                    ..Default::default()
                },
            );
            drop(ticket);
            // The detail above is on the record and in the log; the caller gets
            // an answer that says nothing about what sits behind this relay.
            return crate::relay::upstream_failure(err.status);
        }
    };

    record.backend_id = sent.backend().id.clone();
    record.retries = i64::from(sent.attempts().saturating_sub(1));

    // Requirement 5: the input is on the books the moment the backend has it,
    // not when the answer comes back. A stream that dies halfway, a caller who
    // hangs up, a phone that loses power — the tokens were still spent, and
    // this row is what remembers that.
    let ledgered = Some(ledger_input(&state, &record));

    let response = match sent {
        Sent::Failed { status, body, .. } => {
            finish(
                &state,
                record,
                Outcome {
                    status: i64::from(status),
                    error: &truncate(&body, 500),
                    started: Some(started),
                    ledgered,
                    trace: Some(&trace),
                    ..Default::default()
                },
            );
            crate::relay::upstream_failure(status)
        }
        Sent::Ok { response, .. } => {
            let mut transform = ResponseTransform::merged(
                &cfg.defaults.response_transform,
                &route.response_transform,
            );
            // OpenRouter's two spellings of "answer, but do not show me the
            // working". A caller who asked for that gets it for this request
            // only; the model's own setting is untouched. It can only ever
            // remove the trace, never turn one on that the route keeps off.
            if reasoning_excluded(&body) {
                transform.reasoning = "strip".into();
            }
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
                identity: Identity {
                    id: id.clone(),
                    created: started_wall / 1000,
                    model: route.id.clone(),
                    // The relay's own name. The backend that ran the prompt is
                    // never what goes in here.
                    provider: cfg.openrouter.provider_slug.clone(),
                },
                record,
                started,
                client_wants_stream,
                user_local,
                cache_credit,
                pricing: pricing::resolve(&cfg.pricing, &route),
                effort,
                trace: trace.clone(),
                ledgered,
            };

            if stream_upstream && is_sse {
                return pipe_stream(response, ctx, ticket).await;
            }
            let out = pipe_buffered(response, ctx).await;
            drop(ticket);
            return out;
        }
    };

    drop(ticket);
    response
}

/// A 503 the caller can act on: it says how long to wait before trying again.
fn overloaded(message: &str, retry_after_s: u32) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("retry-after", retry_after_s.to_string())],
        axum::Json(serde_json::json!({
            "error": {"message": message, "type": "server_error", "code": "overloaded"}
        })),
    )
        .into_response()
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
    /// Bytes read off the backend's socket, and bytes written to the caller's.
    bytes_upstream: u64,
    bytes_out: u64,
}

/// Send one frame, counting what went out.
async fn emit_frame(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    out: &mut Pumped,
    frame: Bytes,
) -> bool {
    out.bytes_out += frame.len() as u64;
    if tx.send(Ok(frame)).await.is_err() {
        out.disconnected = true;
        return false;
    }
    true
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

    // Requirement 6: the reply leaves at the speed the route asks for, not at
    // whatever speed the backend managed. Reading is what gets held back, so
    // the pause travels back up the TCP window instead of piling tokens up in
    // this process's memory.
    let mut pacer = Pacer::new(ctx.record.target_tps);

    // Requirement 17: the relay's own keep-alive, never the backend's. An SSE
    // comment holds the connection open through cloudflared and every NAT on
    // the way without saying anything about what is generating the answer.
    let keepalive = Duration::from_millis(ctx.cfg.server.sse_keepalive_ms);
    let keepalive_frame = sse::comment(&ctx.cfg.server.sse_keepalive_text);

    let mut stream = response.bytes_stream();
    // A streaming decoder, so a multi-byte character split across two network
    // chunks is reassembled rather than turning into replacement characters.
    let mut decoder = Utf8Decoder::default();

    'outer: loop {
        let next = match (emit, keepalive.is_zero()) {
            // Nothing to hold open, or nothing to hold it open with.
            (None, _) | (_, true) => stream.next().await,
            (Some(tx), false) => loop {
                match tokio::time::timeout(keepalive, stream.next()).await {
                    Ok(item) => break item,
                    Err(_) => {
                        // The backend has gone quiet — a long reasoning pass,
                        // usually. Say so in our own words and keep waiting.
                        if !emit_frame(tx, &mut out, keepalive_frame.clone()).await {
                            break 'outer;
                        }
                    }
                }
            },
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(c) => c,
            Err(err) => {
                out.error = Some(err.to_string());
                break;
            }
        };
        out.bytes_upstream += chunk.len() as u64;

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

            if let Some(calls) = delta
                .and_then(|d| d.get("tool_calls"))
                .and_then(|v| v.as_array())
            {
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

            let mut shaped =
                transform_chunk(&parsed, &ctx.identity, &ctx.transform, &mut reasoning_state);

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
                pacer.hold(&safe).await;
                set_delta_content(&mut shaped, &safe);
            }

            if !emit_frame(tx, &mut out, sse::format_sse(&shaped)).await {
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
                let chunk = delta_chunk(&ctx.identity, &tail);
                emit_frame(tx, &mut out, sse::format_sse(&chunk)).await;
            }
        }
    }
    out
}

/// Stream to the caller. The pump runs in its own task so the HTTP response can
/// be returned immediately and the body produced as it arrives.
async fn pipe_stream(response: reqwest::Response, ctx: Ctx, ticket: Ticket) -> Response {
    if !ctx.client_wants_stream {
        // We streamed upstream only to measure TTFT; the caller wants JSON.
        let out = pipe_streamed_into_json(response, ctx).await;
        drop(ticket);
        return out;
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let request_id = ctx.record.id.clone();

    tokio::spawn(async move {
        let mut pumped = pump(response, &ctx, Some(&tx)).await;
        let billed = finalise_usage(&ctx, &pumped, true).await;
        let charged = billed.charged_to_caller(ctx.user_local, ctx.cache_credit);

        if !pumped.disconnected {
            // Requirements 16 and 18: the closing frame carries the relay's own
            // input count and what the request came to under the relay's own
            // price list — never the backend's usage block, which stopped at
            // the parser.
            if charged.prompt_tokens > 0 || charged.completion_tokens > 0 {
                let chunk = usage_chunk(&ctx.identity, &ctx.usage_json(&charged, &pumped.text));
                emit_frame(&tx, &mut pumped, sse::format_sse(&chunk)).await;
            }
            if let Some(err) = &pumped.error {
                let chunk = serde_json::json!({
                    "error": {"message": err, "type": "upstream_error"}
                });
                emit_frame(&tx, &mut pumped, sse::format_sse(&chunk)).await;
            }
            emit_frame(&tx, &mut pumped, Bytes::from_static(sse::DONE.as_bytes())).await;
        }

        record_stream_outcome(&ctx, &pumped, &billed, &charged);
        drop(ticket);
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
    let mut pumped = pump(response, &ctx, None).await;
    let billed = finalise_usage(&ctx, &pumped, false).await;
    let charged = billed.charged_to_caller(ctx.user_local, ctx.cache_credit);

    let rules = compile_text_rules(&ctx.transform.replace);
    let body_text = match &rules {
        Some(r) => r.apply(&pumped.text),
        None => pumped.text.clone(),
    };
    let content = format!(
        "{}{body_text}{}",
        ctx.transform.prefix, ctx.transform.suffix
    );

    let assembled = assemble_completion(
        &ctx.identity,
        &content,
        if ctx.transform.reasoning == "strip" {
            ""
        } else {
            &pumped.reasoning
        },
        &pumped.tool_calls,
        if pumped.finish_reason.is_empty() {
            "stop"
        } else {
            &pumped.finish_reason
        },
        // Priced on what the model itself said, not on our rewrite of it.
        &ctx.usage_json(&charged, &pumped.text),
    );

    let body = serde_json::to_vec(&assembled).unwrap_or_default();
    pumped.bytes_out = body.len() as u64;
    record_stream_outcome(&ctx, &pumped, &billed, &charged);
    json_with_id(&ctx.identity.id, assembled)
}

/// The path where the backend answered in one piece.
async fn pipe_buffered(response: reqwest::Response, ctx: Ctx) -> Response {
    let raw = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            let message = format!("backend closed the connection: {err}");
            finish(
                &ctx.state,
                ctx.record.clone(),
                Outcome {
                    status: 502,
                    error: &message,
                    started: Some(ctx.started),
                    ledgered: ctx.ledgered,
                    trace: Some(&ctx.trace),
                    ..Default::default()
                },
            );
            return error_response(502, &message, "upstream_error", None);
        }
    };
    let bytes_upstream = raw.len() as u64;
    let payload: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(err) => {
            let message = format!("backend sent a response the relay could not parse: {err}");
            finish(
                &ctx.state,
                ctx.record.clone(),
                Outcome {
                    status: 502,
                    error: &message,
                    started: Some(ctx.started),
                    ledgered: ctx.ledgered,
                    bytes_upstream,
                    trace: Some(&ctx.trace),
                    ..Default::default()
                },
            );
            return error_response(502, &message, "upstream_error", None);
        }
    };

    // What the model itself said, before any rewriting of ours. A refusal is
    // recognised by the model's own words: a replace rule that renames the
    // backend must not be able to hide one, or to invent one.
    let spoken = flatten_content(
        payload
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| {
                c.get("message")
                    .and_then(|m| m.get("content"))
                    .or_else(|| c.get("text"))
            })
            .unwrap_or(&Value::Null),
    );

    let mut shaped = transform_response(&payload, &ctx.identity, &ctx.transform);
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
            if ctx.transform.reasoning == "strip" {
                ""
            } else {
                &reasoning
            },
            tool_calls,
        )
        .await;

    let billed = reconcile_usage(
        LocalCount {
            prompt: ctx.record.local_prompt as u64,
            completion: completion as u64,
            exact: exact && ctx.record.exact == 1,
        },
        payload.get("usage"),
        ctx.cfg.tokenizer.prefer_upstream_usage,
    );
    let charged = billed.charged_to_caller(ctx.user_local, ctx.cache_credit);

    if let Some(map) = shaped.as_object_mut() {
        map.insert("usage".into(), ctx.usage_json(&charged, &spoken));
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
    let usage_json = ctx.usage_json(&charged, &spoken);
    let out = if ctx.client_wants_stream {
        // The backend could not stream, so replay the finished answer as SSE.
        replay_as_sse(&ctx, &content, &usage_json)
    } else {
        json_with_id(&ctx.identity.id, shaped.clone())
    };

    finish(
        &ctx.state,
        ctx.record.clone(),
        Outcome {
            status: 200,
            error: "",
            first_token_at: None,
            started: Some(ctx.started),
            last_token_at: Some(finished),
            billed: Some(&billed),
            charged: Some(&charged),
            finish_reason: &finish_reason,
            response_preview: &preview,
            ledgered: ctx.ledgered,
            priced: Some(ctx.price(&charged, &spoken)),
            backend_priced: Some(ctx.backend_price(&billed)),
            bytes_out: serde_json::to_vec(&shaped)
                .map(|b| b.len() as u64)
                .unwrap_or(0),
            bytes_upstream,
            trace: Some(&ctx.trace),
        },
    );
    out
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
            if ctx.transform.reasoning == "strip" {
                ""
            } else {
                &pumped.reasoning
            },
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

fn record_stream_outcome(ctx: &Ctx, pumped: &Pumped, billed: &Usage, charged: &Usage) {
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
        Outcome {
            status,
            error: &error,
            started: Some(ctx.started),
            first_token_at: pumped.first_token_at,
            last_token_at: pumped.last_token_at,
            billed: Some(billed),
            charged: Some(charged),
            finish_reason: &pumped.finish_reason,
            response_preview: &preview,
            ledgered: ctx.ledgered,
            priced: Some(ctx.price(charged, &pumped.text)),
            backend_priced: Some(ctx.backend_price(billed)),
            bytes_out: pumped.bytes_out,
            bytes_upstream: pumped.bytes_upstream,
            trace: Some(&ctx.trace),
        },
    );
}

/// Compute the derived metrics, then write both records: the detail row the
/// dashboard browses, and the closing row of the immutable ledger.
fn finish(state: &Arc<AppState>, mut record: RequestRecord, out: Outcome<'_>) {
    let end = Instant::now();
    let total_ms = out
        .started
        .map_or(0.0, |s| end.duration_since(s).as_secs_f64() * 1000.0);
    let ttft_ms = match (out.started, out.first_token_at) {
        (Some(s), Some(f)) => f.duration_since(s).as_secs_f64() * 1000.0,
        _ => 0.0,
    };
    let gen_ms = match (out.first_token_at, out.last_token_at) {
        (Some(f), Some(l)) if l > f => l.duration_since(f).as_secs_f64() * 1000.0,
        _ => 0.0,
    };

    let completion = out.billed.map_or(0, |u| u.completion_tokens);
    record.status = out.status;
    record.error = truncate(out.error, 800);
    record.finish_reason = out.finish_reason.to_string();
    record.total_ms = round(total_ms, 1);
    record.ttft_ms = round(ttft_ms, 1);
    record.gen_ms = round(gen_ms, 1);

    // `prompt_tokens` is the caller's number, and the only one they ever see.
    // What the backend charged is kept beside it rather than in place of it.
    record.billed_prompt_tokens = out
        .billed
        .map_or(record.billed_prompt_tokens, |u| u.prompt_tokens as i64);
    record.user_prompt_tokens = out
        .charged
        .map_or(record.user_prompt_tokens, |u| u.prompt_tokens as i64);
    record.prompt_tokens = record.user_prompt_tokens;
    // `system_prompt_tokens` is deliberately left as the relay's own local
    // measurement of what it injected, rather than restated as billed minus
    // user. It answers "what is my system prompt costing me", which stays a
    // real question even when the caller is billed for it — and it keeps
    // meaning the same thing whether or not the backend reports usage of its
    // own. The overhead actually absorbed is billed minus user, which the
    // dashboard computes from those two.
    record.completion_tokens = completion as i64;
    record.total_tokens = record.prompt_tokens + record.completion_tokens;
    // On the caller's basis, like `prompt_tokens` right above it, so the cache
    // rate the two make is a percentage rather than a number in the thousands:
    // the backend's hit covers the injected prefix the caller never sent.
    record.billed_cached_tokens = out.billed.map_or(0, |u| u.cached_tokens as i64);
    record.cached_tokens = out
        .charged
        .map_or(record.cached_tokens, |u| u.cached_tokens as i64);
    // The hit is still a hit whoever it is credited to: this stays true of the
    // request, so a cache that only ever matches the injected prompt is
    // countable rather than invisible.
    record.cache_hit = i64::from(record.billed_cached_tokens > 0);
    record.reasoning_tokens = out.billed.map_or(0, |u| u.reasoning_tokens as i64);
    record.usage_source = out.billed.map_or(String::new(), |u| u.source.to_string());
    record.exact = out.billed.map_or(record.exact, |u| i64::from(u.exact));
    record.local_prompt = out
        .billed
        .map_or(record.local_prompt, |u| u.local_prompt as i64);
    record.local_completion = out.billed.map_or(0, |u| u.local_completion as i64);
    record.drift_prompt = out.billed.map_or(0, |u| u.drift_prompt);
    record.drift_completion = out.billed.map_or(0, |u| u.drift_completion);
    // Throughput over the generation phase only, which is the number that
    // actually describes how fast the model produced tokens.
    record.tokens_per_sec = if gen_ms > 0.0 && completion > 0 {
        round(completion as f64 / gen_ms * 1000.0, 2)
    } else {
        0.0
    };
    record.res_preview = out.response_preview.to_string();
    record.bytes_out = out.bytes_out;
    record.bytes_upstream = out.bytes_upstream;
    record.rss_mb = crate::relay::trace::rss_mb();
    if let Some(priced) = &out.priced {
        record.proxy_usd = priced.proxy_usd;
        record.price_tiers = priced.tiers.join(", ");
    }
    // The relay's own cost is the backend's rates over the body the backend
    // actually saw, which is a different token count to the one the caller is
    // charged on — that gap is exactly what profit measures.
    if let Some(backend) = &out.backend_priced {
        record.backend_usd = backend.backend_usd;
    }
    record.profit_usd = round(record.proxy_usd - record.backend_usd, 9);

    let tag = if out.status >= 400 || out.status == 0 {
        "error"
    } else {
        "ok"
    };

    // Requirement 5f: one line per request with everything on it, so a phone
    // screen scrolling past shows what a request cost without anyone having to
    // join two logs together.
    match out.trace.filter(|t| t.enabled()) {
        Some(trace) => {
            if out.first_token_at.is_some() {
                trace.timed("ttft", record.ttft_ms, "");
            }
            trace.timed(
                "done",
                record.total_ms,
                format!(
                    "status={} {}{}",
                    record.status,
                    if record.finish_reason.is_empty() {
                        "-"
                    } else {
                        &record.finish_reason
                    },
                    if out.error.is_empty() {
                        String::new()
                    } else {
                        format!(" err={}", truncate(out.error, 160))
                    },
                ),
            );
            trace.phase("sum", summary_line(&record));
        }
        None => state.logger.info(format!(
            "{tag} {} -> {} {}in/{}out ttft={}ms total={}ms tps={} key={}{}",
            record.public_model,
            record.upstream_model,
            record.prompt_tokens,
            record.completion_tokens,
            record.ttft_ms,
            record.total_ms,
            record.tokens_per_sec,
            record.key_label,
            if out.error.is_empty() {
                String::new()
            } else {
                format!(" err={}", truncate(out.error, 160))
            },
        )),
    }

    state.quotas.record(
        &record.key_id,
        &record.day,
        record.total_tokens.max(0) as u64,
    );

    ledger_final(state, &record, out.ledgered);

    if !state.store.insert(record) {
        state
            .logger
            .warn("metrics queue is full; dropped a request row rather than blocking the relay");
    }
}

/// Everything one request did, on one line.
///
/// Written in the order the questions get asked: who, how much memory and
/// network it took, what it counted, what it cost, and how fast it went.
fn summary_line(r: &RequestRecord) -> String {
    use crate::relay::trace::grouped;
    let cache_rate = if r.prompt_tokens > 0 {
        r.cached_tokens as f64 / r.prompt_tokens as f64 * 100.0
    } else {
        0.0
    };
    format!(
        "uid={} model={} key={} user={} effort={} | ram={:.1}MB net={}B in/{}B out/{}B up | \
         tok={} in ({} cached, {:.0}% hit) {} out ({} reasoning) | \
         backend=${:.6} proxy=${:.6} profit=${:.6}{} | \
         latency={}ms ttft={}ms tps={}{}",
        r.id,
        r.public_model,
        if r.key_label.is_empty() {
            "-"
        } else {
            &r.key_label
        },
        if r.user_id.is_empty() {
            "-"
        } else {
            &r.user_id
        },
        if r.reasoning_effort.is_empty() {
            "-"
        } else {
            &r.reasoning_effort
        },
        r.rss_mb,
        grouped(r.bytes_in as i64),
        grouped(r.bytes_out as i64),
        grouped(r.bytes_upstream as i64),
        grouped(r.prompt_tokens),
        grouped(r.cached_tokens),
        cache_rate,
        grouped(r.completion_tokens),
        grouped(r.reasoning_tokens),
        r.backend_usd,
        r.proxy_usd,
        r.profit_usd,
        if r.price_tiers.is_empty() {
            String::new()
        } else {
            format!(" [{}]", r.price_tiers)
        },
        r.total_ms,
        r.ttft_ms,
        r.tokens_per_sec,
        if r.target_tps > 0.0 {
            format!(" (held to {})", r.target_tps)
        } else {
            String::new()
        },
    )
}

/* -------------------------------------------------------------- ledger -- */

/// The shared half of both ledger rows: who asked, for what, and when.
fn ledger_stub(record: &RequestRecord, phase: Phase) -> LedgerEntry {
    LedgerEntry {
        request_id: record.id.clone(),
        phase,
        ts: record.ts,
        day: record.day.clone(),
        hour: record.hour.clone(),
        key_id: record.key_id.clone(),
        public_model: record.public_model.clone(),
        backend_id: record.backend_id.clone(),
        queued_ms: record.queued_ms,
        user_id: record.user_id.clone(),
        key_kind: record.key_kind.as_str().to_string(),
        ..Default::default()
    }
}

fn append(state: &Arc<AppState>, entry: LedgerEntry) {
    if !state.store.ledger(entry) {
        // Unlike a detail row, this one is the accounting record, so losing it
        // is worth saying loudly. It still must not block the request.
        state.logger.error(
            "usage ledger queue is full; a usage row was dropped — the relay is writing to \
             disk more slowly than requests are arriving",
        );
    }
}

/// Record the input the moment the backend has it, and report what went down
/// so the closing row can settle against it.
fn ledger_input(state: &Arc<AppState>, record: &RequestRecord) -> Ledgered {
    let booked = Ledgered {
        input: record.user_prompt_tokens,
        billed: record.billed_prompt_tokens,
    };
    append(
        state,
        LedgerEntry {
            requests: 1,
            input_tokens: booked.input,
            billed_input_tokens: booked.billed,
            ..ledger_stub(record, Phase::Input)
        },
    );
    booked
}

/// Close the request out.
///
/// When no input row was written — a request refused before it ever reached a
/// backend — the input figures appear here in full. Otherwise they are already
/// on the books, and this row carries only what settlement changed.
///
/// It usually changes nothing. The input row goes down before the backend has
/// said anything, so it carries the relay's own count; if the backend then
/// reports a prompt so much cheaper that the caller's share of it comes to
/// less than they were first booked for, the caller is charged the lower
/// figure and the difference belongs on the books too. Ledger rows are never
/// rewritten, so it is posted as a correction: negative, on its own row, and
/// the sum still comes out right.
fn ledger_final(state: &Arc<AppState>, record: &RequestRecord, ledgered: Option<Ledgered>) {
    let (requests, input, billed) = match ledgered {
        Some(b) => (
            0,
            record.user_prompt_tokens - b.input,
            record.billed_prompt_tokens - b.billed,
        ),
        None => (1, record.user_prompt_tokens, record.billed_prompt_tokens),
    };
    append(
        state,
        LedgerEntry {
            status: record.status,
            requests,
            input_tokens: input,
            billed_input_tokens: billed,
            output_tokens: record.completion_tokens,
            cached_tokens: record.cached_tokens,
            reasoning_tokens: record.reasoning_tokens,
            cache_hit: record.cache_hit,
            ttft_ms: record.ttft_ms,
            gen_ms: record.gen_ms,
            total_ms: record.total_ms,
            tokens_per_sec: record.tokens_per_sec,
            // The money lands here and nowhere else. Until the answer is
            // complete there is no price: the output tokens are half of it.
            // This is also the row an invoice bills from, so a request that
            // straddles an invoice is billed on the next one — correctly,
            // since nothing has been charged for it yet.
            proxy_usd: record.proxy_usd,
            backend_usd: record.backend_usd,
            ..ledger_stub(record, Phase::Final)
        },
    );
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

fn delta_chunk(identity: &Identity, content: &str) -> Value {
    let mut out = identity.envelope("chat.completion.chunk");
    out.insert(
        "choices".into(),
        serde_json::json!([{
            "index": 0,
            "delta": {"content": content},
            "finish_reason": null,
            "native_finish_reason": null,
        }]),
    );
    Value::Object(out)
}

fn usage_chunk(identity: &Identity, usage: &Value) -> Value {
    let mut out = identity.envelope("chat.completion.chunk");
    out.insert("choices".into(), Value::Array(Vec::new()));
    out.insert("usage".into(), usage.clone());
    Value::Object(out)
}

fn assemble_completion(
    identity: &Identity,
    content: &str,
    reasoning: &str,
    tool_calls: &[Value],
    finish_reason: &str,
    usage: &Value,
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
    let mut out = identity.envelope("chat.completion");
    out.insert(
        "choices".into(),
        serde_json::json!([{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
            "native_finish_reason": finish_reason,
        }]),
    );
    out.insert("usage".into(), usage.clone());
    Value::Object(out)
}

/// Replay a finished answer as SSE for callers that insisted on streaming.
fn replay_as_sse(ctx: &Ctx, content: &str, usage: &Value) -> Response {
    let frame = |choices: Value| {
        let mut out = ctx.identity.envelope("chat.completion.chunk");
        out.insert("choices".into(), choices);
        Value::Object(out)
    };
    let mut body = Vec::new();

    body.extend_from_slice(&sse::format_sse(&frame(serde_json::json!([{
        "index": 0,
        "delta": {"role": "assistant", "content": ""},
        "finish_reason": null,
        "native_finish_reason": null,
    }]))));

    // Chunk on character boundaries, never bytes, or multi-byte text breaks.
    let chars: Vec<char> = content.chars().collect();
    for piece in chars.chunks(24) {
        let text: String = piece.iter().collect();
        body.extend_from_slice(&sse::format_sse(&frame(serde_json::json!([{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": null,
            "native_finish_reason": null,
        }]))));
    }

    let mut last = ctx.identity.envelope("chat.completion.chunk");
    last.insert(
        "choices".into(),
        serde_json::json!([{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
            "native_finish_reason": "stop",
        }]),
    );
    last.insert("usage".into(), usage.clone());
    body.extend_from_slice(&sse::format_sse(&Value::Object(last)));
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

        let listed = crate::server::catalog::document(&cfg);
        let text = listed.to_string();
        assert!(text.contains("manukmiberai/creative-writer"));
        assert!(
            !text.contains("Deepseek-v4-flash-0731"),
            "backend name leaked"
        );
        assert!(
            !text.contains("hidden"),
            "a disabled model must not be listed"
        );
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

        let keys = KeyIndex::build(&cfg.keys);
        assert!(matches!(
            authenticate(&cfg, &keys, Some("sk-good")),
            Auth::Ok(_)
        ));
        assert!(matches!(
            authenticate(&cfg, &keys, None),
            Auth::Denied { status: 401, .. }
        ));
        assert!(matches!(
            authenticate(&cfg, &keys, Some("sk-wrong")),
            Auth::Denied { status: 401, .. }
        ));
        assert!(matches!(
            authenticate(&cfg, &keys, Some("sk-off")),
            Auth::Denied { status: 403, .. }
        ));

        // With the requirement switched off, anyone gets in as "anonymous".
        cfg.security.require_client_key = false;
        assert!(matches!(authenticate(&cfg, &keys, None), Auth::Ok(k) if k.id == "anonymous"));
    }

    /// The two kinds of key, which differ in exactly one thing: who the request
    /// is on behalf of.
    #[test]
    fn a_private_key_is_its_own_user_and_a_company_key_is_not() {
        let cfg = Config::default();
        let headers = HeaderMap::new();
        let body = serde_json::json!({ "user": "someone-elses-customer" });

        let company = ClientKey {
            id: "key_co".into(),
            key: "Kunci-Zeiko-company".into(),
            kind: crate::config::KeyKind::Company,
            ..Default::default()
        };
        assert_eq!(
            effective_user_id(&cfg, &company, &body, &headers),
            "someone-elses-customer",
            "a company key passes its own end user through",
        );

        let private = ClientKey {
            id: "key_pr".into(),
            key: "Kunci-Zeiko-private".into(),
            kind: crate::config::KeyKind::Private,
            ..Default::default()
        };
        let id = effective_user_id(&cfg, &private, &body, &headers);
        assert_ne!(
            id, "someone-elses-customer",
            "a private caller must not be able to name themselves as anyone else",
        );
        assert_eq!(
            id,
            crate::util::fingerprint(&private.key),
            "the default is the fingerprint of the key",
        );
        assert!(
            !id.contains(&private.key),
            "the key itself must never be what travels upstream",
        );

        // Stable across calls, and different per key: those are the two things
        // a prompt-cache isolation id has to be.
        assert_eq!(
            id,
            effective_user_id(&cfg, &private, &serde_json::json!({}), &headers)
        );
        assert_ne!(
            id,
            effective_user_id(
                &cfg,
                &ClientKey {
                    key: "Kunci-Zeiko-another".into(),
                    ..private.clone()
                },
                &body,
                &headers
            )
        );
    }

    /// An operator who genuinely needs the raw value can have it, but only by
    /// asking for it in as many words.
    #[test]
    fn the_private_id_mode_chooses_what_travels() {
        let private = ClientKey {
            id: "key_pr".into(),
            key: "Kunci-Zeiko-private".into(),
            kind: crate::config::KeyKind::Private,
            ..Default::default()
        };
        let body = serde_json::json!({});
        let headers = HeaderMap::new();

        let mut cfg = Config::default();
        cfg.security.private_user_id = crate::config::PRIVATE_ID_KEY_ID.into();
        assert_eq!(effective_user_id(&cfg, &private, &body, &headers), "key_pr");

        cfg.security.private_user_id = crate::config::PRIVATE_ID_SECRET.into();
        assert_eq!(
            effective_user_id(&cfg, &private, &body, &headers),
            private.key
        );

        // A misspelling falls back to the mode that gives nothing away, rather
        // than to whichever branch happens to be last.
        cfg.security.private_user_id = "whatever".into();
        assert_eq!(
            effective_user_id(&cfg, &private, &body, &headers),
            crate::util::fingerprint(&private.key)
        );
    }

    /// The index the request path actually authenticates against.
    #[test]
    fn the_key_index_finds_a_key_without_scanning_and_refuses_a_near_miss() {
        let keys = vec![
            ClientKey {
                id: "k1".into(),
                key: "Kunci-Zeiko-one".into(),
                ..Default::default()
            },
            ClientKey {
                id: "k2".into(),
                key: "Kunci-Zeiko-two".into(),
                ..Default::default()
            },
            // A key with no secret is not a key, and must not be reachable by
            // sending an empty bearer token.
            ClientKey {
                id: "k3".into(),
                key: String::new(),
                ..Default::default()
            },
        ];
        let index = KeyIndex::build(&keys);
        assert_eq!(index.len(), 2);
        assert_eq!(
            index.get("Kunci-Zeiko-one").map(|k| k.id.as_str()),
            Some("k1")
        );
        assert_eq!(
            index.get("Kunci-Zeiko-two").map(|k| k.id.as_str()),
            Some("k2")
        );
        assert!(index.get("Kunci-Zeiko-thr").is_none());
        assert!(
            index.get("Kunci-Zeiko-on").is_none(),
            "a prefix is not the key"
        );
        assert!(index.get("").is_none());
    }
}
