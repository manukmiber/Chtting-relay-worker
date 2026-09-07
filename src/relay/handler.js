import { newId, dayKey, hourKey, nowMs, round, truncate } from '../util/misc.js';
import { reconcileUsage } from '../tokenizer/index.js';
import { SseParser, StreamRewriter, formatSse, SSE_HEADERS } from './sse.js';
import {
  transformRequest,
  transformResponse,
  transformChunk,
  resolveResponseTransform,
  compileTextRules,
  rulesLookbehind,
  flattenContent,
} from './transform.js';
import { sendError, sendJson } from '../server/http-util.js';

/**
 * The relay proper: authenticate, translate the model name, inject the system
 * prompt, call the backend, reshape what comes back, and record what it cost.
 *
 * Timing vocabulary used throughout:
 *   ttft_ms - request start to the first content token (streams only)
 *   gen_ms  - first token to last token
 *   tps     - completion tokens / gen_ms, i.e. real generation throughput
 */
export class RelayHandler {
  constructor({ config, store, counter, upstream, logger, limiter }) {
    this.config = config;
    this.store = store;
    this.counter = counter;
    this.upstream = upstream;
    this.logger = logger;
    this.limiter = limiter;
  }

  /* ------------------------------------------------------------- auth -- */

  authenticate(secret) {
    const cfg = this.config.get();
    if (!cfg.security.requireClientKey) {
      return { ok: true, key: { id: 'anonymous', label: 'anonymous', models: ['*'], quota: {} } };
    }
    if (!secret) return { ok: false, status: 401, message: 'missing API key: send Authorization: Bearer <key>' };
    const key = this.config.findKeyBySecret(secret);
    if (!key) return { ok: false, status: 401, message: 'invalid API key' };
    if (!key.enabled) return { ok: false, status: 403, message: `key "${key.label || key.id}" is disabled` };
    return { ok: true, key };
  }

  checkAccess(key, modelId) {
    const allowed = key.models ?? ['*'];
    if (allowed.includes('*') || allowed.includes(modelId)) return { ok: true };
    return { ok: false, status: 403, message: `key "${key.label || key.id}" may not use model "${modelId}"` };
  }

  checkQuota(key) {
    const cfg = this.config.get();
    const quota = key.quota ?? {};
    const rate = this.limiter.check(key.id, quota.requestsPerMinute);
    if (!rate.allowed) {
      return {
        ok: false,
        status: 429,
        message: `rate limit reached (${quota.requestsPerMinute}/min)`,
        headers: { 'retry-after': String(rate.retryAfter) },
      };
    }
    if (quota.requestsPerDay > 0 || quota.tokensPerDay > 0) {
      const today = dayKey(Date.now(), cfg.timezone);
      const used = this.store.usageForKey(key.id, today);
      if (quota.requestsPerDay > 0 && used.requests >= quota.requestsPerDay) {
        return { ok: false, status: 429, message: `daily request quota reached (${quota.requestsPerDay})` };
      }
      if (quota.tokensPerDay > 0 && used.tokens >= quota.tokensPerDay) {
        return { ok: false, status: 429, message: `daily token quota reached (${quota.tokensPerDay})` };
      }
    }
    return { ok: true };
  }

  /* ------------------------------------------------------- model list -- */

  listModels() {
    const cfg = this.config.get();
    return {
      object: 'list',
      data: cfg.models.filter((m) => m.enabled).map((m) => ({
        id: m.id,
        object: 'model',
        created: Math.floor((m.createdAt ?? Date.now()) / 1000),
        owned_by: 'chtting-relay',
        // The backend's real name is deliberately not exposed here.
        display_name: m.displayName || m.id,
        description: m.description || undefined,
        context_length: m.contextLength || undefined,
      })),
    };
  }

  /* --------------------------------------------------------- dispatch -- */

  async handleChat(req, res, { body, key, ip, endpoint = 'v1/chat/completions' }) {
    const cfg = this.config.get();
    const started = nowMs();
    const startedWall = Date.now();
    const id = newId('req');

    const record = {
      id,
      ts: startedWall,
      day: dayKey(startedWall, cfg.timezone),
      hour: hourKey(startedWall, cfg.timezone),
      key_id: key.id,
      key_label: key.label || key.id,
      ip,
      user_agent: truncate(req.headers['user-agent'] ?? '', 200),
      endpoint,
      public_model: String(body?.model ?? ''),
    };

    const route = this.config.findModel(body?.model);
    if (!route || !route.enabled) {
      this.#finish(record, { status: 404, error: `model "${body?.model}" is not available` });
      return sendError(res, 404, `model "${body?.model}" is not available on this relay`, { code: 'model_not_found', param: 'model' });
    }

    const access = this.checkAccess(key, route.id);
    if (!access.ok) {
      this.#finish(record, { status: access.status, error: access.message });
      return sendError(res, access.status, access.message, { code: 'model_forbidden' });
    }

    record.backend_id = route.backend;
    record.upstream_model = route.upstreamModel;

    // 2. count what goes in, using the tokenizer of the *backend* model
    const tokenizerName = route.upstreamModel;
    const tokenizerOverride = { tokenizer: route.tokenizer, profile: route.chatProfile };
    const upstreamBody = transformRequest(body, route, cfg.defaults, cfg.systemPrompts);
    const inputCount = await this.counter.countRequest(
      upstreamBody, tokenizerName, cfg.tokenizer.imageDefaults, tokenizerOverride,
    );
    record.local_prompt = inputCount.total;
    record.tokenizer = inputCount.tokenizer;
    record.exact = inputCount.exact ? 1 : 0;

    const maxIn = route.limits?.maxInputTokens ?? 0;
    if (maxIn > 0 && inputCount.total > maxIn) {
      const msg = `prompt is ${inputCount.total} tokens, over this model's ${maxIn} token limit`;
      this.#finish(record, { status: 413, error: msg });
      return sendError(res, 413, msg, { code: 'context_length_exceeded' });
    }

    if (cfg.logging.storeBodies !== 'none') {
      record.req_preview = previewRequest(upstreamBody, cfg.logging);
    }

    const clientWantsStream = Boolean(body?.stream);
    const rt = { ...(cfg.defaults.requestTransform ?? {}), ...(route.requestTransform ?? {}) };
    // Streaming upstream is what makes TTFT and tokens/sec measurable; when the
    // caller asked for a whole response we buffer the stream back together.
    const streamUpstream = rt.forceStream === null || rt.forceStream === undefined
      ? clientWantsStream
      : Boolean(rt.forceStream);

    upstreamBody.stream = streamUpstream;
    if (streamUpstream) {
      const backend = this.config.findBackend(route.backend);
      if (backend?.streamOptions !== false) {
        upstreamBody.stream_options = { ...(upstreamBody.stream_options ?? {}), include_usage: true };
      }
    } else {
      delete upstreamBody.stream_options;
    }

    record.stream = clientWantsStream ? 1 : 0;

    const controller = new AbortController();
    const onClose = () => controller.abort();
    req.once('aborted', onClose);
    res.once('close', () => { if (!res.writableEnded) controller.abort(); });

    let sent;
    try {
      sent = await this.upstream.sendWithFallback({
        route,
        endpoint,
        body: upstreamBody,
        signal: controller.signal,
        stream: streamUpstream,
      });
    } catch (err) {
      const status = err.status ?? 502;
      record.retries = err.attempts ?? 0;
      this.#finish(record, { status, error: err.message, started });
      if (status === 499) return; // the caller hung up; nothing to answer to
      return sendError(res, status, err.message, { type: 'upstream_error' });
    }

    record.backend_id = sent.backend.id;
    record.retries = (sent.attempts ?? 1) - 1;

    if (!sent.res.ok) {
      const detail = sent.errorBody ?? await sent.res.text().catch(() => '');
      const status = sent.res.status;
      this.#finish(record, { status, error: truncate(detail, 500), started });
      return sendError(res, status, `backend rejected the request: ${truncate(detail, 400)}`, { type: 'upstream_error' });
    }

    const transform = resolveResponseTransform(route, cfg.defaults);
    const ctx = {
      record, route, cfg, transform, started, tokenizerName, tokenizerOverride,
      clientWantsStream, streamUpstream, res, req,
    };

    const contentType = sent.res.headers.get('content-type') ?? '';
    const isSse = contentType.includes('text/event-stream');

    if (streamUpstream && isSse) return this.#pipeStream(sent.res, ctx);
    return this.#pipeBuffered(sent.res, ctx);
  }

  /* ------------------------------------------------------- streaming -- */

  async #pipeStream(upstreamRes, ctx) {
    const { record, cfg, transform, started, res, route } = ctx;
    const parser = new SseParser();
    const rules = transform.replace ?? [];
    const rewriteFn = compileTextRules(rules);
    const rewriter = new StreamRewriter(rewriteFn, rulesLookbehind(rules));
    const reasoningState = { open: false };

    let firstTokenAt = 0;
    let lastTokenAt = 0;
    let text = '';
    let reasoningText = '';
    let upstreamUsage = null;
    let finishReason = '';
    let prefixSent = false;
    const toolCalls = new Map();

    // The client may have asked for a whole response while we streamed upstream
    // purely to measure TTFT; in that case nothing is written until the end.
    const streamingToClient = ctx.clientWantsStream;
    if (streamingToClient) {
      res.writeHead(200, { ...SSE_HEADERS, 'x-relay-request-id': record.id });
      res.flushHeaders?.();
    }

    const emit = (chunk) => {
      if (streamingToClient && !res.writableEnded) res.write(formatSse(chunk));
    };

    const emitContent = (chunk, contentDelta) => {
      // Rewrites are applied to a safe prefix so a pattern can span chunks.
      const safe = rewriter.push(contentDelta);
      if (!streamingToClient) return;
      if (contentDelta && !safe) return; // held back, will flush later
      const out = structuredClone(chunk);
      if (out.choices?.[0]?.delta) out.choices[0].delta.content = safe;
      emit(out);
    };

    // A streaming decoder, so a multi-byte character split across two network
    // chunks is reassembled instead of turning into replacement characters.
    const decoder = new TextDecoder('utf-8');

    try {
      for await (const buf of upstreamRes.body) {
        const events = parser.push(decoder.decode(buf, { stream: true }));
        for (const ev of events) {
          if (ev.data === '[DONE]') continue;

          let chunk;
          try {
            chunk = JSON.parse(ev.data);
          } catch {
            continue; // ignore keep-alive comments and malformed frames
          }

          if (chunk.usage) upstreamUsage = chunk.usage;
          const choice = chunk.choices?.[0];
          const delta = choice?.delta ?? {};
          if (choice?.finish_reason) finishReason = choice.finish_reason;

          const contentDelta = typeof delta.content === 'string' ? delta.content : '';
          const reasoningDelta = delta.reasoning_content ?? delta.reasoning ?? '';
          if (Array.isArray(delta.tool_calls)) collectToolCalls(toolCalls, delta.tool_calls);

          if ((contentDelta || reasoningDelta) && !firstTokenAt) firstTokenAt = nowMs();
          if (contentDelta || reasoningDelta) lastTokenAt = nowMs();
          text += contentDelta;
          if (reasoningDelta) reasoningText += reasoningDelta;

          const shaped = transformChunk(chunk, { publicModel: route.id, transform, reasoningState });
          if (shaped.choices?.[0]?.delta) {
            const shapedDelta = shaped.choices[0].delta.content;
            if (typeof shapedDelta === 'string' && shapedDelta) {
              if (!prefixSent && transform.prefix) {
                shaped.choices[0].delta.content = transform.prefix + shapedDelta;
                prefixSent = true;
              }
              emitContent(shaped, shaped.choices[0].delta.content);
              continue;
            }
          }
          emit(shaped);
        }
      }

      // Flush whatever the rewriter was holding back, plus any suffix.
      const tail = rewriter.flush() + (transform.suffix ?? '');
      if (tail && streamingToClient) {
        emit(finalDeltaChunk(record.id, route.id, tail));
      }
    } catch (err) {
      const aborted = ctx.req.destroyed || err.name === 'AbortError';
      this.#finish(record, {
        status: aborted ? 499 : 502,
        error: aborted ? 'client disconnected mid-stream' : err.message,
        started,
        firstTokenAt,
        lastTokenAt,
      });
      if (streamingToClient && !res.writableEnded) {
        res.write(formatSse({ error: { message: err.message, type: 'upstream_error' } }));
        res.end();
      } else if (!res.headersSent) {
        sendError(res, 502, `stream failed: ${err.message}`, { type: 'upstream_error' });
      }
      return undefined;
    }

    const finalText = (transform.prefix && !prefixSent ? transform.prefix : '')
      + (rewriteFn && !streamingToClient ? rewriteFn(text) : text)
      + (streamingToClient ? '' : (transform.suffix ?? ''));

    const outCount = await this.counter.countOutput(
      streamingToClient ? text : finalText,
      ctx.tokenizerName,
      { reasoning: transform.reasoning === 'strip' ? '' : reasoningText, toolCalls: [...toolCalls.values()] },
      ctx.tokenizerOverride,
    );

    const usage = reconcileUsage({
      local: { prompt: record.local_prompt, completion: outCount.total, exact: outCount.exact },
      upstream: upstreamUsage,
      preferUpstream: cfg.tokenizer.preferUpstreamUsage,
    });

    if (streamingToClient) {
      if (usage.prompt_tokens || usage.completion_tokens) {
        emit(usageChunk(record.id, route.id, usage));
      }
      if (!res.writableEnded) {
        res.write('data: [DONE]\n\n');
        res.end();
      }
    } else {
      // Rebuild a normal chat completion from what we streamed.
      const assembled = assembleCompletion({
        id: record.id,
        model: route.id,
        content: finalText,
        reasoning: transform.reasoning === 'strip' ? '' : reasoningText,
        toolCalls: [...toolCalls.values()],
        finishReason: finishReason || 'stop',
        usage,
      });
      sendJson(res, 200, assembled, { 'x-relay-request-id': record.id });
    }

    this.#finish(record, {
      status: 200,
      started,
      firstTokenAt,
      lastTokenAt,
      usage,
      finishReason,
      responsePreview: cfg.logging.storeBodies === 'none' ? '' : truncate(text, cfg.logging.previewChars),
    });
    return undefined;
  }

  /* -------------------------------------------------------- buffered -- */

  async #pipeBuffered(upstreamRes, ctx) {
    const { record, cfg, transform, started, res, route } = ctx;
    let payload;
    try {
      payload = await upstreamRes.json();
    } catch (err) {
      this.#finish(record, { status: 502, error: `backend sent a non-JSON response: ${err.message}`, started });
      return sendError(res, 502, 'backend sent a response the relay could not parse', { type: 'upstream_error' });
    }

    const shaped = transformResponse(payload, { publicModel: route.id, transform });
    const choice = shaped.choices?.[0];
    const content = flattenContent(choice?.message?.content ?? choice?.text ?? '');
    const reasoning = choice?.message?.reasoning_content ?? choice?.message?.reasoning ?? '';

    const outCount = await this.counter.countOutput(content, ctx.tokenizerName, {
      reasoning: transform.reasoning === 'strip' ? '' : reasoning,
      toolCalls: choice?.message?.tool_calls ?? [],
    }, ctx.tokenizerOverride);

    const usage = reconcileUsage({
      local: { prompt: record.local_prompt, completion: outCount.total, exact: outCount.exact },
      upstream: payload.usage,
      preferUpstream: cfg.tokenizer.preferUpstreamUsage,
    });
    shaped.usage = publicUsage(usage);

    if (ctx.clientWantsStream) {
      // The backend could not stream, so replay the finished answer as SSE.
      res.writeHead(200, { ...SSE_HEADERS, 'x-relay-request-id': record.id });
      for (const chunk of simulateStream(record.id, route.id, content, usage)) {
        res.write(formatSse(chunk));
      }
      res.write('data: [DONE]\n\n');
      res.end();
    } else {
      sendJson(res, 200, shaped, { 'x-relay-request-id': record.id });
    }

    const finishedAt = nowMs();
    this.#finish(record, {
      status: 200,
      started,
      firstTokenAt: 0,
      lastTokenAt: finishedAt,
      usage,
      finishReason: choice?.finish_reason ?? '',
      responsePreview: cfg.logging.storeBodies === 'none' ? '' : truncate(content, cfg.logging.previewChars),
    });
    return undefined;
  }

  /* ---------------------------------------------------------- record -- */

  #finish(record, {
    status, error = '', started = 0, firstTokenAt = 0, lastTokenAt = 0,
    usage = null, finishReason = '', responsePreview = '',
  }) {
    const end = nowMs();
    const totalMs = started ? end - started : 0;
    const ttft = firstTokenAt && started ? firstTokenAt - started : 0;
    const genMs = firstTokenAt && lastTokenAt > firstTokenAt ? lastTokenAt - firstTokenAt : 0;
    const completion = usage?.completion_tokens ?? 0;

    const row = {
      ...record,
      status,
      error: truncate(error, 800),
      finish_reason: finishReason,
      total_ms: round(totalMs, 1),
      ttft_ms: round(ttft, 1),
      gen_ms: round(genMs, 1),
      prompt_tokens: usage?.prompt_tokens ?? record.local_prompt ?? 0,
      completion_tokens: completion,
      total_tokens: usage?.total_tokens ?? (usage?.prompt_tokens ?? 0) + completion,
      cached_tokens: usage?.cached_tokens ?? 0,
      reasoning_tokens: usage?.reasoning_tokens ?? 0,
      usage_source: usage?.source ?? '',
      exact: usage ? (usage.exact ? 1 : 0) : (record.exact ?? 1),
      local_prompt: usage?.local?.prompt_tokens ?? record.local_prompt ?? 0,
      local_completion: usage?.local?.completion_tokens ?? 0,
      drift_prompt: usage?.drift?.prompt ?? 0,
      drift_completion: usage?.drift?.completion ?? 0,
      // Throughput over the generation phase only, which is the number that
      // actually describes how fast the model produced tokens.
      tokens_per_sec: genMs > 0 && completion > 0 ? round((completion / genMs) * 1000, 2) : 0,
      res_preview: responsePreview,
    };

    try {
      this.store.insert(row);
    } catch (err) {
      this.logger.error(`failed to record request ${row.id}: ${err.message}`);
    }

    const tag = status >= 400 || status === 0 ? 'error' : 'ok';
    this.logger.info(
      `${tag} ${row.public_model} -> ${row.upstream_model} `
      + `${row.prompt_tokens}in/${row.completion_tokens}out `
      + `ttft=${row.ttft_ms}ms total=${row.total_ms}ms tps=${row.tokens_per_sec} `
      + `key=${row.key_label}${error ? ` err=${truncate(error, 160)}` : ''}`,
    );
    return row;
  }
}

/* ------------------------------------------------------------- helpers -- */

function collectToolCalls(map, deltas) {
  for (const tc of deltas) {
    const idx = tc.index ?? 0;
    const existing = map.get(idx) ?? { id: tc.id, type: 'function', function: { name: '', arguments: '' } };
    if (tc.id) existing.id = tc.id;
    if (tc.function?.name) existing.function.name += tc.function.name;
    if (tc.function?.arguments) existing.function.arguments += tc.function.arguments;
    map.set(idx, existing);
  }
}

function publicUsage(usage) {
  return {
    prompt_tokens: usage.prompt_tokens,
    completion_tokens: usage.completion_tokens,
    total_tokens: usage.total_tokens,
    ...(usage.cached_tokens ? { prompt_tokens_details: { cached_tokens: usage.cached_tokens } } : {}),
    ...(usage.reasoning_tokens ? { completion_tokens_details: { reasoning_tokens: usage.reasoning_tokens } } : {}),
  };
}

function finalDeltaChunk(id, model, content) {
  return {
    id,
    object: 'chat.completion.chunk',
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{ index: 0, delta: { content }, finish_reason: null }],
  };
}

function usageChunk(id, model, usage) {
  return {
    id,
    object: 'chat.completion.chunk',
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [],
    usage: publicUsage(usage),
  };
}

function assembleCompletion({ id, model, content, reasoning, toolCalls, finishReason, usage }) {
  const message = { role: 'assistant', content };
  if (reasoning) message.reasoning_content = reasoning;
  if (toolCalls?.length) message.tool_calls = toolCalls;
  return {
    id,
    object: 'chat.completion',
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{ index: 0, message, finish_reason: finishReason, logprobs: null }],
    usage: publicUsage(usage),
  };
}

/** Replay a finished answer as SSE for callers that insisted on streaming. */
function* simulateStream(id, model, content, usage) {
  const created = Math.floor(Date.now() / 1000);
  yield {
    id, object: 'chat.completion.chunk', created, model,
    choices: [{ index: 0, delta: { role: 'assistant', content: '' }, finish_reason: null }],
  };
  const step = 24;
  for (let i = 0; i < content.length; i += step) {
    yield {
      id, object: 'chat.completion.chunk', created, model,
      choices: [{ index: 0, delta: { content: content.slice(i, i + step) }, finish_reason: null }],
    };
  }
  yield {
    id, object: 'chat.completion.chunk', created, model,
    choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
    usage: publicUsage(usage),
  };
}

function previewRequest(body, logging) {
  if (logging.storeBodies === 'full') return truncate(JSON.stringify(body), 20000);
  const last = [...(body.messages ?? [])].reverse().find((m) => m.role === 'user');
  return truncate(flattenContent(last?.content ?? body.prompt ?? ''), logging.previewChars);
}
