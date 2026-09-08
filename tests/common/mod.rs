//! Test harness: a mock upstream backend and a real relay in front of it.
//!
//! Nothing here stubs the relay itself — the tests drive actual HTTP against
//! the actual server, so routing, streaming, transforms and metrics are all
//! exercised the way a client would exercise them.

#![allow(dead_code)]

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;

use chtting_relay::config::Config;
use chtting_relay::logging::{Level, Logger};
use chtting_relay::state::{AppState, Paths};

/* -------------------------------------------------------- mock backend -- */

#[derive(Clone)]
pub struct MockConfig {
    /// What the assistant "says".
    pub reply: String,
    pub reasoning: Option<String>,
    /// Emitted in the final chunk / body when set.
    pub usage: Option<Value>,
    pub status: u16,
    pub error_body: Option<String>,
    /// Split streamed SSE frames at this byte size, to tear multi-byte
    /// characters across network chunks the way a real link does.
    pub byte_chunk_size: Option<usize>,
    /// Ignore `stream: true` and answer with a whole body anyway.
    pub never_streams: bool,
    pub model_echo: Option<String>,
    pub delay_ms: u64,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            reply: "hello from the backend".into(),
            reasoning: None,
            usage: None,
            status: 200,
            error_body: None,
            byte_chunk_size: None,
            never_streams: false,
            model_echo: None,
            delay_ms: 0,
        }
    }
}

pub struct MockBackend {
    pub addr: SocketAddr,
    pub received: Arc<Mutex<Vec<Value>>>,
    config: Arc<Mutex<MockConfig>>,
}

impl MockBackend {
    pub async fn start(config: MockConfig) -> Self {
        let state = MockState {
            config: Arc::new(Mutex::new(config)),
            received: Arc::new(Mutex::new(Vec::new())),
        };

        let app = Router::new()
            .route("/v1/chat/completions", post(handle))
            .route("/v1/completions", post(handle))
            .route("/v1/embeddings", post(embeddings))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            addr,
            received: state.received,
            config: state.config,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn set(&self, config: MockConfig) {
        *self.config.lock() = config;
    }

    /// The last body the relay actually sent upstream.
    pub fn last_request(&self) -> Value {
        self.received.lock().last().cloned().unwrap_or(Value::Null)
    }

    pub fn request_count(&self) -> usize {
        self.received.lock().len()
    }
}

#[derive(Clone)]
struct MockState {
    config: Arc<Mutex<MockConfig>>,
    received: Arc<Mutex<Vec<Value>>>,
}

async fn embeddings(State(state): State<MockState>, Json(body): Json<Value>) -> Response {
    state.received.lock().push(body);
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
        "model": "backend-embed-1",
        "usage": {"prompt_tokens": 7, "total_tokens": 7},
    }))
    .into_response()
}

async fn handle(State(state): State<MockState>, Json(body): Json<Value>) -> Response {
    state.received.lock().push(body.clone());
    let config = state.config.lock().clone();

    if config.delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(config.delay_ms)).await;
    }

    if config.status != 200 {
        let body = config
            .error_body
            .clone()
            .unwrap_or_else(|| json!({"error": {"message": "mock failure"}}).to_string());
        return (
            StatusCode::from_u16(config.status).unwrap(),
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
    }

    let model = config
        .model_echo
        .clone()
        .or_else(|| {
            body.get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let wants_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if wants_stream && !config.never_streams {
        return stream_response(&config, &model);
    }

    let mut message = json!({"role": "assistant", "content": config.reply});
    if let Some(reasoning) = &config.reasoning {
        message["reasoning_content"] = json!(reasoning);
    }
    let mut payload = json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
    });
    if let Some(usage) = &config.usage {
        payload["usage"] = usage.clone();
    }
    Json(payload).into_response()
}

fn stream_response(config: &MockConfig, model: &str) -> Response {
    let mut raw = String::new();
    let frame = |delta: Value| {
        format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
            })
        )
    };

    raw.push_str(&frame(json!({"role": "assistant", "content": ""})));
    if let Some(reasoning) = &config.reasoning {
        raw.push_str(&frame(json!({"reasoning_content": reasoning})));
    }
    // Emit a few characters at a time so tests exercise real chunking.
    let chars: Vec<char> = config.reply.chars().collect();
    for piece in chars.chunks(4) {
        raw.push_str(&frame(json!({"content": piece.iter().collect::<String>()})));
    }

    raw.push_str(&format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-mock", "object": "chat.completion.chunk",
            "created": 1_700_000_000, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        })
    ));
    if let Some(usage) = &config.usage {
        raw.push_str(&format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-mock", "object": "chat.completion.chunk",
                "created": 1_700_000_000, "model": model,
                "choices": [], "usage": usage,
            })
        ));
    }
    raw.push_str("data: [DONE]\n\n");

    // Optionally cut the byte stream at hostile boundaries.
    let bytes = raw.into_bytes();
    let chunk_size = config.byte_chunk_size.unwrap_or(bytes.len().max(1));
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = bytes
        .chunks(chunk_size.max(1))
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(futures_util::stream::iter(chunks)),
    )
        .into_response()
}

/* ---------------------------------------------------------- relay app -- */

pub struct Harness {
    pub addr: SocketAddr,
    pub state: Arc<AppState>,
    pub client_key: String,
    pub backend: MockBackend,
    _home: tempfile::TempDir,
}

impl Harness {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// A client pointed at the relay, with the test key attached.
    pub fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    pub async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client()
            .post(self.url(path))
            .header("authorization", format!("Bearer {}", self.client_key))
            .json(&body)
            .send()
            .await
            .expect("relay is reachable")
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client()
            .get(self.url(path))
            .header("authorization", format!("Bearer {}", self.client_key))
            .send()
            .await
            .expect("relay is reachable")
    }

    /// Every metrics row recorded so far, newest first.
    pub async fn rows(&self) -> Vec<Value> {
        // The writer batches, so ask it to commit and wait rather than polling
        // until something shows up — polling returns a partial set under load.
        self.state.store.flush().await;
        self.state
            .store
            .read(|conn| {
                let sql = format!(
                    "SELECT {} FROM requests ORDER BY ts DESC, rowid DESC",
                    chtting_relay::store::schema::select_columns()
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows: Vec<Value> = stmt
                    .query_map([], chtting_relay::store::schema::row_to_json)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap_or_default()
    }

    pub async fn last_row(&self) -> Value {
        self.rows().await.first().cloned().unwrap_or(Value::Null)
    }

    /// Every ledger row recorded so far, in the order they were appended.
    pub async fn ledger(&self) -> Vec<Value> {
        self.state.store.flush().await;
        self.state
            .store
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT seq, request_id, phase, status, requests, input_tokens,
                            billed_input_tokens, output_tokens, cached_tokens, cache_hit,
                            ttft_ms, gen_ms, total_ms, queued_ms, tokens_per_sec,
                            prev_hash, row_hash
                     FROM usage_ledger ORDER BY seq ASC",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok(json!({
                            "seq": r.get::<_, i64>(0)?,
                            "request_id": r.get::<_, String>(1)?,
                            "phase": r.get::<_, String>(2)?,
                            "status": r.get::<_, i64>(3)?,
                            "requests": r.get::<_, i64>(4)?,
                            "input_tokens": r.get::<_, i64>(5)?,
                            "billed_input_tokens": r.get::<_, i64>(6)?,
                            "output_tokens": r.get::<_, i64>(7)?,
                            "cached_tokens": r.get::<_, i64>(8)?,
                            "cache_hit": r.get::<_, i64>(9)?,
                            "ttft_ms": r.get::<_, f64>(10)?,
                            "gen_ms": r.get::<_, f64>(11)?,
                            "total_ms": r.get::<_, f64>(12)?,
                            "queued_ms": r.get::<_, f64>(13)?,
                            "tokens_per_sec": r.get::<_, f64>(14)?,
                            "prev_hash": r.get::<_, String>(15)?,
                            "row_hash": r.get::<_, String>(16)?,
                        }))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap_or_default()
    }

    /// Run whatever SQL a test likes against the ledger, so it can try to
    /// tamper with rows the way an operator with a sqlite3 prompt would.
    pub async fn sqlite(&self, sql: &'static str) -> Result<(), String> {
        self.state.store.flush().await;
        self.state
            .store
            .read(move |conn| {
                conn.execute_batch(sql)?;
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())
    }
}

/// Start a relay in front of a mock backend, with one alias configured.
///
/// `customise` gets the config before it is written, so a test can set up
/// transforms, quotas or prompts without duplicating this boilerplate.
pub async fn harness<F>(mock: MockConfig, customise: F) -> Harness
where
    F: FnOnce(&mut Config),
{
    let backend = MockBackend::start(mock).await;
    let home = tempfile::tempdir().expect("temp dir");
    let paths = Paths {
        root: home.path().to_path_buf(),
        home: home.path().to_path_buf(),
        config: home.path().join("config/config.json"),
        data: home.path().join("data"),
        logs: home.path().join("data/logs"),
        tokenizers: home.path().join("data/tokenizers"),
    };

    let client_key = "sk-relay-test-key".to_string();
    let mut cfg = Config::default();
    cfg.server.port = 0;
    cfg.dashboard.enabled = false;
    cfg.dashboard.port = 0;
    cfg.logging.file_enabled = false;
    cfg.logging.level = "silent".into();
    cfg.backends.push(chtting_relay::config::Backend {
        id: "mock".into(),
        name: "mock backend".into(),
        base_url: backend.base_url(),
        api_key: "sk-backend-secret".into(),
        max_retries: 0,
        ..Default::default()
    });
    cfg.models.push(chtting_relay::config::Model {
        id: "manukmiberai/creative-writer".into(),
        backend: "mock".into(),
        upstream_model: "Deepseek-v4-flash-0731".into(),
        enabled: true,
        ..Default::default()
    });
    cfg.keys.push(chtting_relay::config::ClientKey {
        id: "key_test".into(),
        label: "test".into(),
        key: client_key.clone(),
        enabled: true,
        models: vec!["*".into()],
        ..Default::default()
    });
    customise(&mut cfg);

    tokio::fs::create_dir_all(paths.config.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&paths.config, serde_json::to_string_pretty(&cfg).unwrap())
        .await
        .unwrap();

    let state = AppState::build(paths, Logger::console(Level::Silent))
        .await
        .expect("relay state builds");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = chtting_relay::server::public::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    Harness {
        addr,
        state,
        client_key,
        backend,
        _home: home,
    }
}

/// Read a streamed response into its list of parsed SSE payloads.
pub async fn read_sse(response: reqwest::Response) -> (Vec<Value>, String) {
    let text = response.text().await.expect("body reads");
    let mut events = Vec::new();
    for block in text.split("\n\n") {
        let Some(data) = block.trim().strip_prefix("data: ") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            events.push(value);
        }
    }
    (events, text)
}

/// Concatenate the content deltas out of a parsed stream.
pub fn stream_text(events: &[Value]) -> String {
    events
        .iter()
        .filter_map(|e| {
            e.get("choices")?
                .as_array()?
                .first()?
                .get("delta")?
                .get("content")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}
