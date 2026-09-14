import {
  h, card, copy, pill, mount, toast, fmtBytes,
} from '../ui.js';

/**
 * The integration guide, written from the service's own configuration.
 *
 * Every figure on this page — base URL, model ids, token ceilings, per-key
 * rate limits, prices — is read from the running config rather than typed into
 * a document that would start drifting the moment a setting changed. The same
 * model renders twice: once as this page, once as Markdown, so the text an
 * operator pastes into an email is the text they were just looking at.
 *
 * It describes ONE service. Nothing here may name, imply or invite a question
 * about what produces an answer: no second layer, no vendor, no capacity
 * figures. This text goes to customers and gets copied around.
 */
export async function docsView(ctx) {
  const doc = buildDoc(ctx.state ?? {});
  const root = h('div.grid');

  const actions = h('div.row', {},
    h('button.primary', {
      onclick: () => copy(toMarkdown(doc), 'Whole guide copied as Markdown'),
    }, '⧉ Copy as Markdown'),
    h('button.ghost', { onclick: () => download(doc) }, '↓ Download .md'),
    h('button.ghost', {
      onclick: () => copy(doc.baseUrl, 'Base URL copied'),
    }, '⧉ Copy base URL'),
  );

  mount(root,
    card('API documentation', h('div', {},
      h('p.small.muted', {
        text: 'Everything an integrator needs, generated from this service’s live configuration. '
            + 'Copy it as Markdown to send on — it stays accurate because nothing here is hard-coded.',
      }),
      actions,
      doc.warnings.length
        ? h('div.row', { style: { marginTop: '10px' } },
          ...doc.warnings.map((w) => pill(w, 'warn')))
        : null,
    )),
    ...doc.sections.map(renderSection),
  );
  return root;
}

/* ------------------------------------------------------------ the doc -- */

function buildDoc(state) {
  const cfg = state.config ?? {};
  const server = cfg.server ?? {};
  const security = cfg.security ?? {};
  const models = (cfg.models ?? []).filter((m) => m.enabled !== false);
  const keys = (cfg.keys ?? []).filter((k) => k.enabled !== false);
  const backends = cfg.backends ?? [];
  const tunnelUrl = state.tunnel?.url ?? '';
  const localUrl = state.relay?.localUrl ?? `http://127.0.0.1:${server.port ?? 8788}`;
  const origin = tunnelUrl || localUrl;
  const baseUrl = `${origin.replace(/\/+$/, '')}/v1`;
  const sample = models[0]?.id ?? 'your-model-id';
  const keyPlaceholder = 'Kunci-Zeiko-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX';

  const warnings = [];
  if (!tunnelUrl) warnings.push('No tunnel is up — the base URL below is local only');
  if (!models.length) warnings.push('No models are enabled yet');
  if (!keys.length) warnings.push('No client keys exist yet');

  return {
    baseUrl,
    title: 'API integration guide',
    warnings,
    sections: [
      endpointSection(baseUrl, origin, localUrl, tunnelUrl, cfg),
      authSection(security, keys, keyPlaceholder),
      modelSection(models, cfg),
      chatSection(sample, models),
      streamSection(server, sample),
      usageSection(cfg),
      identitySection(),
      limitSection(server, models, keys, backends, security),
      errorSection(),
      exampleSection(baseUrl, sample, keyPlaceholder),
    ],
  };
}

function endpointSection(baseUrl, origin, localUrl, tunnelUrl, cfg) {
  const rows = [
    ['GET', '/health', 'no', 'Liveness, and how many models are available.'],
    ['GET', '/v1/models', 'yes', 'The model catalogue: `context_length`, `max_completion_tokens`, `architecture`, `pricing` with its hour-and-day windows, `supported_parameters`, `reasoning`.'],
    ['GET', '/models', 'yes', 'The same listing, for clients that omit `/v1`.'],
    ['GET', '/v1/models/{id}', 'yes', 'One model, or 404 with `model_not_found`. Ids containing a `/` work as written.'],
    ['GET', '/models/{id}', 'yes', 'The same, without the prefix.'],
    ['POST', '/v1/chat/completions', 'yes', 'Chat, streaming and non-streaming.'],
    ['POST', '/chat/completions', 'yes', 'The same handler, for clients that omit `/v1`.'],
    ['POST', '/v1/completions', 'yes', 'Legacy text completions, same routing and billing.'],
    ['POST', '/completions', 'yes', 'The same, without the prefix.'],
    ['POST', '/v1/embeddings', 'yes', 'Embeddings, on models that offer them.'],
    ['POST', '/embeddings', 'yes', 'The same, without the prefix.'],
    ['OPTIONS', 'any of the above', 'no', 'CORS preflight.'],
  ];
  // The partner listing is deliberately absent: it is not part of the API a
  // customer integrates against, and naming it here would describe how this
  // service is distributed rather than how to call it.
  return {
    title: 'Base URL and endpoints',
    blocks: [
      { p: 'Point any OpenAI-compatible client at the base URL. Paths below are relative to the origin, so a client that appends `/v1` itself should be given the origin instead.' },
      {
        columns: ['', 'Value'],
        rows: [
          ['**Base URL**', `\`${baseUrl}\``],
          ['Origin', `\`${origin}\``],
          tunnelUrl ? ['Public tunnel', `\`${tunnelUrl}\``] : ['Public tunnel', '_not running_'],
          ['Local', `\`${localUrl}\``],
        ],
      },
      { columns: ['Method', 'Path', 'Key', 'What it does'], rows },
      { note: 'The tunnel address changes whenever the tunnel restarts unless a named tunnel is configured. Agree on a stable hostname before an integrator hard-codes it.' },
    ],
  };
}

function authSection(security, keys, keyPlaceholder) {
  const blocks = [
    { p: 'Every endpoint except `/health` and preflight takes your API key as a bearer token.' },
    { code: `Authorization: Bearer ${keyPlaceholder}`, lang: 'http' },
    { p: 'Keys are the literal prefix `Kunci-Zeiko-` followed by 32 characters mixing digits, lower case, upper case and symbols. A key sent bare, without the `Bearer` scheme, is also accepted \u2014 some clients send it that way.' },
  ];
  if (security.requireClientKey === false) {
    blocks.push({ note: 'This service currently accepts requests **without** a key (`security.requireClientKey` is off). Turn it on before handing the URL out.' });
  }
  if (keys.length) {
    // The kind a key is issued as changes how it is accounted for here, which
    // is ours to know and not a customer's to read: the column named it and
    // the paragraph under it explained the difference. What a holder needs is
    // what they may call and how often.
    blocks.push({
      columns: ['Key', 'Models', 'Requests/min', 'Requests/day', 'Tokens/day'],
      rows: keys.map((k) => [
        k.label || k.id,
        (k.models ?? ['*']).join(', '),
        limitText(k.quota?.requestsPerMinute),
        limitText(k.quota?.requestsPerDay),
        limitText(k.quota?.tokensPerDay),
      ]),
    });
    blocks.push({ note: 'Key values are never shown here. Reveal one on the Keys tab and send it over a channel you trust \u2014 not in the same message as this guide.' });
  }
  return { title: 'Authentication', blocks };
}

function modelSection(models, cfg) {
  if (!models.length) {
    return { title: 'Models', blocks: [{ p: 'No models are enabled. Add one on the Models tab and this section fills itself in.' }] };
  }
  const priced = cfg.pricing?.enabled || models.some((m) => m.pricing?.enabled);
  const blocks = [
    { p: 'These are the exact ids `GET /v1/models` returns and the only values `model` accepts. Aliases resolve to the same route.' },
    {
      columns: ['Model id', 'Aliases', 'Context', 'Max output', 'Stream cap'],
      rows: models.map((m) => [
        `\`${m.id}\``,
        (m.aliases ?? []).length ? (m.aliases ?? []).map((a) => `\`${a}\``).join(', ') : '—',
        m.contextLength ? `${exact(m.contextLength)} tokens` : '—',
        m.limits?.maxOutputTokens ? `${exact(m.limits.maxOutputTokens)} tokens` : 'model default',
        m.maxTokensPerSecond > 0 ? `${m.maxTokensPerSecond} tok/s` : 'full speed',
      ]),
    },
  ];

  if (priced) {
    blocks.push({
      p: 'Rates are USD per million tokens, as charged to the caller. Output is priced '
        + 'by the thinking band the request is on: `reasoning_effort: "max"` (or a '
        + 'thinking budget that large) pays the max column, thinking turned off or set '
        + 'to minimal pays the no-thinking column, and so does a request that never '
        + 'mentioned thinking at all. Everything in between pays the standard column. '
        + 'Reasoning tokens are billed as output at the band\u2019s own rate.',
    });
    blocks.push({
      columns: ['Model', 'Input', 'Cache read', 'Output', 'Output, max thinking',
        'Output, no thinking', 'Per request', 'Refused'],
      rows: models.map((m) => {
        const p = resolvePricing(cfg.pricing ?? {}, m.pricing ?? {});
        return [
          `\`${m.id}\``,
          rate(p.input), rate(p.cachedInput), rate(p.output),
          rate(p.maxThinking.output), rate(p.nonThinking.output),
          p.requestUsd ? `$${p.requestUsd}` : '—',
          p.refusalUsd ? `$${trimZeros(p.refusalUsd)}` : '—',
        ];
      }),
    });
    // The input and cache rates are one per card in every price list we
    // publish; when a band moves them too, the table above would be lying by
    // omission, so it says so rather than showing the standard rate alone.
    const bandInputs = models.flatMap((m) => {
      const p = resolvePricing(cfg.pricing ?? {}, m.pricing ?? {});
      return [['max thinking', p.maxThinking], ['no thinking', p.nonThinking]]
        .filter(([, b]) => (b.input && b.input !== p.input) || (b.cachedInput && b.cachedInput !== p.cachedInput))
        .map(([name, b]) => `\`${m.id}\` on ${name}: input ${rate(b.input || p.input)}, cache read ${rate(b.cachedInput || p.cachedInput)}`);
    });
    if (bandInputs.length) {
      blocks.push({ note: `Input and cache read also change by band — ${bandInputs.join('; ')}.` });
    }
    const refusing = models.some((m) => resolvePricing(cfg.pricing ?? {}, m.pricing ?? {}).refusalUsd > 0);
    if (refusing) {
      blocks.push({
        note: 'A reply that declines the request is billed at the flat "Refused" price instead of its '
          + 'tokens — it still arrives as a normal `200`, and `usage.usage` carries that price.',
      });
    }
    const tiers = [...(cfg.pricing?.tiers ?? []), ...models.flatMap((m) => m.pricing?.tiers ?? [])]
      .filter((t) => t.enabled !== false);
    if (tiers.length) {
      blocks.push({
        columns: ['Tier', 'Applies when', 'Effect'],
        rows: tiers.map((t) => [t.name || t.id, tierCondition(t.when ?? {}), tierEffect(t)]),
      });
      blocks.push({ note: 'Tiers stack in order, so one request can match several. What a request actually cost comes back in `usage.usage`, so nothing has to be re-derived from this table.' });
    }
  }
  return { title: 'Models', blocks };
}

function chatSection(sample, models) {
  const dropped = new Set();
  const forced = new Set();
  for (const m of models) {
    for (const d of m.requestTransform?.dropParams ?? []) dropped.add(d);
    for (const f of Object.keys(m.forceParams ?? {})) forced.add(f);
  }
  const blocks = [
    { p: '`POST /v1/chat/completions` takes the OpenAI request body. Whether a given parameter has an effect depends on the model \u2014 each model\u2019s `supported_parameters` in the catalogue is the authority.' },
    {
      columns: ['Parameter', 'Notes'],
      rows: [
        ['`model`', 'Required. One of the ids above.'],
        ['`messages`', 'Required. The usual `role` / `content` array.'],
        ['`stream`', '`true` returns SSE, `false` one JSON body. Both are supported on every model.'],
        ['`user_id`', 'Who the request is for \u2014 the prompt-cache partition key. See below.'],
        ['`max_tokens` / `max_completion_tokens`', 'Clamped down to the model\u2019s ceiling; whichever field you send is the one kept.'],
        ['`stop`', 'Up to four entries, as OpenAI.'],
        ['`reasoning_effort`', '`none`, `minimal`, `low`, `medium`, `high`, `max`. Also read from `reasoning.effort`, `thinking.effort`, `thinking.budget_tokens` and `enable_thinking`. Can move the price.'],
        ['`stream_options`', 'Ignored: usage always arrives on the final chunk, so `include_usage` is not needed.'],
        ['`reasoning.exclude`, `include_reasoning`', 'Leave the reasoning trace out of this reply. Only ever removes one \u2014 neither can turn on a trace the model keeps off.'],
        ['`temperature`, `top_p`, `top_k`, `min_p`, `presence_penalty`, `frequency_penalty`, `seed`', 'Standard sampling and scoring parameters.'],
        ['`response_format` (JSON mode), `tools`, `tool_choice`', 'JSON mode and tool calling, on models that list them.'],
      ],
    },
  ];
  if (dropped.size) {
    blocks.push({ p: `Currently ignored: ${[...dropped].map((d) => `\`${d}\``).join(', ')}.` });
  }
  if (forced.size) {
    blocks.push({ p: `Fixed for this model, so a caller’s value is overridden: ${[...forced].map((f) => `\`${f}\``).join(', ')}.` });
  }
  blocks.push({
    code: JSON.stringify({
      model: sample,
      messages: [
        { role: 'system', content: 'You are a helpful assistant.' },
        { role: 'user', content: 'Hello!' },
      ],
      stream: true,
      temperature: 0.7,
      max_tokens: 512,
      user_id: 'tenant-42:user-9f3c1ab7',
    }, null, 2),
    lang: 'json',
  });
  return { title: 'Chat completions', blocks };
}

function streamSection(server, sample) {
  const keepalive = server.sseKeepaliveText || 'Zeiko is still here, Just be patience';
  const every = server.sseKeepaliveMs ?? 0;
  return {
    title: 'Streaming',
    blocks: [
      { p: 'With `"stream": true` the response is `text/event-stream`: one `data:` frame per chunk, then a final frame carrying usage, then `[DONE]`.' },
      {
        code: [
          `data: {"id":"3f2a…","object":"chat.completion.chunk","created":1789936200,"model":"${sample}","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}`,
          '',
          `data: {"id":"3f2a…","object":"chat.completion.chunk","created":1789936200,"model":"${sample}","choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}`,
          '',
          `: ${keepalive}`,
          '',
          `data: {"id":"3f2a…","object":"chat.completion.chunk","created":1789936200,"model":"${sample}","choices":[],"usage":{"prompt_tokens":18,"completion_tokens":256,"total_tokens":274,"usage":0.010293,"cost":0.010293}}`,
          '',
          'data: [DONE]',
          '',
        ].join('\n'),
        lang: 'text',
      },
      {
        list: [
          `Lines beginning with \`:\` are SSE comments \u2014 keep-alive only, ${every ? `sent after about ${every} ms of quiet` : 'currently switched off'}. Discard them; a compliant SSE client already does.`,
          'The final data frame has an empty `choices` array and carries `usage`. It arrives whether or not `stream_options.include_usage` was sent.',
          '`id` is a UUIDv4 for this request, stable across every frame of one response.',
          '`model` is always the public id the caller asked for.',
          'A failure mid-stream arrives as a frame with an `error` object before `[DONE]`, so a stream that has already started never ends silently.',
        ],
      },
    ],
  };
}

function usageSection(cfg) {
  const currency = cfg.pricing?.currency || 'USD';
  return {
    title: 'Usage and billing fields',
    blocks: [
      { p: 'These are the exact paths to bill from. Prompt tokens count the caller\u2019s own messages as they arrived \u2014 anything added on their behalf is **not** charged, so the figure matches what they sent.' },
      {
        columns: ['Field', 'Type', 'Meaning'],
        rows: [
          ['`usage.prompt_tokens`', 'integer', 'Input tokens, over the caller\u2019s own messages.'],
          ['`usage.completion_tokens`', 'integer', 'Output tokens, reasoning included.'],
          ['`usage.total_tokens`', 'integer', 'The two above, added.'],
          ['`usage.prompt_tokens_details.cached_tokens`', 'integer', 'Cache-hit portion of the input. Absent when zero, never larger than `prompt_tokens`.'],
          ['`usage.completion_tokens_details.reasoning_tokens`', 'integer', 'Reasoning portion of the output. Absent when zero.'],
          ['`usage.usage`', 'number', `What this request cost, in ${currency}, to nine decimal places. Absent only for a model that has no published price at all.`],
          ['`usage.cost`', 'number', 'The same number under OpenRouter\u2019s spelling. Never a second price.'],
        ],
      },
      { p: 'Non-streaming responses carry the same object on the body. Streaming responses carry it on the final chunk described above.' },
      { code: JSON.stringify({
        usage: {
          prompt_tokens: 18,
          completion_tokens: 256,
          total_tokens: 274,
          prompt_tokens_details: { cached_tokens: 12 },
          completion_tokens_details: { reasoning_tokens: 64 },
          usage: 0.102949871,
          cost: 0.102949871,
        },
      }, null, 2), lang: 'json' },
    ],
  };
}

/**
 * One field, and what it buys.
 *
 * This used to publish the whole resolution order — two body spellings, four
 * headers — and then describe, field by field, what each one was rewritten
 * into on the way out. All of that is still true and none of it is a
 * customer's to think about: they send one value, and the translation is ours.
 * Every extra spelling printed here was one more way for an integrator to send
 * a different id on different calls and lose every cache hit they had.
 */
function identitySection() {
  return {
    title: 'Identifying your end users',
    blocks: [
      { p: 'Send one field, `user_id`, on every request. It is a stable, opaque identifier for the person the request is for \u2014 and it is the prompt-cache partition key.' },
      { code: JSON.stringify({
        model: 'your-model-id',
        messages: ['\u2026'],
        user_id: 'tenant-42:user-9f3c1ab7',
      }, null, 2).replace('"\u2026"', '\u2026'), lang: 'json' },
      { p: 'That is the whole integration \u2014 there is nothing else to set and no header to add.' },
      {
        list: [
          '**Isolation.** Requests carrying different `user_id` values can never reuse each other\u2019s cached prompt prefix. Without the field, everyone on one API key shares a single partition.',
          '**Cost and speed.** A cached prefix is billed at the cache-read rate, a fraction of the fresh-input rate, and removes most of the time-to-first-token on a long system prompt.',
          '**Stable, unique, opaque.** The same person gets the same string every time, two people never share one, and it is a hash or an internal id \u2014 never an email address, a username or a real name. Up to 120 characters.',
          '`usage.prompt_tokens_details.cached_tokens` appears on the response once a prefix is being reused, and is absent while it is not. That is how you check it is working.',
        ],
      },
      { p: 'Omitting it is allowed and the request still succeeds \u2014 you simply get no isolation, and in practice far fewer cache hits.' },
      { note: 'Nothing about a prompt is retained beyond the truncated preview on the Requests tab.' },
    ],
  };
}

function limitSection(server, models, keys, backends, security) {
  const quota = (field) => keys.map((k) => k.quota?.[field] ?? 0).filter((n) => n > 0);
  const perModel = (pick) => models.map(pick).filter((n) => n > 0);
  const rows = [
    ['Request body',
      server.maxBodyBytes ? `${fmtBytes(server.maxBodyBytes)} (${exact(server.maxBodyBytes)} bytes)` : 'unlimited',
      'A larger body is refused with 413 before it is read.'],
    // How many requests run at once, and how deep the queue behind them is,
    // size this service for whoever reads it. A customer needs to know that a
    // busy moment answers 503 with a `Retry-After`, which the error table says.
    ['Requests per minute', spread(quota('requestsPerMinute'), 'per key') ?? 'unlimited',
      'Set per key. Over it: 429 with a `Retry-After` header.'],
    ['Requests per day', spread(quota('requestsPerDay'), 'per key') ?? 'unlimited',
      'Resets daily.'],
    ['Tokens per day', spread(quota('tokensPerDay'), 'per key') ?? 'unlimited',
      'Input and output together.'],
  ];
  const inputs = spread(perModel((m) => m.limits?.maxInputTokens ?? 0), 'tokens');
  if (inputs) rows.push(['Max input tokens', inputs, 'Per model; over it the request is refused, not truncated.']);
  const outputs = spread(perModel((m) => m.limits?.maxOutputTokens ?? 0), 'tokens');
  if (outputs) rows.push(['Max output tokens', outputs, 'Per model; a larger `max_tokens` is clamped down, not refused.']);
  const timeouts = spread(backends.map((b) => b.timeoutMs ?? 0).filter((n) => n > 0), 'ms');
  if (timeouts) rows.push(['Maximum request duration', timeouts, 'Set the client timeout above this; a slower answer comes back as 504.']);

  const origins = security.corsOrigins ?? [];
  return {
    title: 'Limits',
    blocks: [
      { p: 'There is no message-count ceiling of its own: a conversation is bounded by the model\u2019s context length and by the body size below.' },
      { columns: ['Limit', 'Value', 'Notes'], rows },
      { p: `CORS: ${origins.includes('*') ? 'any origin' : origins.map((o) => `\`${o}\``).join(', ') || 'no origin is allowed'}. Preflight is answered on every public path.` },
    ],
  };
}

function errorSection() {
  return {
    title: 'Errors',
    blocks: [
      { p: 'Failures come back in the OpenAI error shape, with the HTTP status matching.' },
      { code: JSON.stringify({ error: { message: 'model "gpt-9" not found', type: 'invalid_request_error', code: 'model_not_found' } }, null, 2), lang: 'json' },
      {
        columns: ['Status', 'Code', 'When'],
        rows: [
          ['400', '—', 'Malformed JSON, no `model` field, or parameters this model will not accept.'],
          ['401', '`invalid_api_key`', 'Missing, unknown or disabled API key.'],
          ['403', '`model_forbidden`', 'The key is not entitled to that model.'],
          ['404', '`model_not_found`', 'Unknown model id, or a path that does not exist.'],
          ['413', '`too_large`', 'The body is over the size limit, or the prompt is over the model context length.'],
          ['429', '`rate_limit_exceeded`', 'A per-minute rate limit or a daily quota is spent. The per-minute refusal carries a `Retry-After` header.'],
          ['502', '`model_unavailable`', 'The model could not answer. Retry with backoff; if it persists, get in touch with an `X-Request-Id`.'],
          ['503', '`overloaded`', 'Temporarily at capacity. Carries `Retry-After`.'],
          ['504', '`timeout`', 'The model took too long to answer.'],
        ],
      },
      { note: 'A `502` never means your key was refused: a credential problem on our side still reads as `model_unavailable`, because yours was fine and saying otherwise would send you debugging the wrong thing.' },
    ],
  };
}

function exampleSection(baseUrl, sample, keyPlaceholder) {
  return {
    title: 'Examples',
    blocks: [
      { p: 'Streaming, with curl:' },
      {
        code: `curl -N ${baseUrl}/chat/completions \\
  -H "Authorization: Bearer ${keyPlaceholder}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${sample}",
    "messages": [{"role": "user", "content": "Hello!"}],
    "user_id": "tenant-42:user-9f3c1ab7",
    "stream": true
  }'`,
        lang: 'bash',
      },
      { p: 'Non-streaming, with the OpenAI Python SDK:' },
      {
        code: `from openai import OpenAI

client = OpenAI(base_url="${baseUrl}", api_key="${keyPlaceholder}")

reply = client.chat.completions.create(
    model="${sample}",
    messages=[{"role": "user", "content": "Hello!"}],
    extra_body={"user_id": "tenant-42:user-9f3c1ab7"},   # cache isolation
)
print(reply.choices[0].message.content)
print(reply.usage.prompt_tokens, reply.usage.completion_tokens)`,
        lang: 'python',
      },
      { p: 'Listing the models:' },
      { code: `curl ${baseUrl}/models -H "Authorization: Bearer ${keyPlaceholder}"`, lang: 'bash' },
    ],
  };
}

/* ------------------------------------------------------------ helpers -- */

function limitText(n) {
  return n > 0 ? exact(n) : 'unlimited';
}

/**
 * Numbers in full, grouped but never abbreviated.
 *
 * `fmtNum` renders 200000 as "200.0k", which is right for a dashboard tile and
 * wrong for a document somebody is going to write a rate limiter from.
 */
function exact(n) {
  return Number(n ?? 0).toLocaleString('en-US');
}

/** A min-max pair over the configured models or keys, collapsed when they agree. */
function spread(values, unit = '') {
  if (!values.length) return null;
  const low = Math.min(...values);
  const high = Math.max(...values);
  const range = low === high ? exact(low) : `${exact(low)}\u2013${exact(high)}`;
  return unit ? `${range} ${unit}` : range;
}

function rate(value) {
  return value > 0 ? `$${trimZeros(value)}` : '—';
}

function trimZeros(n) {
  return String(Number(n.toFixed(6)));
}

/**
 * The sell-side rates as `pricing::resolve` and `pricing::price` work them
 * out: an explicit rate wins, otherwise the backend rate plus the margin.
 */
function resolvePricing(defaults, model) {
  const pick = (a, b) => (a > 0 ? a : b) ?? 0;
  const backendInput = pick(model.backendInputUsdPerM, defaults.backendInputUsdPerM);
  const backendOutput = pick(model.backendOutputUsdPerM, defaults.backendOutputUsdPerM);
  const margin = 1 + (pick(model.marginPercent, defaults.marginPercent) || 0) / 100;
  const sell = (explicit, cost) => (explicit > 0 ? explicit : (cost || 0) * margin);
  const backendCached = pick(model.backendCachedInputUsdPerM, defaults.backendCachedInputUsdPerM) || backendInput;
  const backendReasoning = pick(model.backendReasoningUsdPerM, defaults.backendReasoningUsdPerM) || backendOutput;
  const standard = {
    input: sell(pick(model.inputUsdPerM, defaults.inputUsdPerM), backendInput),
    cachedInput: sell(pick(model.cachedInputUsdPerM, defaults.cachedInputUsdPerM), backendCached),
    output: sell(pick(model.outputUsdPerM, defaults.outputUsdPerM), backendOutput),
    reasoning: sell(pick(model.reasoningUsdPerM, defaults.reasoningUsdPerM), backendReasoning),
  };
  // A band left unpriced is the standard band, not a free one.
  const band = (name) => {
    const own = model[name] ?? {};
    const house = defaults[name] ?? {};
    const at = (key) => pick(own[key], house[key]);
    const output = at('outputUsdPerM') || standard.output;
    return {
      input: at('inputUsdPerM') || standard.input,
      cachedInput: at('cachedInputUsdPerM') || standard.cachedInput,
      output,
      reasoning: at('reasoningUsdPerM') || output,
    };
  };
  return {
    ...standard,
    maxThinking: band('maxThinking'),
    nonThinking: band('nonThinking'),
    requestUsd: pick(model.requestUsd, defaults.requestUsd) || 0,
    refusalUsd: pick(model.refusalUsd, defaults.refusalUsd) || 0,
  };
}

function tierCondition(when) {
  const parts = [];
  if (when.models?.length) parts.push(`model ${when.models.join(', ')}`);
  if (when.efforts?.length) parts.push(`effort ${when.efforts.join(', ')}`);
  if (when.minEffort) parts.push(`effort ≥ ${when.minEffort}`);
  if (when.maxEffort) parts.push(`effort ≤ ${when.maxEffort}`);
  for (const hr of when.hours ?? []) parts.push(`${pad(hr.from)}:00–${pad(hr.to)}:59 local`);
  if (when.weekdays?.length) parts.push(`on ${when.weekdays.map(dayName).join(', ')}`);
  if (when.minInputTokens) parts.push(`input ≥ ${exact(when.minInputTokens)}`);
  if (when.maxInputTokens) parts.push(`input ≤ ${exact(when.maxInputTokens)}`);
  if (when.minOutputTokens) parts.push(`output ≥ ${exact(when.minOutputTokens)}`);
  if (when.maxOutputTokens) parts.push(`output ≤ ${exact(when.maxOutputTokens)}`);
  if (when.minTotalTokens) parts.push(`total ≥ ${exact(when.minTotalTokens)}`);
  if (when.maxTotalTokens) parts.push(`total ≤ ${exact(when.maxTotalTokens)}`);
  if (when.streamed === true) parts.push('streaming');
  if (when.streamed === false) parts.push('not streaming');
  if (when.cacheHit === true) parts.push('on a cache hit');
  if (when.cacheHit === false) parts.push('without a cache hit');
  return parts.length ? parts.join(', ') : 'every request';
}

function tierEffect(t) {
  const parts = [];
  if (t.inputUsdPerM != null) parts.push(`input $${trimZeros(t.inputUsdPerM)}/M`);
  else if (t.inputMultiplier > 0 && t.inputMultiplier !== 1) parts.push(`input ×${t.inputMultiplier}`);
  if (t.cachedInputUsdPerM != null) parts.push(`cached $${trimZeros(t.cachedInputUsdPerM)}/M`);
  if (t.outputUsdPerM != null) parts.push(`output $${trimZeros(t.outputUsdPerM)}/M`);
  else if (t.outputMultiplier > 0 && t.outputMultiplier !== 1) parts.push(`output ×${t.outputMultiplier}`);
  if (t.reasoningUsdPerM != null) parts.push(`reasoning $${trimZeros(t.reasoningUsdPerM)}/M`);
  else if (t.reasoningMultiplier > 0 && t.reasoningMultiplier !== 1) parts.push(`reasoning ×${t.reasoningMultiplier}`);
  if (t.surchargeUsd) parts.push(`+$${trimZeros(t.surchargeUsd)}`);
  if (t.stop) parts.push('then stop');
  return parts.length ? parts.join(', ') : 'no change';
}

function pad(n) {
  return String(n ?? 0).padStart(2, '0');
}

function dayName(n) {
  return ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'][n] ?? String(n);
}

/* ------------------------------------------------------------- render -- */

function renderSection(section) {
  return card(section.title, h('div.grid', {}, ...section.blocks.map(renderBlock)));
}

function renderBlock(block) {
  if (block.p) return h('p.small', { html: inline(block.p) });
  if (block.note) return h('p.small.muted', { html: `ℹ︎ ${inline(block.note)}` });
  if (block.code) return h('pre.log', { text: block.code });
  if (block.list) {
    return h('ul.small', { style: { margin: '0', paddingLeft: '18px' } },
      ...block.list.map((item) => h('li', { html: inline(item), style: { marginBottom: '4px' } })));
  }
  if (block.columns) {
    return h('div.table-wrap', {}, h('table', {},
      h('thead', {}, h('tr', {}, ...block.columns.map((c) => h('th', { text: c })))),
      h('tbody', {}, ...block.rows.map((row) => h('tr', {},
        ...row.map((cell) => h('td', { html: inline(String(cell)) })))))));
  }
  return null;
}

/**
 * The small Markdown subset the document is written in — `code`, **bold**,
 * _italic_ — as HTML. Everything is escaped first, so a model id with an angle
 * bracket in it cannot become markup.
 */
function inline(text) {
  return escapeHtml(text)
    .replace(/`([^`]+)`/g, '<code class="mono">$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/_([^_]+)_/g, '<em>$1</em>');
}

function escapeHtml(text) {
  return String(text)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

/* ----------------------------------------------------------- markdown -- */

export function toMarkdown(doc) {
  const out = [`# ${doc.title}`, ''];
  for (const section of doc.sections) {
    out.push(`## ${section.title}`, '');
    for (const block of section.blocks) {
      if (block.p) out.push(block.p, '');
      else if (block.note) out.push(`> ${block.note}`, '');
      else if (block.code) out.push('```' + (block.lang ?? ''), block.code, '```', '');
      else if (block.list) out.push(...block.list.map((i) => `- ${i}`), '');
      else if (block.columns) {
        out.push(`| ${block.columns.join(' | ')} |`);
        out.push(`| ${block.columns.map(() => '---').join(' | ')} |`);
        for (const row of block.rows) {
          out.push(`| ${row.map((c) => String(c).replace(/\|/g, '\\|')).join(' | ')} |`);
        }
        out.push('');
      }
    }
  }
  return out.join('\n');
}

function download(doc) {
  try {
    const blob = new Blob([toMarkdown(doc)], { type: 'text/markdown' });
    const url = URL.createObjectURL(blob);
    const a = h('a', { href: url, download: 'api-documentation.md' });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  } catch {
    toast('Could not build the file — copy it as Markdown instead', 'err');
  }
}
