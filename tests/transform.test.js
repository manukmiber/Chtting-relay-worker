import test from 'node:test';
import assert from 'node:assert/strict';
import {
  transformRequest,
  transformResponse,
  injectSystemPrompt,
  compileTextRules,
  flattenContent,
} from '../src/relay/transform.js';
import { SseParser, StreamRewriter } from '../src/relay/sse.js';
import { unmaskSecrets } from '../src/server/dashboard.js';
import { maskSecret } from '../src/util/misc.js';
import { RateLimiter } from '../src/util/ratelimit.js';

const DEFAULTS = {
  systemPrompt: { mode: 'none', text: '' },
  requestTransform: { dropParams: [], renameParams: {}, replace: [], injectStop: [] },
  responseTransform: { renameModel: true, reasoning: 'keep', stripFields: [], replace: [] },
};

const ROUTE = {
  id: 'manukmiberai/creative-writer',
  upstreamModel: 'Deepseek-v4-flash-0731',
  systemPrompt: { mode: 'none', text: '' },
  params: {},
  forceParams: {},
  limits: {},
  requestTransform: {},
  responseTransform: {},
};

test('the request carries the backend model name, never the alias', () => {
  const out = transformRequest(
    { model: 'manukmiberai/creative-writer', messages: [{ role: 'user', content: 'hi' }] },
    ROUTE,
    DEFAULTS,
  );
  assert.equal(out.model, 'Deepseek-v4-flash-0731');
});

test('route params are defaults and forceParams are not negotiable', () => {
  const route = { ...ROUTE, params: { temperature: 0.7, top_p: 0.9 }, forceParams: { temperature: 1.5 } };
  const out = transformRequest({ model: 'x', messages: [], top_p: 0.1 }, route, DEFAULTS);
  assert.equal(out.top_p, 0.1, 'a caller-supplied value wins over a default');
  assert.equal(out.temperature, 1.5, 'forceParams override whatever the caller sent');
});

test('params can be dropped and renamed for picky backends', () => {
  const route = {
    ...ROUTE,
    requestTransform: { dropParams: ['logit_bias'], renameParams: { max_completion_tokens: 'max_tokens' } },
  };
  const out = transformRequest(
    { model: 'x', messages: [], logit_bias: { 1: 2 }, max_completion_tokens: 64 },
    route,
    DEFAULTS,
  );
  assert.equal(out.logit_bias, undefined);
  assert.equal(out.max_completion_tokens, undefined);
  assert.equal(out.max_tokens, 64);
});

test('every system prompt injection mode places the text correctly', () => {
  const messages = [{ role: 'system', content: 'THEIRS' }, { role: 'user', content: 'hi' }];
  const spec = (mode) => ({ mode, text: 'OURS' });

  const prepend = injectSystemPrompt(messages, spec('prepend'), DEFAULTS.systemPrompt, []);
  assert.deepEqual(prepend.map((m) => m.content), ['OURS', 'THEIRS', 'hi']);

  const append = injectSystemPrompt(messages, spec('append'), DEFAULTS.systemPrompt, []);
  assert.deepEqual(append.map((m) => m.content), ['THEIRS', 'OURS', 'hi']);

  const replace = injectSystemPrompt(messages, spec('replace'), DEFAULTS.systemPrompt, []);
  assert.deepEqual(replace.map((m) => m.content), ['OURS', 'hi']);

  const merge = injectSystemPrompt(messages, spec('merge'), DEFAULTS.systemPrompt, []);
  assert.deepEqual(merge.map((m) => m.content), ['OURS\n\nTHEIRS', 'hi']);

  const none = injectSystemPrompt(messages, spec('none'), DEFAULTS.systemPrompt, []);
  assert.deepEqual(none.map((m) => m.content), ['THEIRS', 'hi']);
});

test('a system prompt can come from the shared library by id', () => {
  const library = [{ id: 'sp1', name: 'Writer', text: 'FROM LIBRARY' }];
  const out = injectSystemPrompt(
    [{ role: 'user', content: 'hi' }],
    { mode: 'prepend', promptId: 'sp1' },
    DEFAULTS.systemPrompt,
    library,
  );
  assert.equal(out[0].content, 'FROM LIBRARY');
});

test('the response is renamed and reshaped', () => {
  const upstream = {
    model: 'Deepseek-v4-flash-0731',
    system_fingerprint: 'fp_x',
    choices: [{ index: 0, message: { role: 'assistant', content: 'made by DeepSeek' }, finish_reason: 'stop' }],
  };
  const out = transformResponse(upstream, {
    publicModel: 'manukmiberai/creative-writer',
    transform: {
      renameModel: true,
      stripFields: ['system_fingerprint'],
      replace: [{ pattern: 'DeepSeek', flags: 'g', replacement: 'Creative Writer' }],
      setFields: { owned_by: 'manukmiber' },
    },
  });
  assert.equal(out.model, 'manukmiberai/creative-writer');
  assert.equal(out.system_fingerprint, undefined);
  assert.equal(out.choices[0].message.content, 'made by Creative Writer');
  assert.equal(out.owned_by, 'manukmiber');
});

test('a broken rewrite rule is ignored instead of taking the relay down', () => {
  const rewrite = compileTextRules([
    { pattern: '([unclosed', flags: 'g', replacement: 'x' },
    { pattern: 'ok', flags: 'g', replacement: 'fine' },
  ]);
  assert.equal(rewrite('ok then'), 'fine then');
});

test('literal rules do not treat the pattern as a regex', () => {
  const rewrite = compileTextRules([{ pattern: 'a.b', literal: true, replacement: 'X' }]);
  assert.equal(rewrite('a.b acb'), 'X acb');
});

test('content parts flatten to plain text', () => {
  assert.equal(flattenContent('hi'), 'hi');
  assert.equal(flattenContent([{ type: 'text', text: 'a' }, { type: 'text', text: 'b' }]), 'ab');
  assert.equal(flattenContent(null), '');
});

test('the SSE parser handles split frames and CRLF', () => {
  const parser = new SseParser();
  assert.deepEqual(parser.push('data: {"a":1}\n\ndata: {"b'), [{ event: 'message', data: '{"a":1}' }]);
  assert.deepEqual(parser.push('":2}\r\n\r\n'), [{ event: 'message', data: '{"b":2}' }]);
  assert.deepEqual(parser.push('data: [DONE]'), []);
  assert.deepEqual(parser.flush(), [{ event: 'message', data: '[DONE]' }]);
});

test('the stream rewriter never lets a pattern slip through a chunk boundary', () => {
  const rewrite = compileTextRules([{ pattern: 'DeepSeek', flags: 'g', replacement: 'Writer' }]);
  const rewriter = new StreamRewriter(rewrite, 16);
  let out = '';
  // every chunk boundary lands inside an occurrence of the pattern
  for (const chunk of ['ask Dee', 'pSeek ab', 'out Deep', 'Seek now']) out += rewriter.push(chunk);
  out += rewriter.flush();
  assert.equal(out, 'ask Writer about Writer now');
});

test('the stream rewriter emits progressively rather than buffering everything', () => {
  const rewrite = compileTextRules([{ pattern: 'x', flags: 'g', replacement: 'y' }]);
  const rewriter = new StreamRewriter(rewrite, 8);
  let emitted = '';
  for (let i = 0; i < 10; i++) emitted += rewriter.push('0123456789');
  assert.ok(emitted.length > 0, 'text is released as it becomes safe, not held to the end');
  const all = emitted + rewriter.flush();
  assert.equal(all, '0123456789'.repeat(10));
});

test('a single character split across chunks survives the rewriter', () => {
  const rewrite = compileTextRules([{ pattern: 'nope', flags: 'g', replacement: '' }]);
  const rewriter = new StreamRewriter(rewrite, 8);
  let out = '';
  for (const chunk of ['你好', '世界', '，这是测试', '句子。']) out += rewriter.push(chunk);
  out += rewriter.flush();
  assert.equal(out, '你好世界，这是测试句子。');
});

test('a rewriter with no rules passes text straight through', () => {
  const rewriter = new StreamRewriter(null, 16);
  assert.equal(rewriter.push('abc'), 'abc');
  assert.equal(rewriter.flush(), '');
});

test('saving a masked secret keeps the stored value', () => {
  const current = { backends: [{ id: 'b1', apiKey: 'sk-real-secret-value-1234', baseUrl: 'https://a' }] };
  const patch = {
    backends: [{ id: 'b1', apiKey: maskSecret('sk-real-secret-value-1234'), baseUrl: 'https://b' }],
  };
  const merged = unmaskSecrets(patch, current);
  assert.equal(merged.backends[0].apiKey, 'sk-real-secret-value-1234');
  assert.equal(merged.backends[0].baseUrl, 'https://b');
});

test('a genuinely new secret replaces the old one', () => {
  const current = { backends: [{ id: 'b1', apiKey: 'sk-old' }] };
  const patch = { backends: [{ id: 'b1', apiKey: 'sk-brand-new' }] };
  assert.equal(unmaskSecrets(patch, current).backends[0].apiKey, 'sk-brand-new');
});

test('the rate limiter allows up to the limit and then refuses', () => {
  const limiter = new RateLimiter();
  const now = Date.now();
  assert.equal(limiter.check('k', 2, now).allowed, true);
  assert.equal(limiter.check('k', 2, now).allowed, true);
  const third = limiter.check('k', 2, now);
  assert.equal(third.allowed, false);
  assert.ok(third.retryAfter > 0);
  assert.equal(limiter.check('k', 2, now + 61000).allowed, true, 'the window slides');
  assert.equal(limiter.check('k', 0, now).allowed, true, 'zero means unlimited');
});
