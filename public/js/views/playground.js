import { api } from '../api.js';
import {
  h, card, field, textarea, select, number, toast, clear, pill, fmtMs, fmtNum, mount,
} from '../ui.js';

/**
 * Send a request through the relay itself, so you can see exactly what a
 * caller receives after name translation, prompt injection and reshaping.
 */
export async function playgroundView(ctx) {
  const models = ctx.state.config.models ?? [];
  const root = h('div');
  const out = h('div');

  const modelSelect = select(ctx.store.pgModel ?? models[0]?.id ?? '', models.map((m) => [m.id, m.id]));
  const prompt = textarea(ctx.store.pgPrompt ?? 'Say hello in one short sentence.', { rows: 4 });
  const system = textarea(ctx.store.pgSystem ?? '', { rows: 3, placeholder: 'optional extra system message from the caller' });
  const maxTokens = number(ctx.store.pgMax ?? 256, { min: 1, max: 32768 });
  const temperature = h('input', { type: 'number', value: ctx.store.pgTemp ?? '', step: '0.1', placeholder: 'model default' });

  async function send() {
    ctx.store.pgModel = modelSelect.value;
    ctx.store.pgPrompt = prompt.value;
    ctx.store.pgSystem = system.value;
    ctx.store.pgMax = maxTokens.value;
    ctx.store.pgTemp = temperature.value;

    clear(out).append(h('div.empty', { text: 'Sending through the relay…' }));
    const messages = [];
    if (system.value.trim()) messages.push({ role: 'system', content: system.value });
    messages.push({ role: 'user', content: prompt.value });

    try {
      const result = await api.playground({
        model: modelSelect.value,
        messages,
        max_tokens: Number(maxTokens.value) || undefined,
        ...(temperature.value === '' ? {} : { temperature: Number(temperature.value) }),
      });

      const body = result.body ?? {};
      const choice = body.choices?.[0];
      const ok = result.status >= 200 && result.status < 300;

      mount(clear(out),
        h('div.row', { style: { marginBottom: '10px' } },
          pill(result.status ? `HTTP ${result.status}` : 'failed', ok ? 'ok' : 'err'),
          pill(fmtMs(result.ms)),
          body.model ? pill(`model: ${body.model}`, 'accent') : null,
          body.usage ? pill(`${fmtNum(body.usage.prompt_tokens)} in / ${fmtNum(body.usage.completion_tokens)} out`) : null,
        ),
        result.error ? h('pre.log', { text: result.error }) : null,
        choice?.message?.content
          ? h('div', {}, h('h3', { style: { fontSize: '13px' }, text: 'Reply' }), h('pre.log', { text: choice.message.content }))
          : null,
        choice?.message?.reasoning_content
          ? h('div', {}, h('h3', { style: { fontSize: '13px' }, text: 'Reasoning' }), h('pre.log', { text: choice.message.reasoning_content }))
          : null,
        h('details.section', {}, h('summary', { text: 'Raw response' }), h('div', {}, h('pre.log', { text: JSON.stringify(body, null, 2) }))),
      );
      await ctx.refreshState();
    } catch (err) {
      clear(out).append(h('div.empty', { text: err.message }));
      toast(err.message, 'err');
    }
  }

  if (!models.length) {
    root.append(card('No models yet', h('p.muted', { text: 'Add a backend and a model alias first.' })));
    return root;
  }

  root.append(card('Try a model', h('div', {},
    h('div.grid.form', {},
      field('Model alias', modelSelect),
      field('Max tokens', maxTokens),
      field('Temperature', temperature),
    ),
    field('System message (as a caller would send it)', system),
    field('User message', prompt),
    h('div.row', {}, h('button.primary', { onclick: send }, 'Send through the relay')),
    h('p.small.muted', {
      text: 'This goes over HTTP to the relay port using your first enabled client key, '
        + 'so it exercises auth, translation, injection, reshaping and logging exactly like a real caller.',
    }),
  )));

  root.append(card('Result', out));
  return root;
}
