import { mkdir } from 'node:fs/promises';
import path from 'node:path';
import { CREATE_SQL, REQUEST_FIELDS, emptyRecord } from './schema.js';
import { percentile, round } from '../util/misc.js';

/**
 * SQLite-backed metrics store using Node's built-in `node:sqlite`, so Termux
 * needs no native module compilation (`npm install` stays a no-op).
 */
export class SqliteStore {
  constructor(db, file) {
    this.db = db;
    this.file = file;
    this.kind = 'sqlite';
  }

  static async open(file) {
    const { DatabaseSync } = await import('node:sqlite');
    await mkdir(path.dirname(file), { recursive: true });
    const db = new DatabaseSync(file);
    db.exec('PRAGMA journal_mode = WAL;');
    db.exec('PRAGMA synchronous = NORMAL;');
    db.exec(CREATE_SQL);
    const store = new SqliteStore(db, file);
    store.#prepare();
    return store;
  }

  #prepare() {
    const cols = REQUEST_FIELDS.join(', ');
    const marks = REQUEST_FIELDS.map(() => '?').join(', ');
    this.insertStmt = this.db.prepare(`INSERT OR REPLACE INTO requests (${cols}) VALUES (${marks})`);
  }

  insert(partial) {
    const rec = { ...emptyRecord(), ...partial };
    this.insertStmt.run(...REQUEST_FIELDS.map((f) => coerce(rec[f])));
    return rec;
  }

  get(id) {
    return this.db.prepare('SELECT * FROM requests WHERE id = ?').get(id) ?? null;
  }

  list({ limit = 50, offset = 0, model, keyId, status, day, since, q } = {}) {
    const where = [];
    const args = [];
    if (model) { where.push('public_model = ?'); args.push(model); }
    if (keyId) { where.push('key_id = ?'); args.push(keyId); }
    if (day) { where.push('day = ?'); args.push(day); }
    if (since) { where.push('ts >= ?'); args.push(Number(since)); }
    if (status === 'error') where.push('(status >= 400 OR status = 0)');
    else if (status === 'ok') where.push('(status >= 200 AND status < 400)');
    if (q) {
      where.push('(req_preview LIKE ? OR res_preview LIKE ? OR error LIKE ?)');
      const like = `%${q}%`;
      args.push(like, like, like);
    }
    const clause = where.length ? `WHERE ${where.join(' AND ')}` : '';
    const rows = this.db
      .prepare(`SELECT * FROM requests ${clause} ORDER BY ts DESC LIMIT ? OFFSET ?`)
      .all(...args, Number(limit), Number(offset));
    const total = this.db.prepare(`SELECT COUNT(*) AS n FROM requests ${clause}`).get(...args)?.n ?? 0;
    return { rows, total };
  }

  summary({ since = 0, until = Number.MAX_SAFE_INTEGER } = {}) {
    const row = this.db.prepare(`
      SELECT
        COUNT(*) AS requests,
        COUNT(DISTINCT key_id) AS users,
        SUM(prompt_tokens) AS prompt_tokens,
        SUM(completion_tokens) AS completion_tokens,
        SUM(total_tokens) AS total_tokens,
        SUM(cached_tokens) AS cached_tokens,
        SUM(reasoning_tokens) AS reasoning_tokens,
        SUM(CASE WHEN status >= 400 OR status = 0 THEN 1 ELSE 0 END) AS errors,
        SUM(CASE WHEN stream = 1 THEN 1 ELSE 0 END) AS streamed,
        AVG(NULLIF(ttft_ms, 0)) AS avg_ttft,
        AVG(NULLIF(total_ms, 0)) AS avg_total,
        AVG(NULLIF(tokens_per_sec, 0)) AS avg_tps
      FROM requests WHERE ts >= ? AND ts <= ?
    `).get(since, until) ?? {};

    const lat = this.db
      .prepare('SELECT ttft_ms, total_ms, tokens_per_sec FROM requests WHERE ts >= ? AND ts <= ? AND status < 400 AND status > 0 ORDER BY ts DESC LIMIT 20000')
      .all(since, until);
    return withPercentiles(row, lat);
  }

  daily({ days = 30 } = {}) {
    return this.db.prepare(`
      SELECT day,
        COUNT(*) AS requests,
        COUNT(DISTINCT key_id) AS users,
        SUM(prompt_tokens) AS prompt_tokens,
        SUM(completion_tokens) AS completion_tokens,
        SUM(total_tokens) AS total_tokens,
        SUM(CASE WHEN status >= 400 OR status = 0 THEN 1 ELSE 0 END) AS errors,
        AVG(NULLIF(ttft_ms, 0)) AS avg_ttft,
        AVG(NULLIF(tokens_per_sec, 0)) AS avg_tps
      FROM requests GROUP BY day ORDER BY day DESC LIMIT ?
    `).all(Number(days)).reverse();
  }

  hourly({ hours = 48 } = {}) {
    return this.db.prepare(`
      SELECT hour,
        COUNT(*) AS requests,
        SUM(total_tokens) AS total_tokens,
        AVG(NULLIF(ttft_ms, 0)) AS avg_ttft,
        AVG(NULLIF(tokens_per_sec, 0)) AS avg_tps
      FROM requests GROUP BY hour ORDER BY hour DESC LIMIT ?
    `).all(Number(hours)).reverse();
  }

  groupBy(column, { since = 0, limit = 50 } = {}) {
    if (!['public_model', 'key_id', 'backend_id', 'upstream_model'].includes(column)) {
      throw new Error(`cannot group by ${column}`);
    }
    return this.db.prepare(`
      SELECT ${column} AS name,
        COUNT(*) AS requests,
        COUNT(DISTINCT key_id) AS users,
        SUM(prompt_tokens) AS prompt_tokens,
        SUM(completion_tokens) AS completion_tokens,
        SUM(total_tokens) AS total_tokens,
        SUM(CASE WHEN status >= 400 OR status = 0 THEN 1 ELSE 0 END) AS errors,
        AVG(NULLIF(ttft_ms, 0)) AS avg_ttft,
        AVG(NULLIF(total_ms, 0)) AS avg_total,
        AVG(NULLIF(tokens_per_sec, 0)) AS avg_tps
      FROM requests WHERE ts >= ?
      GROUP BY ${column} ORDER BY requests DESC LIMIT ?
    `).all(since, Number(limit));
  }

  /** Requests and tokens used by one key today, for quota enforcement. */
  usageForKey(keyId, day) {
    return this.db.prepare(`
      SELECT COUNT(*) AS requests, COALESCE(SUM(total_tokens), 0) AS tokens
      FROM requests WHERE key_id = ? AND day = ?
    `).get(keyId, day) ?? { requests: 0, tokens: 0 };
  }

  prune(retentionDays) {
    if (!retentionDays || retentionDays <= 0) return 0;
    const cutoff = Date.now() - retentionDays * 86400000;
    const before = this.db.prepare('SELECT COUNT(*) AS n FROM requests WHERE ts < ?').get(cutoff)?.n ?? 0;
    this.db.prepare('DELETE FROM requests WHERE ts < ?').run(cutoff);
    if (before) this.db.exec('VACUUM;');
    return before;
  }

  count() {
    return this.db.prepare('SELECT COUNT(*) AS n FROM requests').get()?.n ?? 0;
  }

  close() {
    try { this.db.close(); } catch { /* already closed */ }
  }
}

export function withPercentiles(row, latRows) {
  const ttft = latRows.map((r) => r.ttft_ms).filter((n) => n > 0).sort((a, b) => a - b);
  const total = latRows.map((r) => r.total_ms).filter((n) => n > 0).sort((a, b) => a - b);
  const tps = latRows.map((r) => r.tokens_per_sec).filter((n) => n > 0).sort((a, b) => a - b);
  const requests = row.requests ?? 0;
  const errors = row.errors ?? 0;
  return {
    requests,
    users: row.users ?? 0,
    errors,
    streamed: row.streamed ?? 0,
    error_rate: requests ? round((errors / requests) * 100, 2) : 0,
    prompt_tokens: row.prompt_tokens ?? 0,
    completion_tokens: row.completion_tokens ?? 0,
    total_tokens: row.total_tokens ?? 0,
    cached_tokens: row.cached_tokens ?? 0,
    reasoning_tokens: row.reasoning_tokens ?? 0,
    avg_ttft_ms: round(row.avg_ttft ?? 0, 1),
    avg_total_ms: round(row.avg_total ?? 0, 1),
    avg_tps: round(row.avg_tps ?? 0, 2),
    p50_ttft_ms: round(percentile(ttft, 50), 1),
    p95_ttft_ms: round(percentile(ttft, 95), 1),
    p50_total_ms: round(percentile(total, 50), 1),
    p95_total_ms: round(percentile(total, 95), 1),
    p50_tps: round(percentile(tps, 50), 2),
    p95_tps: round(percentile(tps, 95), 2),
  };
}

function coerce(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === 'boolean') return v ? 1 : 0;
  if (typeof v === 'number') return Number.isFinite(v) ? v : 0;
  return String(v);
}
