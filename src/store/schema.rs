//! One row per relayed request.
//!
//! The column names are the ones the dashboard's JavaScript already reads, so
//! the vanilla-JS frontend carries over from the Node version untouched.

use rusqlite::types::Value as SqlValue;
use rusqlite::Row;
use serde::{Deserialize, Serialize};

pub const FIELDS: [&str; 56] = [
    "id",
    "ts",
    "day",
    "hour",
    "key_id",
    "key_label",
    "ip",
    "user_agent",
    "public_model",
    "backend_id",
    "upstream_model",
    "endpoint",
    "stream",
    "status",
    "error",
    "finish_reason",
    "ttft_ms",
    "total_ms",
    "gen_ms",
    "prompt_tokens",
    "completion_tokens",
    "total_tokens",
    "cached_tokens",
    "reasoning_tokens",
    "tokens_per_sec",
    "usage_source",
    "tokenizer",
    "exact",
    "local_prompt",
    "local_completion",
    "drift_prompt",
    "drift_completion",
    "req_preview",
    "res_preview",
    "retries",
    "user_prompt_tokens",
    "billed_prompt_tokens",
    "system_prompt_tokens",
    "cache_hit",
    "queued_ms",
    "user_id",
    "reasoning_effort",
    "prompt_id",
    "local_hour",
    "local_weekday",
    "tokenize_ms",
    "inject_ms",
    "bytes_in",
    "bytes_out",
    "bytes_upstream",
    "rss_mb",
    "backend_usd",
    "proxy_usd",
    "profit_usd",
    "price_tiers",
    "target_tps",
];

/// The table itself, and nothing else.
///
/// Kept apart from the indexes on purpose. `CREATE TABLE IF NOT EXISTS` does
/// nothing to a table that already exists, so a database written by an earlier
/// version still has that version's columns when this runs — and an index over
/// a column added later would fail with "no such column" before the migration
/// that adds it ever got a chance. Table first, then the migration, then
/// [`INDEX_SQL`].
pub const CREATE_SQL: &str = "
CREATE TABLE IF NOT EXISTS requests (
  id TEXT PRIMARY KEY,
  ts INTEGER NOT NULL,
  day TEXT NOT NULL,
  hour TEXT NOT NULL,
  key_id TEXT, key_label TEXT, ip TEXT, user_agent TEXT,
  public_model TEXT, backend_id TEXT, upstream_model TEXT, endpoint TEXT,
  stream INTEGER, status INTEGER, error TEXT, finish_reason TEXT,
  ttft_ms REAL, total_ms REAL, gen_ms REAL,
  prompt_tokens INTEGER, completion_tokens INTEGER, total_tokens INTEGER,
  cached_tokens INTEGER, reasoning_tokens INTEGER,
  tokens_per_sec REAL, usage_source TEXT, tokenizer TEXT, exact INTEGER,
  local_prompt INTEGER, local_completion INTEGER,
  drift_prompt INTEGER, drift_completion INTEGER,
  req_preview TEXT, res_preview TEXT, retries INTEGER,
  user_prompt_tokens INTEGER, billed_prompt_tokens INTEGER,
  system_prompt_tokens INTEGER, cache_hit INTEGER, queued_ms REAL,
  user_id TEXT, reasoning_effort TEXT, prompt_id TEXT,
  local_hour INTEGER, local_weekday INTEGER,
  tokenize_ms REAL, inject_ms REAL,
  bytes_in INTEGER, bytes_out INTEGER, bytes_upstream INTEGER, rss_mb REAL,
  backend_usd REAL, proxy_usd REAL, profit_usd REAL, price_tiers TEXT,
  target_tps REAL
);
";

/// The indexes, built once every column they name is certain to exist.
pub const INDEX_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts DESC);
CREATE INDEX IF NOT EXISTS idx_requests_day ON requests(day);
CREATE INDEX IF NOT EXISTS idx_requests_model ON requests(public_model);
CREATE INDEX IF NOT EXISTS idx_requests_key ON requests(key_id);
-- Quota seeding groups by (key_id, day) on every start.
CREATE INDEX IF NOT EXISTS idx_requests_key_day ON requests(key_id, day);
-- The Usage screen groups by caller, which is a different question to by-key.
CREATE INDEX IF NOT EXISTS idx_requests_user ON requests(user_id);
";

pub const INSERT_SQL: &str = "INSERT OR REPLACE INTO requests (
  id, ts, day, hour,
  key_id, key_label, ip, user_agent,
  public_model, backend_id, upstream_model, endpoint,
  stream, status, error, finish_reason,
  ttft_ms, total_ms, gen_ms,
  prompt_tokens, completion_tokens, total_tokens,
  cached_tokens, reasoning_tokens,
  tokens_per_sec, usage_source, tokenizer, exact,
  local_prompt, local_completion, drift_prompt, drift_completion,
  req_preview, res_preview, retries,
  user_prompt_tokens, billed_prompt_tokens, system_prompt_tokens,
  cache_hit, queued_ms,
  user_id,
  reasoning_effort,
  prompt_id,
  local_hour,
  local_weekday,
  tokenize_ms,
  inject_ms,
  bytes_in,
  bytes_out,
  bytes_upstream,
  rss_mb,
  backend_usd,
  proxy_usd,
  profit_usd,
  price_tiers,
  target_tps
) VALUES (
  ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
  ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
  ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30,
  ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40,
  ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48, ?49, ?50,
  ?51, ?52, ?53, ?54, ?55, ?56
)";

/// Columns added after the first release. `CREATE TABLE IF NOT EXISTS` leaves
/// an existing table alone, so an install that predates them needs each one
/// added by hand; SQLite has no `ADD COLUMN IF NOT EXISTS`, so the caller
/// checks `PRAGMA table_info` first.
pub const ADDED_COLUMNS: [(&str, &str); 21] = [
    ("user_prompt_tokens", "INTEGER"),
    ("billed_prompt_tokens", "INTEGER"),
    ("system_prompt_tokens", "INTEGER"),
    ("cache_hit", "INTEGER"),
    ("queued_ms", "REAL"),
    ("user_id", "TEXT"),
    ("reasoning_effort", "TEXT"),
    ("prompt_id", "TEXT"),
    ("local_hour", "INTEGER"),
    ("local_weekday", "INTEGER"),
    ("tokenize_ms", "REAL"),
    ("inject_ms", "REAL"),
    ("bytes_in", "INTEGER"),
    ("bytes_out", "INTEGER"),
    ("bytes_upstream", "INTEGER"),
    ("rss_mb", "REAL"),
    ("backend_usd", "REAL"),
    ("proxy_usd", "REAL"),
    ("profit_usd", "REAL"),
    ("price_tiers", "TEXT"),
    ("target_tps", "REAL"),
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub ts: i64,
    pub day: String,
    pub hour: String,
    pub key_id: String,
    pub key_label: String,
    pub ip: String,
    pub user_agent: String,
    pub public_model: String,
    pub backend_id: String,
    /// The backend's real model name. Recorded locally, never sent to callers.
    pub upstream_model: String,
    pub endpoint: String,
    pub stream: i64,
    pub status: i64,
    pub error: String,
    pub finish_reason: String,
    /// Request start to first content token. Streams only, so the average is
    /// not diluted by calls that never streamed.
    pub ttft_ms: f64,
    pub total_ms: f64,
    /// First token to last token: the generation window.
    pub gen_ms: f64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    /// completion_tokens / gen_ms, i.e. real generation throughput.
    pub tokens_per_sec: f64,
    pub usage_source: String,
    pub tokenizer: String,
    pub exact: i64,
    pub local_prompt: i64,
    pub local_completion: i64,
    pub drift_prompt: i64,
    pub drift_completion: i64,
    pub req_preview: String,
    pub res_preview: String,
    pub retries: i64,
    /// What the caller is charged for: their own prompt, without the system
    /// prompt the relay injected on their behalf. This is the number that
    /// `prompt_tokens` reports back to them.
    pub user_prompt_tokens: i64,
    /// What the backend charged for the same prompt, injection included. The
    /// gap between the two is the relay's own cost of doing business.
    pub billed_prompt_tokens: i64,
    /// The injected prompt itself, as this relay's own tokenizer counts it —
    /// so it answers what the system prompt costs, independently of whether the
    /// backend reported any usage. Negative when the route replaced a longer
    /// system prompt of the caller's with a shorter one.
    pub system_prompt_tokens: i64,
    /// 1 when the backend served part of this prompt from its cache.
    pub cache_hit: i64,
    /// Time spent waiting for a concurrency slot, before any work began.
    pub queued_ms: f64,
    /// Who the caller said they were, from the body's `user` field or the
    /// `x-user-id` header. Passed upstream as the prompt-cache isolation key.
    pub user_id: String,
    /// How hard the caller asked the model to think, normalised: `none`,
    /// `minimal`, `low`, `medium`, `high`, `max`, or `default` when they said
    /// nothing. Drives both the system prompt and the price.
    pub reasoning_effort: String,
    /// Which of the model's system prompt rules answered, if one did.
    pub prompt_id: String,
    /// Hour of day and day of week in the relay's own zone, so a peak-hour
    /// price can be checked against the row that paid it without re-deriving
    /// the timezone months later.
    pub local_hour: u32,
    /// Monday is 0.
    pub local_weekday: u32,
    pub tokenize_ms: f64,
    pub inject_ms: f64,
    /// Request body size on the wire.
    pub bytes_in: u64,
    /// What was written back to the caller.
    pub bytes_out: u64,
    /// What was read from the backend.
    pub bytes_upstream: u64,
    /// Resident memory of the relay when this request finished.
    pub rss_mb: f64,
    /// What the backend charges for this request.
    pub backend_usd: f64,
    /// What the caller is charged.
    pub proxy_usd: f64,
    /// The difference. Negative when a tier discounted below cost.
    pub profit_usd: f64,
    /// The price tiers that applied, comma separated.
    pub price_tiers: String,
    /// The tokens-a-second ceiling this reply was held to, or 0.
    pub target_tps: f64,
}

impl RequestRecord {
    /// Bind values in the same order as `INSERT_SQL`.
    pub fn as_params(&self) -> Vec<SqlValue> {
        vec![
            SqlValue::Text(self.id.clone()),
            SqlValue::Integer(self.ts),
            SqlValue::Text(self.day.clone()),
            SqlValue::Text(self.hour.clone()),
            SqlValue::Text(self.key_id.clone()),
            SqlValue::Text(self.key_label.clone()),
            SqlValue::Text(self.ip.clone()),
            SqlValue::Text(self.user_agent.clone()),
            SqlValue::Text(self.public_model.clone()),
            SqlValue::Text(self.backend_id.clone()),
            SqlValue::Text(self.upstream_model.clone()),
            SqlValue::Text(self.endpoint.clone()),
            SqlValue::Integer(self.stream),
            SqlValue::Integer(self.status),
            SqlValue::Text(self.error.clone()),
            SqlValue::Text(self.finish_reason.clone()),
            SqlValue::Real(self.ttft_ms),
            SqlValue::Real(self.total_ms),
            SqlValue::Real(self.gen_ms),
            SqlValue::Integer(self.prompt_tokens),
            SqlValue::Integer(self.completion_tokens),
            SqlValue::Integer(self.total_tokens),
            SqlValue::Integer(self.cached_tokens),
            SqlValue::Integer(self.reasoning_tokens),
            SqlValue::Real(self.tokens_per_sec),
            SqlValue::Text(self.usage_source.clone()),
            SqlValue::Text(self.tokenizer.clone()),
            SqlValue::Integer(self.exact),
            SqlValue::Integer(self.local_prompt),
            SqlValue::Integer(self.local_completion),
            SqlValue::Integer(self.drift_prompt),
            SqlValue::Integer(self.drift_completion),
            SqlValue::Text(self.req_preview.clone()),
            SqlValue::Text(self.res_preview.clone()),
            SqlValue::Integer(self.retries),
            SqlValue::Integer(self.user_prompt_tokens),
            SqlValue::Integer(self.billed_prompt_tokens),
            SqlValue::Integer(self.system_prompt_tokens),
            SqlValue::Integer(self.cache_hit),
            SqlValue::Real(self.queued_ms),
            SqlValue::Text(self.user_id.clone()),
            SqlValue::Text(self.reasoning_effort.clone()),
            SqlValue::Text(self.prompt_id.clone()),
            SqlValue::Integer(i64::from(self.local_hour)),
            SqlValue::Integer(i64::from(self.local_weekday)),
            SqlValue::Real(self.tokenize_ms),
            SqlValue::Real(self.inject_ms),
            SqlValue::Integer(self.bytes_in as i64),
            SqlValue::Integer(self.bytes_out as i64),
            SqlValue::Integer(self.bytes_upstream as i64),
            SqlValue::Real(self.rss_mb),
            SqlValue::Real(self.backend_usd),
            SqlValue::Real(self.proxy_usd),
            SqlValue::Real(self.profit_usd),
            SqlValue::Text(self.price_tiers.clone()),
            SqlValue::Real(self.target_tps),
        ]
    }
}

/// Read a `requests` row into the JSON the dashboard expects.
pub fn row_to_json(row: &Row<'_>) -> rusqlite::Result<serde_json::Value> {
    let mut out = serde_json::Map::with_capacity(FIELDS.len());
    for (i, name) in FIELDS.iter().enumerate() {
        let value = match row.get_ref(i)? {
            rusqlite::types::ValueRef::Null => serde_json::Value::Null,
            rusqlite::types::ValueRef::Integer(n) => serde_json::Value::from(n),
            rusqlite::types::ValueRef::Real(f) => serde_json::Value::from(f),
            rusqlite::types::ValueRef::Text(t) => {
                serde_json::Value::String(String::from_utf8_lossy(t).into_owned())
            }
            rusqlite::types::ValueRef::Blob(_) => serde_json::Value::Null,
        };
        out.insert((*name).to_string(), value);
    }
    Ok(serde_json::Value::Object(out))
}

/// `SELECT` list in the exact order [`row_to_json`] reads.
pub fn select_columns() -> String {
    FIELDS.join(", ")
}
