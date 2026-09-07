/**
 * Server-sent-event plumbing for OpenAI-style streams.
 */

/** Incremental SSE parser: feed bytes, get back complete events. */
export class SseParser {
  constructor() {
    this.buf = '';
  }

  /** @returns {{event:string,data:string}[]} */
  push(text) {
    this.buf += text;
    const events = [];
    let idx;
    // events are separated by a blank line; tolerate \r\n from proxies
    while ((idx = findSeparator(this.buf)) !== -1) {
      const raw = this.buf.slice(0, idx.start);
      this.buf = this.buf.slice(idx.end);
      const parsed = parseEvent(raw);
      if (parsed) events.push(parsed);
    }
    return events;
  }

  /** Anything left after the upstream closed without a trailing blank line. */
  flush() {
    const rest = this.buf;
    this.buf = '';
    const parsed = rest.trim() ? parseEvent(rest) : null;
    return parsed ? [parsed] : [];
  }
}

function findSeparator(buf) {
  const a = buf.indexOf('\n\n');
  const b = buf.indexOf('\r\n\r\n');
  if (a === -1 && b === -1) return -1;
  if (b !== -1 && (a === -1 || b < a)) return { start: b, end: b + 4 };
  return { start: a, end: a + 2 };
}

function parseEvent(raw) {
  let event = 'message';
  const data = [];
  for (const line of raw.split(/\r?\n/)) {
    if (!line || line.startsWith(':')) continue;
    const colon = line.indexOf(':');
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'event') event = value;
    else if (field === 'data') data.push(value);
  }
  if (!data.length && event === 'message') return null;
  return { event, data: data.join('\n') };
}

export function formatSse(data, event) {
  const payload = typeof data === 'string' ? data : JSON.stringify(data);
  const prefix = event && event !== 'message' ? `event: ${event}\n` : '';
  return `${prefix}data: ${payload}\n\n`;
}

export const SSE_HEADERS = {
  'content-type': 'text/event-stream; charset=utf-8',
  'cache-control': 'no-cache, no-transform',
  connection: 'keep-alive',
  'x-accel-buffering': 'no', // stops nginx/cloudflared from buffering the stream
};

/**
 * Applies text rewrites to a stream without letting a pattern that straddles
 * two chunks slip through.
 *
 * Holding back a fixed tail is not enough on its own: a match can begin inside
 * the part about to be emitted and end inside the tail. So the cut point is
 * also pulled back behind any match that straddles it, leaving that text
 * buffered until the rest of it arrives.
 */
export class StreamRewriter {
  /**
   * @param {(s:string)=>string|null} rewrite  compiled replacement function
   * @param {number} lookbehind  characters always withheld; must exceed the
   *   longest pattern so an incomplete match at the buffer end stays buffered
   * @param {RegExp[]} patterns  the rules' own regexes, used to find straddles
   */
  constructor(rewrite, lookbehind = 64, patterns = []) {
    this.rewrite = rewrite;
    this.lookbehind = Math.max(1, lookbehind);
    this.patterns = patterns.length ? patterns : (rewrite?.patterns ?? []);
    this.pending = '';
  }

  push(text) {
    if (!text) return '';
    if (!this.rewrite) return text;
    this.pending += text;
    const cut = this.#safeCut();
    if (cut <= 0) return '';
    const safe = this.pending.slice(0, cut);
    this.pending = this.pending.slice(cut);
    return this.rewrite(safe);
  }

  flush() {
    if (!this.pending) return '';
    const out = this.rewrite ? this.rewrite(this.pending) : this.pending;
    this.pending = '';
    return out;
  }

  /** Largest prefix length that cannot contain the start of an open match. */
  #safeCut() {
    let cut = this.pending.length - this.lookbehind;
    if (cut <= 0) return 0;
    for (const re of this.patterns) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(this.pending)) !== null) {
        if (m[0] === '') { re.lastIndex += 1; continue; }
        const start = m.index;
        const end = start + m[0].length;
        if (start >= cut) break;      // begins in the withheld tail already
        if (end > cut) cut = start;   // straddles the cut: keep it whole
      }
    }
    return Math.max(0, cut);
  }
}
