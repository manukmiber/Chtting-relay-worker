import { api } from '../api.js';
import {
  h, card, table, pill, drawer, field, text, number, select, toast,
  confirmDialog, copy, fmtNum, fmtTime, parseList,
} from '../ui.js';

/** Client API keys: who may call the relay, for which models, and how much. */
export async function keysView(ctx) {
  const keys = ctx.state.config.keys ?? [];
  const models = ctx.state.config.models ?? [];
  const usage = await api.groupBy('key', '30d').catch(() => []);
  const usageById = new Map(usage.map((u) => [u.name, u]));

  const root = h('div');

  if (!ctx.state.config.security?.requireClientKey) {
    root.append(card('Open relay', h('p.muted', {
      text: 'Client keys are currently not required — anyone who reaches the relay can use it. '
        + 'Turn "Require a client key" back on under Settings once your tunnel is public.',
    })));
  }

  root.append(card('Client keys', table(
    [{ label: 'Label' }, { label: 'Key' }, { label: 'Models' }, { label: 'Quota' },
      { label: 'Requests 30d', num: true }, { label: 'Tokens 30d', num: true }],
    keys,
    (k) => {
      const u = usageById.get(k.id);
      return h('tr.clickable', { onclick: () => editKey(ctx, k, models) },
        h('td', {},
          h('div', { text: k.label || k.id }),
          k.enabled ? null : pill('disabled', 'warn'),
        ),
        h('td.mono.small', {}, k.key, ' ', h('button.ghost.sm', {
          title: 'Copy the full key',
          onclick: async (e) => {
            e.stopPropagation();
            const { key } = await api.revealKey(k.id);
            copy(key, 'Key copied');
          },
        }, '⧉')),
        h('td.small', { text: (k.models ?? ['*']).includes('*') ? 'all' : (k.models ?? []).join(', ') }),
        h('td.small', { text: quotaLabel(k.quota) }),
        h('td.num', { text: fmtNum(u?.requests ?? 0) }),
        h('td.num', { text: fmtNum(u?.total_tokens ?? 0) }),
      );
    },
  ), [
    h('button.primary.sm', { onclick: () => generate(ctx) }, '+ Generate key'),
  ]));

  return root;
}

function quotaLabel(q) {
  const parts = [];
  if (q?.requestsPerMinute) parts.push(`${q.requestsPerMinute}/min`);
  if (q?.requestsPerDay) parts.push(`${fmtNum(q.requestsPerDay)} req/day`);
  if (q?.tokensPerDay) parts.push(`${fmtNum(q.tokensPerDay)} tok/day`);
  return parts.length ? parts.join(' · ') : 'unlimited';
}

async function generate(ctx) {
  try {
    const { item } = await api.generateKey({ label: 'new key' });
    await ctx.reload();
    // The plaintext key is returned exactly once; show it before it is masked.
    drawer('Key created', () => h('div', {},
      h('p.small.muted', { text: 'Copy this now — the dashboard only ever shows a masked version afterwards.' }),
      h('pre.log', { text: item.key }),
      h('div.row', {}, h('button.primary', { onclick: () => copy(item.key, 'Key copied') }, 'Copy key')),
      h('p.small.muted', { style: { marginTop: '16px' }, text: 'Use it like any OpenAI key:' }),
      h('pre.log', {
        text: `curl ${location.protocol}//${location.hostname}:${ctx.state.relay.port}/v1/chat/completions \\\n`
          + `  -H "Authorization: Bearer ${item.key}" \\\n`
          + '  -H "Content-Type: application/json" \\\n'
          + `  -d '{"model":"${ctx.state.config.models?.[0]?.id ?? 'your-model-alias'}","messages":[{"role":"user","content":"hi"}]}'`,
      }),
    ));
  } catch (err) {
    toast(err.message, 'err');
  }
}

function editKey(ctx, k, models) {
  const i = {};
  drawer(k.label || k.id, () => {
    i.label = text(k.label, { placeholder: 'my phone' });
    i.enabled = h('input', { type: 'checkbox', checked: k.enabled !== false });
    i.models = select((k.models ?? ['*']).includes('*') ? '*' : 'some', [
      ['*', 'all published models'],
      ['some', 'only the models listed below'],
    ]);
    i.modelList = text((k.models ?? []).filter((m) => m !== '*').join(', '), {
      placeholder: models.map((m) => m.id).slice(0, 2).join(', '),
    });
    i.rpm = number(k.quota?.requestsPerMinute ?? 0, { min: 0 });
    i.rpd = number(k.quota?.requestsPerDay ?? 0, { min: 0 });
    i.tpd = number(k.quota?.tokensPerDay ?? 0, { min: 0 });
    i.note = text(k.note ?? '');

    return h('div', {},
      field('Label', i.label),
      h('label.switch', { style: { marginBottom: '14px' } }, i.enabled, h('span', { text: 'Enabled' })),
      field('Model access', i.models),
      field('Allowed models', i.modelList, 'comma separated public names'),
      h('div.grid.form', {},
        field('Requests / minute', i.rpm, '0 = unlimited'),
        field('Requests / day', i.rpd, '0 = unlimited'),
        field('Tokens / day', i.tpd, '0 = unlimited'),
      ),
      field('Note', i.note),
      h('p.small.muted', { text: `Created ${fmtTime(k.createdAt)}` }),
    );
  }, {
    extra: [h('button.danger', {
      onclick: async () => {
        if (!confirmDialog(`Delete key "${k.label || k.id}"? Anything using it stops working immediately.`)) return;
        await api.remove('keys', k.id);
        toast('Key deleted', 'ok');
        await ctx.reload();
      },
    }, 'Delete')],
    onSave: async (close) => {
      try {
        await api.save('keys', {
          id: k.id,
          label: i.label.value,
          enabled: i.enabled.checked,
          models: i.models.value === '*' ? ['*'] : parseList(i.modelList.value),
          quota: {
            requestsPerMinute: Number(i.rpm.value) || 0,
            requestsPerDay: Number(i.rpd.value) || 0,
            tokensPerDay: Number(i.tpd.value) || 0,
          },
          note: i.note.value,
        });
        toast('Key saved', 'ok');
        close();
        await ctx.reload();
      } catch (err) {
        toast(err.message, 'err');
      }
    },
  });
}
