import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import path from 'node:path';

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
};

export function sendJson(res, status, body, extraHeaders = {}) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(payload),
    ...extraHeaders,
  });
  res.end(payload);
}

/** OpenAI-shaped error envelope, so existing clients surface it properly. */
export function sendError(res, status, message, { type = 'invalid_request_error', code = null, param = null } = {}) {
  sendJson(res, status, { error: { message, type, code, param } });
}

export function sendText(res, status, text, contentType = 'text/plain; charset=utf-8') {
  res.writeHead(status, { 'content-type': contentType, 'content-length': Buffer.byteLength(text) });
  res.end(text);
}

export async function readBody(req, maxBytes) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > maxBytes) {
      const err = new Error(`request body exceeds ${maxBytes} bytes`);
      err.status = 413;
      throw err;
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

export async function readJson(req, maxBytes) {
  const buf = await readBody(req, maxBytes);
  if (!buf.length) return {};
  try {
    return JSON.parse(buf.toString('utf8'));
  } catch (err) {
    throw Object.assign(new Error(`invalid JSON body: ${err.message}`), { status: 400 });
  }
}

export function applyCors(req, res, origins = ['*']) {
  const origin = req.headers.origin;
  const allow = origins.includes('*') ? '*' : (origins.includes(origin) ? origin : null);
  if (!allow) return;
  res.setHeader('access-control-allow-origin', allow);
  res.setHeader('vary', 'origin');
  res.setHeader('access-control-allow-headers', 'authorization, content-type, x-api-key, x-relay-key, anthropic-version');
  res.setHeader('access-control-allow-methods', 'GET, POST, PUT, DELETE, OPTIONS');
  res.setHeader('access-control-max-age', '86400');
}

export function clientIp(req, trustProxy = true) {
  if (trustProxy) {
    const fwd = req.headers['cf-connecting-ip'] || req.headers['x-forwarded-for'];
    if (fwd) return String(fwd).split(',')[0].trim();
  }
  return req.socket?.remoteAddress ?? '';
}

export function bearerToken(req) {
  const auth = req.headers.authorization;
  if (auth && /^bearer\s+/i.test(auth)) return auth.replace(/^bearer\s+/i, '').trim();
  return String(req.headers['x-api-key'] ?? req.headers['x-relay-key'] ?? '').trim() || null;
}

/** Serve a file from `root`, refusing to escape it. */
export async function serveStatic(res, root, urlPath, { cache = 'no-cache' } = {}) {
  const rel = decodeURIComponent(urlPath.split('?')[0]).replace(/^\/+/, '') || 'index.html';
  const full = path.join(root, rel);
  if (!full.startsWith(path.resolve(root))) {
    sendText(res, 403, 'forbidden');
    return true;
  }
  try {
    const s = await stat(full);
    if (!s.isFile()) return false;
    res.writeHead(200, {
      'content-type': MIME[path.extname(full).toLowerCase()] ?? 'application/octet-stream',
      'content-length': s.size,
      'cache-control': cache,
    });
    createReadStream(full).pipe(res);
    return true;
  } catch {
    return false;
  }
}

/** Tiny path router: `route('POST', '/v1/chat/completions', handler)`. */
export class Router {
  constructor() {
    this.routes = [];
  }

  add(method, pattern, handler) {
    const keys = [];
    const source = pattern
      .replace(/[.+*?^${}()|[\]\\]/g, '\\$&')
      .replace(/\\\*\\\*/g, '.*')
      .replace(/:(\w+)/g, (_, k) => { keys.push(k); return '([^/]+)'; });
    this.routes.push({ method, re: new RegExp(`^${source}$`), keys, handler });
    return this;
  }

  get(p, h) { return this.add('GET', p, h); }
  post(p, h) { return this.add('POST', p, h); }
  put(p, h) { return this.add('PUT', p, h); }
  delete(p, h) { return this.add('DELETE', p, h); }

  match(method, pathname) {
    for (const r of this.routes) {
      if (r.method !== method) continue;
      const m = r.re.exec(pathname);
      if (!m) continue;
      const params = {};
      r.keys.forEach((k, i) => { params[k] = decodeURIComponent(m[i + 1]); });
      return { handler: r.handler, params };
    }
    return null;
  }
}
