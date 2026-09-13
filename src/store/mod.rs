//! Metrics storage and the in-memory counters that keep quota checks off the
//! hot path.
//!
//! Three pieces, all shaped by the same constraint — a request handler must
//! never block on the disk:
//!
//! * [`Store`] owns one writer task fed by a channel. Handlers hand over a
//!   finished row and move on; the writer batches whatever has piled up into a
//!   single transaction.
//! * [`QuotaTracker`] answers "has this key used up its day?" from memory,
//!   seeded from SQLite at startup. The Node version ran a `SELECT` per
//!   request, which at a few hundred concurrent callers is a few hundred
//!   synchronous disk reads per second.
//! * [`RateLimiter`] is a sharded sliding window, so one busy key does not
//!   serialise everyone else behind a single mutex.

pub mod invoice;
pub mod ledger;
pub mod schema;

use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub use ledger::{LedgerEntry, Phase};
pub use schema::RequestRecord;

/// How many rows the writer will commit in one transaction.
const WRITE_BATCH: usize = 128;

pub struct Store {
    file: PathBuf,
    tx: mpsc::Sender<WriteMsg>,
    /// A second connection used only for dashboard reads. SQLite in WAL mode
    /// lets it read while the writer commits.
    reader: Arc<Mutex<Connection>>,
    /// A third, for the writes an operator asks for by hand — see
    /// [`Store::write`].
    admin: Arc<Mutex<Connection>>,
}

impl Store {
    pub async fn open(file: &Path, logger: Arc<crate::logging::Logger>) -> Result<Arc<Self>> {
        if let Some(dir) = file.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }

        let path = file.to_path_buf();
        let setup = path.clone();
        let last_hash = tokio::task::spawn_blocking(move || -> Result<String> {
            let conn = Connection::open(&setup)?;
            conn.execute_batch(schema::CREATE_SQL)?;
            conn.execute_batch(ledger::CREATE_TABLE_SQL)?;
            conn.execute_batch(invoice::CREATE_SQL)?;
            // Before the indexes, never after: an older database still has an
            // older table, and indexing a column it has not got yet fails the
            // whole start-up. The ledger has exactly the same problem now that
            // it has columns of its own that were added later, which is why
            // its table and its indexes are two separate statements too.
            migrate(&conn)?;
            conn.execute_batch(schema::INDEX_SQL)?;
            conn.execute_batch(ledger::INDEX_SQL)?;
            Ok(last_ledger_hash(&conn)?)
        })
        .await??;

        let open_extra = |path: PathBuf| async move {
            tokio::task::spawn_blocking(move || -> Result<Connection> {
                let conn = Connection::open(&path)?;
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")?;
                Ok(conn)
            })
            .await?
        };
        let reader = open_extra(path.clone()).await?;
        let admin = open_extra(path.clone()).await?;

        let (tx, rx) = mpsc::channel::<WriteMsg>(8192);
        spawn_writer(path.clone(), rx, logger, last_hash);

        Ok(Arc::new(Self {
            file: file.to_path_buf(),
            tx,
            reader: Arc::new(Mutex::new(reader)),
            admin: Arc::new(Mutex::new(admin)),
        }))
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn kind(&self) -> &'static str {
        "sqlite"
    }

    /// Hand a finished request to the writer.
    ///
    /// Never blocks and never fails the request: if the queue is full the row
    /// is dropped and said so out loud, because losing a metrics row is always
    /// better than making a user wait on the disk.
    pub fn insert(&self, record: RequestRecord) -> bool {
        // Boxed so the channel's slots stay small; the record is 35 fields wide.
        self.tx.try_send(WriteMsg::Row(Box::new(record))).is_ok()
    }

    /// Append a row to the immutable usage ledger.
    ///
    /// Same channel as [`insert`](Self::insert), and for the same reason: the
    /// request path never waits on the disk. It also means the ledger's rows
    /// reach the writer in the order they happened, which is what lets the
    /// writer chain them together.
    pub fn ledger(&self, entry: LedgerEntry) -> bool {
        self.tx.try_send(WriteMsg::Ledger(Box::new(entry))).is_ok()
    }

    /// Wait until everything queued so far has been committed.
    ///
    /// The channel is FIFO, so by the time the writer reaches this marker every
    /// row sent before it has already been written. Used on shutdown, so a
    /// Ctrl-C does not discard the last un-committed batch, and by tests that
    /// need to read back what they just recorded.
    pub async fn flush(&self) -> bool {
        let (ack, wait) = tokio::sync::oneshot::channel();
        if self.tx.send(WriteMsg::Flush(ack)).await.is_err() {
            return false;
        }
        wait.await.is_ok()
    }

    /// Run a blocking query against the read connection.
    pub async fn read<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.reader.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            work(&guard)
        })
        .await?
    }

    /// Run a blocking statement that writes, on the admin connection.
    ///
    /// Separate from the writer task on purpose. That task exists to keep the
    /// *request path* off the disk and to be the single appender of the ledger
    /// chain; this is for the things an operator does — issuing an invoice,
    /// pruning old rows — which are rare, need an answer, and must not be able
    /// to stall behind a queue of request rows.
    ///
    /// SQLite serialises the two: WAL allows one writer at a time and
    /// `busy_timeout` waits for it rather than failing. Nothing here touches
    /// `usage_ledger`, which stays the writer task's alone.
    pub async fn write<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.admin.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock();
            work(&mut guard)
        })
        .await?
    }
}

/// What the writer task accepts.
enum WriteMsg {
    Row(Box<RequestRecord>),
    Ledger(Box<LedgerEntry>),
    /// Commit whatever is pending, then answer.
    Flush(tokio::sync::oneshot::Sender<()>),
}

fn spawn_writer(
    path: PathBuf,
    mut rx: mpsc::Receiver<WriteMsg>,
    logger: Arc<crate::logging::Logger>,
    last_hash: String,
) {
    tokio::task::spawn_blocking(move || {
        let mut conn = match Connection::open(&path) {
            Ok(c) => c,
            Err(err) => {
                logger.error(format!(
                    "metrics writer cannot open {}: {err}",
                    path.display()
                ));
                return;
            }
        };
        // WAL lets the dashboard read while this connection writes; NORMAL
        // sync is the right trade on a phone, where an fsync per row would
        // dominate the cost of a request.
        if let Err(err) = conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
        ) {
            logger.error(format!("metrics writer cannot configure sqlite: {err}"));
            return;
        }

        let mut batch: Vec<RequestRecord> = Vec::with_capacity(WRITE_BATCH);
        let mut entries: Vec<LedgerEntry> = Vec::with_capacity(WRITE_BATCH);
        let mut acks: Vec<tokio::sync::oneshot::Sender<()>> = Vec::new();
        // The chain's head. Only this task appends, so holding it in memory is
        // enough to keep every row linked to the one before it.
        let mut prev_hash = last_hash;
        // blocking_recv_many is not available, so drain by hand: take one
        // blocking read, then greedily pull whatever else is already queued.
        while let Some(first) = rx.blocking_recv() {
            match first {
                WriteMsg::Row(record) => batch.push(*record),
                WriteMsg::Ledger(entry) => entries.push(*entry),
                WriteMsg::Flush(ack) => acks.push(ack),
            }
            while batch.len() + entries.len() < WRITE_BATCH {
                match rx.try_recv() {
                    Ok(WriteMsg::Row(record)) => batch.push(*record),
                    Ok(WriteMsg::Ledger(entry)) => entries.push(*entry),
                    Ok(WriteMsg::Flush(ack)) => acks.push(ack),
                    Err(_) => break,
                }
            }
            if !batch.is_empty() || !entries.is_empty() {
                match write_batch(&mut conn, &batch, &entries, &prev_hash) {
                    Ok(head) => prev_hash = head,
                    Err(err) => logger.error(format!(
                        "failed to record {} request(s) and {} ledger row(s): {err}",
                        batch.len(),
                        entries.len()
                    )),
                }
                batch.clear();
                entries.clear();
            }
            // Answer only after the commit, so a waiter can read the rows back.
            for ack in acks.drain(..) {
                let _ = ack.send(());
            }
        }
    });
}

/// Commit a batch and return the new head of the ledger chain.
///
/// Both tables go into one transaction, so a request row and its ledger rows
/// either both land or neither does. On failure the caller keeps the old head,
/// which is correct: nothing was appended.
fn write_batch(
    conn: &mut Connection,
    batch: &[RequestRecord],
    entries: &[LedgerEntry],
    prev_hash: &str,
) -> Result<String> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(schema::INSERT_SQL)?;
        for record in batch {
            stmt.execute(rusqlite::params_from_iter(record.as_params()))?;
        }
    }
    let mut head = prev_hash.to_string();
    {
        let mut stmt = tx.prepare_cached(ledger::INSERT_SQL)?;
        for entry in entries {
            let row_hash = entry.hash_with(&head);
            stmt.execute(rusqlite::params_from_iter(
                entry.as_params(&head, &row_hash),
            ))?;
            head = row_hash;
        }
    }
    tx.commit()?;
    Ok(head)
}

/// Add columns that a database created by an earlier version does not have.
fn migrate(conn: &Connection) -> Result<()> {
    add_columns(conn, "requests", &schema::ADDED_COLUMNS)?;
    add_columns(conn, "usage_ledger", &ledger::ADDED_COLUMNS)?;
    Ok(())
}

/// `ALTER TABLE ... ADD COLUMN` for each column the table has not got.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so what it has got is read first.
/// The names come from a `const` in this crate and never from input, so the
/// formatting into SQL is not a place a value can be injected.
fn add_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (name, kind) in columns {
        if !existing.iter().any(|c| c == name) {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {kind}"))?;
        }
    }
    Ok(())
}

/// The hash of the newest ledger row, or the genesis value on an empty table.
fn last_ledger_hash(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row(
        "SELECT row_hash FROM usage_ledger ORDER BY seq DESC LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok(ledger::GENESIS.to_string()),
        other => Err(other),
    })
}

/* -------------------------------------------------------------- quotas -- */

#[derive(Debug, Clone, Default)]
pub struct DayUsage {
    pub requests: u64,
    pub tokens: u64,
}

/// Per-key usage for the current local day, held in memory.
pub struct QuotaTracker {
    day: RwLock<String>,
    usage: RwLock<HashMap<String, DayUsage>>,
}

impl QuotaTracker {
    pub fn new(day: String) -> Self {
        Self {
            day: RwLock::new(day),
            usage: RwLock::new(HashMap::new()),
        }
    }

    /// Load today's totals so a restart does not hand everyone a fresh quota.
    pub async fn seed(&self, store: &Store, day: &str) -> Result<()> {
        let day_owned = day.to_string();
        let rows: Vec<(String, u64, u64)> = store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT key_id, COUNT(*), COALESCE(SUM(total_tokens), 0)
                     FROM requests WHERE day = ? GROUP BY key_id",
                )?;
                let rows = stmt
                    .query_map([&day_owned], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)? as u64,
                            r.get::<_, i64>(2)? as u64,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut usage = self.usage.write();
        usage.clear();
        for (key, requests, tokens) in rows {
            usage.insert(key, DayUsage { requests, tokens });
        }
        *self.day.write() = day.to_string();
        Ok(())
    }

    /// Usage so far today, rolling over automatically at local midnight.
    pub fn get(&self, key_id: &str, today: &str) -> DayUsage {
        self.roll_over(today);
        self.usage.read().get(key_id).cloned().unwrap_or_default()
    }

    pub fn record(&self, key_id: &str, today: &str, tokens: u64) {
        self.roll_over(today);
        let mut usage = self.usage.write();
        // `entry` would need an owned key, which means allocating the id on
        // every single request just to throw it away again — the map already
        // has it after the first call. Look it up borrowed, and only allocate
        // the one time a key is seen.
        if let Some(entry) = usage.get_mut(key_id) {
            entry.requests += 1;
            entry.tokens += tokens;
            return;
        }
        usage.insert(
            key_id.to_string(),
            DayUsage {
                requests: 1,
                tokens,
            },
        );
    }

    fn roll_over(&self, today: &str) {
        if *self.day.read() == today {
            return;
        }
        let mut day = self.day.write();
        // Re-check: another thread may have rolled it while we waited.
        if *day != today {
            *day = today.to_string();
            self.usage.write().clear();
        }
    }
}

/* --------------------------------------------------------- rate limits -- */

const SHARDS: usize = 16;

/// A per-key sliding window over the last minute.
///
/// Sharded by key hash so two busy keys rarely touch the same mutex; a single
/// global map would turn every request into a contention point.
///
/// The window is a `VecDeque` rather than a `Vec`: expiring the front of a
/// `Vec` shifts everything behind it, which at a high per-minute limit is a
/// memmove of the whole window on every request. A deque drops from the front
/// in constant time.
pub struct RateLimiter {
    shards: Vec<Mutex<HashMap<String, VecDeque<Instant>>>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn shard(&self, key: &str) -> &Mutex<HashMap<String, VecDeque<Instant>>> {
        // FNV-1a: good enough spread, and no hasher state to carry around.
        let mut hash = 0xcbf29ce484222325u64;
        for byte in key.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        &self.shards[(hash as usize) % SHARDS]
    }

    /// `Ok(())` when the call may proceed, `Err(seconds)` when it must wait.
    pub fn check(&self, key: &str, per_minute: u32) -> Result<(), u64> {
        if per_minute == 0 {
            return Ok(());
        }
        let window = Duration::from_secs(60);
        let now = Instant::now();
        let mut shard = self.shard(key).lock();

        // Same reason as `QuotaTracker::record`: `entry` needs an owned key,
        // and allocating the id on every request to look up something already
        // in the map is a per-request allocation for nothing.
        let hits = match shard.get_mut(key) {
            Some(hits) => hits,
            None => shard.entry(key.to_string()).or_default(),
        };
        // The window is in arrival order, so everything expired is at the
        // front and stopping at the first live entry is enough.
        while hits
            .front()
            .is_some_and(|t| now.duration_since(*t) >= window)
        {
            hits.pop_front();
        }

        if hits.len() as u32 >= per_minute {
            let oldest = hits.front().copied().unwrap_or(now);
            let wait = window.saturating_sub(now.duration_since(oldest));
            return Err(wait.as_secs().max(1));
        }
        hits.push_back(now);
        Ok(())
    }

    /// Drop keys that have gone quiet, so an unbounded set of client keys
    /// cannot grow the map forever.
    pub fn sweep(&self) {
        let now = Instant::now();
        for shard in &self.shards {
            let mut map = shard.lock();
            map.retain(|_, hits| {
                while hits
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
                {
                    hits.pop_front();
                }
                !hits.is_empty()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::Logger;

    #[test]
    fn the_rate_limiter_admits_up_to_the_limit_then_asks_for_a_wait() {
        let limiter = RateLimiter::new();
        for i in 0..3 {
            assert!(limiter.check("k1", 3).is_ok(), "call {i} should be allowed");
        }
        let Err(wait) = limiter.check("k1", 3) else {
            panic!("the fourth call must be refused");
        };
        assert!((1..=60).contains(&wait));
        // A different key has its own window.
        assert!(limiter.check("k2", 3).is_ok());
        // Zero means unlimited.
        for _ in 0..100 {
            assert!(limiter.check("k3", 0).is_ok());
        }
    }

    #[test]
    fn sweeping_drops_keys_that_have_gone_quiet() {
        let limiter = RateLimiter::new();
        let _ = limiter.check("gone", 10);
        let populated: usize = limiter.shards.iter().map(|s| s.lock().len()).sum();
        assert_eq!(populated, 1);
        // Nothing has expired yet, so a sweep keeps it.
        limiter.sweep();
        let still: usize = limiter.shards.iter().map(|s| s.lock().len()).sum();
        assert_eq!(still, 1);
    }

    #[test]
    fn quota_usage_resets_when_the_local_day_rolls_over() {
        let tracker = QuotaTracker::new("2026-09-07".into());
        tracker.record("key1", "2026-09-07", 500);
        tracker.record("key1", "2026-09-07", 250);
        let used = tracker.get("key1", "2026-09-07");
        assert_eq!((used.requests, used.tokens), (2, 750));

        let tomorrow = tracker.get("key1", "2026-09-08");
        assert_eq!((tomorrow.requests, tomorrow.tokens), (0, 0));
    }

    /// A ledger written before there were cost columns has to keep working —
    /// and, more than that, has to keep *verifying*. Its rows were hashed over
    /// fewer fields, so a build that assumed its own format would report every
    /// one of them as tampered with the first time it looked.
    #[tokio::test]
    async fn a_ledger_from_before_the_cost_columns_still_verifies() {
        let dir = std::env::temp_dir().join(crate::util::new_id("ledger"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("relay.db");

        // The shape the ledger shipped in: no user, no kind, no money, no fmt.
        let old = Connection::open(&file).unwrap();
        old.execute_batch(
            "CREATE TABLE usage_ledger (
               seq INTEGER PRIMARY KEY AUTOINCREMENT,
               request_id TEXT NOT NULL, phase TEXT NOT NULL, ts INTEGER NOT NULL,
               day TEXT NOT NULL, hour TEXT NOT NULL, key_id TEXT NOT NULL,
               public_model TEXT NOT NULL, backend_id TEXT NOT NULL,
               status INTEGER NOT NULL, requests INTEGER NOT NULL,
               input_tokens INTEGER NOT NULL, billed_input_tokens INTEGER NOT NULL,
               output_tokens INTEGER NOT NULL, cached_tokens INTEGER NOT NULL,
               reasoning_tokens INTEGER NOT NULL, cache_hit INTEGER NOT NULL,
               ttft_ms REAL NOT NULL, gen_ms REAL NOT NULL, total_ms REAL NOT NULL,
               queued_ms REAL NOT NULL, tokens_per_sec REAL NOT NULL,
               prev_hash TEXT NOT NULL, row_hash TEXT NOT NULL
             );",
        )
        .unwrap();

        // Two rows, chained, hashed the way that build hashed them.
        let mut prev = ledger::GENESIS.to_string();
        for n in 1..=2 {
            let entry = LedgerEntry {
                request_id: format!("req_{n}"),
                phase: Phase::Final,
                ts: 1_700_000_000_000 + n,
                day: "2026-09-01".into(),
                key_id: "key_old".into(),
                requests: 1,
                input_tokens: 100 * n,
                ..Default::default()
            };
            let hash = entry.hash_as(&prev, ledger::RowFormat::Tokens);
            old.execute(
                "INSERT INTO usage_ledger (
                   request_id, phase, ts, day, hour, key_id, public_model, backend_id,
                   status, requests, input_tokens, billed_input_tokens, output_tokens,
                   cached_tokens, reasoning_tokens, cache_hit, ttft_ms, gen_ms,
                   total_ms, queued_ms, tokens_per_sec, prev_hash, row_hash
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,
                           ?17,?18,?19,?20,?21,?22,?23)",
                rusqlite::params![
                    entry.request_id,
                    entry.phase.as_str(),
                    entry.ts,
                    entry.day,
                    entry.hour,
                    entry.key_id,
                    entry.public_model,
                    entry.backend_id,
                    entry.status,
                    entry.requests,
                    entry.input_tokens,
                    entry.billed_input_tokens,
                    entry.output_tokens,
                    entry.cached_tokens,
                    entry.reasoning_tokens,
                    entry.cache_hit,
                    entry.ttft_ms,
                    entry.gen_ms,
                    entry.total_ms,
                    entry.queued_ms,
                    entry.tokens_per_sec,
                    prev,
                    hash,
                ],
            )
            .unwrap();
            prev = hash;
        }
        drop(old);

        let store = Store::open(&file, Logger::console(crate::logging::Level::Silent))
            .await
            .expect("a ledger from an older build must open");

        // A new row on top of the old chain, in the current format.
        assert!(store.ledger(LedgerEntry {
            request_id: "req_3".into(),
            phase: Phase::Final,
            ts: 1_700_000_000_003,
            day: "2026-09-13".into(),
            key_id: "key_old".into(),
            requests: 1,
            input_tokens: 300,
            proxy_usd: 0.004_2,
            backend_usd: 0.000_42,
            user_id: "u_abc".into(),
            key_kind: "private".into(),
            ..Default::default()
        }));
        store.flush().await;

        // Old rows and new one, each checked against the format it was written
        // in, all links intact.
        let check = store.read(|conn| Ok(ledger::verify(conn)?)).await.unwrap();
        assert!(check.ok, "{}", check.message);
        assert_eq!(check.rows, 3);

        // The old rows read as costing nothing rather than as null.
        let (rows, spent): (i64, f64) = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(proxy_usd),0) FROM usage_ledger",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(rows, 3);
        assert!((spent - 0.004_2).abs() < 1e-9);

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this guards against: a phone that had been running an older
    /// build came back with "no such column: user_id" and refused to start,
    /// because the index over that column was created before the migration
    /// that adds it.
    #[tokio::test]
    async fn a_database_from_an_older_build_is_migrated_before_it_is_indexed() {
        let dir = std::env::temp_dir().join(crate::util::new_id("store"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("relay.db");

        // The shape the first release wrote: none of the columns added since.
        let old = Connection::open(&file).unwrap();
        old.execute_batch(
            "CREATE TABLE requests (
               id TEXT PRIMARY KEY, ts INTEGER NOT NULL,
               day TEXT NOT NULL, hour TEXT NOT NULL,
               key_id TEXT, public_model TEXT
             );
             INSERT INTO requests (id, ts, day, hour) VALUES ('old', 1, '2026-09-01', '00');",
        )
        .unwrap();
        drop(old);

        let store = Store::open(&file, Logger::console(crate::logging::Level::Silent))
            .await
            .expect("an older database must open, not refuse to start");
        drop(store);

        let conn = Connection::open(&file).unwrap();
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(requests)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for (name, _) in schema::ADDED_COLUMNS {
            assert!(columns.iter().any(|c| c == name), "{name} was not added");
        }

        let indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_requests_user'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 1, "the index over a migrated column must exist");

        // And the rows that were already there survived.
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kept, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
