import test from 'node:test';
import assert from 'node:assert/strict';
import { startTestApp } from './helpers/app.js';
import { startMockBackend } from './helpers/mock-backend.js';

const PUBLIC_MODEL = 'manukmiberai/creative-writer';
const UPSTREAM_MODEL = 'Deepseek-v4-flash-0731';
const CLIENT_KEY = 'sk-relay-test-key';

/** A relay wired to a mock backend, with the model alias from the README. */
async function setup(backendOpts = {}, modelPatch = {}) {
  const backend = await startMockBackend(backendOpts);
  const env = await startTestApp();
  try {
    await env.app.config.update({
      backends: [{
        id: 'ds', name: 'Mock DeepSeek', baseUrl: backend.baseUrl, apiKey: 'sk-upstream', enabled: true,
      }],
      models: [{
        id: PUBLIC_MODEL,
        backend: 'ds',
        upstreamModel: UPSTREAM_MODEL,
        displayName: 'Creative Writer',
        tokenizer: 'cl100k_base',
        systemPrompt: { mode: 'prepend', text: 'You are Creative Writer.' },
        params: { temperature: 1.1 },
        ...modelPatch,
      }],
      keys: [{ id: 'k1', label: 'tester', key: CLIENT_KEY, enabled: true, models: ['*'] }],
    });
  } catch (err) {
    // never leave listening servers behind, or the test runner hangs
    await env.close();
    await backend.close();
    throw err;
  }

  const call = (body, headers = {}) => fetch(`${env.relayUrl}/v1/chat/completions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${CLIENT_KEY}`, ...headers },
    body: JSON.stringify(body),
  });

  return {
    backend,
    env,
    call,
    async close() {
      await env.close();
      await backend.close();
    },
  };
}

test('translates the public model name into the backend name', async (t) => {
  const s = await setup();
  t.after(() => s.close());

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'hi' }] });
  assert.equal(res.status, 200);
  const body = await res.json();

  // what the backend saw
  const sent = s.backend.received.at(-1).body;
  assert.equal(sent.model, UPSTREAM_MODEL, 'backend receives the real model name');
  assert.equal(sent.temperature, 1.1, 'route params are applied');

  // what the caller sees: the alias, never the backend name
  assert.equal(body.model, PUBLIC_MODEL);
  assert.ok(!JSON.stringify(body).includes(UPSTREAM_MODEL), 'backend name never leaks to the caller');
});

test('injects the configured system prompt ahead of the caller prompt', async (t) => {
  const s = await setup();
  t.after(() => s.close());

  await s.call({
    model: PUBLIC_MODEL,
    messages: [{ role: 'system', content: 'Be brief.' }, { role: 'user', content: 'hi' }],
  });

  const sent = s.backend.received.at(-1).body;
  assert.deepEqual(sent.messages.map((m) => m.role), ['system', 'system', 'user']);
  assert.equal(sent.messages[0].content, 'You are Creative Writer.');
  assert.equal(sent.messages[1].content, 'Be brief.');
});

test('rejects unknown models and unknown keys', async (t) => {
  const s = await setup();
  t.after(() => s.close());

  const unknownModel = await s.call({ model: 'nope', messages: [] });
  assert.equal(unknownModel.status, 404);

  const badKey = await fetch(`${s.env.relayUrl}/v1/chat/completions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: 'Bearer wrong' },
    body: JSON.stringify({ model: PUBLIC_MODEL, messages: [] }),
  });
  assert.equal(badKey.status, 401);
});

test('/v1/models lists aliases only', async (t) => {
  const s = await setup();
  t.after(() => s.close());

  const res = await fetch(`${s.env.relayUrl}/v1/models`, { headers: { authorization: `Bearer ${CLIENT_KEY}` } });
  const body = await res.json();
  assert.equal(body.data.length, 1);
  assert.equal(body.data[0].id, PUBLIC_MODEL);
  assert.ok(!JSON.stringify(body).includes(UPSTREAM_MODEL));
});

test('streams SSE and records TTFT, tokens/sec and token counts', async (t) => {
  const s = await setup({ text: 'The quick brown fox jumps over the lazy dog.', firstChunkDelayMs: 60, chunkDelayMs: 8 });
  t.after(() => s.close());

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'write' }], stream: true });
  assert.equal(res.status, 200);
  assert.match(res.headers.get('content-type'), /text\/event-stream/);

  const raw = await res.text();
  assert.ok(raw.includes('data: [DONE]'));

  const text = collectStreamText(raw);
  assert.equal(text, 'The quick brown fox jumps over the lazy dog.');
  assert.ok(!raw.includes(UPSTREAM_MODEL), 'streamed chunks carry the alias');

  const row = s.env.app.store.list({ limit: 1 }).rows[0];
  assert.equal(row.public_model, PUBLIC_MODEL);
  assert.equal(row.upstream_model, UPSTREAM_MODEL);
  assert.equal(row.stream, 1);
  assert.ok(row.ttft_ms >= 50, `ttft should reflect the backend delay, got ${row.ttft_ms}`);
  assert.ok(row.gen_ms > 0, 'generation window is measured');
  assert.ok(row.tokens_per_sec > 0, 'tokens/sec is derived from the generation window');
  assert.ok(row.prompt_tokens > 0 && row.completion_tokens > 0);
});

test('prefers the usage the backend reports and records the local drift', async (t) => {
  const s = await setup({ usage: { prompt_tokens: 100, completion_tokens: 50, total_tokens: 150 } });
  t.after(() => s.close());

  await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'hi' }] });
  const row = s.env.app.store.list({ limit: 1 }).rows[0];

  assert.equal(row.usage_source, 'upstream');
  assert.equal(row.prompt_tokens, 100);
  assert.equal(row.completion_tokens, 50);
  assert.ok(row.local_prompt > 0, 'the local count is kept for comparison');
  assert.equal(row.drift_prompt, row.local_prompt - 100);
});

test('falls back to local counting when the backend reports no usage', async (t) => {
  const s = await setup({ usage: null });
  t.after(() => s.close());

  await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'hello world' }] });
  const row = s.env.app.store.list({ limit: 1 }).rows[0];

  assert.equal(row.usage_source, 'local');
  assert.ok(row.prompt_tokens > 0);
  assert.ok(row.completion_tokens > 0);
});

test('reshapes the response: renames, strips fields and rewrites text', async (t) => {
  const s = await setup(
    { text: 'I am DeepSeek, made by DeepSeek.' },
    {
      responseTransform: {
        stripFields: ['system_fingerprint'],
        replace: [{ pattern: 'DeepSeek', flags: 'gi', replacement: 'Creative Writer' }],
        suffix: ' ✎',
      },
    },
  );
  t.after(() => s.close());

  const body = await (await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'who are you' }] })).json();
  assert.equal(body.choices[0].message.content, 'I am Creative Writer, made by Creative Writer. ✎');
  assert.equal(body.system_fingerprint, undefined);
  assert.equal(body.model, PUBLIC_MODEL);
});

test('rewrites streamed text even when a match straddles two chunks', async (t) => {
  // the mock emits 6-character chunks, so "DeepSeek" always spans a boundary
  const s = await setup(
    { text: 'ask DeepSeek about DeepSeek please' },
    { responseTransform: { replace: [{ pattern: 'DeepSeek', flags: 'g', replacement: 'Writer' }] } },
  );
  t.after(() => s.close());

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }], stream: true });
  const text = collectStreamText(await res.text());
  assert.equal(text, 'ask Writer about Writer please');
});

test('reasoning traces can be inlined into the content stream', async (t) => {
  const s = await setup(
    { text: 'Answer.', reasoning: 'Thinking hard.' },
    { responseTransform: { reasoning: 'inline', reasoningTags: ['<think>', '</think>'] } },
  );
  t.after(() => s.close());

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }], stream: true });
  const text = collectStreamText(await res.text());
  assert.equal(text, '<think>Thinking hard.</think>Answer.');
});

test('reasoning traces can be stripped entirely', async (t) => {
  const s = await setup(
    { text: 'Answer.', reasoning: 'Secret chain of thought.' },
    { responseTransform: { reasoning: 'strip' } },
  );
  t.after(() => s.close());

  const body = await (await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }] })).json();
  assert.equal(body.choices[0].message.content, 'Answer.');
  assert.equal(body.choices[0].message.reasoning_content, undefined);
  assert.ok(!JSON.stringify(body).includes('Secret chain of thought'));
});

test('a non-streaming backend can still be served as a stream', async (t) => {
  const s = await setup({ text: 'Buffered answer.' }, { requestTransform: { forceStream: false } });
  t.after(() => s.close());

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }], stream: true });
  assert.match(res.headers.get('content-type'), /text\/event-stream/);
  const raw = await res.text();
  assert.equal(collectStreamText(raw), 'Buffered answer.');
  assert.equal(s.backend.received.at(-1).body.stream, false, 'the backend was asked for a whole response');
});

test('a streaming backend can be buffered into one response, still measuring TTFT', async (t) => {
  const s = await setup({ text: 'Streamed then buffered.', firstChunkDelayMs: 50 }, { requestTransform: { forceStream: true } });
  t.after(() => s.close());

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }] });
  const body = await res.json();
  assert.equal(body.object, 'chat.completion');
  assert.equal(body.choices[0].message.content, 'Streamed then buffered.');
  assert.equal(s.backend.received.at(-1).body.stream, true);

  const row = s.env.app.store.list({ limit: 1 }).rows[0];
  assert.ok(row.ttft_ms >= 40, `TTFT is measured even for buffered replies, got ${row.ttft_ms}`);
});

test('enforces the per-model input token ceiling', async (t) => {
  const s = await setup({}, { limits: { maxInputTokens: 5, maxOutputTokens: 0 } });
  t.after(() => s.close());

  const res = await s.call({
    model: PUBLIC_MODEL,
    messages: [{ role: 'user', content: 'this prompt is definitely longer than five tokens in total' }],
  });
  assert.equal(res.status, 413);
  const body = await res.json();
  assert.match(body.error.message, /token limit/);
});

test('caps max_tokens at the route ceiling', async (t) => {
  const s = await setup({}, { limits: { maxInputTokens: 0, maxOutputTokens: 32 } });
  t.after(() => s.close());

  await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'hi' }], max_tokens: 4096 });
  assert.equal(s.backend.received.at(-1).body.max_tokens, 32);
});

test('surfaces backend failures as upstream errors and logs them', async (t) => {
  const s = await setup({ status: 503, errorBody: { error: { message: 'backend on fire' } } });
  t.after(() => s.close());

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }] });
  assert.equal(res.status, 503);

  const row = s.env.app.store.list({ limit: 1 }).rows[0];
  assert.equal(row.status, 503);
  assert.match(row.error, /backend on fire/);
});

test('per-key daily request quota is enforced', async (t) => {
  const s = await setup();
  t.after(() => s.close());
  await s.env.app.config.update({
    keys: [{ id: 'k1', label: 'tester', key: CLIENT_KEY, enabled: true, models: ['*'], quota: { requestsPerDay: 1 } }],
  });

  const first = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }] });
  assert.equal(first.status, 200);
  const second = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }] });
  assert.equal(second.status, 429);
});

test('a key restricted to one model cannot use another', async (t) => {
  const s = await setup();
  t.after(() => s.close());
  await s.env.app.config.update({
    keys: [{ id: 'k1', label: 'tester', key: CLIENT_KEY, enabled: true, models: ['something-else'] }],
  });

  const res = await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'x' }] });
  assert.equal(res.status, 403);
});

test('daily stats aggregate requests, users and tokens', async (t) => {
  const s = await setup();
  t.after(() => s.close());

  await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'one' }] });
  await s.call({ model: PUBLIC_MODEL, messages: [{ role: 'user', content: 'two' }] });

  const summary = s.env.app.store.summary({});
  assert.equal(summary.requests, 2);
  assert.equal(summary.users, 1);
  assert.ok(summary.total_tokens > 0);

  const daily = s.env.app.store.daily({ days: 7 });
  assert.equal(daily.at(-1).requests, 2);

  const byModel = s.env.app.store.groupBy('public_model', {});
  assert.equal(byModel[0].name, PUBLIC_MODEL);
  assert.equal(byModel[0].requests, 2);
});

function collectStreamText(raw) {
  let out = '';
  for (const line of raw.split('\n')) {
    if (!line.startsWith('data: ')) continue;
    const data = line.slice(6);
    if (data === '[DONE]') continue;
    try {
      const chunk = JSON.parse(data);
      out += chunk.choices?.[0]?.delta?.content ?? '';
    } catch { /* ignore non-JSON frames */ }
  }
  return out;
}
