import http from 'node:http';
import path from 'node:path';
import { readFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { Router, sendJson, sendError, sendText, readJson, serveStatic, applyCors } from './http-util.js';
import { newId, safeEqual, dayKey, isPlainObject, maskSecret } from '../util/misc.js';
import { CHAT_PROFILES } from '../tokenizer/index.js';

/**
 * The local control panel. It binds to 127.0.0.1 by default and is never
 * routed through the tunnel, so the relay can be public while its settings,
 * keys and logs stay on the phone.
 */
export function createDashboardServer(ctx) {
  const {
    config, store, counter, tunnel, logger, publicDir, paths, relayServer,
  } = ctx;
  const sessions = new Map(); // token -> expiry
  const router = new Router();

  const passwordSet = () => Boolean(config.get().dashboard.password);

  const authed = (req) => {
    if (!passwordSet()) return true;
    const token = cookie(req, 'chtting_session');
    if (!token) return false;
    const exp = sessions.get(token);
    if (!exp || exp < Date.now()) {
      sessions.delete(token);
      return false;
    }
    return true;
  };

  /* ------------------------------------------------------------ auth -- */

  router.post('/api/login', async (req, res) => {
    const body = await readJson(req, 8192);
    const expected = config.get().dashboard.password;
    if (expected && !safeEqual(body.password ?? '', expected)) {
      return sendError(res, 401, 'wrong password');
    }
    const token = randomBytes(24).toString('hex');
    sessions.set(token, Date.now() + config.get().dashboard.sessionTtlMs);
    res.setHeader('set-cookie', `chtting_session=${token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=${Math.floor(config.get().dashboard.sessionTtlMs / 1000)}`);
    return sendJson(res, 200, { ok: true });
  });

  router.post('/api/logout', (req, res) => {
    const token = cookie(req, 'chtting_session');
    if (token) sessions.delete(token);
    res.setHeader('set-cookie', 'chtting_session=; HttpOnly; Path=/; Max-Age=0');
    sendJson(res, 200, { ok: true });
  });

  router.get('/api/session', (req, res) => {
    sendJson(res, 200, { authenticated: authed(req), passwordSet: passwordSet() });
  });

  /* ----------------------------------------------------------- state -- */

  router.get('/api/state', async (req, res) => {
    const cfg = config.get();
    sendJson(res, 200, {
      config: config.redacted(),
      tunnel: tunnel.status(),
      cloudflared: await tunnel.version(),
      store: { kind: store.kind, rows: store.count(), file: store.file },
      tokenizers: await counter.registry.inventory(),
      profiles: Object.keys(CHAT_PROFILES),
      paths,
      relay: {
        listening: Boolean(relayServer?.listening),
        host: cfg.server.host,
        port: cfg.server.port,
        localUrl: `http://127.0.0.1:${cfg.server.port}`,
      },
      runtime: {
        node: process.version,
        platform: process.platform,
        arch: process.arch,
        uptime_s: Math.round(process.uptime()),
        rss_mb: Math.round(process.memoryUsage().rss / 1048576),
        termux: Boolean(process.env.PREFIX?.includes('com.termux')),
      },
      today: dayKey(Date.now(), cfg.timezone),
    });
  });

  /* ---------------------------------------------------------- config -- */

  router.get('/api/config', (req, res) => sendJson(res, 200, config.redacted()));

  router.put('/api/config', async (req, res) => {
    const patch = await readJson(req, 4 * 1024 * 1024);
    const merged = unmaskSecrets(patch, config.get());
    const next = await config.update(merged);
    counter.registry.setRules(next.tokenizer.rules, next.tokenizer.fallback);
    logger.setLevel(next.logging.level);
    return sendJson(res, 200, config.redacted());
  });

  for (const name of ['models', 'backends', 'keys', 'systemPrompts']) {
    router.get(`/api/${name}`, (req, res) => sendJson(res, 200, config.redacted()[name]));

    router.post(`/api/${name}`, async (req, res) => {
      const item = await readJson(req, 1024 * 1024);
      const current = (config.get()[name] ?? []).find((x) => x.id === item.id);
      const saved = await config.upsert(name, current ? unmaskSecrets(item, current) : item);
      return sendJson(res, 200, { ok: true, item: maskItem(saved) });
    });

    router.delete(`/api/${name}/:id`, async (req, res, { params }) => {
      await config.remove(name, params.id);
      return sendJson(res, 200, { ok: true });
    });
  }

  router.post('/api/keys/generate', async (req, res) => {
    const body = await readJson(req, 8192).catch(() => ({}));
    const key = `sk-relay-${randomBytes(24).toString('base64url')}`;
    const saved = await config.upsert('keys', {
      id: newId('key'),
      label: body.label || 'new key',
      key,
      enabled: true,
      models: body.models ?? ['*'],
      quota: body.quota ?? { requestsPerDay: 0, tokensPerDay: 0, requestsPerMinute: 0 },
    });
    // Returned once in the clear; afterwards the dashboard only sees the mask.
    return sendJson(res, 200, { ok: true, item: { ...maskItem(saved), key } });
  });

  router.get('/api/keys/:id/reveal', (req, res, { params }) => {
    const key = config.get().keys.find((k) => k.id === params.id);
    if (!key) return sendError(res, 404, 'key not found');
    return sendJson(res, 200, { key: key.key });
  });

  router.post('/api/backends/:id/test', async (req, res, { params }) => {
    const backend = config.findBackend(params.id);
    if (!backend) return sendError(res, 404, 'backend not found');
    return sendJson(res, 200, await probeBackend(backend));
  });

  /* ------------------------------------------------------- statistics -- */

  router.get('/api/stats/summary', (req, res, { url }) => {
    const since = rangeStart(url.searchParams.get('range') ?? '30d');
    sendJson(res, 200, {
      range: url.searchParams.get('range') ?? '30d',
      since,
      all: store.summary({}),
      window: store.summary({ since }),
      today: store.summary({ since: startOfToday(config.get().timezone) }),
    });
  });

  router.get('/api/stats/daily', (req, res, { url }) => {
    sendJson(res, 200, store.daily({ days: Number(url.searchParams.get('days') ?? 30) }));
  });

  router.get('/api/stats/hourly', (req, res, { url }) => {
    sendJson(res, 200, store.hourly({ hours: Number(url.searchParams.get('hours') ?? 48) }));
  });

  router.get('/api/stats/by/:column', (req, res, { params, url }) => {
    const map = {
      model: 'public_model', key: 'key_id', backend: 'backend_id', upstream: 'upstream_model',
    };
    const column = map[params.column];
    if (!column) return sendError(res, 400, `cannot group by "${params.column}"`);
    const since = rangeStart(url.searchParams.get('range') ?? '30d');
    const rows = store.groupBy(column, { since, limit: Number(url.searchParams.get('limit') ?? 50) });
    if (column === 'key_id') {
      const labels = new Map(config.get().keys.map((k) => [k.id, k.label || k.id]));
      for (const r of rows) r.label = labels.get(r.name) ?? r.name;
    }
    return sendJson(res, 200, rows);
  });

  router.get('/api/requests', (req, res, { url }) => {
    const p = url.searchParams;
    sendJson(res, 200, store.list({
      limit: Math.min(200, Number(p.get('limit') ?? 50)),
      offset: Number(p.get('offset') ?? 0),
      model: p.get('model') || undefined,
      keyId: p.get('key') || undefined,
      status: p.get('status') || undefined,
      day: p.get('day') || undefined,
      q: p.get('q') || undefined,
      since: p.get('since') ? Number(p.get('since')) : undefined,
    }));
  });

  router.get('/api/requests/:id', (req, res, { params }) => {
    const row = store.get(params.id);
    if (!row) return sendError(res, 404, 'request not found');
    return sendJson(res, 200, row);
  });

  router.post('/api/maintenance/prune', async (req, res) => {
    const removed = await store.prune(config.get().logging.retentionDays);
    return sendJson(res, 200, { ok: true, removed });
  });

  router.get('/api/logs', async (req, res, { url }) => {
    if (!logger.file) return sendText(res, 200, 'file logging is disabled');
    const lines = Number(url.searchParams.get('lines') ?? 300);
    try {
      const raw = await readFile(logger.file, 'utf8');
      return sendText(res, 200, raw.split('\n').slice(-lines).join('\n'));
    } catch (err) {
      return sendText(res, 200, `could not read log: ${err.message}`);
    }
  });

  /* -------------------------------------------------------- tokenizer -- */

  router.get('/api/tokenizer/inventory', async (req, res) => {
    sendJson(res, 200, await counter.registry.inventory());
  });

  router.post('/api/tokenizer/count', async (req, res) => {
    const body = await readJson(req, 4 * 1024 * 1024);
    const route = config.findModel(body.model);
    // Resolve exactly as the relay does: the model name drives the rules, and
    // an explicitly pinned vocabulary wins over them.
    const model = route?.upstreamModel || body.model || '';
    const override = {
      tokenizer: body.tokenizer || route?.tokenizer || '',
      profile: body.profile || route?.chatProfile || '',
    };

    if (Array.isArray(body.messages)) {
      const cfg = config.get();
      const upstreamShape = { messages: body.messages, tools: body.tools };
      const counted = await counter.countRequest(upstreamShape, model, cfg.tokenizer.imageDefaults, override);
      const resolved = await counter.resolve(model, override);
      return sendJson(res, 200, {
        mode: 'messages',
        ...counted,
        profile: resolved.profile,
        resolved: { tokenizer: resolved.tokenizer, profile: resolved.profile },
      });
    }

    const detail = await counter.pieces(body.text ?? '', model, Number(body.limit ?? 2000), override);
    const resolved = await counter.resolve(model, override);
    return sendJson(res, 200, {
      mode: 'text',
      ...detail,
      resolved: { tokenizer: resolved.tokenizer, profile: resolved.profile },
    });
  });

  router.post('/api/tokenizer/install', async (req, res) => {
    const body = await readJson(req, 8192);
    const args = body.hf ? ['--hf', body.hf, '--as', body.as || body.hf.split('/').pop().toLowerCase()]
      : body.url ? ['--url', body.url, '--as', body.as]
        : [body.name];
    if (!args.filter(Boolean).length) return sendError(res, 400, 'pass name, hf or url');

    const script = path.join(paths.root, 'scripts', 'fetch-tokenizer.mjs');
    const out = await runNode([script, ...args], { cwd: paths.root, env: process.env });
    counter.registry.invalidate();
    return sendJson(res, out.code === 0 ? 200 : 500, {
      ok: out.code === 0,
      output: out.output,
      inventory: await counter.registry.inventory(),
    });
  });

  /* ----------------------------------------------------------- tunnel -- */

  router.get('/api/tunnel', async (req, res) => {
    sendJson(res, 200, { ...tunnel.status(), cloudflared: await tunnel.version() });
  });

  router.post('/api/tunnel/:action', async (req, res, { params }) => {
    try {
      if (params.action === 'start') return sendJson(res, 200, await tunnel.start());
      if (params.action === 'stop') return sendJson(res, 200, await tunnel.stop());
      if (params.action === 'restart') return sendJson(res, 200, await tunnel.restart());
      return sendError(res, 400, `unknown tunnel action "${params.action}"`);
    } catch (err) {
      return sendError(res, err.status ?? 500, err.message);
    }
  });

  /* ------------------------------------------------------- playground -- */

  router.post('/api/playground', async (req, res) => {
    const body = await readJson(req, 1024 * 1024);
    const cfg = config.get();
    const keyRow = cfg.keys.find((k) => k.enabled);
    if (cfg.security.requireClientKey && !keyRow) {
      return sendError(res, 400, 'create a client key first, or turn off security.requireClientKey');
    }
    const started = Date.now();
    try {
      const upstream = await fetch(`http://127.0.0.1:${cfg.server.port}/v1/chat/completions`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          ...(keyRow ? { authorization: `Bearer ${keyRow.key}` } : {}),
        },
        body: JSON.stringify({ ...body, stream: false }),
        signal: AbortSignal.timeout(300000),
      });
      const payload = await upstream.json().catch(() => ({}));
      return sendJson(res, 200, { status: upstream.status, ms: Date.now() - started, body: payload });
    } catch (err) {
      return sendJson(res, 200, { status: 0, ms: Date.now() - started, error: err.message });
    }
  });

  /* ------------------------------------------------------------ serve -- */

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, `http://${req.headers.host ?? 'localhost'}`);
    applyCors(req, res, ['*']);
    if (req.method === 'OPTIONS') {
      res.writeHead(204);
      return res.end();
    }

    const match = router.match(req.method, url.pathname);
    if (match) {
      const open = url.pathname === '/api/login' || url.pathname === '/api/session';
      if (!open && !authed(req)) return sendError(res, 401, 'not signed in', { type: 'unauthorized' });
      try {
        await match.handler(req, res, { params: match.params, url });
      } catch (err) {
        logger.error(`dashboard ${req.method} ${url.pathname}: ${err.stack ?? err.message}`);
        if (!res.headersSent) {
          sendJson(res, err.status ?? 500, { error: { message: err.message, details: err.errors ?? null } });
        }
      }
      return undefined;
    }

    if (req.method === 'GET') {
      if (await serveStatic(res, publicDir, url.pathname)) return undefined;
      if (await serveStatic(res, publicDir, '/index.html')) return undefined;
    }
    return sendError(res, 404, 'not found');
  });

  return server;
}

/* --------------------------------------------------------------- utils -- */

function cookie(req, name) {
  const raw = req.headers.cookie;
  if (!raw) return null;
  for (const part of raw.split(';')) {
    const [k, ...v] = part.trim().split('=');
    if (k === name) return v.join('=');
  }
  return null;
}

/**
 * The dashboard only ever sees masked secrets, so a save that echoes a mask
 * back must keep the stored value rather than overwrite it with the mask.
 */
export function unmaskSecrets(patch, current) {
  const SECRETS = new Set(['apiKey', 'key', 'password', 'token']);
  const walk = (p, c) => {
    if (Array.isArray(p)) {
      return p.map((item) => {
        if (!isPlainObject(item) || !item.id) return walk(item, null);
        const match = Array.isArray(c) ? c.find((x) => x?.id === item.id) : null;
        return walk(item, match);
      });
    }
    if (!isPlainObject(p)) return p;
    const out = {};
    for (const [k, v] of Object.entries(p)) {
      const cur = isPlainObject(c) ? c[k] : undefined;
      if (SECRETS.has(k) && typeof v === 'string' && typeof cur === 'string' && v === maskSecret(cur)) {
        out[k] = cur; // unchanged mask: keep the real secret
      } else if (isPlainObject(v) || Array.isArray(v)) {
        out[k] = walk(v, cur);
      } else {
        out[k] = v;
      }
    }
    return out;
  };
  return walk(patch, current);
}

function maskItem(item) {
  const out = { ...item };
  for (const k of ['apiKey', 'key', 'token', 'password']) {
    if (typeof out[k] === 'string' && out[k]) out[k] = maskSecret(out[k]);
  }
  return out;
}

async function probeBackend(backend) {
  const base = backend.baseUrl.replace(/\/+$/, '');
  const url = /\/v\d+$/.test(base) ? `${base}/models` : `${base}/v1/models`;
  const started = Date.now();
  try {
    const res = await fetch(url, {
      headers: backend.apiKey
        ? (backend.type === 'anthropic'
          ? { 'x-api-key': backend.apiKey, 'anthropic-version': '2023-06-01' }
          : { authorization: `Bearer ${backend.apiKey}` })
        : {},
      signal: AbortSignal.timeout(20000),
    });
    const text = await res.text();
    let models = [];
    try {
      models = (JSON.parse(text)?.data ?? []).map((m) => m.id).filter(Boolean);
    } catch { /* not every gateway returns a model list */ }
    return {
      ok: res.ok,
      status: res.status,
      ms: Date.now() - started,
      models: models.slice(0, 200),
      body: res.ok ? undefined : text.slice(0, 400),
      url,
    };
  } catch (err) {
    return { ok: false, status: 0, ms: Date.now() - started, error: err.message, url };
  }
}

function runNode(args, opts) {
  return new Promise((resolve) => {
    const proc = spawn(process.execPath, args, { ...opts, stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    proc.stdout.on('data', (b) => { output += b.toString(); });
    proc.stderr.on('data', (b) => { output += b.toString(); });
    proc.on('close', (code) => resolve({ code, output: output.slice(-8000) }));
    proc.on('error', (err) => resolve({ code: 1, output: err.message }));
  });
}

function rangeStart(range) {
  const m = /^(\d+)([hd])$/.exec(String(range));
  if (!m) return 0;
  const n = Number(m[1]);
  return Date.now() - n * (m[2] === 'h' ? 3600000 : 86400000);
}

function startOfToday(timeZone) {
  const today = dayKey(Date.now(), timeZone);
  // Derive the epoch ms of local midnight by probing back through the day.
  const guess = new Date(`${today}T00:00:00Z`).getTime();
  for (let offset = -14; offset <= 14; offset++) {
    const candidate = guess - offset * 3600000;
    if (dayKey(candidate, timeZone) === today && dayKey(candidate - 1, timeZone) !== today) return candidate;
  }
  return guess;
}
