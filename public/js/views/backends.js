import { api } from '../api.js';
import {
  h, card, table, pill, drawer, field, text, number, textarea, select, toast,
  confirmDialog, parseKeyValues, stringifyKeyValues, fmtMs,
} from '../ui.js';

/** Upstream providers. Their API keys never leave this machine. */
export async function backendsView(ctx) {
  const backends = ctx.state.config.backends ?? [];
  const models = ctx.state.config.models ?? [];

  return h('div', {}, card('Backends', table(
    [{ label: 'Name' }, { label: 'Base URL' }, { label: 'API key' }, { label: 'Models', num: true }, { label: '' }],
    backends,
    (b) => h('tr.clickable', { onclick: () => editBackend(ctx, b) },
      h('td', {},
        h('div', { text: b.name }),
        b.enabled ? null : pill('disabled', 'warn'),
      ),
      h('td.mono.small', {}, h('span.truncate', { text: b.baseUrl, title: b.baseUrl })),
      h('td.mono.small', { text: b.apiKey || '—' }),
      h('td.num', { text: String(models.filter((m) => m.backend === b.id).length) }),
      h('td', {}, h('button.sm', {
        onclick: async (e) => {
          e.stopPropagation();
          const btn = e.currentTarget;
          btn.disabled = true;
          btn.textContent = 'Testing…';
          try {
            const result = await api.testBackend(b.id);
            showTestResult(b, result);
          } catch (err) {
            toast(err.message, 'err');
          } finally {
            btn.disabled = false;
            btn.textContent = 'Test';
          }
        },
      }, 'Test')),
    ),
  ), [
    h('button.primary.sm', { onclick: () => editBackend(ctx, null) }, '+ Add backend'),
  ]));
}

function showTestResult(backend, result) {
  drawer(`Test: ${backend.name}`, () => h('div', {},
    h('div.row', { style: { marginBottom: '12px' } },
      pill(result.ok ? `HTTP ${result.status}` : `HTTP ${result.status || 'failed'}`, result.ok ? 'ok' : 'err'),
      pill(fmtMs(result.ms)),
    ),
    h('p.small.muted.mono', { text: result.url }),
    result.error ? h('pre.log', { text: result.error }) : null,
    result.body ? h('pre.log', { text: result.body }) : null,
    result.models?.length
      ? h('div', {},
        h('p.small.muted', { text: `${result.models.length} model(s) reported by this backend — copy the exact name into a model alias:` }),
        h('pre.log', { text: result.models.join('\n') }))
      : (result.ok ? h('p.small.muted', { text: 'The backend answered but did not list any models.' }) : null),
  ));
}

function editBackend(ctx, existing) {
  const isNew = !existing;
  const b = structuredClone(existing ?? {
    id: '',
    name: '',
    type: 'openai',
    baseUrl: '',
    apiKey: '',
    enabled: true,
    timeoutMs: 600000,
    maxRetries: 1,
    headers: {},
    note: '',
  });
  const i = {};

  drawer(isNew ? 'New backend' : b.name, () => {
    i.name = text(b.name, { placeholder: 'DeepSeek official' });
    i.baseUrl = text(b.baseUrl, { placeholder: 'https://api.deepseek.com/v1', class: 'mono' });
    i.apiKey = h('input', { type: 'text', value: b.apiKey ?? '', class: 'mono', placeholder: 'sk-…' });
    i.type = select(b.type ?? 'openai', [
      ['openai', 'OpenAI-compatible (Authorization: Bearer)'],
      ['anthropic', 'Anthropic style (x-api-key)'],
    ]);
    i.enabled = h('input', { type: 'checkbox', checked: b.enabled !== false });
    i.timeoutMs = number(b.timeoutMs ?? 600000, { min: 1000, step: 1000 });
    i.maxRetries = number(b.maxRetries ?? 1, { min: 0, max: 5 });
    i.streamOptions = h('input', { type: 'checkbox', checked: b.streamOptions !== false });
    i.headers = textarea(stringifyKeyValues(b.headers), { placeholder: 'HTTP-Referer=https://example.com', rows: 3 });
    i.note = text(b.note ?? '');

    return h('div', {},
      field('Name', i.name),
      field('Base URL', i.baseUrl, 'ending in /v1 for most providers'),
      field('API key', i.apiKey, existing ? 'leave the masked value to keep the stored key' : ''),
      field('Auth style', i.type),
      h('label.switch', { style: { marginBottom: '14px' } }, i.enabled, h('span', { text: 'Enabled' })),
      h('div.grid.form', {},
        field('Timeout (ms)', i.timeoutMs),
        field('Retries', i.maxRetries, 'on 429, 5xx and dropped connections'),
      ),
      h('label.switch', { style: { marginBottom: '14px' } }, i.streamOptions,
        h('span', { text: 'Ask for usage while streaming (stream_options)' })),
      field('Extra headers', i.headers),
      field('Note', i.note),
    );
  }, {
    saveLabel: isNew ? 'Create' : 'Save',
    extra: isNew ? [] : [h('button.danger', {
      onclick: async () => {
        const used = (ctx.state.config.models ?? []).filter((m) => m.backend === b.id);
        const warning = used.length
          ? `${used.length} model alias(es) point at this backend and will stop working. Delete anyway?`
          : `Delete backend "${b.name}"?`;
        if (!confirmDialog(warning)) return;
        await api.remove('backends', b.id);
        toast('Backend deleted', 'ok');
        await ctx.reload();
      },
    }, 'Delete')],
    onSave: async (close) => {
      try {
        await api.save('backends', {
          ...(isNew ? {} : { id: b.id }),
          name: i.name.value.trim() || 'backend',
          baseUrl: i.baseUrl.value.trim(),
          apiKey: i.apiKey.value,
          type: i.type.value,
          enabled: i.enabled.checked,
          timeoutMs: Number(i.timeoutMs.value) || 600000,
          maxRetries: Number(i.maxRetries.value) || 0,
          streamOptions: i.streamOptions.checked,
          headers: parseKeyValues(i.headers.value),
          note: i.note.value,
        });
        toast('Backend saved', 'ok');
        close();
        await ctx.reload();
      } catch (err) {
        toast(err.message, 'err');
      }
    },
  });
}
