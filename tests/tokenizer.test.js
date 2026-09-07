import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadTiktoken } from '../src/tokenizer/tiktoken.js';
import { loadHfTokenizer } from '../src/tokenizer/hf.js';
import { approxCount } from '../src/tokenizer/approx.js';
import { splitWithBehavior } from '../src/tokenizer/pretokenizers.js';
import { countChatRequest, renderTools, imageTokens } from '../src/tokenizer/chat.js';
import { TokenizerRegistry, globMatch } from '../src/tokenizer/registry.js';
import { reconcileUsage, normalizeUsage } from '../src/tokenizer/index.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const VOCAB_DIR = path.join(ROOT, 'data', 'tokenizers');

const fixture = JSON.parse(
  await readFile(path.join(ROOT, 'tests', 'fixtures', 'expected-counts.json'), 'utf8'),
);

/**
 * The fixture holds counts produced by the reference implementations
 * (OpenAI `tiktoken` and HuggingFace `tokenizers`). A vocabulary that is not
 * installed skips its test rather than failing, so a fresh clone still runs
 * green before `npm run tokenizer:fetch`.
 */
for (const name of ['cl100k_base', 'o200k_base']) {
  test(`${name} matches tiktoken exactly`, { skip: skipUnless(`${name}.tiktoken`) }, async () => {
    const tok = await loadTiktoken(name, path.join(VOCAB_DIR, `${name}.tiktoken`));
    fixture.cases.forEach((text, i) => {
      assert.equal(tok.count(text), fixture.counts[name][i], `count for ${JSON.stringify(text)}`);
      assert.deepEqual(tok.encode(text), fixture.ids[name][i], `ids for ${JSON.stringify(text)}`);
    });
  });
}

for (const name of ['deepseek', 'qwen', 'llama2']) {
  test(`${name} matches HuggingFace tokenizers exactly`, { skip: skipUnless(`${name}.tokenizer.json`) }, async () => {
    const tok = await loadHfTokenizer(name, path.join(VOCAB_DIR, `${name}.tokenizer.json`));
    fixture.cases.forEach((text, i) => {
      assert.equal(tok.count(text), fixture.counts[name][i], `count for ${JSON.stringify(text)}`);
    });
  });
}

test('pieces round-trip back to the original text', { skip: skipUnless('cl100k_base.tiktoken') }, async () => {
  const tok = await loadTiktoken('cl100k_base', path.join(VOCAB_DIR, 'cl100k_base.tiktoken'));
  for (const text of fixture.cases) {
    assert.equal(tok.pieces(text).map((p) => p.text).join(''), text);
  }
});

test('a long unbroken CJK run stays fast', { skip: skipUnless('cl100k_base.tiktoken') }, async () => {
  const tok = await loadTiktoken('cl100k_base', path.join(VOCAB_DIR, 'cl100k_base.tiktoken'));
  const started = Date.now();
  const n = tok.count('测试'.repeat(4000));
  const ms = Date.now() - started;
  assert.ok(n > 0);
  assert.ok(ms < 3000, `8000 CJK characters took ${ms}ms; the merge loop should stay sub-quadratic`);
});

test('the estimator is used when a vocabulary is missing, and says so', async () => {
  const registry = new TokenizerRegistry({
    dir: path.join(ROOT, 'no', 'such', 'dir'),
    rules: [{ match: '*', tokenizer: 'nope' }],
    logger: { warn() {} },
  });
  const tok = await registry.get('nope');
  assert.equal(tok.exact, false);
  assert.ok(tok.count('hello world') > 0);
});

test('the estimator stays in a sane band for latin and CJK text', () => {
  const latin = approxCount('The quick brown fox jumps over the lazy dog.');
  assert.ok(latin >= 8 && latin <= 14, `expected roughly 9-11 tokens, got ${latin}`);
  const cjk = approxCount('你好世界');
  assert.ok(cjk >= 4 && cjk <= 8, `expected roughly one token per han character, got ${cjk}`);
  assert.equal(approxCount(''), 0);
});

test('split behaviours follow the HuggingFace semantics', () => {
  const re = () => /\p{N}{1,3}/gu;
  assert.deepEqual(splitWithBehavior('ab123cd', re(), 'Isolated'), ['ab', '123', 'cd']);
  assert.deepEqual(splitWithBehavior('ab123cd', re(), 'Removed'), ['ab', 'cd']);
  assert.deepEqual(splitWithBehavior('ab123cd', re(), 'MergedWithPrevious'), ['ab123', 'cd']);
  assert.deepEqual(splitWithBehavior('ab123cd', re(), 'MergedWithNext'), ['ab', '123cd']);
});

test('chat counting adds the template overhead and splits by role', async () => {
  const tok = existsSync(path.join(VOCAB_DIR, 'cl100k_base.tiktoken'))
    ? await loadTiktoken('cl100k_base', path.join(VOCAB_DIR, 'cl100k_base.tiktoken'))
    : { count: approxCount, exact: false, name: 'approx' };

  const body = {
    messages: [
      { role: 'system', content: 'You are helpful.' },
      { role: 'user', content: 'Hello there' },
    ],
  };
  const counted = countChatRequest(body, tok, { profile: 'openai' });
  const bare = tok.count('You are helpful.') + tok.count('Hello there');

  assert.ok(counted.total > bare, 'the chat template costs more than the raw text');
  assert.ok(counted.breakdown.system > 0 && counted.breakdown.user > 0);
  assert.ok(counted.breakdown.overhead >= 6, 'two messages plus the reply primer');
});

test('tool definitions are rendered and counted', async () => {
  const rendered = renderTools([{
    type: 'function',
    function: {
      name: 'get_weather',
      description: 'Get the weather',
      parameters: {
        type: 'object',
        properties: { city: { type: 'string', description: 'City name' } },
        required: ['city'],
      },
    },
  }]);
  assert.match(rendered, /namespace functions/);
  assert.match(rendered, /type get_weather = \(_: \{/);
  assert.match(rendered, /city: string,/);
});

test('image tokens follow the tiling rule', () => {
  assert.equal(imageTokens({ detail: 'low' }), 85);
  assert.equal(imageTokens({ detail: 'high', width: 512, height: 512 }), 85 + 170);
  assert.ok(imageTokens({ detail: 'high', width: 2048, height: 2048 }) > 85 + 170);
});

test('model names match tokenizer rules by glob', () => {
  assert.ok(globMatch('deepseek-v3-chat', 'deepseek*'));
  assert.ok(globMatch('GPT-4o-mini', 'gpt-4o*'));
  assert.ok(!globMatch('claude-3', 'gpt-4*'));
  assert.ok(globMatch('anything', '*'));
});

test('usage reconciliation prefers the backend and reports the drift', () => {
  const withUpstream = reconcileUsage({
    local: { prompt: 95, completion: 40, exact: true },
    upstream: { prompt_tokens: 100, completion_tokens: 42, total_tokens: 142 },
  });
  assert.equal(withUpstream.source, 'upstream');
  assert.equal(withUpstream.prompt_tokens, 100);
  assert.deepEqual(withUpstream.drift, { prompt: -5, completion: -2 });

  const localOnly = reconcileUsage({ local: { prompt: 95, completion: 40, exact: true }, upstream: null });
  assert.equal(localOnly.source, 'local');
  assert.equal(localOnly.total_tokens, 135);
  assert.equal(localOnly.drift, null);
});

test('usage from other provider dialects is normalised', () => {
  const anthropic = normalizeUsage({ input_tokens: 10, output_tokens: 5, cache_read_input_tokens: 3 });
  assert.equal(anthropic.prompt_tokens, 10);
  assert.equal(anthropic.completion_tokens, 5);
  assert.equal(anthropic.cached_tokens, 3);
  assert.equal(normalizeUsage(null), null);
});

function skipUnless(file) {
  return existsSync(path.join(VOCAB_DIR, file))
    ? false
    : `${file} is not installed (run: npm run tokenizer:fetch)`;
}
