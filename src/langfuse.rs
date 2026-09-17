//! Ships one span per relayed request to Langfuse, over OTLP/HTTP.
//!
//! # Why OTLP and not `/api/public/ingestion`
//!
//! Langfuse's own OpenAPI document marks the batch ingestion endpoint
//! deprecated: on Langfuse Cloud, v4-only write mode begins 2026-11-16, and
//! from then on that endpoint **rejects trace and observation events** — it
//! keeps accepting scores and nothing else. Its replacement for anything that
//! is not an official SDK is the OTLP/HTTP trace endpoint,
//! `POST /api/public/otel/v1/traces`, which takes Basic auth with the same
//! project keys and accepts JSON-encoded protobuf (`Content-Type:
//! application/json`).
//!
//! # What makes it v4-ready
//!
//! Langfuse v4 is observations-first: there is no separately ingested trace
//! entity any more, only spans correlated by a trace id, and the queries the
//! UI and the v2 APIs run read *observations* rather than traces. Four things
//! follow from that, and this module owes all four.
//!
//! * **`x-langfuse-ingestion-version: 4` travels on every POST.** Without it
//!   Langfuse routes the batch down the legacy compatibility path, where a
//!   span can take up to fifteen minutes to appear on the v4 data model and
//!   on the v2 Observations and Metrics APIs. These traces are read while
//!   somebody is still looking at the request that made one, so that delay is
//!   the whole value of them gone. A Langfuse too old to know the header
//!   ignores it, so it is safe to send at a self-hosted host of any version.
//! * **Input and output sit on the observation, never on the trace.**
//!   `langfuse.trace.input` and `langfuse.trace.output` are deprecated in v4
//!   and kept only so that legacy trace-level LLM-as-a-judge evaluators keep
//!   running. The span sent here *is* the trace's root observation, so
//!   `langfuse.observation.input`/`output` on it already *are* the overall
//!   request and response — there is nothing for the deprecated pair to add,
//!   and adding them back would be starting a dependency on a compatibility
//!   shim rather than keeping one.
//! * **Every correlating attribute is on that span.** User, session, trace
//!   name, tags, release, version and environment are what v4 filters and
//!   aggregates observations by, and an attribute that exists only on a parent
//!   is invisible to those queries. One span per trace makes that free — but
//!   it is exactly why a *second* span added here would have to be given the
//!   same set rather than inherit it.
//! * **A span is exported once, after it has ended.** Langfuse v4 does not
//!   reliably deduplicate a span id it has already accepted; re-exporting one
//!   to correct it produces a second observation, inflating every count drawn
//!   from it. Everything a span will ever say is assembled in
//!   [`Langfuse::record`] and posted exactly once.
//!
//! That is also the cheaper of the two here. JSON protobuf means no `prost`,
//! no build-time code generation and no second HTTP client — `serde_json` and
//! the `reqwest` already in the tree are the whole dependency list, which on a
//! device that builds its own binary is worth as much as the correctness is.
//!
//! # What is sent, and what never is
//!
//! One span per request, carrying the four things this relay knows that the
//! backend's own logs do not:
//!
//! * **the caller's body** — what they actually asked for, before the relay
//!   touched it;
//! * **the injection** — the system prompt the relay put in front of it, and
//!   the body as the backend received it;
//! * **the answer** — after reshaping, which is what the caller saw;
//! * **the arithmetic** — tokens counted locally, tokens the backend charged,
//!   what it cost, what it sold for, and where the milliseconds went.
//!
//! Three things never travel, whatever the config says:
//!
//! * client keys and backend API keys — a key is not in a request body, and
//!   the one place one could reach a trace is a private key's upstream id when
//!   `privateUserId` is `secret`. That case is hashed here rather than sent.
//! * the relay's own `Authorization` headers, which are not part of a body.
//! * anything at all, when the queue is full — the relay drops the span and
//!   counts it. Telemetry never gets to slow a request down or fail one.
//!
//! # Shape of the work
//!
//! [`Langfuse::submit`] is a `try_send` on a bounded channel and nothing else:
//! no allocation of a request, no lock the relay path can contend on, no
//! `await`. A single background task batches what arrives, posts it, and backs
//! off when Langfuse is unreachable. The relay never learns whether a span
//! arrived; the dashboard does, through [`Langfuse::status`].

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::config::{Config, ConfigStore, LangfuseConfig};
use crate::logging::Logger;
use crate::store::RequestRecord;

/// Selects Langfuse's v4 ingestion path for the batch that carries it.
///
/// Without this header a directly exported OTLP span goes down the legacy
/// compatibility path and can be up to fifteen minutes late to the v4 data
/// model and the v2 Observations and Metrics APIs. A Langfuse that predates
/// the header ignores it like any other unknown request header, so it costs a
/// self-hosted deployment on an older version nothing.
const INGESTION_VERSION_HEADER: &str = "x-langfuse-ingestion-version";
const INGESTION_VERSION: &str = "4";

/// OTLP span kind: this relay is a client of the backend.
const SPAN_KIND_CLIENT: i64 = 3;
/// OTLP status codes.
const STATUS_UNSET: i64 = 0;
const STATUS_OK: i64 = 1;
const STATUS_ERROR: i64 = 2;

/// Everything one finished request contributes to a trace.
///
/// Built by the relay at the moment the request is recorded and handed over
/// whole, so the background task never reaches back into a `Ctx` that has
/// since been dropped.
pub struct Span {
    /// 32 hex characters. Derived from the request's own uuid, so the id in
    /// `relay.log` and the id in Langfuse are the same string.
    pub trace_id: String,
    pub span_id: String,
    /// The caller's own body, already trimmed to `maxFieldChars`.
    pub input: String,
    /// The system prompt the relay injected, if any.
    pub injected: String,
    /// The body as the backend received it.
    pub upstream_input: String,
    /// What the caller was sent back.
    pub output: String,
    /// The model's reasoning trace, when the route did not strip it.
    pub reasoning: String,
    /// A snapshot of the finished request row.
    pub record: Box<RequestRecord>,
    /// `session.id`. The caller's user id when there is one, else the key.
    pub session: String,
    /// `user.id`, already made safe to publish.
    pub user: String,
    pub environment: String,
    pub release: String,
    pub tags: Vec<String>,
}

/// The request side of a trace, while the request is still running.
///
/// Holds the bodies by `Arc` rather than by value: they are the one thing in a
/// request that can be megabytes, and a trace must never be the reason one is
/// copied. Nothing here is serialised until [`Langfuse::record`] decides a
/// span is actually going out.
#[derive(Clone)]
pub struct Traced {
    /// What the dice said for this request, assuming it succeeds.
    sampled: bool,
    body: Arc<Value>,
    upstream: Option<Arc<Value>>,
    injected: String,
}

impl Traced {
    /// The system prompt the relay put in front of the caller's words.
    pub fn set_injected(&mut self, text: &str) {
        self.injected = text.to_string();
    }

    /// The body as the backend received it.
    pub fn set_upstream(&mut self, body: Arc<Value>) {
        self.upstream = Some(body);
    }
}

/// A short, stable, one-way name for a value that must not travel as itself.
fn fingerprint(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let digest = crate::util::digest(value.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("anon-{hex}")
}

#[derive(Default)]
struct Stats {
    queued: AtomicU64,
    sent: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
    last_error: parking_lot::Mutex<String>,
    last_ok_at: AtomicI64,
}

/// The handle the relay holds.
pub struct Langfuse {
    tx: mpsc::Sender<Box<Span>>,
    stats: Arc<Stats>,
}

impl Langfuse {
    /// Start the exporter. Always starts: whether anything is exported is read
    /// from the live config on every request and every flush, so switching
    /// Langfuse on in the dashboard takes effect without a restart.
    pub fn start(config: Arc<ConfigStore>, logger: Arc<Logger>) -> Arc<Self> {
        let capacity = config.current().langfuse.queue_capacity.clamp(16, 100_000);
        let (tx, rx) = mpsc::channel::<Box<Span>>(capacity);
        let stats = Arc::new(Stats::default());
        let exporter = Exporter {
            config,
            logger,
            stats: stats.clone(),
            client: None,
        };
        tokio::spawn(exporter.run(rx));
        Arc::new(Self { tx, stats })
    }

    /// Open a trace for a request that is about to run, or decline to.
    ///
    /// `None` means this request will never produce a span, and the relay can
    /// stop carrying anything for it — which is the point of deciding here
    /// rather than at the end. What it returns holds `Arc`s of bodies the
    /// request already owns, so opening a trace allocates nothing and
    /// serialises nothing; a span that is never sent costs one `Arc` clone.
    ///
    /// The dice are rolled now rather than at the end because a sampled-out
    /// request must not pay to capture what it will then throw away. A request
    /// that fails is kept regardless, so when `captureErrors` is on the
    /// capture has to survive the roll.
    pub fn begin(&self, cfg: &Config, body: Arc<Value>) -> Option<Traced> {
        let lf = &cfg.langfuse;
        if !lf.ready() {
            return None;
        }
        let rate = lf.sample_rate;
        let sampled = if rate >= 1.0 {
            true
        } else if rate <= 0.0 {
            false
        } else {
            rand::random::<f64>() < rate
        };
        if !sampled && !lf.capture_errors {
            return None;
        }
        Some(Traced {
            sampled,
            body,
            upstream: None,
            injected: String::new(),
        })
    }

    /// Turn a finished request into a span and queue it, if it earned one.
    ///
    /// Everything expensive happens here and only here: the bodies are
    /// serialised, trimmed and redacted at the moment it is certain a span is
    /// going out, on a request that has already answered its caller.
    pub fn record(
        &self,
        cfg: &Config,
        traced: &Traced,
        record: &RequestRecord,
        answer: &str,
        reasoning: &str,
    ) {
        let lf = &cfg.langfuse;
        if !lf.ready() {
            return;
        }
        let failed = record.status >= 400 || record.status == 0;
        // A failure is the thing a trace viewer is opened to explain, so it is
        // never sampled away — only switched off outright.
        if !(traced.sampled || (failed && lf.capture_errors)) {
            return;
        }
        let limit = lf.max_field_chars;

        // A private key whose `privateUserId` is `secret` files its usage
        // under the client key itself. That is a working credential, and a
        // trace is a third party, so what travels is a fingerprint of it
        // rather than the thing.
        let user = if record.key_kind.is_private()
            && cfg.security.private_user_id == crate::config::PRIVATE_ID_SECRET
        {
            fingerprint(&record.user_id)
        } else {
            record.user_id.clone()
        };
        let session = if user.is_empty() {
            record.key_id.clone()
        } else {
            user.clone()
        };

        let mut tags = lf.tags.clone();
        if !record.key_kind.as_str().is_empty() {
            tags.push(format!("key:{}", record.key_kind.as_str()));
        }

        self.submit(Box::new(Span {
            trace_id: trace_id_from(&record.id),
            span_id: span_id_from(&record.id),
            input: if lf.capture_input {
                redact_body(&traced.body, limit)
            } else {
                String::new()
            },
            injected: if lf.capture_system_prompt {
                crate::util::truncate(&traced.injected, limit)
            } else {
                String::new()
            },
            upstream_input: match (&traced.upstream, lf.capture_system_prompt) {
                (Some(body), true) => redact_body(body, limit),
                _ => String::new(),
            },
            output: if lf.capture_output {
                crate::util::truncate(answer, limit)
            } else {
                String::new()
            },
            reasoning: if lf.capture_reasoning {
                crate::util::truncate(reasoning, limit)
            } else {
                String::new()
            },
            record: Box::new(record.clone()),
            session,
            user,
            environment: lf.environment.clone(),
            release: if lf.release.is_empty() {
                env!("CARGO_PKG_VERSION").to_string()
            } else {
                lf.release.clone()
            },
            tags,
        }));
    }

    /// Hand a span over. Never blocks, never fails a request.
    pub fn submit(&self, span: Box<Span>) {
        match self.tx.try_send(span) {
            Ok(()) => {
                self.stats.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn status(&self) -> Value {
        json!({
            "queued": self.stats.queued.load(Ordering::Relaxed),
            "sent": self.stats.sent.load(Ordering::Relaxed),
            "dropped": self.stats.dropped.load(Ordering::Relaxed),
            "failed": self.stats.failed.load(Ordering::Relaxed),
            "inFlight": self.tx.max_capacity() - self.tx.capacity(),
            "capacity": self.tx.max_capacity(),
            "lastError": self.stats.last_error.lock().clone(),
            "lastOkAt": self.stats.last_ok_at.load(Ordering::Relaxed),
        })
    }
}

/* ------------------------------------------------------------ exporter -- */

struct Exporter {
    config: Arc<ConfigStore>,
    logger: Arc<Logger>,
    stats: Arc<Stats>,
    /// Built on first use and kept: a TLS handshake per batch would cost more
    /// than the batch.
    client: Option<reqwest::Client>,
}

impl Exporter {
    async fn run(mut self, mut rx: mpsc::Receiver<Box<Span>>) {
        let mut batch: Vec<Box<Span>> = Vec::new();
        // Doubles on each failed post so an unreachable Langfuse costs one
        // request a minute rather than one per flush.
        let mut backoff = Duration::ZERO;

        loop {
            let cfg = self.config.current();
            let lf = cfg.langfuse.clone();
            let flush_every = Duration::from_millis(lf.flush_interval_ms.clamp(250, 300_000));
            let want = lf.batch_size.clamp(1, 512);

            let deadline = tokio::time::sleep(flush_every);
            tokio::pin!(deadline);

            // Fill until the batch is worth posting or the timer goes off.
            let closed = loop {
                tokio::select! {
                    received = rx.recv() => match received {
                        Some(span) => {
                            batch.push(span);
                            if batch.len() >= want {
                                break false;
                            }
                        }
                        // Every sender is gone: the relay is shutting down, so
                        // post what is left and stop.
                        None => break true,
                    },
                    _ = &mut deadline => break false,
                }
            };

            if batch.is_empty() {
                if closed {
                    return;
                }
                continue;
            }
            if !lf.ready() {
                // Switched off while these were queued. Dropping them is the
                // right answer: they were captured under a config that no
                // longer says to publish anything.
                self.stats
                    .dropped
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
                batch.clear();
                if closed {
                    return;
                }
                continue;
            }

            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
            }
            let count = batch.len() as u64;
            match self.post(&lf, &batch).await {
                Ok(()) => {
                    self.stats.sent.fetch_add(count, Ordering::Relaxed);
                    self.stats
                        .last_ok_at
                        .store(crate::util::now_ms(), Ordering::Relaxed);
                    self.stats.last_error.lock().clear();
                    backoff = Duration::ZERO;
                }
                Err(err) => {
                    self.stats.failed.fetch_add(count, Ordering::Relaxed);
                    let text = err.to_string();
                    self.logger
                        .warn(format!("langfuse: dropped {count} span(s): {text}"));
                    *self.stats.last_error.lock() = crate::util::truncate(&text, 300);
                    backoff = if backoff.is_zero() {
                        Duration::from_secs(2)
                    } else {
                        (backoff * 2).min(Duration::from_secs(60))
                    };
                }
            }
            batch.clear();
            if closed {
                return;
            }
        }
    }

    async fn post(&mut self, lf: &LangfuseConfig, batch: &[Box<Span>]) -> anyhow::Result<()> {
        let client = match &self.client {
            Some(client) => client.clone(),
            None => {
                let built = reqwest::Client::builder()
                    .pool_max_idle_per_host(2)
                    .pool_idle_timeout(Duration::from_secs(90))
                    .user_agent(concat!("chtting-relay/", env!("CARGO_PKG_VERSION")))
                    .build()?;
                self.client = Some(built.clone());
                built
            }
        };

        let payload = export_request(lf, batch);
        let response = client
            .post(lf.traces_url())
            .basic_auth(lf.public_key.trim(), Some(lf.secret_key.trim()))
            .header(INGESTION_VERSION_HEADER, INGESTION_VERSION)
            .timeout(Duration::from_millis(lf.timeout_ms.clamp(500, 120_000)))
            .json(&payload)
            .send()
            .await?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // The body is the only thing that says *why* — a 401 here is a wrong
        // key pair and a 403 is usually the wrong host for the pair. Truncated
        // because it can be an HTML error page from something in front.
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "langfuse answered {}: {}",
            status.as_u16(),
            crate::util::truncate(body.trim(), 200)
        )
    }
}

/* -------------------------------------------------------------- OTLP -- */

/// One `ExportTraceServiceRequest`, JSON-encoded.
fn export_request(lf: &LangfuseConfig, batch: &[Box<Span>]) -> Value {
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    attr("service.name", str_value("chtting-relay")),
                    attr("service.version", str_value(env!("CARGO_PKG_VERSION"))),
                    attr("telemetry.sdk.name", str_value("chtting-relay")),
                    attr("telemetry.sdk.language", str_value("rust")),
                ],
            },
            "scopeSpans": [{
                "scope": {
                    "name": "chtting-relay",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "spans": batch.iter().map(|s| otel_span(lf, s)).collect::<Vec<_>>(),
            }],
        }],
    })
}

fn attr(key: &str, value: Value) -> Value {
    json!({ "key": key, "value": value })
}

fn str_value(s: &str) -> Value {
    json!({ "stringValue": s })
}

fn int_value(n: i64) -> Value {
    // uint64/int64 travel as strings in protobuf's JSON mapping; int64 within
    // the JSON-number range is accepted either way, and a token count is never
    // near that edge.
    json!({ "intValue": n })
}

fn double_value(n: f64) -> Value {
    json!({ "doubleValue": if n.is_finite() { n } else { 0.0 } })
}

fn array_value(items: &[String]) -> Value {
    json!({ "arrayValue": { "values": items.iter().map(|s| str_value(s)).collect::<Vec<_>>() } })
}

/// Milliseconds since the epoch, as the nanosecond string OTLP wants.
fn nanos(ms: i64) -> String {
    (ms.max(0) as i128 * 1_000_000).to_string()
}

fn otel_span(lf: &LangfuseConfig, span: &Span) -> Value {
    let r = &span.record;
    let start_ms = r.ts;
    // `total_ms` is a float of milliseconds; the end has to be at or after the
    // start, or the span is rejected as having negative duration.
    let end_ms = start_ms + r.total_ms.max(0.0).round() as i64;

    let failed = r.status >= 400 || r.status == 0;
    let mut attrs = Vec::with_capacity(40);

    /* ---- what Langfuse hangs the trace itself off ---- */
    attrs.push(attr(
        "langfuse.trace.name",
        str_value(if r.endpoint.is_empty() {
            "chat.completions"
        } else {
            &r.endpoint
        }),
    ));
    if !span.user.is_empty() {
        attrs.push(attr("langfuse.user.id", str_value(&span.user)));
        attrs.push(attr("user.id", str_value(&span.user)));
    }
    if !span.session.is_empty() {
        attrs.push(attr("langfuse.session.id", str_value(&span.session)));
        attrs.push(attr("session.id", str_value(&span.session)));
    }
    if !span.release.is_empty() {
        attrs.push(attr("langfuse.release", str_value(&span.release)));
        // The same string under the name v4 groups *observations* by.
        // `langfuse.release` describes the trace; `langfuse.version` is the
        // one a query over the observations table can filter on, which is
        // where "did this regress with the last deploy?" is now asked.
        attrs.push(attr("langfuse.version", str_value(&span.release)));
    }
    if !span.environment.is_empty() {
        attrs.push(attr("langfuse.environment", str_value(&span.environment)));
    }
    if !span.tags.is_empty() {
        attrs.push(attr("langfuse.trace.tags", array_value(&span.tags)));
    }

    /* ---- the observation: this is a generation ---- */
    attrs.push(attr("langfuse.observation.type", str_value("generation")));
    attrs.push(attr(
        "langfuse.observation.model.name",
        str_value(&r.public_model),
    ));
    attrs.push(attr("gen_ai.request.model", str_value(&r.public_model)));
    if !r.upstream_model.is_empty() {
        attrs.push(attr("gen_ai.response.model", str_value(&r.upstream_model)));
    }
    attrs.push(attr("gen_ai.operation.name", str_value("chat")));

    /* ---- the words ---- */
    //
    // On the observation, and deliberately nowhere else. This span is the
    // trace's root, and Langfuse reads a root observation's input and output
    // as the trace's overall pair — so these two already answer "what was
    // asked, what came back" at both levels. `langfuse.trace.input` and
    // `langfuse.trace.output` are the deprecated v3 way of saying it, alive
    // only for trace-level evaluators written before v4; writing them here
    // would be taking on that shim rather than keeping one.
    if !span.input.is_empty() {
        attrs.push(attr("langfuse.observation.input", str_value(&span.input)));
    }
    if !span.output.is_empty() {
        attrs.push(attr("langfuse.observation.output", str_value(&span.output)));
    }

    /* ---- the numbers ---- */
    //
    // Two figures, deliberately both. `input`/`output` are what the *caller*
    // is charged for, because that is what a trace is read to explain; the
    // relay's own cost sits beside them under its own names, so a Langfuse
    // dashboard adding up `input` does not silently include tokens the caller
    // never wrote.
    let usage = json!({
        "input": r.prompt_tokens.max(0),
        "output": r.completion_tokens.max(0),
        "total": r.total_tokens.max(0),
        "cache_read_input_tokens": r.cached_tokens.max(0),
        "reasoning": r.reasoning_tokens.max(0),
        "upstream_input": r.billed_prompt_tokens.max(0),
        "injected_system_prompt": r.system_prompt_tokens.max(0),
    });
    attrs.push(attr(
        "langfuse.observation.usage_details",
        str_value(&usage.to_string()),
    ));
    attrs.push(attr(
        "langfuse.observation.cost_details",
        str_value(
            &json!({
                "total": r.proxy_usd,
                "backend": r.backend_usd,
                "profit": r.profit_usd,
            })
            .to_string(),
        ),
    ));
    attrs.push(attr(
        "gen_ai.usage.input_tokens",
        int_value(r.prompt_tokens.max(0)),
    ));
    attrs.push(attr(
        "gen_ai.usage.output_tokens",
        int_value(r.completion_tokens.max(0)),
    ));
    attrs.push(attr("gen_ai.usage.cost", double_value(r.proxy_usd)));

    /* ---- level and why ---- */
    attrs.push(attr(
        "langfuse.observation.level",
        str_value(if failed { "ERROR" } else { "DEFAULT" }),
    ));
    if !r.error.is_empty() {
        attrs.push(attr(
            "langfuse.observation.status_message",
            str_value(&r.error),
        ));
    }

    /* ---- everything an operator asks next ---- */
    let mut meta: Vec<(&str, Value)> = vec![
        ("request_id", str_value(&r.id)),
        ("status", int_value(r.status)),
        ("key_label", str_value(&r.key_label)),
        ("key_kind", str_value(r.key_kind.as_str())),
        ("endpoint", str_value(&r.endpoint)),
        ("backend_id", str_value(&r.backend_id)),
        (
            "stream",
            str_value(if r.stream == 1 { "true" } else { "false" }),
        ),
        ("reasoning_effort", str_value(&r.reasoning_effort)),
        ("finish_reason", str_value(&r.finish_reason)),
        ("tokenizer", str_value(&r.tokenizer)),
        (
            "tokens_exact",
            str_value(if r.exact == 1 { "true" } else { "false" }),
        ),
        ("usage_source", str_value(&r.usage_source)),
        (
            "cache_hit",
            str_value(if r.cache_hit == 1 { "true" } else { "false" }),
        ),
        ("price_tiers", str_value(&r.price_tiers)),
        ("ttft_ms", double_value(r.ttft_ms)),
        ("total_ms", double_value(r.total_ms)),
        ("gen_ms", double_value(r.gen_ms)),
        ("queued_ms", double_value(r.queued_ms)),
        ("tokenize_ms", double_value(r.tokenize_ms)),
        ("inject_ms", double_value(r.inject_ms)),
        ("tokens_per_sec", double_value(r.tokens_per_sec)),
        ("retries", int_value(r.retries)),
        ("bytes_in", int_value(r.bytes_in as i64)),
        ("bytes_out", int_value(r.bytes_out as i64)),
    ];
    if !r.prompt_id.is_empty() {
        meta.push(("system_prompt_rule", str_value(&r.prompt_id)));
    }
    // The point of the whole exercise: what the relay put in front of the
    // caller's words, and the body the backend actually received.
    if !span.injected.is_empty() {
        meta.push(("injected_system_prompt", str_value(&span.injected)));
    }
    if !span.upstream_input.is_empty() {
        meta.push(("upstream_input", str_value(&span.upstream_input)));
    }
    if !span.reasoning.is_empty() {
        meta.push(("reasoning", str_value(&span.reasoning)));
    }
    if lf.capture_client_ip && !r.ip.is_empty() {
        meta.push(("client_ip", str_value(&r.ip)));
    }
    for (key, value) in meta {
        attrs.push(attr(&format!("langfuse.observation.metadata.{key}"), value));
    }

    json!({
        "traceId": span.trace_id,
        "spanId": span.span_id,
        "name": if r.public_model.is_empty() { "chat".to_string() } else { r.public_model.clone() },
        "kind": SPAN_KIND_CLIENT,
        "startTimeUnixNano": nanos(start_ms),
        "endTimeUnixNano": nanos(end_ms),
        "attributes": attrs,
        "status": {
            "code": if failed { STATUS_ERROR } else if r.status == 0 { STATUS_UNSET } else { STATUS_OK },
            "message": r.error.clone(),
        },
    })
}

/* --------------------------------------------------------------- ids -- */

/// The request's own uuid, as a 32-character OTLP trace id.
///
/// A uuid v4 is sixteen bytes and a trace id is sixteen bytes, so the only
/// work is dropping the dashes. Keeping them the same value is what lets an
/// operator paste a uid out of `relay.log` straight into Langfuse's search.
pub fn trace_id_from(uid: &str) -> String {
    let hex: String = uid
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(32)
        .collect::<String>()
        .to_ascii_lowercase();
    if hex.len() == 32 && hex.bytes().any(|b| b != b'0') {
        return hex;
    }
    // Not a uuid, or a uuid of zeros — an all-zero trace id is invalid, so
    // derive one that cannot be.
    let digest = crate::util::digest(uid.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Eight bytes of the same value, which is all a single-span trace needs.
pub fn span_id_from(uid: &str) -> String {
    let digest = crate::util::digest(uid.as_bytes());
    let id: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    if id.bytes().all(|b| b == b'0') {
        return "1".repeat(16);
    }
    id
}

/* ---------------------------------------------------------- redaction -- */

/// The caller's body as a trace should show it: the conversation, and the
/// parameters that shaped it, with nothing that identifies a credential.
///
/// `user` and whatever `userIdField` names are dropped here rather than
/// trimmed, because for a private key with `privateUserId: "secret"` that
/// field holds the client key itself — the one way a working credential could
/// reach a third party through this module.
pub fn redact_body(body: &Value, limit: usize) -> String {
    let Some(map) = body.as_object() else {
        return crate::util::truncate(&body.to_string(), limit);
    };
    let mut out = Map::with_capacity(map.len());
    for (key, value) in map {
        let lower = key.to_ascii_lowercase();
        if lower == "user"
            || lower == "user_id"
            || lower == "userid"
            || lower.contains("api_key")
            || lower.contains("apikey")
            || lower == "key"
            || lower == "authorization"
            || lower == "token"
        {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    crate::util::truncate(&Value::Object(out).to_string(), limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RequestRecord {
        RequestRecord {
            id: "6f1c9a2b-7d43-4e0f-9c1a-2b3c4d5e6f70".into(),
            ts: 1_760_000_000_000,
            total_ms: 1_234.5,
            status: 200,
            public_model: "zeiko-pro".into(),
            upstream_model: "deepseek-chat".into(),
            endpoint: "/v1/chat/completions".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            proxy_usd: 0.002,
            backend_usd: 0.000_5,
            profit_usd: 0.001_5,
            ..Default::default()
        }
    }

    fn span() -> Box<Span> {
        Box::new(Span {
            trace_id: trace_id_from(&record().id),
            span_id: span_id_from(&record().id),
            input: "{\"messages\":[]}".into(),
            injected: "You are Zeiko.".into(),
            upstream_input: "{\"messages\":[]}".into(),
            output: "hello".into(),
            reasoning: String::new(),
            record: Box::new(record()),
            session: "sess".into(),
            user: "u1".into(),
            environment: "production".into(),
            release: "2.0.0".into(),
            tags: vec!["relay".into()],
        })
    }

    /// A uuid is already sixteen bytes; the trace id is the same sixteen, so
    /// the id in the log and the id in Langfuse are one string.
    #[test]
    fn the_trace_id_is_the_requests_own_uuid() {
        assert_eq!(
            trace_id_from("6f1c9a2b-7d43-4e0f-9c1a-2b3c4d5e6f70"),
            "6f1c9a2b7d434e0f9c1a2b3c4d5e6f70"
        );
        assert_eq!(
            trace_id_from("6f1c9a2b-7d43-4e0f-9c1a-2b3c4d5e6f70").len(),
            32
        );
        assert_eq!(
            span_id_from("6f1c9a2b-7d43-4e0f-9c1a-2b3c4d5e6f70").len(),
            16
        );
    }

    /// An all-zero id is invalid in OTLP and would have the span dropped
    /// silently at the far end.
    #[test]
    fn a_degenerate_id_never_produces_an_all_zero_trace_id() {
        for uid in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "not-a-uuid",
            "zzzz",
        ] {
            let trace = trace_id_from(uid);
            assert_eq!(trace.len(), 32, "{uid:?}");
            assert!(trace.bytes().any(|b| b != b'0'), "{uid:?} -> {trace}");
            let span = span_id_from(uid);
            assert_eq!(span.len(), 16, "{uid:?}");
            assert!(span.bytes().any(|b| b != b'0'), "{uid:?} -> {span}");
        }
    }

    /// The one way a working credential could reach Langfuse: a private key
    /// whose upstream id *is* the key, written into the body the relay sends.
    #[test]
    fn a_body_never_carries_the_field_that_can_hold_a_client_key() {
        let body = json!({
            "model": "zeiko-pro",
            "messages": [{"role": "user", "content": "hi"}],
            "user": "Kunci-Zeiko-deadbeef-dead-beef-dead-beefdeadbeef",
            "user_id": "Kunci-Zeiko-deadbeef-dead-beef-dead-beefdeadbeef",
            "api_key": "sk-nope",
        });
        let redacted = redact_body(&body, 10_000);
        assert!(!redacted.contains("Kunci-Zeiko"), "{redacted}");
        assert!(!redacted.contains("sk-nope"), "{redacted}");
        assert!(redacted.contains("zeiko-pro"), "{redacted}");
        assert!(redacted.contains("\"hi\""), "{redacted}");
    }

    #[test]
    fn a_long_body_is_trimmed_rather_than_posted_whole() {
        let body = json!({ "messages": "x".repeat(10_000) });
        assert!(redact_body(&body, 200).len() <= 220);
    }

    /// The span has to survive OTLP's own rules: hex ids, nanosecond strings,
    /// and an end that is never before the start.
    #[test]
    fn the_exported_span_is_shaped_the_way_otlp_requires() {
        let lf = LangfuseConfig::default();
        let batch = vec![span()];
        let payload = export_request(&lf, &batch);
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];

        assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        let start: i128 = span["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
        let end: i128 = span["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
        assert!(end >= start, "a span cannot end before it starts");
        assert_eq!(start, 1_760_000_000_000i128 * 1_000_000);
        assert_eq!(span["kind"], SPAN_KIND_CLIENT);
        assert_eq!(span["status"]["code"], STATUS_OK);
    }

    /// A request that never reached the backend has `total_ms` of zero and a
    /// status of zero, which used to be the shape most likely to produce an
    /// invalid span.
    #[test]
    fn a_request_that_failed_before_it_started_still_makes_a_valid_span() {
        let mut s = span();
        s.record.status = 404;
        s.record.total_ms = 0.0;
        s.record.error = "model \"x\" is not available on this relay".into();
        let payload = export_request(&LangfuseConfig::default(), &[s]);
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["status"]["code"], STATUS_ERROR);
        assert_eq!(
            span["startTimeUnixNano"].as_str(),
            span["endTimeUnixNano"].as_str(),
            "a zero-length span is legal; a backwards one is not"
        );
        let level = attribute(span, "langfuse.observation.level");
        assert_eq!(level["stringValue"], "ERROR");
    }

    #[test]
    fn the_injection_travels_as_its_own_metadata_field() {
        let payload = export_request(&LangfuseConfig::default(), &[span()]);
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(
            attribute(span, "langfuse.observation.metadata.injected_system_prompt")["stringValue"],
            "You are Zeiko."
        );
        assert_eq!(
            attribute(span, "langfuse.observation.type")["stringValue"],
            "generation"
        );
    }

    /// v4 has no trace input or output. The root observation's pair *is* the
    /// trace's, and the deprecated attributes exist only to keep evaluators
    /// written before v4 running — so a span that carries them is a span that
    /// has quietly opted into a compatibility shim.
    #[test]
    fn the_overall_words_go_on_the_root_observation_and_not_on_the_trace() {
        let payload = export_request(&LangfuseConfig::default(), &[span()]);
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];

        // The root observation carries the overall request and response.
        assert_eq!(
            attribute(span, "langfuse.observation.input")["stringValue"],
            "{\"messages\":[]}"
        );
        assert_eq!(
            attribute(span, "langfuse.observation.output")["stringValue"],
            "hello"
        );
        // And this span is a root: nothing parents it, which is what makes
        // those two the trace's own pair.
        assert!(
            span.get("parentSpanId").is_none(),
            "the one span of a trace has to be its root"
        );

        for deprecated in ["langfuse.trace.input", "langfuse.trace.output"] {
            assert!(
                span["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|a| a["key"] != deprecated),
                "{deprecated} is deprecated in v4 and must not be sent"
            );
        }
    }

    /// What v4 filters observations by has to be *on the observation*. With one
    /// span per trace that is free; the test is here so that it stays true if a
    /// second span is ever added.
    #[test]
    fn every_correlating_attribute_rides_on_the_span_itself() {
        let payload = export_request(&LangfuseConfig::default(), &[span()]);
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];

        assert_eq!(attribute(span, "langfuse.user.id")["stringValue"], "u1");
        assert_eq!(
            attribute(span, "langfuse.session.id")["stringValue"],
            "sess"
        );
        assert_eq!(
            attribute(span, "langfuse.trace.name")["stringValue"],
            "/v1/chat/completions"
        );
        assert_eq!(
            attribute(span, "langfuse.environment")["stringValue"],
            "production"
        );
        assert_eq!(attribute(span, "langfuse.release")["stringValue"], "2.0.0");
        // `release` names the deploy on the trace; `version` is the name the
        // observations table can group by, and it is the same string.
        assert_eq!(attribute(span, "langfuse.version")["stringValue"], "2.0.0");
        assert_eq!(
            attribute(span, "langfuse.trace.tags")["arrayValue"]["values"][0]["stringValue"],
            "relay"
        );
    }

    /// The caller's address is somebody's personal data; it goes only when the
    /// operator has said so.
    #[test]
    fn the_callers_address_is_opt_in() {
        let mut s = span();
        s.record.ip = "203.0.113.9".into();
        let off = export_request(&LangfuseConfig::default(), &[s]);
        let text = off.to_string();
        assert!(!text.contains("203.0.113.9"), "{text}");
    }

    fn attribute<'a>(span: &'a Value, key: &str) -> &'a Value {
        span["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == key)
            .unwrap_or_else(|| panic!("no attribute {key}"))
            .get("value")
            .unwrap()
    }

    async fn exporter(
        langfuse: LangfuseConfig,
    ) -> (Arc<ConfigStore>, Arc<Langfuse>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        let cfg = Config {
            langfuse,
            ..Default::default()
        };
        tokio::fs::write(&file, serde_json::to_string(&cfg).unwrap())
            .await
            .unwrap();
        let store = Arc::new(ConfigStore::load(&file).await.unwrap());
        let lf = Langfuse::start(
            store.clone(),
            Logger::console(crate::logging::Level::Silent),
        );
        (store, lf, dir)
    }

    fn keyed(sample_rate: f64) -> LangfuseConfig {
        LangfuseConfig {
            enabled: true,
            public_key: "pk-lf-test".into(),
            secret_key: "sk-lf-test".into(),
            sample_rate,
            ..Default::default()
        }
    }

    /// Sampling is decided when the request starts, and a failure is never
    /// sampled out — which is why a sampled-out request is still captured
    /// while `captureErrors` is on.
    #[tokio::test]
    async fn errors_are_kept_even_at_a_sample_rate_of_zero() {
        let (store, lf, _dir) = exporter(keyed(0.0)).await;
        let cfg = store.current();

        let traced = lf
            .begin(&cfg, Arc::new(json!({"messages": []})))
            .expect("captureErrors keeps the capture alive through a zero sample rate");
        assert!(!traced.sampled);

        let mut ok = record();
        ok.status = 200;
        lf.record(&cfg, &traced, &ok, "hi", "");
        assert_eq!(lf.status()["queued"], 0, "a sampled-out success is dropped");

        let mut bad = record();
        bad.status = 502;
        bad.error = "the model is unavailable right now".into();
        lf.record(&cfg, &traced, &bad, "", "");
        assert_eq!(
            lf.status()["queued"],
            1,
            "a failure is what a trace is read for"
        );
    }

    /// With `captureErrors` off as well, a sampled-out request carries nothing
    /// at all — no `Arc`, no string, no branch later on.
    #[tokio::test]
    async fn a_sampled_out_request_carries_nothing_when_errors_are_off_too() {
        let (store, lf, _dir) = exporter(LangfuseConfig {
            capture_errors: false,
            ..keyed(0.0)
        })
        .await;
        assert!(lf.begin(&store.current(), Arc::new(json!({}))).is_none());
    }

    #[tokio::test]
    async fn nothing_is_traced_until_both_keys_are_set() {
        let (store, lf, _dir) = exporter(LangfuseConfig {
            secret_key: String::new(),
            ..keyed(1.0)
        })
        .await;
        assert!(
            lf.begin(&store.current(), Arc::new(json!({}))).is_none(),
            "half a key pair is not a key pair"
        );
    }

    /// The one credential that can reach a trace: a private key whose upstream
    /// identity is configured to be the key itself.
    #[tokio::test]
    async fn a_private_keys_secret_identity_is_fingerprinted_rather_than_sent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        let mut cfg = Config {
            langfuse: keyed(1.0),
            ..Default::default()
        };
        cfg.security.private_user_id = crate::config::PRIVATE_ID_SECRET.into();
        tokio::fs::write(&file, serde_json::to_string(&cfg).unwrap())
            .await
            .unwrap();
        let store = Arc::new(ConfigStore::load(&file).await.unwrap());
        let lf = Langfuse::start(
            store.clone(),
            Logger::console(crate::logging::Level::Silent),
        );
        let cfg = store.current();

        let traced = lf.begin(&cfg, Arc::new(json!({}))).unwrap();
        let mut r = record();
        r.key_kind = crate::config::KeyKind::Private;
        r.user_id = "Kunci-Zeiko-deadbeef-dead-beef-dead-beefdeadbeef".into();
        lf.record(&cfg, &traced, &r, "hi", "");

        // Nothing reached the wire yet, but the span that was queued is the
        // one that would have: check it directly.
        let span = Box::new(Span {
            trace_id: trace_id_from(&r.id),
            span_id: span_id_from(&r.id),
            input: String::new(),
            injected: String::new(),
            upstream_input: String::new(),
            output: String::new(),
            reasoning: String::new(),
            record: Box::new(r.clone()),
            session: fingerprint(&r.user_id),
            user: fingerprint(&r.user_id),
            environment: String::new(),
            release: String::new(),
            tags: Vec::new(),
        });
        let text = export_request(&cfg.langfuse, &[span]).to_string();
        assert!(!text.contains("Kunci-Zeiko"), "{text}");
        assert!(text.contains("anon-"), "{text}");
    }

    /// Telemetry must never be able to block the relay: a full queue drops the
    /// span and says so in the counters.
    #[tokio::test]
    async fn a_full_queue_drops_spans_rather_than_waiting() {
        let (tx, _rx) = mpsc::channel::<Box<Span>>(1);
        let lf = Langfuse {
            tx,
            stats: Arc::new(Stats::default()),
        };
        for _ in 0..10 {
            lf.submit(span());
        }
        let status = lf.status();
        assert_eq!(status["queued"], 1);
        assert_eq!(status["dropped"], 9);
    }
}
