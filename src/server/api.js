import http from 'node:http';
import { Router, sendJson, sendError, readJson, applyCors, clientIp, bearerToken } from './http-util.js';
import { truncate, dayKey, hourKey, newId, nowMs, round } from '../util/misc.js';

/**
 * The public, tunnel-facing server. It speaks the OpenAI HTTP API so any
 * existing client works unchanged, and exposes nothing about the real backend.
 */
export function createApiServer({ config, handler, logger, store, counter }) {
  const router = new Router();

  router.get('/health', (req, res) => {
    const cfg = config.get();
    sendJson(res, 200, {
      status: 'ok',
      service: 'chtting-relay',
      models: cfg.models.filter((m) => m.enabled).length,
      backends: cfg.backends.filter((b) => b.enabled).length,
      uptime_s: Math.round(process.uptime()),
    });
  });

  router.get('/v1/models', (req, res, { auth }) => {
    if (!auth.ok) return sendError(res, auth.status, auth.message, { code: 'invalid_api_key' });
    return sendJson(res, 200, handler.listModels());
  });

  router.get('/v1/models/:id', (req, res, { auth, params }) => {
    if (!auth.ok) return sendError(res, auth.status, auth.message, { code: 'invalid_api_key' });
    const found = handler.listModels().data.find((m) => m.id === params.id);
    if (!found) return sendError(res, 404, `model "${params.id}" not found`, { code: 'model_not_found' });
    return sendJson(res, 200, found);
  });

  const chat = async (req, res, ctx) => {
    if (!ctx.auth.ok) return sendError(res, ctx.auth.status, ctx.auth.message, { code: 'invalid_api_key' });
    const quota = handler.checkQuota(ctx.auth.key);
    if (!quota.ok) {
      for (const [k, v] of Object.entries(quota.headers ?? {})) res.setHeader(k, v);
      return sendError(res, quota.status, quota.message, { code: 'rate_limit_exceeded' });
    }
    const body = await readJson(req, config.get().server.maxBodyBytes);
    if (!body?.model) return sendError(res, 400, 'the "model" field is required', { param: 'model' });
    return handler.handleChat(req, res, {
      body,
      key: ctx.auth.key,
      ip: ctx.ip,
      endpoint: ctx.endpoint,
    });
  };

  router.post('/v1/chat/completions', (req, res, ctx) => chat(req, res, { ...ctx, endpoint: 'v1/chat/completions' }));
  router.post('/chat/completions', (req, res, ctx) => chat(req, res, { ...ctx, endpoint: 'v1/chat/completions' }));
  router.post('/v1/completions', (req, res, ctx) => chat(req, res, { ...ctx, endpoint: 'v1/completions' }));

  router.post('/v1/embeddings', async (req, res, ctx) => {
    if (!ctx.auth.ok) return sendError(res, ctx.auth.status, ctx.auth.message, { code: 'invalid_api_key' });
    return handleEmbeddings(req, res, ctx, { config, handler, store, counter, logger });
  });

  const server = http.createServer(async (req, res) => {
    const cfg = config.get();
    const url = new URL(req.url, `http://${req.headers.host ?? 'localhost'}`);
    const ip = clientIp(req, cfg.security.trustProxyHeaders);

    applyCors(req, res, cfg.security.corsOrigins);
    if (req.method === 'OPTIONS') {
      res.writeHead(204);
      return res.end();
    }

    if (cfg.security.blockedIps?.includes(ip)) {
      return sendError(res, 403, 'blocked', { type: 'forbidden' });
    }

    const match = router.match(req.method, url.pathname);
    if (!match) {
      return sendError(res, 404, `no route for ${req.method} ${url.pathname}`, { type: 'not_found' });
    }

    const auth = handler.authenticate(bearerToken(req));
    try {
      await match.handler(req, res, { auth, ip, params: match.params, url });
    } catch (err) {
      logger.error(`unhandled error on ${req.method} ${url.pathname}: ${err.stack ?? err.message}`);
      if (!res.headersSent) {
        sendError(res, err.status ?? 500, err.status ? err.message : 'internal relay error', { type: 'server_error' });
      } else if (!res.writableEnded) {
        res.end();
      }
    }
    return undefined;
  });

  server.keepAliveTimeout = config.get().server.keepAliveTimeoutMs;
  server.headersTimeout = server.keepAliveTimeout + 5000;
  server.requestTimeout = 0; // long generations must not be cut off by the relay
  return server;
}

/**
 * Embeddings get the same alias translation and token accounting as chat, but
 * there is no stream and no reshaping beyond the model name.
 */
async function handleEmbeddings(req, res, ctx, { config, handler, store, counter, logger }) {
  const cfg = config.get();
  const body = await readJson(req, cfg.server.maxBodyBytes);
  const route = config.findModel(body?.model);
  if (!route || !route.enabled) {
    return sendError(res, 404, `model "${body?.model}" is not available on this relay`, { code: 'model_not_found' });
  }
  const access = handler.checkAccess(ctx.auth.key, route.id);
  if (!access.ok) return sendError(res, access.status, access.message, { code: 'model_forbidden' });

  const backend = config.findBackend(route.backend);
  if (!backend?.enabled) return sendError(res, 502, 'backend unavailable', { type: 'upstream_error' });

  const started = nowMs();
  const startedWall = Date.now();
  const inputs = Array.isArray(body.input) ? body.input : [body.input ?? ''];
  const counted = await Promise.all(inputs.map((t) => counter.countText(String(t), route.tokenizer || route.upstreamModel)));
  const localPrompt = counted.reduce((a, c) => a + c.total, 0);

  const upstreamBody = { ...body, model: route.upstreamModel };
  let status = 502;
  let error = '';
  let payload = null;
  try {
    const { res: upRes } = await handler.upstream.send({ backend, endpoint: 'v1/embeddings', body: upstreamBody, stream: false });
    status = upRes.status;
    payload = await upRes.json().catch(() => null);
    if (!upRes.ok) error = payload?.error?.message ?? `backend returned ${status}`;
  } catch (err) {
    error = err.message;
  }

  const promptTokens = payload?.usage?.prompt_tokens || localPrompt;
  store.insert({
    id: newId('emb'),
    ts: startedWall,
    day: dayKey(startedWall, cfg.timezone),
    hour: hourKey(startedWall, cfg.timezone),
    key_id: ctx.auth.key.id,
    key_label: ctx.auth.key.label || ctx.auth.key.id,
    ip: ctx.ip,
    user_agent: truncate(req.headers['user-agent'] ?? '', 200),
    endpoint: 'v1/embeddings',
    public_model: route.id,
    backend_id: backend.id,
    upstream_model: route.upstreamModel,
    status,
    error: truncate(error, 500),
    total_ms: round(nowMs() - started, 1),
    prompt_tokens: promptTokens,
    total_tokens: promptTokens,
    local_prompt: localPrompt,
    drift_prompt: localPrompt - promptTokens,
    usage_source: payload?.usage?.prompt_tokens ? 'upstream' : 'local',
    tokenizer: counted[0]?.tokenizer ?? '',
    exact: counted.every((c) => c.exact) ? 1 : 0,
  });

  if (error) {
    logger.warn(`embeddings failed for ${route.id}: ${error}`);
    return sendError(res, status >= 400 ? status : 502, error, { type: 'upstream_error' });
  }
  return sendJson(res, 200, { ...payload, model: route.id });
}
