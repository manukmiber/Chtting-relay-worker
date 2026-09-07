/**
 * Byte-pair-encoding core.
 *
 * The merge loop is shared by two rank conventions:
 *   - tiktoken   : rank is looked up by the *concatenated result* of a pair
 *   - HuggingFace: rank is the merge-rule index, looked up by the *pair itself*
 *
 * Both operate on a "unit string": a JS string in which every UTF-16 code unit
 * is exactly one atomic symbol. tiktoken uses latin-1 (1 char = 1 byte),
 * HF byte-level BPE uses the GPT-2 byte->unicode alphabet. That lets one
 * implementation serve both without copying byte arrays around.
 *
 * A lazy binary heap keeps the merge loop at O(n log n) so that pathological
 * pieces (long CJK runs, base64 blobs) do not turn into a quadratic stall.
 */

const MAX_RANK = Infinity;

// Ties broken by left-most position, matching both reference implementations.
function cmp(x, y) {
  return x.rank - y.rank || x.node - y.node;
}

/** Minimal binary min-heap over merge candidates. */
class MergeHeap {
  constructor() { this.a = []; }

  get size() { return this.a.length; }

  push(item) {
    const a = this.a;
    a.push(item);
    let i = a.length - 1;
    while (i > 0) {
      const p = (i - 1) >> 1;
      if (cmp(a[i], a[p]) >= 0) break;
      const t = a[i]; a[i] = a[p]; a[p] = t;
      i = p;
    }
  }

  pop() {
    const a = this.a;
    const top = a[0];
    const last = a.pop();
    if (a.length) {
      a[0] = last;
      let i = 0;
      for (;;) {
        const l = 2 * i + 1;
        const r = l + 1;
        let m = i;
        if (l < a.length && cmp(a[l], a[m]) < 0) m = l;
        if (r < a.length && cmp(a[r], a[m]) < 0) m = r;
        if (m === i) break;
        const t = a[i]; a[i] = a[m]; a[m] = t;
        i = m;
      }
    }
    return top;
  }
}

/**
 * Merge `units` under `rankOf(leftSymbol, rightSymbol)`.
 * Returns the boundary offsets of the resulting symbols, e.g. [0, 2, 5, 7].
 */
export function bpeBoundaries(units, rankOf) {
  const n = units.length;
  if (n === 0) return [0];
  if (n === 1) return [0, 1];

  // Doubly linked list over symbol slots: start[i] is the offset of slot i,
  // end[i] the exclusive offset. Absorbed slots are marked dead.
  const start = new Int32Array(n);
  const end = new Int32Array(n);
  const prev = new Int32Array(n);
  const next = new Int32Array(n);
  const alive = new Uint8Array(n);
  const version = new Int32Array(n);
  for (let i = 0; i < n; i++) {
    start[i] = i;
    end[i] = i + 1;
    prev[i] = i - 1;
    next[i] = i + 1 < n ? i + 1 : -1;
    alive[i] = 1;
  }

  const heap = new MergeHeap();
  const consider = (i) => {
    const j = next[i];
    if (j < 0) return;
    const rank = rankOf(units.slice(start[i], end[i]), units.slice(start[j], end[j]));
    if (rank === undefined || rank === null || !Number.isFinite(rank)) return;
    heap.push({ rank, node: i, version: version[i] });
  };
  for (let i = 0; i < n; i++) consider(i);

  while (heap.size) {
    const top = heap.pop();
    const i = top.node;
    if (!alive[i] || version[i] !== top.version) continue; // stale candidate
    const j = next[i];
    if (j < 0 || !alive[j]) continue;

    end[i] = end[j]; // absorb j into i
    alive[j] = 0;
    const k = next[j];
    next[i] = k;
    if (k >= 0) prev[k] = i;

    version[i]++;
    const p = prev[i];
    if (p >= 0) { version[p]++; consider(p); }
    consider(i);
  }

  const out = [0];
  for (let i = 0; i >= 0; i = next[i]) out.push(end[i]);
  return out;
}

/** tiktoken convention: rank keyed by the concatenated result. */
export function makeResultRanker(ranks) {
  return (l, r) => {
    const v = ranks.get(l + r);
    return v === undefined ? MAX_RANK : v;
  };
}

/** HuggingFace convention: rank keyed by the ordered pair. */
export function makePairRanker(pairRanks) {
  return (l, r) => {
    const v = pairRanks.get(`${l} ${r}`);
    return v === undefined ? MAX_RANK : v;
  };
}

/** Number of tokens a single pre-tokenized piece collapses into. */
export function bpeCount(units, rankOf, cache) {
  if (units.length <= 1) return units.length;
  if (cache) {
    const hit = cache.get(units);
    if (hit !== undefined) return hit;
  }
  const count = bpeBoundaries(units, rankOf).length - 1;
  if (cache && units.length <= 64) cache.set(units, count);
  return count;
}

/** Token strings (in unit-space) a piece collapses into. */
export function bpeSplit(units, rankOf) {
  if (units.length <= 1) return units.length ? [units] : [];
  const b = bpeBoundaries(units, rankOf);
  const out = [];
  for (let i = 0; i + 1 < b.length; i++) out.push(units.slice(b[i], b[i + 1]));
  return out;
}
