import { api } from '../api.js';
import {
  h, card, table, pill, field, text, textarea, select, toast, clear, fmtBytes, fmtNum, mount,
} from '../ui.js';

const HF_PRESETS = [
  ['deepseek', 'deepseek-ai/DeepSeek-V3'],
  ['deepseek-r1', 'deepseek-ai/DeepSeek-R1'],
  ['qwen', 'Qwen/Qwen2.5-7B-Instruct'],
  ['qwen3', 'Qwen/Qwen3-8B'],
  ['llama3', 'meta-llama/Meta-Llama-3-8B-Instruct'],
  ['mistral', 'mistralai/Mistral-7B-Instruct-v0.3'],
  ['gemma', 'google/gemma-2-9b-it'],
  ['glm', 'THUDM/glm-4-9b-chat'],
];

/**
 * Vocabulary management plus a live playground, so you can see exactly how a
 * prompt is split before it costs anything.
 */
export async function tokenizerView(ctx) {
  const inv = ctx.state.tokenizers;
  const root = h('div');

  /* -------------------------------------------------------- playground */
  const input = textarea(ctx.store.tokText ?? 'Halo, ini relay LLM untuk Termux. Berapa token?', { rows: 5 });
  const modelSelect = select(ctx.store.tokModel ?? '', [
    ['', '— pick a model alias —'],
    ...(ctx.state.config.models ?? []).map((m) => [m.id, `${m.id} → ${m.upstreamModel}`]),
  ]);
  const vocabSelect = select(ctx.store.tokVocab ?? '', [
    ['', 'follow the model'],
    ...inv.installed.map((t) => [t.name, `${t.name} (${t.kind})`]),
  ]);
  const output = h('div');

  async function run() {
    clear(output).append(h('div.empty', { text: 'Counting…' }));
    ctx.store.tokText = input.value;
    ctx.store.tokModel = modelSelect.value;
    ctx.store.tokVocab = vocabSelect.value;
    try {
      const result = await api.countTokens({
        text: input.value,
        model: modelSelect.value || undefined,
        tokenizer: vocabSelect.value || undefined,
        limit: 3000,
      });
      mount(clear(output),
        h('div.row', { style: { marginBottom: '10px' } },
          pill(`${fmtNum(result.count)} tokens`, 'accent'),
          pill(result.tokenizer),
          result.kind ? pill(result.kind) : null,
          result.exact ? pill('exact', 'ok') : pill('estimated', 'warn'),
          pill(`${input.value.length} chars`),
        ),
        h('div.tokens', {}, ...result.pieces.map((p) => h(
          `span.tok${p.special ? '.special' : ''}`,
          { title: p.id >= 0 ? `id ${p.id}` : 'no id' },
          p.text === '' ? '·' : p.text.replace(/\n/g, '⏎\n'),
        ))),
        result.truncated ? h('p.small.muted', { text: 'Preview truncated; the count above covers the whole text.' }) : null,
      );
    } catch (err) {
      clear(output).append(h('div.empty', { text: err.message }));
    }
  }

  root.append(card('Playground', h('div', {},
    h('div.grid.form', {}, field('Model alias', modelSelect), field('Or a specific vocabulary', vocabSelect)),
    field('Text', input),
    h('div.row', {}, h('button.primary', { onclick: run }, 'Count tokens')),
    h('div', { style: { marginTop: '14px' } }, output),
  )));
  await run();

  /* ---------------------------------------------------- chat counting */
  const chatInput = textarea(ctx.store.chatJson ?? JSON.stringify({
    messages: [
      { role: 'system', content: 'You are a helpful assistant.' },
      { role: 'user', content: 'Explain tokenizers in one sentence.' },
    ],
  }, null, 2), { rows: 8 });
  const chatModel = select(ctx.store.tokModel ?? '', [
    ['', '— pick a model alias —'],
    ...(ctx.state.config.models ?? []).map((m) => [m.id, m.id]),
  ]);
  const chatOut = h('div');

  root.append(card('Chat request cost', h('div', {},
    h('p.small.muted', { text: 'Counts a whole request the way the backend will see it, including the chat template overhead, tools and images.' }),
    field('Model alias', chatModel),
    field('Request JSON', chatInput),
    h('div.row', {}, h('button.primary', {
      onclick: async () => {
        ctx.store.chatJson = chatInput.value;
        clear(chatOut).append(h('div.empty', { text: 'Counting…' }));
        try {
          const body = JSON.parse(chatInput.value);
          const result = await api.countTokens({ ...body, model: chatModel.value || undefined });
          mount(clear(chatOut),
            h('div.row', { style: { marginBottom: '10px' } },
              pill(`${fmtNum(result.total)} tokens`, 'accent'),
              pill(result.tokenizer),
              result.profile ? pill(`${result.profile} template`) : null,
              result.exact ? pill('exact', 'ok') : pill('estimated', 'warn'),
            ),
            table(
              [{ label: 'Part' }, { label: 'Tokens', num: true }],
              Object.entries(result.breakdown).filter(([, v]) => v > 0),
              ([k, v]) => h('tr', {}, h('td', { text: k }), h('td.num', { text: fmtNum(v) })),
            ),
          );
        } catch (err) {
          clear(chatOut).append(h('div.empty', { text: err.message }));
        }
      },
    }, 'Count request')),
    h('div', { style: { marginTop: '14px' } }, chatOut),
  )));

  /* --------------------------------------------------------- installed */
  root.append(card('Installed vocabularies', table(
    [{ label: 'Name' }, { label: 'Kind' }, { label: 'Size', num: true }, { label: 'File' }],
    inv.installed,
    (t) => h('tr', {},
      h('td.mono', { text: t.name }),
      h('td', {}, pill(t.kind)),
      h('td.num', { text: fmtBytes(t.size) }),
      h('td.mono.small.muted', { text: t.file }),
    ),
  ), [h('span.small.muted', { text: inv.dir })]));

  if (!inv.installed.length) {
    root.append(card('No vocabulary installed', h('p.muted', {
      text: 'Token counts are estimates until you install a vocabulary below. '
        + 'Install cl100k_base and o200k_base for GPT-family models, and the matching '
        + 'HuggingFace tokenizer for whatever open model your backend runs.',
    })));
  }

  /* ----------------------------------------------------------- install */
  const installOut = h('div');
  const tiktokenSelect = select('', [
    ['', '— choose —'],
    ...inv.available.map((a) => [a.name, `${a.name}${a.installed ? ' (installed)' : ''}`]),
  ]);
  const hfRepo = text('', { placeholder: 'deepseek-ai/DeepSeek-V3', class: 'mono' });
  const hfAs = text('', { placeholder: 'deepseek', class: 'mono' });

  const install = async (payload) => {
    clear(installOut).append(h('div.empty', { text: 'Downloading… this needs network access once, then works offline.' }));
    try {
      const result = await api.installTokenizer(payload);
      clear(installOut).append(h('pre.log', { text: result.output.trim() || 'done' }));
      toast(result.ok ? 'Vocabulary installed' : 'Install failed', result.ok ? 'ok' : 'err');
      if (result.ok) await ctx.reload();
    } catch (err) {
      clear(installOut).append(h('pre.log', { text: err.message }));
      toast(err.message, 'err');
    }
  };

  root.append(card('Install a vocabulary', h('div', {},
    h('div.grid.two', {},
      h('div', {},
        h('h3', { style: { fontSize: '13px', margin: '0 0 8px' }, text: 'OpenAI rank files' }),
        field('Encoding', tiktokenSelect),
        h('button.primary.sm', {
          onclick: () => tiktokenSelect.value && install({ name: tiktokenSelect.value }),
        }, 'Download'),
      ),
      h('div', {},
        h('h3', { style: { fontSize: '13px', margin: '0 0 8px' }, text: 'HuggingFace tokenizer.json' }),
        field('Repo id', hfRepo),
        field('Save as', hfAs, 'the name you pick in a model alias'),
        h('div.row', {},
          h('button.primary.sm', {
            onclick: () => hfRepo.value && install({ hf: hfRepo.value.trim(), as: hfAs.value.trim() || undefined }),
          }, 'Download'),
          select('', [['', 'presets…'], ...HF_PRESETS.map(([name, repo]) => [repo, `${name} — ${repo}`])], {
            style: { width: 'auto' },
            onchange: (e) => {
              const preset = HF_PRESETS.find(([, repo]) => repo === e.target.value);
              if (preset) { hfRepo.value = preset[1]; hfAs.value = preset[0]; }
            },
          }),
        ),
      ),
    ),
    h('p.small.muted', { text: 'Gated repos (Llama, Gemma) need HF_TOKEN set in the environment before starting the relay.' }),
    h('div', { style: { marginTop: '10px' } }, installOut),
  )));

  return root;
}
