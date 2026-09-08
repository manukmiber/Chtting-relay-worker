/** Thin wrapper over the dashboard's admin API. */

class ApiError extends Error {
  constructor(message, status, details) {
    super(message);
    this.status = status;
    this.details = details;
  }
}

async function request(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

  const type = res.headers.get('content-type') ?? '';
  const payload = type.includes('json') ? await res.json().catch(() => ({})) : await res.text();

  if (!res.ok) {
    const message = typeof payload === 'string'
      ? payload.slice(0, 300)
      : payload?.error?.message ?? `${method} ${path} failed (${res.status})`;
    throw new ApiError(message, res.status, payload?.error?.details ?? null);
  }
  return payload;
}

export const api = {
  ApiError,

  session: () => request('GET', '/api/session'),
  login: (password) => request('POST', '/api/login', { password }),
  logout: () => request('POST', '/api/logout'),

  state: () => request('GET', '/api/state'),
  config: () => request('GET', '/api/config'),
  saveConfig: (patch) => request('PUT', '/api/config', patch),

  list: (name) => request('GET', `/api/${name}`),
  save: (name, item) => request('POST', `/api/${name}`, item),
  remove: (name, id) => request('DELETE', `/api/${name}/${encodeURIComponent(id)}`),

  generateKey: (opts) => request('POST', '/api/keys/generate', opts ?? {}),
  revealKey: (id) => request('GET', `/api/keys/${encodeURIComponent(id)}/reveal`),
  testBackend: (id) => request('POST', `/api/backends/${encodeURIComponent(id)}/test`),

  summary: (range) => request('GET', `/api/stats/summary?range=${encodeURIComponent(range)}`),
  daily: (days) => request('GET', `/api/stats/daily?days=${days}`),
  hourly: (hours) => request('GET', `/api/stats/hourly?hours=${hours}`),
  groupBy: (column, range) => request('GET', `/api/stats/by/${column}?range=${encodeURIComponent(range)}`),

  requests: (params) => request('GET', `/api/requests?${new URLSearchParams(params)}`),
  requestDetail: (id) => request('GET', `/api/requests/${encodeURIComponent(id)}`),
  prune: () => request('POST', '/api/maintenance/prune'),
  logs: (lines = 300) => request('GET', `/api/logs?lines=${lines}`),

  tokenizers: () => request('GET', '/api/tokenizer/inventory'),
  countTokens: (payload) => request('POST', '/api/tokenizer/count', payload),
  installTokenizer: (payload) => request('POST', '/api/tokenizer/install', payload),

  usageSummary: (range) => request('GET', `/api/usage/summary?range=${encodeURIComponent(range)}`),
  usageDaily: (days) => request('GET', `/api/usage/daily?days=${days}`),
  usageLedger: (limit = 100) => request('GET', `/api/usage/ledger?limit=${limit}`),
  verifyLedger: () => request('GET', '/api/usage/verify'),
  queue: () => request('GET', '/api/queue'),
  openrouterPreview: () => request('GET', '/api/openrouter/preview'),

  tunnel: () => request('GET', '/api/tunnel'),
  tunnelAction: (action) => request('POST', `/api/tunnel/${action}`),

  playground: (body) => request('POST', '/api/playground', body),
};
