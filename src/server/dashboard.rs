//! The local control panel.
//!
//! It binds to `127.0.0.1` by default and is never routed through the tunnel,
//! so the relay can be public while its settings, keys and logs stay on the
//! phone. The endpoints match the ones the vanilla-JS frontend in `public/`
//! already calls, so that frontend carries over from the Node version
//! untouched — it is embedded into the binary at build time.

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{mask_item, unmask_secrets};
use crate::state::AppState;
use crate::store::schema;
use crate::tokenizer::chat::PROFILE_NAMES;
use crate::util::{new_client_key, new_id, percentile, random_hex, round, safe_equal, start_of_today};

/// The dashboard's assets, compiled into the binary so there is no "where did
/// `public/` go" failure mode when the relay is started from another directory.
static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/public");

const COOKIE: &str = "chtting_session";
const COLLECTIONS: [&str; 4] = ["models", "backends", "keys", "systemPrompts"];

#[derive(Default)]
pub struct Sessions {
    tokens: RwLock<HashMap<String, i64>>,
}

pub struct Dashboard {
    pub state: Arc<AppState>,
    sessions: Sessions,
}

pub fn router(state: Arc<AppState>) -> Router {
    let dash = Arc::new(Dashboard {
        state,
        sessions: Sessions::default(),
    });

    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session))
        .route("/api/state", get(app_state))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/models", get(list_collection).post(upsert_collection))
        .route("/api/backends", get(list_collection).post(upsert_collection))
        .route("/api/keys", get(list_collection).post(upsert_collection))
        .route("/api/systemPrompts", get(list_collection).post(upsert_collection))
        .route("/api/{collection}/{id}", delete(delete_item))
        .route("/api/keys/generate", post(generate_key))
        .route("/api/keys/{id}/reveal", get(reveal_key))
        .route("/api/backends/{id}/test", post(test_backend))
        .route("/api/stats/summary", get(stats_summary))
        .route("/api/stats/daily", get(stats_daily))
        .route("/api/stats/hourly", get(stats_hourly))
        .route("/api/stats/by/{column}", get(stats_by))
        .route("/api/requests", get(list_requests))
        .route("/api/requests/{id}", get(get_request))
        .route("/api/maintenance/prune", post(prune))
        .route("/api/logs", get(read_logs))
        .route("/api/tokenizer/inventory", get(tokenizer_inventory))
        .route("/api/tokenizer/count", post(tokenizer_count))
        .route("/api/tokenizer/install", post(tokenizer_install))
        .route("/api/tunnel", get(tunnel_status))
        .route("/api/tunnel/{action}", post(tunnel_action))
        .route("/api/playground", post(playground))
        .fallback(static_files)
        .layer(middleware::from_fn_with_state(dash.clone(), guard))
        .with_state(dash)
}

/* --------------------------------------------------------------- auth -- */

fn password_set(dash: &Dashboard) -> bool {
    !dash.state.config.current().dashboard.password.is_empty()
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn authed(dash: &Dashboard, headers: &HeaderMap) -> bool {
    if !password_set(dash) {
        return true;
    }
    let Some(token) = cookie_value(headers, COOKIE) else {
        return false;
    };
    let now = crate::util::now_ms();
    let mut tokens = dash.sessions.tokens.write();
    match tokens.get(&token) {
        Some(expiry) if *expiry > now => true,
        Some(_) => {
            tokens.remove(&token);
            false
        }
        None => false,
    }
}

/// Everything except sign-in itself needs a session when a password is set.
async fn guard(State(dash): State<Arc<Dashboard>>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let open = path == "/api/login" || path == "/api/session" || !path.starts_with("/api/");
    if !open && !authed(&dash, request.headers()) {
        return error(401, "not signed in");
    }
    next.run(request).await
}

fn error(status: u16, message: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({ "error": { "message": message } })),
    )
        .into_response()
}

async fn login(
    State(dash): State<Arc<Dashboard>>,
    Json(body): Json<Value>,
) -> Response {
    let cfg = dash.state.config.current();
    let expected = &cfg.dashboard.password;
    let given = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if !expected.is_empty() && !safe_equal(given, expected) {
        return error(401, "wrong password");
    }

    let token = random_hex(24);
    let ttl = cfg.dashboard.session_ttl_ms.max(60_000);
    dash.sessions
        .tokens
        .write()
        .insert(token.clone(), crate::util::now_ms() + ttl);

    let cookie = format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        ttl / 1000
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn logout(State(dash): State<Arc<Dashboard>>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, COOKIE) {
        dash.sessions.tokens.write().remove(&token);
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!("{COOKIE}=; HttpOnly; Path=/; Max-Age=0"),
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn session(State(dash): State<Arc<Dashboard>>, headers: HeaderMap) -> Response {
    Json(json!({
        "authenticated": authed(&dash, &headers),
        "passwordSet": password_set(&dash),
    }))
    .into_response()
}

/* -------------------------------------------------------------- state -- */

async fn app_state(State(dash): State<Arc<Dashboard>>) -> Response {
    let state = &dash.state;
    let cfg = state.config.current();
    let rows = state
        .store
        .read(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM requests", [], |r| r.get::<_, i64>(0))?)
        })
        .await
        .unwrap_or(0);

    Json(json!({
        "config": state.config.redacted(),
        "tunnel": state.tunnel.status(),
        "cloudflared": state.tunnel.version().await,
        "store": {
            "kind": state.store.kind(),
            "rows": rows,
            "file": state.store.file().to_string_lossy(),
        },
        "tokenizers": state.counter.registry.inventory().await,
        "profiles": PROFILE_NAMES,
        "paths": {
            "root": state.paths.root.to_string_lossy(),
            "home": state.paths.home.to_string_lossy(),
            "config": state.paths.config.to_string_lossy(),
            "data": state.paths.data.to_string_lossy(),
            "logs": state.paths.logs.to_string_lossy(),
            "tokenizers": state.paths.tokenizers.to_string_lossy(),
        },
        "relay": {
            "listening": true,
            "host": cfg.server.host,
            "port": cfg.server.port,
            "localUrl": format!("http://127.0.0.1:{}", cfg.server.port),
        },
        "runtime": {
            "runtime": "rust",
            "version": env!("CARGO_PKG_VERSION"),
            "rustc": env!("CARGO_PKG_RUST_VERSION"),
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "uptime_s": state.stats.uptime_s(),
            "workers": cfg.server.worker_threads,
            "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "in_flight": state.stats.in_flight.load(std::sync::atomic::Ordering::Relaxed),
            "max_concurrent": cfg.server.max_concurrent_requests,
            "rejected_overload": state.stats.rejected_overload.load(std::sync::atomic::Ordering::Relaxed),
            "termux": std::env::var("PREFIX").is_ok_and(|p| p.contains("com.termux")),
        },
        "today": state.today(),
    }))
    .into_response()
}

/* ------------------------------------------------------------- config -- */

async fn get_config(State(dash): State<Arc<Dashboard>>) -> Response {
    Json(dash.state.config.redacted()).into_response()
}

async fn put_config(State(dash): State<Arc<Dashboard>>, Json(patch): Json<Value>) -> Response {
    let state = &dash.state;
    let current = match serde_json::to_value(&*state.config.current()) {
        Ok(v) => v,
        Err(err) => return error(500, &err.to_string()),
    };
    // A masked value echoed back must not overwrite the real secret.
    let patch = unmask_secrets(&patch, &current);

    match state.config.update(patch).await {
        Ok(next) => {
            state
                .logger
                .set_level(crate::logging::Level::parse(&next.logging.level));
            // Tokenizer rules may have changed; drop cached choices.
            state.counter.registry.invalidate(None);
            Json(state.config.redacted()).into_response()
        }
        Err(err) => error(400, &err.to_string()),
    }
}

async fn list_collection(State(dash): State<Arc<Dashboard>>, request: Request) -> Response {
    let name = collection_name(request.uri().path());
    Json(dash.state.config.redacted()[name].clone()).into_response()
}

async fn upsert_collection(
    State(dash): State<Arc<Dashboard>>,
    request: Request,
) -> Response {
    let name = collection_name(request.uri().path()).to_string();
    let body = match read_json(request).await {
        Ok(v) => v,
        Err(response) => return response,
    };

    let state = &dash.state;
    let current = serde_json::to_value(&*state.config.current()).unwrap_or(Value::Null);
    let existing = current
        .get(&name)
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            let id = body.get("id").and_then(|v| v.as_str())?;
            arr.iter()
                .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(id))
        })
        .cloned()
        .unwrap_or(Value::Null);

    let item = unmask_secrets(&body, &existing);
    match state.config.upsert(&name, item).await {
        Ok(saved) => Json(json!({ "ok": true, "item": mask_item(&saved) })).into_response(),
        Err(err) => error(400, &err.to_string()),
    }
}

async fn delete_item(
    State(dash): State<Arc<Dashboard>>,
    Path((collection, id)): Path<(String, String)>,
) -> Response {
    if !COLLECTIONS.contains(&collection.as_str()) {
        return error(404, "no such collection");
    }
    match dash.state.config.remove(&collection, &id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(err) => error(400, &err.to_string()),
    }
}

fn collection_name(path: &str) -> &str {
    path.trim_start_matches("/api/")
}

async fn read_json(request: Request) -> Result<Value, Response> {
    let bytes = axum::body::to_bytes(request.into_body(), 8 * 1024 * 1024)
        .await
        .map_err(|err| error(413, &format!("body too large or unreadable: {err}")))?;
    serde_json::from_slice(&bytes).map_err(|err| error(400, &format!("invalid JSON: {err}")))
}

async fn generate_key(State(dash): State<Arc<Dashboard>>, Json(body): Json<Value>) -> Response {
    let key = new_client_key();
    let item = json!({
        "id": new_id("key"),
        "label": body.get("label").and_then(|v| v.as_str()).unwrap_or("new key"),
        "key": key,
        "enabled": true,
        "models": body.get("models").cloned().unwrap_or_else(|| json!(["*"])),
        "quota": body.get("quota").cloned().unwrap_or_else(|| json!({
            "requestsPerDay": 0, "tokensPerDay": 0, "requestsPerMinute": 0
        })),
    });

    match dash.state.config.upsert("keys", item).await {
        Ok(saved) => {
            let mut masked = mask_item(&saved);
            if let Some(map) = masked.as_object_mut() {
                // Shown in the clear exactly once; afterwards only the mask.
                map.insert("key".into(), Value::String(key));
            }
            Json(json!({ "ok": true, "item": masked })).into_response()
        }
        Err(err) => error(400, &err.to_string()),
    }
}

async fn reveal_key(State(dash): State<Arc<Dashboard>>, Path(id): Path<String>) -> Response {
    match dash.state.config.current().keys.iter().find(|k| k.id == id) {
        Some(key) => Json(json!({ "key": key.key })).into_response(),
        None => error(404, "key not found"),
    }
}

async fn test_backend(State(dash): State<Arc<Dashboard>>, Path(id): Path<String>) -> Response {
    let cfg = dash.state.config.current();
    let Some(backend) = cfg.find_backend(&id) else {
        return error(404, "backend not found");
    };

    let base = backend.base_url.trim_end_matches('/');
    let versioned = base
        .rsplit('/')
        .next()
        .is_some_and(|last| last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit()));
    let url = if versioned {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };

    let started = std::time::Instant::now();
    let mut req = dash
        .state
        .upstream
        .client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(20));
    if !backend.api_key.is_empty() {
        req = if backend.kind == "anthropic" {
            req.header("x-api-key", &backend.api_key)
                .header("anthropic-version", "2023-06-01")
        } else {
            req.header("authorization", format!("Bearer {}", backend.api_key))
        };
    }

    let result = match req.send().await {
        Ok(res) => {
            let status = res.status().as_u16();
            let ok = res.status().is_success();
            let text = res.text().await.unwrap_or_default();
            let models: Vec<String> = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("data").and_then(|d| d.as_array()).cloned())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
                        .take(200)
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "ok": ok,
                "status": status,
                "ms": started.elapsed().as_millis(),
                "models": models,
                "body": if ok { Value::Null } else { Value::String(text.chars().take(400).collect()) },
                "url": url,
            })
        }
        Err(err) => json!({
            "ok": false, "status": 0, "ms": started.elapsed().as_millis(),
            "error": err.to_string(), "url": url,
        }),
    };
    Json(result).into_response()
}

/* ---------------------------------------------------------- statistics -- */

#[derive(serde::Deserialize)]
struct RangeQuery {
    range: Option<String>,
    days: Option<i64>,
    hours: Option<i64>,
    limit: Option<i64>,
}

fn range_start(range: &str) -> i64 {
    let Some(unit) = range.chars().last() else {
        return 0;
    };
    let Ok(n) = range[..range.len() - 1].parse::<i64>() else {
        return 0;
    };
    let ms = match unit {
        'h' => 3_600_000,
        'd' => 86_400_000,
        _ => return 0,
    };
    crate::util::now_ms() - n * ms
}

const SUMMARY_SQL: &str = "
    SELECT
      COUNT(*), COUNT(DISTINCT key_id),
      COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0),
      COALESCE(SUM(total_tokens),0), COALESCE(SUM(cached_tokens),0),
      COALESCE(SUM(reasoning_tokens),0),
      SUM(CASE WHEN status >= 400 OR status = 0 THEN 1 ELSE 0 END),
      SUM(CASE WHEN stream = 1 THEN 1 ELSE 0 END),
      AVG(NULLIF(ttft_ms,0)), AVG(NULLIF(total_ms,0)), AVG(NULLIF(tokens_per_sec,0))
    FROM requests WHERE ts >= ?1 AND ts <= ?2";

/// The aggregate row `SUMMARY_SQL` produces, named so the columns cannot be
/// silently transposed.
struct SummaryRow {
    requests: i64,
    users: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    cached_tokens: i64,
    reasoning_tokens: i64,
    errors: i64,
    streamed: i64,
    avg_ttft: Option<f64>,
    avg_total: Option<f64>,
    avg_tps: Option<f64>,
}

fn summary(conn: &rusqlite::Connection, since: i64, until: i64) -> anyhow::Result<Value> {
    let row = conn.query_row(SUMMARY_SQL, [since, until], |r| {
        Ok(SummaryRow {
            requests: r.get(0)?,
            users: r.get(1)?,
            prompt_tokens: r.get(2)?,
            completion_tokens: r.get(3)?,
            total_tokens: r.get(4)?,
            cached_tokens: r.get(5)?,
            reasoning_tokens: r.get(6)?,
            errors: r.get(7)?,
            streamed: r.get(8)?,
            avg_ttft: r.get(9)?,
            avg_total: r.get(10)?,
            avg_tps: r.get(11)?,
        })
    })?;

    // Percentiles need the raw values; cap the scan so a huge table cannot
    // make the dashboard hang.
    let mut stmt = conn.prepare(
        "SELECT ttft_ms, total_ms, tokens_per_sec FROM requests
         WHERE ts >= ?1 AND ts <= ?2 AND status < 400 AND status > 0
         ORDER BY ts DESC LIMIT 20000",
    )?;
    let mut ttft = Vec::new();
    let mut total = Vec::new();
    let mut tps = Vec::new();
    for row in stmt.query_map([since, until], |r| {
        Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
    })? {
        let (a, b, c) = row?;
        if a > 0.0 { ttft.push(a); }
        if b > 0.0 { total.push(b); }
        if c > 0.0 { tps.push(c); }
    }
    for v in [&mut ttft, &mut total, &mut tps] {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }

    let SummaryRow { requests, errors, .. } = row;
    Ok(json!({
        "requests": requests,
        "users": row.users,
        "errors": errors,
        "streamed": row.streamed,
        "error_rate": if requests > 0 { round(errors as f64 / requests as f64 * 100.0, 2) } else { 0.0 },
        "prompt_tokens": row.prompt_tokens,
        "completion_tokens": row.completion_tokens,
        "total_tokens": row.total_tokens,
        "cached_tokens": row.cached_tokens,
        "reasoning_tokens": row.reasoning_tokens,
        "avg_ttft_ms": round(row.avg_ttft.unwrap_or(0.0), 1),
        "avg_total_ms": round(row.avg_total.unwrap_or(0.0), 1),
        "avg_tps": round(row.avg_tps.unwrap_or(0.0), 2),
        "p50_ttft_ms": round(percentile(&ttft, 50.0), 1),
        "p95_ttft_ms": round(percentile(&ttft, 95.0), 1),
        "p50_total_ms": round(percentile(&total, 50.0), 1),
        "p95_total_ms": round(percentile(&total, 95.0), 1),
        "p50_tps": round(percentile(&tps, 50.0), 2),
        "p95_tps": round(percentile(&tps, 95.0), 2),
    }))
}

async fn stats_summary(
    State(dash): State<Arc<Dashboard>>,
    Query(q): Query<RangeQuery>,
) -> Response {
    let range = q.range.unwrap_or_else(|| "30d".into());
    let since = range_start(&range);
    let today = start_of_today(&dash.state.config.current().tz());

    let result = dash
        .state
        .store
        .read(move |conn| {
            Ok(json!({
                "range": range,
                "since": since,
                "all": summary(conn, 0, i64::MAX)?,
                "window": summary(conn, since, i64::MAX)?,
                "today": summary(conn, today, i64::MAX)?,
            }))
        })
        .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(err) => error(500, &err.to_string()),
    }
}

async fn stats_daily(State(dash): State<Arc<Dashboard>>, Query(q): Query<RangeQuery>) -> Response {
    let days = q.days.unwrap_or(30).clamp(1, 3650);
    let result = dash
        .state
        .store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT day, COUNT(*), COUNT(DISTINCT key_id),
                        COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0),
                        COALESCE(SUM(total_tokens),0),
                        SUM(CASE WHEN status >= 400 OR status = 0 THEN 1 ELSE 0 END),
                        AVG(NULLIF(ttft_ms,0)), AVG(NULLIF(tokens_per_sec,0))
                 FROM requests GROUP BY day ORDER BY day DESC LIMIT ?1",
            )?;
            let mut rows: Vec<Value> = stmt
                .query_map([days], |r| {
                    Ok(json!({
                        "day": r.get::<_, String>(0)?,
                        "requests": r.get::<_, i64>(1)?,
                        "users": r.get::<_, i64>(2)?,
                        "prompt_tokens": r.get::<_, i64>(3)?,
                        "completion_tokens": r.get::<_, i64>(4)?,
                        "total_tokens": r.get::<_, i64>(5)?,
                        "errors": r.get::<_, i64>(6)?,
                        "avg_ttft": round(r.get::<_, Option<f64>>(7)?.unwrap_or(0.0), 1),
                        "avg_tps": round(r.get::<_, Option<f64>>(8)?.unwrap_or(0.0), 2),
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.reverse();
            Ok(Value::Array(rows))
        })
        .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(err) => error(500, &err.to_string()),
    }
}

async fn stats_hourly(State(dash): State<Arc<Dashboard>>, Query(q): Query<RangeQuery>) -> Response {
    let hours = q.hours.unwrap_or(48).clamp(1, 8760);
    let result = dash
        .state
        .store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT hour, COUNT(*), COALESCE(SUM(total_tokens),0),
                        AVG(NULLIF(ttft_ms,0)), AVG(NULLIF(tokens_per_sec,0))
                 FROM requests GROUP BY hour ORDER BY hour DESC LIMIT ?1",
            )?;
            let mut rows: Vec<Value> = stmt
                .query_map([hours], |r| {
                    Ok(json!({
                        "hour": r.get::<_, String>(0)?,
                        "requests": r.get::<_, i64>(1)?,
                        "total_tokens": r.get::<_, i64>(2)?,
                        "avg_ttft": round(r.get::<_, Option<f64>>(3)?.unwrap_or(0.0), 1),
                        "avg_tps": round(r.get::<_, Option<f64>>(4)?.unwrap_or(0.0), 2),
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.reverse();
            Ok(Value::Array(rows))
        })
        .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(err) => error(500, &err.to_string()),
    }
}

async fn stats_by(
    State(dash): State<Arc<Dashboard>>,
    Path(column): Path<String>,
    Query(q): Query<RangeQuery>,
) -> Response {
    // Whitelisted, never interpolated from user input directly.
    let sql_column = match column.as_str() {
        "model" => "public_model",
        "key" => "key_id",
        "backend" => "backend_id",
        "upstream" => "upstream_model",
        _ => return error(400, &format!("cannot group by \"{column}\"")),
    };
    let since = range_start(&q.range.unwrap_or_else(|| "30d".into()));
    let limit = q.limit.unwrap_or(50).clamp(1, 500);

    let result = dash
        .state
        .store
        .read(move |conn| {
            let sql = format!(
                "SELECT {sql_column} AS name, COUNT(*), COUNT(DISTINCT key_id),
                        COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0),
                        COALESCE(SUM(total_tokens),0),
                        SUM(CASE WHEN status >= 400 OR status = 0 THEN 1 ELSE 0 END),
                        AVG(NULLIF(ttft_ms,0)), AVG(NULLIF(total_ms,0)), AVG(NULLIF(tokens_per_sec,0))
                 FROM requests WHERE ts >= ?1
                 GROUP BY {sql_column} ORDER BY COUNT(*) DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<Value> = stmt
                .query_map([since, limit], |r| {
                    Ok(json!({
                        "name": r.get::<_, String>(0)?,
                        "requests": r.get::<_, i64>(1)?,
                        "users": r.get::<_, i64>(2)?,
                        "prompt_tokens": r.get::<_, i64>(3)?,
                        "completion_tokens": r.get::<_, i64>(4)?,
                        "total_tokens": r.get::<_, i64>(5)?,
                        "errors": r.get::<_, i64>(6)?,
                        "avg_ttft": round(r.get::<_, Option<f64>>(7)?.unwrap_or(0.0), 1),
                        "avg_total": round(r.get::<_, Option<f64>>(8)?.unwrap_or(0.0), 1),
                        "avg_tps": round(r.get::<_, Option<f64>>(9)?.unwrap_or(0.0), 2),
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Value::Array(rows))
        })
        .await;

    match result {
        Ok(Value::Array(mut rows)) => {
            if sql_column == "key_id" {
                // Show the human label next to the opaque key id.
                let cfg = dash.state.config.current();
                for row in &mut rows {
                    let label = row
                        .get("name")
                        .and_then(|v| v.as_str())
                        .and_then(|id| cfg.keys.iter().find(|k| k.id == id))
                        .map(|k| if k.label.is_empty() { k.id.clone() } else { k.label.clone() });
                    if let (Some(map), Some(label)) = (row.as_object_mut(), label) {
                        map.insert("label".into(), Value::String(label));
                    }
                }
            }
            Json(Value::Array(rows)).into_response()
        }
        Ok(other) => Json(other).into_response(),
        Err(err) => error(500, &err.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct RequestsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    model: Option<String>,
    key: Option<String>,
    status: Option<String>,
    day: Option<String>,
    q: Option<String>,
    since: Option<i64>,
}

async fn list_requests(
    State(dash): State<Arc<Dashboard>>,
    Query(q): Query<RequestsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let result = dash
        .state
        .store
        .read(move |conn| {
            let mut clauses: Vec<String> = Vec::new();
            let mut args: Vec<rusqlite::types::Value> = Vec::new();

            if let Some(model) = q.model.filter(|s| !s.is_empty()) {
                clauses.push("public_model = ?".into());
                args.push(model.into());
            }
            if let Some(key) = q.key.filter(|s| !s.is_empty()) {
                clauses.push("key_id = ?".into());
                args.push(key.into());
            }
            if let Some(day) = q.day.filter(|s| !s.is_empty()) {
                clauses.push("day = ?".into());
                args.push(day.into());
            }
            if let Some(since) = q.since {
                clauses.push("ts >= ?".into());
                args.push(since.into());
            }
            match q.status.as_deref() {
                Some("error") => clauses.push("(status >= 400 OR status = 0)".into()),
                Some("ok") => clauses.push("(status >= 200 AND status < 400)".into()),
                _ => {}
            }
            if let Some(search) = q.q.filter(|s| !s.is_empty()) {
                clauses.push("(req_preview LIKE ? OR res_preview LIKE ? OR error LIKE ?)".into());
                let like = format!("%{search}%");
                args.push(like.clone().into());
                args.push(like.clone().into());
                args.push(like.into());
            }

            let where_clause = if clauses.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", clauses.join(" AND "))
            };

            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM requests {where_clause}"),
                rusqlite::params_from_iter(args.iter()),
                |r| r.get(0),
            )?;

            let sql = format!(
                "SELECT {} FROM requests {where_clause} ORDER BY ts DESC LIMIT ? OFFSET ?",
                schema::select_columns()
            );
            let mut page_args = args.clone();
            page_args.push(limit.into());
            page_args.push(offset.into());

            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<Value> = stmt
                .query_map(rusqlite::params_from_iter(page_args.iter()), schema::row_to_json)?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            Ok(json!({ "rows": rows, "total": total }))
        })
        .await;

    match result {
        Ok(v) => Json(v).into_response(),
        Err(err) => error(500, &err.to_string()),
    }
}

async fn get_request(State(dash): State<Arc<Dashboard>>, Path(id): Path<String>) -> Response {
    let result = dash
        .state
        .store
        .read(move |conn| {
            let sql = format!(
                "SELECT {} FROM requests WHERE id = ?1",
                schema::select_columns()
            );
            let row = conn
                .query_row(&sql, [&id], schema::row_to_json)
                .ok();
            Ok(row)
        })
        .await;
    match result {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => error(404, "request not found"),
        Err(err) => error(500, &err.to_string()),
    }
}

async fn prune(State(dash): State<Arc<Dashboard>>) -> Response {
    let days = dash.state.config.current().logging.retention_days;
    if days <= 0 {
        return Json(json!({ "ok": true, "removed": 0 })).into_response();
    }
    let cutoff = crate::util::now_ms() - days * 86_400_000;
    let result = dash
        .state
        .store
        .read(move |conn| {
            let removed: i64 =
                conn.query_row("SELECT COUNT(*) FROM requests WHERE ts < ?1", [cutoff], |r| {
                    r.get(0)
                })?;
            conn.execute("DELETE FROM requests WHERE ts < ?1", [cutoff])?;
            if removed > 0 {
                conn.execute_batch("VACUUM;")?;
            }
            Ok(removed)
        })
        .await;
    match result {
        Ok(removed) => Json(json!({ "ok": true, "removed": removed })).into_response(),
        Err(err) => error(500, &err.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct LogQuery {
    lines: Option<usize>,
}

async fn read_logs(State(dash): State<Arc<Dashboard>>, Query(q): Query<LogQuery>) -> Response {
    let Some(file) = dash.state.logger.file() else {
        return text("file logging is disabled");
    };
    let wanted = q.lines.unwrap_or(300).clamp(1, 10_000);
    match tokio::fs::read_to_string(file).await {
        Ok(raw) => {
            let lines: Vec<&str> = raw.lines().collect();
            let start = lines.len().saturating_sub(wanted);
            text(&lines[start..].join("\n"))
        }
        Err(err) => text(&format!("could not read log: {err}")),
    }
}

fn text(body: &str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.to_string(),
    )
        .into_response()
}

/* ---------------------------------------------------------- tokenizer -- */

async fn tokenizer_inventory(State(dash): State<Arc<Dashboard>>) -> Response {
    Json(dash.state.counter.registry.inventory().await).into_response()
}

async fn tokenizer_count(State(dash): State<Arc<Dashboard>>, Json(body): Json<Value>) -> Response {
    let state = &dash.state;
    let cfg = state.config.current();
    let asked_for = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let route = cfg.find_model(asked_for);

    // Resolve exactly as the relay does: the *backend* model name drives the
    // rules, and an explicitly pinned vocabulary wins over them. Feeding a
    // pinned name back through the rules is the bug this guards against.
    let model = route
        .map(|r| r.upstream_model.clone())
        .unwrap_or_else(|| asked_for.to_string());
    let pinned_tokenizer = body
        .get("tokenizer")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| route.map(|r| r.tokenizer.clone()))
        .unwrap_or_default();
    let pinned_profile = body
        .get("profile")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| route.map(|r| r.chat_profile.clone()))
        .unwrap_or_default();

    let resolved = state
        .counter
        .resolve(&cfg, &model, &pinned_tokenizer, &pinned_profile);

    if body.get("messages").is_some_and(|v| v.is_array()) {
        let shape = json!({
            "messages": body.get("messages").cloned().unwrap_or(Value::Null),
            "tools": body.get("tools").cloned().unwrap_or(Value::Null),
        });
        let counted = state
            .counter
            .count_request(&shape, &resolved, &cfg.tokenizer.image_defaults)
            .await;
        return Json(json!({
            "mode": "messages",
            "total": counted.total,
            "breakdown": counted.breakdown,
            "exact": counted.exact,
            "tokenizer": counted.tokenizer,
            "profile": resolved.profile,
            "resolved": {"tokenizer": resolved.tokenizer, "profile": resolved.profile},
        }))
        .into_response();
    }

    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let limit = body
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(2000)
        .clamp(1, 20_000) as usize;
    let mut detail = state.counter.pieces(text, &resolved, limit).await;
    if let Some(map) = detail.as_object_mut() {
        map.insert("mode".into(), Value::String("text".into()));
        map.insert(
            "resolved".into(),
            json!({"tokenizer": resolved.tokenizer, "profile": resolved.profile}),
        );
    }
    Json(detail).into_response()
}

/// Download a vocabulary straight into `data/tokenizers/`.
///
/// The Node version shelled out to a helper script; doing it natively is why
/// the Rust build needs no Node on the phone at all.
async fn tokenizer_install(State(dash): State<Arc<Dashboard>>, Json(body): Json<Value>) -> Response {
    let state = &dash.state;
    let str_field = |key: &str| {
        body.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let (url, name) = if let Some(repo) = str_field("hf") {
        let name = str_field("as").unwrap_or_else(|| {
            repo.rsplit('/').next().unwrap_or(&repo).to_lowercase()
        });
        (crate::tokenizer::registry::hf_url(&repo), name)
    } else if let Some(url) = str_field("url") {
        match str_field("as") {
            Some(name) => (url, name),
            None => return error(400, "--url needs a name to save it as"),
        }
    } else if let Some(preset) = str_field("name") {
        match crate::tokenizer::registry::HF_PRESETS
            .iter()
            .find(|(n, _)| *n == preset)
        {
            Some((n, repo)) => (crate::tokenizer::registry::hf_url(repo), (*n).to_string()),
            None if crate::tokenizer::registry::BUILTIN_TIKTOKEN.contains(&preset.as_str()) => {
                return Json(json!({
                    "ok": true,
                    "output": format!("\"{preset}\" is compiled into the relay; nothing to download."),
                    "inventory": state.counter.registry.inventory().await,
                }))
                .into_response();
            }
            None => return error(400, &format!("unknown tokenizer \"{preset}\"")),
        }
    } else {
        return error(400, "pass name, hf or url");
    };

    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return error(400, "the name may only contain letters, digits, dot, dash and underscore");
    }

    let dir = state.counter.registry.dir().to_path_buf();
    if let Err(err) = tokio::fs::create_dir_all(&dir).await {
        return error(500, &format!("cannot create {}: {err}", dir.display()));
    }

    let started = std::time::Instant::now();
    let response = match state
        .upstream
        .client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => {
            return Json(json!({
                "ok": false,
                "output": format!("{url}\nHTTP {}", res.status().as_u16()),
                "inventory": state.counter.registry.inventory().await,
            }))
            .into_response()
        }
        Err(err) => {
            return Json(json!({
                "ok": false,
                "output": format!("{url}\n{err}"),
                "inventory": state.counter.registry.inventory().await,
            }))
            .into_response()
        }
    };

    let bytes = match response.bytes().await {
        Ok(b) => b,
        Err(err) => return error(502, &format!("download failed: {err}")),
    };

    // Validate before saving: a half-downloaded or HTML error page must not
    // land in the vocabulary directory looking installed.
    if tokenizers::Tokenizer::from_bytes(&bytes).is_err() {
        return Json(json!({
            "ok": false,
            "output": format!("{url}\ndownloaded {} bytes, but it is not a valid tokenizer.json", bytes.len()),
            "inventory": state.counter.registry.inventory().await,
        }))
        .into_response();
    }

    let path = dir.join(format!("{name}.tokenizer.json"));
    if let Err(err) = tokio::fs::write(&path, &bytes).await {
        return error(500, &format!("cannot write {}: {err}", path.display()));
    }
    state.counter.registry.invalidate(Some(&name));

    Json(json!({
        "ok": true,
        "output": format!(
            "{url}\nsaved {} ({} bytes) in {:?}",
            path.display(), bytes.len(), started.elapsed()
        ),
        "inventory": state.counter.registry.inventory().await,
    }))
    .into_response()
}

/* ------------------------------------------------------------- tunnel -- */

async fn tunnel_status(State(dash): State<Arc<Dashboard>>) -> Response {
    let mut status = dash.state.tunnel.status();
    if let Some(map) = status.as_object_mut() {
        map.insert("cloudflared".into(), dash.state.tunnel.version().await);
    }
    Json(status).into_response()
}

async fn tunnel_action(State(dash): State<Arc<Dashboard>>, Path(action): Path<String>) -> Response {
    let tunnel = &dash.state.tunnel;
    let result = match action.as_str() {
        "start" => tunnel.start().await,
        "stop" => tunnel.stop().await,
        "restart" => tunnel.restart().await,
        other => return error(400, &format!("unknown tunnel action \"{other}\"")),
    };
    match result {
        Ok(status) => Json(status).into_response(),
        Err(err) => error(400, &err.to_string()),
    }
}

/* --------------------------------------------------------- playground -- */

/// Send a real call through the relay's own public port, so the playground
/// exercises the same path a client would.
async fn playground(State(dash): State<Arc<Dashboard>>, Json(body): Json<Value>) -> Response {
    let state = &dash.state;
    let cfg = state.config.current();
    let key = cfg.keys.iter().find(|k| k.enabled);
    if cfg.security.require_client_key && key.is_none() {
        return error(
            400,
            "create a client key first, or turn off security.requireClientKey",
        );
    }

    let mut payload = body.clone();
    if let Some(map) = payload.as_object_mut() {
        map.insert("stream".into(), Value::Bool(false));
    }

    let started = std::time::Instant::now();
    let mut req = state
        .upstream
        .client()
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", cfg.server.port))
        .timeout(std::time::Duration::from_secs(300))
        .json(&payload);
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {}", key.key));
    }

    match req.send().await {
        Ok(res) => {
            let status = res.status().as_u16();
            let body: Value = res.json().await.unwrap_or(Value::Null);
            Json(json!({
                "status": status,
                "ms": started.elapsed().as_millis(),
                "body": body,
            }))
            .into_response()
        }
        Err(err) => Json(json!({
            "status": 0,
            "ms": started.elapsed().as_millis(),
            "error": err.to_string(),
        }))
        .into_response(),
    }
}

/* ------------------------------------------------------------- static -- */

async fn static_files(request: Request) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // A single-page app: unknown paths fall back to the shell.
    let file = ASSETS
        .get_file(path)
        .or_else(|| ASSETS.get_file("index.html"));

    match file {
        Some(file) => {
            let mime = match file.path().extension().and_then(|e| e.to_str()) {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "text/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("json") => "application/json; charset=utf-8",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("ico") => "image/x-icon",
                _ => "application/octet-stream",
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime)],
                Body::from(file.contents()),
            )
                .into_response()
        }
        None => error(404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_into_a_start_timestamp() {
        let now = crate::util::now_ms();
        let day = range_start("1d");
        assert!((now - day - 86_400_000).abs() < 1000);
        let hour = range_start("6h");
        assert!((now - hour - 6 * 3_600_000).abs() < 1000);
        // Anything unparseable means "everything".
        assert_eq!(range_start("all"), 0);
        assert_eq!(range_start(""), 0);
    }

    #[test]
    fn the_dashboard_assets_are_embedded_in_the_binary() {
        assert!(
            ASSETS.get_file("index.html").is_some(),
            "index.html must be compiled in"
        );
        assert!(ASSETS.get_file("js/app.js").is_some());
        assert!(ASSETS.get_file("css/app.css").is_some());
    }

    #[test]
    fn a_cookie_header_is_parsed_without_a_library() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            header::HeaderValue::from_static("other=1; chtting_session=abc123; last=2"),
        );
        assert_eq!(cookie_value(&headers, COOKIE), Some("abc123".into()));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }
}
