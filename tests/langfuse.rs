//! A relay in front of a fake Langfuse, checked end to end.
//!
//! The point of these is what *arrives*: a span is only useful if it carries
//! the caller's own words, the prompt the relay put in front of them, and the
//! answer that came back — and only safe if it carries neither the client key
//! nor the backend's.

mod common;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use common::{harness, MockConfig};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/* --------------------------------------------------- a fake Langfuse -- */

#[derive(Clone)]
struct Collector {
    batches: Arc<Mutex<Vec<Value>>>,
    auth: Arc<Mutex<Vec<String>>>,
    /// `x-langfuse-ingestion-version` as it arrived, per POST. Empty string
    /// for a POST that did not carry it, which is the failure this records.
    version: Arc<Mutex<Vec<String>>>,
}

struct FakeLangfuse {
    url: String,
    batches: Arc<Mutex<Vec<Value>>>,
    auth: Arc<Mutex<Vec<String>>>,
    version: Arc<Mutex<Vec<String>>>,
}

impl FakeLangfuse {
    async fn start() -> Self {
        let state = Collector {
            batches: Arc::new(Mutex::new(Vec::new())),
            auth: Arc::new(Mutex::new(Vec::new())),
            version: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route(
                "/api/public/otel/v1/traces",
                post(
                    |State(state): State<Collector>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        state.auth.lock().push(
                            headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_string(),
                        );
                        state.version.lock().push(
                            headers
                                .get("x-langfuse-ingestion-version")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_string(),
                        );
                        state.batches.lock().push(body);
                        Json(json!({}))
                    },
                ),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{addr}"),
            batches: state.batches,
            auth: state.auth,
            version: state.version,
        }
    }

    /// Every span posted so far, flattened out of its batches.
    fn spans(&self) -> Vec<Value> {
        self.batches
            .lock()
            .iter()
            .flat_map(|batch| {
                batch["resourceSpans"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .flat_map(|rs| rs["scopeSpans"].as_array().cloned().unwrap_or_default())
            .flat_map(|ss| ss["spans"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    /// Wait for at least `want` spans, or give up. The exporter batches on a
    /// timer, so a test that read once would read nothing.
    async fn wait_for(&self, want: usize) -> Vec<Value> {
        for _ in 0..100 {
            let spans = self.spans();
            if spans.len() >= want {
                return spans;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.spans()
    }
}

/// The value of one attribute on a span, whatever type it carries.
fn attr(span: &Value, key: &str) -> Value {
    span["attributes"]
        .as_array()
        .map(|list| {
            list.iter()
                .find(|a| a["key"] == key)
                .and_then(|a| a.get("value"))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .unwrap_or(Value::Null)
}

fn text(span: &Value, key: &str) -> String {
    attr(span, key)["stringValue"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/* -------------------------------------------------------------- tests -- */

/// The whole point: one request, one span, carrying the three things the
/// backend's own logs cannot tell you — what was asked, what the relay put in
/// front of it, and what came back.
#[tokio::test]
async fn a_relayed_request_arrives_at_langfuse_with_input_injection_and_output() {
    let langfuse = FakeLangfuse::start().await;
    let host = langfuse.url.clone();

    let relay = harness(
        MockConfig {
            reply: "the moon is made of basalt".into(),
            ..Default::default()
        },
        move |cfg| {
            cfg.models[0]
                .system_prompts
                .push(chtting_relay::config::SystemPromptRule {
                    id: "spr_test".into(),
                    enabled: true,
                    prompt: chtting_relay::config::SystemPromptSpec {
                        mode: "prepend".into(),
                        text: "You are Zeiko, and you never mention the backend.".into(),
                        prompt_id: String::new(),
                    },
                    ..Default::default()
                });
            cfg.langfuse = chtting_relay::config::LangfuseConfig {
                enabled: true,
                host,
                public_key: "pk-lf-test".into(),
                secret_key: "sk-lf-secret".into(),
                sample_rate: 1.0,
                flush_interval_ms: 250,
                batch_size: 1,
                ..Default::default()
            };
        },
    )
    .await;

    let response = relay
        .post(
            "/v1/chat/completions",
            json!({
                "model": "manukmiberai/creative-writer",
                "messages": [{"role": "user", "content": "what is the moon made of?"}],
            }),
        )
        .await;
    assert_eq!(response.status(), 200);

    let spans = langfuse.wait_for(1).await;
    assert_eq!(spans.len(), 1, "one request, one span");
    let span = &spans[0];

    // The identifiers OTLP insists on.
    assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
    assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
    assert_eq!(span["status"]["code"], 1);

    // What the caller asked.
    let input = text(span, "langfuse.observation.input");
    assert!(input.contains("what is the moon made of?"), "{input}");

    // What the relay injected, which is the thing the backend's logs cannot
    // separate from the caller's own words.
    let injected = text(span, "langfuse.observation.metadata.injected_system_prompt");
    assert_eq!(
        injected,
        "You are Zeiko, and you never mention the backend."
    );
    let upstream = text(span, "langfuse.observation.metadata.upstream_input");
    assert!(upstream.contains("You are Zeiko"), "{upstream}");
    assert!(upstream.contains("what is the moon made of?"), "{upstream}");

    // What came back.
    let output = text(span, "langfuse.observation.output");
    assert_eq!(output, "the moon is made of basalt");

    // And that it is filed as a generation, against the model the caller named
    // rather than the one behind it.
    assert_eq!(text(span, "langfuse.observation.type"), "generation");
    assert_eq!(
        text(span, "langfuse.observation.model.name"),
        "manukmiberai/creative-writer"
    );

    // Usage travels as a JSON blob Langfuse parses; the caller's own figures
    // are the headline ones.
    let usage: Value =
        serde_json::from_str(&text(span, "langfuse.observation.usage_details")).unwrap();
    assert!(usage["input"].as_i64().unwrap() > 0, "{usage}");
    assert!(usage["output"].as_i64().unwrap() > 0, "{usage}");

    // Basic auth, with the project's own key pair.
    let auth = langfuse.auth.lock().first().cloned().unwrap_or_default();
    assert!(auth.starts_with("Basic "), "{auth}");
    let decoded = {
        use base64::Engine;
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(auth.trim_start_matches("Basic "))
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(decoded, "pk-lf-test:sk-lf-secret");

    // And the header that puts the batch on v4's ingestion path. Without it
    // Langfuse accepts the span and then takes up to fifteen minutes to show
    // it on the v4 data model and the v2 APIs, which for a trace read while
    // the request is still on somebody's screen is the same as losing it.
    assert_eq!(
        langfuse.version.lock().first().cloned().unwrap_or_default(),
        "4",
        "every POST carries x-langfuse-ingestion-version: 4"
    );
}

/// v4 has no trace input or output: a trace is just the observations sharing a
/// trace id, and the root observation's pair is the overall one. The deprecated
/// attributes survive only for trace-level evaluators written before v4, so
/// what matters here is that they do not go out at all.
#[tokio::test]
async fn the_overall_words_arrive_on_the_root_observation_and_not_on_the_trace() {
    let langfuse = FakeLangfuse::start().await;
    let host = langfuse.url.clone();

    let relay = harness(
        MockConfig {
            reply: "basalt, mostly".into(),
            ..Default::default()
        },
        move |cfg| {
            cfg.langfuse = chtting_relay::config::LangfuseConfig {
                enabled: true,
                host,
                public_key: "pk-lf-test".into(),
                secret_key: "sk-lf-secret".into(),
                flush_interval_ms: 250,
                batch_size: 1,
                release: "v4-canary".into(),
                ..Default::default()
            };
        },
    )
    .await;

    let response = relay
        .post(
            "/v1/chat/completions",
            json!({
                "model": "manukmiberai/creative-writer",
                "messages": [{"role": "user", "content": "what is the moon made of?"}],
            }),
        )
        .await;
    assert_eq!(response.status(), 200);

    let spans = langfuse.wait_for(1).await;
    let span = &spans[0];

    // The root observation carries the overall request and response.
    assert!(
        text(span, "langfuse.observation.input").contains("what is the moon made of?"),
        "{span}"
    );
    assert_eq!(text(span, "langfuse.observation.output"), "basalt, mostly");
    // It is a root: one span, nothing parenting it.
    assert!(
        span.get("parentSpanId").is_none(),
        "the one span of a trace has to be its root"
    );

    // The deprecated pair never reaches the wire, under any key.
    let posted = serde_json::to_string(&*langfuse.batches.lock()).unwrap();
    for deprecated in ["langfuse.trace.input", "langfuse.trace.output"] {
        assert!(
            !posted.contains(deprecated),
            "{deprecated} is deprecated in v4 and was still exported"
        );
    }

    // What v4 filters observations by has to be on the observation, including
    // the version the deploy is pinned to.
    assert_eq!(text(span, "langfuse.release"), "v4-canary");
    assert_eq!(text(span, "langfuse.version"), "v4-canary");
    assert_eq!(text(span, "langfuse.environment"), "production");
    assert_eq!(text(span, "langfuse.trace.name"), "v1/chat/completions");
    assert!(!text(span, "langfuse.session.id").is_empty(), "{span}");
}

/// A span Langfuse has already accepted is never sent again: v4 does not
/// deduplicate a repeated span id on the read path, so a second copy becomes a
/// second observation and every count drawn from it is wrong.
#[tokio::test]
async fn a_span_id_is_exported_exactly_once() {
    let langfuse = FakeLangfuse::start().await;
    let host = langfuse.url.clone();

    let relay = harness(MockConfig::default(), move |cfg| {
        cfg.langfuse = chtting_relay::config::LangfuseConfig {
            enabled: true,
            host,
            public_key: "pk-lf-test".into(),
            secret_key: "sk-lf-secret".into(),
            flush_interval_ms: 250,
            batch_size: 1,
            ..Default::default()
        };
    })
    .await;

    for _ in 0..3 {
        let response = relay
            .post(
                "/v1/chat/completions",
                json!({
                    "model": "manukmiberai/creative-writer",
                    "messages": [{"role": "user", "content": "hello"}],
                }),
            )
            .await;
        assert_eq!(response.status(), 200);
    }

    let spans = langfuse.wait_for(3).await;
    assert_eq!(spans.len(), 3, "three requests, three spans");

    let mut ids: Vec<String> = spans
        .iter()
        .map(|s| s["spanId"].as_str().unwrap_or_default().to_string())
        .collect();
    let posted = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), posted, "a span id was exported twice");

    let mut traces: Vec<String> = spans
        .iter()
        .map(|s| s["traceId"].as_str().unwrap_or_default().to_string())
        .collect();
    traces.sort();
    traces.dedup();
    assert_eq!(traces.len(), posted, "two requests shared a trace id");

    // Every batch, not just the first, is on the v4 path.
    let versions = langfuse.version.lock().clone();
    assert!(!versions.is_empty());
    assert!(
        versions.iter().all(|v| v == "4"),
        "a POST went out without the v4 ingestion header: {versions:?}"
    );
}

/// A trace is a third party. Neither the caller's key nor the backend's may
/// ever reach one, whatever else does.
#[tokio::test]
async fn no_credential_of_any_kind_reaches_langfuse() {
    let langfuse = FakeLangfuse::start().await;
    let host = langfuse.url.clone();

    let relay = harness(MockConfig::default(), move |cfg| {
        cfg.langfuse = chtting_relay::config::LangfuseConfig {
            enabled: true,
            host,
            public_key: "pk-lf-test".into(),
            secret_key: "sk-lf-secret".into(),
            flush_interval_ms: 250,
            batch_size: 1,
            capture_client_ip: false,
            ..Default::default()
        };
    })
    .await;

    relay
        .post(
            "/v1/chat/completions",
            json!({
                "model": "manukmiberai/creative-writer",
                "messages": [{"role": "user", "content": "hello"}],
                // A caller who puts their own key in the body cannot make the
                // relay publish it either.
                "api_key": "sk-relay-test-key",
            }),
        )
        .await;

    langfuse.wait_for(1).await;
    let posted = serde_json::to_string(&*langfuse.batches.lock()).unwrap();
    assert!(
        !posted.contains("sk-relay-test-key"),
        "the caller's key reached Langfuse"
    );
    assert!(
        !posted.contains("sk-backend-secret"),
        "the backend's key reached Langfuse"
    );
}

/// A failure is the thing a trace viewer is opened to explain, so it is traced
/// even though it never reached a backend.
#[tokio::test]
async fn a_request_that_never_reached_a_backend_is_still_traced() {
    let langfuse = FakeLangfuse::start().await;
    let host = langfuse.url.clone();

    let relay = harness(MockConfig::default(), move |cfg| {
        cfg.langfuse = chtting_relay::config::LangfuseConfig {
            enabled: true,
            host,
            public_key: "pk-lf-test".into(),
            secret_key: "sk-lf-secret".into(),
            // Nothing successful is kept; the failure still is.
            sample_rate: 0.0,
            capture_errors: true,
            flush_interval_ms: 250,
            batch_size: 1,
            ..Default::default()
        };
    })
    .await;

    let response = relay
        .post(
            "/v1/chat/completions",
            json!({
                "model": "a-model-that-does-not-exist",
                "messages": [{"role": "user", "content": "hello"}],
            }),
        )
        .await;
    assert_eq!(response.status(), 404);

    let spans = langfuse.wait_for(1).await;
    assert_eq!(spans.len(), 1, "a 404 on the model name earns a span");
    let span = &spans[0];
    assert_eq!(span["status"]["code"], 2, "OTLP error");
    assert_eq!(text(span, "langfuse.observation.level"), "ERROR");
    assert!(
        text(span, "langfuse.observation.status_message").contains("not available"),
        "{span}"
    );
    // A request refused before it ran has no duration, and a span that ended
    // before it started would be rejected outright.
    let start: i128 = span["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let end: i128 = span["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(end >= start);
}

/// Langfuse being down, slow or wrong must never be something a caller can
/// notice.
#[tokio::test]
async fn an_unreachable_langfuse_does_not_touch_the_relay() {
    let relay = harness(MockConfig::default(), |cfg| {
        cfg.langfuse = chtting_relay::config::LangfuseConfig {
            enabled: true,
            // Nothing listens here.
            host: "http://127.0.0.1:1".into(),
            public_key: "pk-lf-test".into(),
            secret_key: "sk-lf-secret".into(),
            flush_interval_ms: 250,
            batch_size: 1,
            timeout_ms: 500,
            ..Default::default()
        };
    })
    .await;

    for _ in 0..5 {
        let response = relay
            .post(
                "/v1/chat/completions",
                json!({
                    "model": "manukmiberai/creative-writer",
                    "messages": [{"role": "user", "content": "hello"}],
                }),
            )
            .await;
        assert_eq!(response.status(), 200, "a dead exporter failed a request");
    }

    // And the relay says so rather than going quiet about it.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let status = relay.state.langfuse.status();
    assert!(
        status["failed"].as_u64().unwrap_or(0) > 0 || status["queued"].as_u64().unwrap_or(0) > 0,
        "{status}"
    );
}

/// Off is off: no keys, no host reached, nothing queued.
#[tokio::test]
async fn nothing_is_exported_while_langfuse_is_switched_off() {
    let langfuse = FakeLangfuse::start().await;
    let host = langfuse.url.clone();

    let relay = harness(MockConfig::default(), move |cfg| {
        cfg.langfuse = chtting_relay::config::LangfuseConfig {
            enabled: false,
            host,
            public_key: "pk-lf-test".into(),
            secret_key: "sk-lf-secret".into(),
            flush_interval_ms: 250,
            batch_size: 1,
            ..Default::default()
        };
    })
    .await;

    relay
        .post(
            "/v1/chat/completions",
            json!({
                "model": "manukmiberai/creative-writer",
                "messages": [{"role": "user", "content": "hello"}],
            }),
        )
        .await;

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(langfuse.spans().is_empty(), "a disabled exporter exported");
    assert_eq!(relay.state.langfuse.status()["queued"], 0);
}

/// A streamed answer is assembled as it goes; the span has to carry the whole
/// of it rather than the first frame.
#[tokio::test]
async fn a_streamed_answer_reaches_the_span_whole() {
    let langfuse = FakeLangfuse::start().await;
    let host = langfuse.url.clone();

    let relay = harness(
        MockConfig {
            reply: "one two three four five six seven eight".into(),
            ..Default::default()
        },
        move |cfg| {
            cfg.langfuse = chtting_relay::config::LangfuseConfig {
                enabled: true,
                host,
                public_key: "pk-lf-test".into(),
                secret_key: "sk-lf-secret".into(),
                flush_interval_ms: 250,
                batch_size: 1,
                ..Default::default()
            };
        },
    )
    .await;

    let response = relay
        .post(
            "/v1/chat/completions",
            json!({
                "model": "manukmiberai/creative-writer",
                "messages": [{"role": "user", "content": "count"}],
                "stream": true,
            }),
        )
        .await;
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    let spans = langfuse.wait_for(1).await;
    assert_eq!(
        text(&spans[0], "langfuse.observation.output"),
        "one two three four five six seven eight"
    );
    assert_eq!(
        text(&spans[0], "langfuse.observation.metadata.stream"),
        "true"
    );
}
