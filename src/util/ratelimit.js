/** Sliding-window request limiter, keyed by client key id. */
export class RateLimiter {
  constructor() {
    this.hits = new Map(); // key -> timestamps within the window
  }

  /** @returns {{allowed:boolean, retryAfter:number, used:number}} */
  check(key, limitPerMinute, now = Date.now()) {
    if (!limitPerMinute || limitPerMinute <= 0) return { allowed: true, retryAfter: 0, used: 0 };
    const windowStart = now - 60000;
    const list = (this.hits.get(key) ?? []).filter((t) => t > windowStart);
    if (list.length >= limitPerMinute) {
      const retryAfter = Math.ceil((list[0] + 60000 - now) / 1000);
      this.hits.set(key, list);
      return { allowed: false, retryAfter: Math.max(1, retryAfter), used: list.length };
    }
    list.push(now);
    this.hits.set(key, list);
    return { allowed: true, retryAfter: 0, used: list.length };
  }

  /** Drop keys with no recent activity so the map cannot grow forever. */
  sweep(now = Date.now()) {
    const windowStart = now - 60000;
    for (const [key, list] of this.hits) {
      const kept = list.filter((t) => t > windowStart);
      if (kept.length) this.hits.set(key, kept);
      else this.hits.delete(key);
    }
  }
}
