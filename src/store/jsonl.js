import { createWriteStream, existsSync } from 'node:fs';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import path from 'node:path';
import { emptyRecord } from './schema.js';
import { withPercentiles } from './sqlite.js';
import { round } from '../util/misc.js';

/**
 * Append-only fallback store for Node builds without `node:sqlite`.
 * Rows live in memory for querying and are appended to disk as JSONL, which is
 * plenty for a phone-hosted relay and survives restarts.
 */
export class JsonlStore {
  constructor(file, rows) {
    this.file = file;
    this.rows = rows; // newest last
    this.kind = 'jsonl';
    this.stream = createWriteStream(file, { flags: 'a' });
  }

  static async open(file) {
    await mkdir(path.dirname(file), { recursive: true });
    const rows = [];
    if (existsSync(file)) {
      const raw = await readFile(file, 'utf8');
      for (const line of raw.split('\n')) {
        if (!line.trim()) continue;
        try { rows.push(JSON.parse(line)); } catch { /* skip a torn final line */ }
      }
    }
    return new JsonlStore(file, rows);
  }

  insert(partial) {
    const rec = { ...emptyRecord(), ...partial };
    this.rows.push(rec);
    this.stream.write(`${JSON.stringify(rec)}\n`);
    return rec;
  }

  get(id) {
    return this.rows.find((r) => r.id === id) ?? null;
  }

  list({ limit = 50, offset = 0, model, keyId, status, day, since, q } = {}) {
    let rows = this.rows;
    if (model) rows = rows.filter((r) => r.public_model === model);
    if (keyId) rows = rows.filter((r) => r.key_id === keyId);
    if (day) rows = rows.filter((r) => r.day === day);
    if (since) rows = rows.filter((r) => r.ts >= Number(since));
    if (status === 'error') rows = rows.filter((r) => r.status >= 400 || r.status === 0);
    else if (status === 'ok') rows = rows.filter((r) => r.status >= 200 && r.status < 400);
    if (q) {
      const needle = String(q).toLowerCase();
      rows = rows.filter((r) => `${r.req_preview}${r.res_preview}${r.error}`.toLowerCase().includes(needle));
    }
    const sorted = [...rows].sort((a, b) => b.ts - a.ts);
    return { rows: sorted.slice(Number(offset), Number(offset) + Number(limit)), total: sorted.length };
  }

  #window(since, until) {
    return this.rows.filter((r) => r.ts >= since && r.ts <= until);
  }

  summary({ since = 0, until = Number.MAX_SAFE_INTEGER } = {}) {
    const rows = this.#window(since, until);
    return withPercentiles(aggregate(rows), rows.filter((r) => r.status > 0 && r.status < 400));
  }

  daily({ days = 30 } = {}) {
    return this.#bucket('day').slice(-Number(days));
  }

  hourly({ hours = 48 } = {}) {
    return this.#bucket('hour').slice(-Number(hours)).map((b) => ({
      hour: b.hour,
      requests: b.requests,
      total_tokens: b.total_tokens,
      avg_ttft: b.avg_ttft,
      avg_tps: b.avg_tps,
    }));
  }

  #bucket(field) {
    const map = new Map();
    for (const r of this.rows) {
      const k = r[field];
      if (!map.has(k)) map.set(k, []);
      map.get(k).push(r);
    }
    return [...map.entries()]
      .sort((a, b) => String(a[0]).localeCompare(String(b[0])))
      .map(([k, rows]) => ({ [field]: k, ...aggregate(rows) }));
  }

  groupBy(column, { since = 0, limit = 50 } = {}) {
    const map = new Map();
    for (const r of this.rows) {
      if (r.ts < since) continue;
      const k = r[column] ?? '';
      if (!map.has(k)) map.set(k, []);
      map.get(k).push(r);
    }
    return [...map.entries()]
      .map(([name, rows]) => ({ name, ...aggregate(rows) }))
      .sort((a, b) => b.requests - a.requests)
      .slice(0, Number(limit));
  }

  usageForKey(keyId, day) {
    let requests = 0;
    let tokens = 0;
    for (const r of this.rows) {
      if (r.key_id === keyId && r.day === day) { requests += 1; tokens += r.total_tokens ?? 0; }
    }
    return { requests, tokens };
  }

  async prune(retentionDays) {
    if (!retentionDays || retentionDays <= 0) return 0;
    const cutoff = Date.now() - retentionDays * 86400000;
    const before = this.rows.length;
    this.rows = this.rows.filter((r) => r.ts >= cutoff);
    const removed = before - this.rows.length;
    if (removed > 0) {
      await writeFile(this.file, this.rows.map((r) => JSON.stringify(r)).join('\n') + (this.rows.length ? '\n' : ''));
    }
    return removed;
  }

  count() {
    return this.rows.length;
  }

  close() {
    this.stream.end();
  }
}

function aggregate(rows) {
  const users = new Set();
  let prompt = 0; let completion = 0; let total = 0; let cached = 0; let reasoning = 0;
  let errors = 0; let streamed = 0;
  let ttftSum = 0; let ttftN = 0;
  let totalSum = 0; let totalN = 0;
  let tpsSum = 0; let tpsN = 0;
  for (const r of rows) {
    if (r.key_id) users.add(r.key_id);
    prompt += r.prompt_tokens ?? 0;
    completion += r.completion_tokens ?? 0;
    total += r.total_tokens ?? 0;
    cached += r.cached_tokens ?? 0;
    reasoning += r.reasoning_tokens ?? 0;
    if (r.status >= 400 || r.status === 0) errors += 1;
    if (r.stream) streamed += 1;
    if (r.ttft_ms > 0) { ttftSum += r.ttft_ms; ttftN += 1; }
    if (r.total_ms > 0) { totalSum += r.total_ms; totalN += 1; }
    if (r.tokens_per_sec > 0) { tpsSum += r.tokens_per_sec; tpsN += 1; }
  }
  return {
    requests: rows.length,
    users: users.size,
    errors,
    streamed,
    prompt_tokens: prompt,
    completion_tokens: completion,
    total_tokens: total,
    cached_tokens: cached,
    reasoning_tokens: reasoning,
    avg_ttft: ttftN ? round(ttftSum / ttftN, 1) : 0,
    avg_total: totalN ? round(totalSum / totalN, 1) : 0,
    avg_tps: tpsN ? round(tpsSum / tpsN, 2) : 0,
  };
}
