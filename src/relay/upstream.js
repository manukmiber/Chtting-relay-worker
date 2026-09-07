import { sleep } from '../util/misc.js';

/**
 * Talks to the configured backends.
 *
 * Mobile networks drop connections constantly, so a request retries on
 * connection-level failures and 5xx/429 responses, then falls through to the
 * route's fallback backends before giving up.
 */
export class Upstream {
  constructor({ config, logger }) {
    this.config = config;
    this.logger = logger;
  }

  endpointUrl(backend, endpoint) {
    const base = backend.baseUrl.replace(/\/+$/, '');
    // A base URL that already carries a version segment is used as-is.
    const path = endpoint.replace(/^\/+/, '');
    if (/\/v\d+$/.test(base)) return `${base}/${path.replace(/^v\d+\//, '')}`;
    return `${base}/${path}`;
  }

  headersFor(backend, incoming = {}) {
    const headers = {
      'content-type': 'application/json',
      accept: incoming.accept ?? 'application/json',
      'user-agent': 'chtting-relay/1.0',
      ...Object.fromEntries(Object.entries(backend.headers ?? {})),
    };
    if (backend.apiKey) {
      if (backend.type === 'anthropic') {
        headers['x-api-key'] = backend.apiKey;
        headers['anthropic-version'] = backend.headers?.['anthropic-version'] ?? '2023-06-01';
      } else {
        headers.authorization = `Bearer ${backend.apiKey}`;
      }
    }
    return headers;
  }

  /**
   * Send one request. Returns the raw `Response`; the caller decides whether to
   * stream it or buffer it. Throws with `.status` when every attempt failed.
   */
  async send({ backend, endpoint, body, signal, stream }) {
    const url = this.endpointUrl(backend, endpoint);
    const headers = this.headersFor(backend, { accept: stream ? 'text/event-stream' : 'application/json' });
    const maxAttempts = Math.max(1, (backend.maxRetries ?? 1) + 1);

    let lastError = null;
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      const timeout = AbortSignal.timeout(backend.timeoutMs ?? 600000);
      const composite = signal ? AbortSignal.any([signal, timeout]) : timeout;
      const startedAt = Date.now();
      try {
        const res = await fetch(url, {
          method: 'POST',
          headers,
          body: JSON.stringify(body),
          signal: composite,
          // Node keeps the response streaming; do not let it buffer.
          duplex: 'half',
        });

        if (res.ok) return { res, attempt, url };

        // Retryable server-side conditions: back off and try again.
        if (attempt < maxAttempts && (res.status === 429 || res.status >= 500)) {
          const retryAfter = Number(res.headers.get('retry-after'));
          const wait = Number.isFinite(retryAfter) && retryAfter > 0
            ? Math.min(retryAfter * 1000, 10000)
            : Math.min(500 * 2 ** (attempt - 1), 8000);
          this.logger.warn(`upstream ${backend.id} ${res.status}, retry ${attempt}/${maxAttempts - 1} in ${wait}ms`);
          await res.body?.cancel().catch(() => {});
          await sleep(wait);
          continue;
        }
        return { res, attempt, url };
      } catch (err) {
        lastError = err;
        const aborted = signal?.aborted;
        if (aborted) throw Object.assign(new Error('client disconnected'), { status: 499, cause: err });
        const elapsed = Date.now() - startedAt;
        this.logger.warn(`upstream ${backend.id} attempt ${attempt} failed after ${elapsed}ms: ${err.message}`);
        if (attempt < maxAttempts) {
          await sleep(Math.min(500 * 2 ** (attempt - 1), 8000));
          continue;
        }
      }
    }

    throw Object.assign(
      new Error(`backend "${backend.name}" unreachable: ${lastError?.message ?? 'unknown error'}`),
      { status: 502, cause: lastError },
    );
  }

  /** Try the primary backend, then each fallback, returning the first success. */
  async sendWithFallback({ route, endpoint, body, signal, stream }) {
    const ids = [route.backend, ...(route.fallbacks ?? [])];
    const errors = [];
    let attempts = 0;
    // Remembered so a single-backend route reports the backend's own status
    // rather than a generic 502 that hides what actually went wrong.
    let lastHttpFailure = null;

    for (const id of ids) {
      const backend = this.config.findBackend(id);
      if (!backend) { errors.push(`unknown backend "${id}"`); continue; }
      if (!backend.enabled) { errors.push(`backend "${backend.name}" is disabled`); continue; }

      try {
        const result = await this.send({ backend, endpoint, body, signal, stream });
        attempts += result.attempt;
        if (result.res.ok) return { ...result, backend, attempts };

        const detail = await readErrorBody(result.res);
        errors.push(`${backend.name}: ${result.res.status} ${detail}`);
        lastHttpFailure = { ...result, backend, errorBody: detail };
        // 4xx from the first backend usually means a bad request, not a bad
        // backend, so only fall through on auth/rate/server problems.
        if (![401, 402, 403, 408, 409, 429].includes(result.res.status) && result.res.status < 500) {
          return { ...result, backend, attempts, errorBody: detail };
        }
      } catch (err) {
        if (err.status === 499) throw err;
        attempts += 1;
        errors.push(err.message);
      }
    }

    if (lastHttpFailure) return { ...lastHttpFailure, attempts };
    throw Object.assign(new Error(errors.join(' | ') || 'no usable backend'), { status: 502, attempts });
  }
}

async function readErrorBody(res) {
  try {
    const text = await res.text();
    try {
      const json = JSON.parse(text);
      return json?.error?.message ?? json?.message ?? text.slice(0, 400);
    } catch {
      return text.slice(0, 400);
    }
  } catch {
    return '<unreadable>';
  }
}
