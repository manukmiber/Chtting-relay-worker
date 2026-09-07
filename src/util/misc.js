import { randomBytes, createHash, timingSafeEqual } from 'node:crypto';

/** Short, URL-safe, sortable-ish id. */
export function newId(prefix = '') {
  const t = Date.now().toString(36);
  const r = randomBytes(6).toString('hex');
  return `${prefix}${prefix ? '_' : ''}${t}${r}`;
}

export function sha256(s) {
  return createHash('sha256').update(String(s)).digest('hex');
}

/** Constant-time string compare that never throws on length mismatch. */
export function safeEqual(a, b) {
  const ba = Buffer.from(String(a ?? ''));
  const bb = Buffer.from(String(b ?? ''));
  if (ba.length !== bb.length) {
    // still burn a comparison so timing does not leak length
    timingSafeEqual(ba, ba);
    return false;
  }
  return timingSafeEqual(ba, bb);
}

export function clamp(n, lo, hi) {
  return Math.min(hi, Math.max(lo, n));
}

export function deepMerge(base, patch) {
  if (!isPlainObject(base) || !isPlainObject(patch)) return structuredClone(patch ?? base);
  const out = { ...base };
  for (const [k, v] of Object.entries(patch)) {
    if (v === undefined) continue;
    if (isPlainObject(v) && isPlainObject(out[k])) out[k] = deepMerge(out[k], v);
    else out[k] = structuredClone(v);
  }
  return out;
}

export function isPlainObject(v) {
  return v !== null && typeof v === 'object' && !Array.isArray(v);
}

/** Mask a secret for display: sk-abc...wxyz */
export function maskSecret(s) {
  const v = String(s ?? '');
  if (v.length <= 10) return v ? '•'.repeat(Math.max(4, v.length)) : '';
  return `${v.slice(0, 6)}…${v.slice(-4)}`;
}

export function nowMs() {
  return Number(process.hrtime.bigint() / 1000000n);
}

export function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

/** yyyy-mm-dd in a fixed IANA zone (dashboard "daily" buckets). */
export function dayKey(ts = Date.now(), timeZone = 'Asia/Jakarta') {
  try {
    const fmt = new Intl.DateTimeFormat('en-CA', {
      timeZone, year: 'numeric', month: '2-digit', day: '2-digit',
    });
    return fmt.format(new Date(ts));
  } catch {
    return new Date(ts).toISOString().slice(0, 10);
  }
}

export function hourKey(ts = Date.now(), timeZone = 'Asia/Jakarta') {
  try {
    const fmt = new Intl.DateTimeFormat('en-CA', {
      timeZone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', hour12: false,
    });
    // en-CA gives "2026-09-07, 13"
    const [d, h] = fmt.format(new Date(ts)).split(', ');
    return `${d}T${String(h).padStart(2, '0')}`;
  } catch {
    return new Date(ts).toISOString().slice(0, 13);
  }
}

export function percentile(sortedNums, p) {
  if (!sortedNums.length) return 0;
  const idx = clamp(Math.ceil((p / 100) * sortedNums.length) - 1, 0, sortedNums.length - 1);
  return sortedNums[idx];
}

export function round(n, digits = 2) {
  if (!Number.isFinite(n)) return 0;
  const f = 10 ** digits;
  return Math.round(n * f) / f;
}

/** Truncate a string for storage/preview without splitting surrogate pairs badly. */
export function truncate(s, max = 2000) {
  const v = String(s ?? '');
  return v.length <= max ? v : `${v.slice(0, max)}…[+${v.length - max} chars]`;
}
