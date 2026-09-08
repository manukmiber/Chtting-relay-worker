//! The accounting record.
//!
//! Every number the relay reports about usage comes from here, and nothing
//! here can be edited after the fact:
//!
//! * The table takes `INSERT` only — `UPDATE` and `DELETE` are refused by
//!   SQLite triggers, so no code path in this program (or any other process
//!   holding the file open) can quietly restate a figure.
//! * Each row carries the hash of the row before it, so tampering that goes
//!   around SQLite entirely — dropping the triggers, editing the file — leaves
//!   a break in the chain that [`verify`] finds and points at.
//!
//! Rows come in two phases. The `input` row is written as soon as the request
//! is handed to the backend, so tokens the caller has already spent survive a
//! crash, a hang-up, or a backend that never answers. The `final` row adds
//! what could only be known at the end: output tokens, cache hits and timing.
//! The two never restate the same figure, so a plain `SUM` over the table is
//! the right answer.

use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The hash a chain starts from.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub const CREATE_SQL: &str = "
CREATE TABLE IF NOT EXISTS usage_ledger (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id TEXT NOT NULL,
  phase TEXT NOT NULL,
  ts INTEGER NOT NULL,
  day TEXT NOT NULL,
  hour TEXT NOT NULL,
  key_id TEXT NOT NULL,
  public_model TEXT NOT NULL,
  backend_id TEXT NOT NULL,
  status INTEGER NOT NULL,
  requests INTEGER NOT NULL,
  input_tokens INTEGER NOT NULL,
  billed_input_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  cached_tokens INTEGER NOT NULL,
  reasoning_tokens INTEGER NOT NULL,
  cache_hit INTEGER NOT NULL,
  ttft_ms REAL NOT NULL,
  gen_ms REAL NOT NULL,
  total_ms REAL NOT NULL,
  queued_ms REAL NOT NULL,
  tokens_per_sec REAL NOT NULL,
  prev_hash TEXT NOT NULL,
  row_hash TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ledger_day ON usage_ledger(day);
CREATE INDEX IF NOT EXISTS idx_ledger_ts ON usage_ledger(ts DESC);
CREATE INDEX IF NOT EXISTS idx_ledger_key ON usage_ledger(key_id);
CREATE INDEX IF NOT EXISTS idx_ledger_model ON usage_ledger(public_model);
CREATE INDEX IF NOT EXISTS idx_ledger_request ON usage_ledger(request_id);

-- The point of the whole table. SQLite runs these for every statement, from
-- this process or any other, so a recorded figure has no legitimate way to
-- change.
CREATE TRIGGER IF NOT EXISTS usage_ledger_no_update
BEFORE UPDATE ON usage_ledger
BEGIN
  SELECT RAISE(ABORT, 'usage_ledger is append-only: a recorded number cannot be changed');
END;
CREATE TRIGGER IF NOT EXISTS usage_ledger_no_delete
BEFORE DELETE ON usage_ledger
BEGIN
  SELECT RAISE(ABORT, 'usage_ledger is append-only: a recorded number cannot be deleted');
END;
";

pub const INSERT_SQL: &str = "INSERT INTO usage_ledger (
  request_id, phase, ts, day, hour, key_id, public_model, backend_id, status,
  requests, input_tokens, billed_input_tokens, output_tokens, cached_tokens,
  reasoning_tokens, cache_hit, ttft_ms, gen_ms, total_ms, queued_ms,
  tokens_per_sec, prev_hash, row_hash
) VALUES (
  ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
  ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23
)";

/// Which half of a request a row accounts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Written once the request is on its way upstream.
    Input,
    /// Written when the request is over, however it ended.
    Final,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Input => "input",
            Phase::Final => "final",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub request_id: String,
    pub phase: Phase,
    pub ts: i64,
    pub day: String,
    pub hour: String,
    pub key_id: String,
    pub public_model: String,
    pub backend_id: String,
    pub status: i64,
    /// 1 on the row that first accounts for this request, 0 on the other, so
    /// summing the column counts each request exactly once.
    pub requests: i64,
    /// Charged to the caller: their own prompt, without the relay's injection.
    pub input_tokens: i64,
    /// What the backend charged for the same prompt, injection included.
    pub billed_input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    /// 1 when the backend served part of this prompt from its cache.
    pub cache_hit: i64,
    pub ttft_ms: f64,
    pub gen_ms: f64,
    pub total_ms: f64,
    /// How long this request waited for a slot before any work started.
    pub queued_ms: f64,
    pub tokens_per_sec: f64,
}

impl Default for LedgerEntry {
    fn default() -> Self {
        Self {
            request_id: String::new(),
            phase: Phase::Final,
            ts: 0,
            day: String::new(),
            hour: String::new(),
            key_id: String::new(),
            public_model: String::new(),
            backend_id: String::new(),
            status: 0,
            requests: 0,
            input_tokens: 0,
            billed_input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            reasoning_tokens: 0,
            cache_hit: 0,
            ttft_ms: 0.0,
            gen_ms: 0.0,
            total_ms: 0.0,
            queued_ms: 0.0,
            tokens_per_sec: 0.0,
        }
    }
}

impl LedgerEntry {
    /// The bytes this row is hashed over.
    ///
    /// Every field that carries meaning is in here, in a fixed order, with
    /// floats at a fixed precision so the same row hashes the same way on any
    /// machine. `seq` is left out because SQLite assigns it after the hash is
    /// computed; position in the chain is already pinned by `prev_hash`.
    fn canonical(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:.3}|{:.3}|{:.3}|{:.3}|{:.3}",
            self.request_id,
            self.phase.as_str(),
            self.ts,
            self.day,
            self.hour,
            self.key_id,
            self.public_model,
            self.backend_id,
            self.status,
            self.requests,
            self.input_tokens,
            self.billed_input_tokens,
            self.output_tokens,
            self.cached_tokens,
            self.reasoning_tokens,
            self.cache_hit,
            self.ttft_ms,
            self.gen_ms,
            self.total_ms,
            self.queued_ms,
            self.tokens_per_sec,
        )
    }

    pub fn hash_with(&self, prev: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(prev.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.canonical().as_bytes());
        hex(&hasher.finalize())
    }

    /// Bind values in the order [`INSERT_SQL`] expects.
    pub fn as_params(&self, prev_hash: &str, row_hash: &str) -> Vec<SqlValue> {
        vec![
            SqlValue::Text(self.request_id.clone()),
            SqlValue::Text(self.phase.as_str().to_string()),
            SqlValue::Integer(self.ts),
            SqlValue::Text(self.day.clone()),
            SqlValue::Text(self.hour.clone()),
            SqlValue::Text(self.key_id.clone()),
            SqlValue::Text(self.public_model.clone()),
            SqlValue::Text(self.backend_id.clone()),
            SqlValue::Integer(self.status),
            SqlValue::Integer(self.requests),
            SqlValue::Integer(self.input_tokens),
            SqlValue::Integer(self.billed_input_tokens),
            SqlValue::Integer(self.output_tokens),
            SqlValue::Integer(self.cached_tokens),
            SqlValue::Integer(self.reasoning_tokens),
            SqlValue::Integer(self.cache_hit),
            SqlValue::Real(self.ttft_ms),
            SqlValue::Real(self.gen_ms),
            SqlValue::Real(self.total_ms),
            SqlValue::Real(self.queued_ms),
            SqlValue::Real(self.tokens_per_sec),
            SqlValue::Text(prev_hash.to_string()),
            SqlValue::Text(row_hash.to_string()),
        ]
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// What a chain check found.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verification {
    pub ok: bool,
    pub rows: i64,
    /// The `seq` of the first row that does not match its own hash, if any.
    pub broken_at: Option<i64>,
    pub message: String,
}

/// Recompute the chain and report the first row that does not match.
///
/// Reads in `seq` order and rebuilds each hash from the row's own values plus
/// the hash of the row before it, so both a changed number and a removed row
/// show up — a removed row breaks the link of the one that followed it.
pub fn verify(conn: &rusqlite::Connection) -> rusqlite::Result<Verification> {
    let mut stmt = conn.prepare(
        "SELECT seq, request_id, phase, ts, day, hour, key_id, public_model, backend_id,
                status, requests, input_tokens, billed_input_tokens, output_tokens,
                cached_tokens, reasoning_tokens, cache_hit, ttft_ms, gen_ms, total_ms,
                queued_ms, tokens_per_sec, prev_hash, row_hash
         FROM usage_ledger ORDER BY seq ASC",
    )?;

    let mut rows = stmt.query([])?;
    let mut prev = GENESIS.to_string();
    let mut count: i64 = 0;

    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let phase = match row.get::<_, String>(2)?.as_str() {
            "input" => Phase::Input,
            _ => Phase::Final,
        };
        let entry = LedgerEntry {
            request_id: row.get(1)?,
            phase,
            ts: row.get(3)?,
            day: row.get(4)?,
            hour: row.get(5)?,
            key_id: row.get(6)?,
            public_model: row.get(7)?,
            backend_id: row.get(8)?,
            status: row.get(9)?,
            requests: row.get(10)?,
            input_tokens: row.get(11)?,
            billed_input_tokens: row.get(12)?,
            output_tokens: row.get(13)?,
            cached_tokens: row.get(14)?,
            reasoning_tokens: row.get(15)?,
            cache_hit: row.get(16)?,
            ttft_ms: row.get(17)?,
            gen_ms: row.get(18)?,
            total_ms: row.get(19)?,
            queued_ms: row.get(20)?,
            tokens_per_sec: row.get(21)?,
        };
        let stored_prev: String = row.get(22)?;
        let stored_hash: String = row.get(23)?;

        if stored_prev != prev || entry.hash_with(&prev) != stored_hash {
            return Ok(Verification {
                ok: false,
                rows: count,
                broken_at: Some(seq),
                message: format!(
                    "row {seq} does not match the chain: it was changed or a row before it was removed"
                ),
            });
        }
        prev = stored_hash;
        count += 1;
    }

    Ok(Verification {
        ok: true,
        rows: count,
        broken_at: None,
        message: format!("all {count} rows match the chain"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tokens: i64) -> LedgerEntry {
        LedgerEntry {
            request_id: "req_1".into(),
            phase: Phase::Input,
            ts: 1_700_000_000_000,
            day: "2026-09-08".into(),
            input_tokens: tokens,
            requests: 1,
            ..Default::default()
        }
    }

    #[test]
    fn the_same_row_always_hashes_the_same_way() {
        assert_eq!(entry(10).hash_with(GENESIS), entry(10).hash_with(GENESIS));
    }

    #[test]
    fn changing_one_token_changes_the_hash() {
        assert_ne!(entry(10).hash_with(GENESIS), entry(11).hash_with(GENESIS));
    }

    #[test]
    fn the_same_row_in_a_different_place_hashes_differently() {
        // Otherwise a row could be moved, or duplicated, without detection.
        let elsewhere = entry(10).hash_with("aa");
        assert_ne!(entry(10).hash_with(GENESIS), elsewhere);
    }
}
