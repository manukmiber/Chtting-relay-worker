import { api } from '../api.js';
import {
  h, card, table, pill, drawer, field, text, number, select, toast,
  confirmDialog, copy, fmtNum, fmtUsd, fmtTime, parseList,
} from '../ui.js';

/**
 * Client API keys: who may call the relay, for which models, how much, and
 * what they owe.
 *
 * A key is one of two kinds, and the difference is who the request is on
 * behalf of:
 *
 * * **Company** — one customer with many people behind it. Every call carries
 *   its own end user's id, and that id is what goes upstream and what the
 *   usage breaks down by.
 * * **Private** — one holder, who *is* the user. Whatever they send as `user`
 *   is ignored; the key's own identity answers instead, and their replies are
 *   never held back by a model's tokens-a-second throttle.
 */
export async function keysView(ctx) {
  const keys = ctx.state.config.keys ?? [];
  const models = ctx.state.config.models ?? [];
  const usage = await api.usageByKey().catch(() => []);
  const usageById = new Map(usage.map((u) => [u.keyId, u]));

  const root = h('div');

  if (!ctx.state.config.security?.requireClientKey) {
    root.append(card('Open relay', h('p.muted', {
      text: 'Client keys are currently not required — anyone who reaches the relay can use it. '
        + 'Turn "Require a client key" back on under Settings once your tunnel is public.',
    })));
  }

  root.append(card('Client keys', table(
    [{ label: 'Label' }, { label: 'Kind' }, { label: 'Key' }, { label: 'Models' },
      { label: 'Quota' }, { label: 'Requests', num: true }, { label: 'Unbilled', num: true }],
    keys,
    (k) => {
      const u = usageById.get(k.id);
      const owed = u?.current?.subtotalUsd ?? 0;
      return h('tr.clickable', { onclick: () => editKey(ctx, k, models) },
        h('td', {},
          h('div', { text: k.label || k.id }),
          k.enabled ? null : pill('disabled', 'warn'),
        ),
        h('td', {}, kindPill(k.kind)),
        h('td.mono.small', {}, k.key, ' ', h('button.ghost.sm', {
          title: 'Copy the full key',
          onclick: async (e) => {
            e.stopPropagation();
            const { key } = await api.revealKey(k.id);
            copy(key, 'Key copied');
          },
        }, '⧉')),
        h('td.small', { text: (k.models ?? ['*']).includes('*') ? 'all' : (k.models ?? []).join(', ') }),
        h('td.small', { text: quotaLabel(k.quota, k.kind) }),
        h('td.num', { text: fmtNum(u?.current?.requests ?? 0) }),
        h('td.num', { text: owed ? fmtUsd(owed) : '—' }),
      );
    },
  ), [
    h('button.sm', { onclick: () => generate(ctx, 'company') }, '+ Company key'),
    h('button.primary.sm', { onclick: () => generate(ctx, 'private') }, '+ Private key'),
  ]));

  root.append(card('What the two kinds mean', h('div.grid.form', {},
    h('div', {},
      h('p', {}, kindPill('company'), ' '),
      h('p.small.muted', {
        text: 'A reseller. Each call must carry its own end user — in the "user" '
          + 'field or an x-user-id header — and that id is what the backend sees, '
          + 'what isolates their prompt cache, and what the invoice breaks down by. '
          + 'The model’s tokens-a-second throttle applies.',
      }),
    ),
    h('div', {},
      h('p', {}, kindPill('private'), ' '),
      h('p.small.muted', {
        text: 'One holder. Anything they send as "user" is ignored and the key’s '
          + 'own identity is used instead, so they cannot claim to be somebody '
          + 'else’s caller. Replies are never paced: they stream at whatever '
          + 'speed the backend manages.',
      }),
    ),
  )));

  return root;
}

function kindPill(kind) {
  return kind === 'private' ? pill('private', 'ok') : pill('company', '');
}

function quotaLabel(q, kind) {
  const parts = [];
  if (q?.requestsPerMinute) parts.push(`${q.requestsPerMinute}/min`);
  if (q?.requestsPerDay) parts.push(`${fmtNum(q.requestsPerDay)} req/day`);
  if (q?.tokensPerDay) parts.push(`${fmtNum(q.tokensPerDay)} tok/day`);
  if (kind === 'private') parts.push('uncapped tok/s');
  return parts.length ? parts.join(' · ') : 'unlimited';
}

async function generate(ctx, kind) {
  try {
    const { item } = await api.generateKey({ label: `new ${kind} key`, kind });
    await ctx.reload();
    // The plaintext key is returned exactly once; show it before it is masked.
    drawer(`${kind === 'private' ? 'Private' : 'Company'} key created`, () => h('div', {},
      h('p.small.muted', { text: 'Copy this now — the dashboard only ever shows a masked version afterwards.' }),
      h('pre.log', { text: item.key }),
      h('div.row', {}, h('button.primary', { onclick: () => copy(item.key, 'Key copied') }, 'Copy key')),
      h('p.small.muted', { style: { marginTop: '16px' }, text: 'Use it like any OpenAI key:' }),
      h('pre.log', {
        text: `curl ${location.protocol}//${location.hostname}:${ctx.state.relay.port}/v1/chat/completions \\\n`
          + `  -H "Authorization: Bearer ${item.key}" \\\n`
          + '  -H "Content-Type: application/json" \\\n'
          + (kind === 'company' ? '  -H "X-User-Id: your-end-user" \\\n' : '')
          + `  -d '{"model":"${ctx.state.config.models?.[0]?.id ?? 'your-model-alias'}","messages":[{"role":"user","content":"hi"}]}'`,
      }),
      kind === 'company'
        ? h('p.small.muted', {
          text: 'Send the end user on every call, either as X-User-Id or as "user" in '
            + 'the body. Without it the relay cannot tell your customers apart, and '
            + 'neither can the backend’s prompt cache.',
        })
        : h('p.small.muted', {
          text: 'Nothing else to send: this key is the user. Its replies are not paced.',
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
    i.kind = select(k.kind ?? 'company', [
      ['company', 'Company — the caller sends its own end user'],
      ['private', 'Private — the key is the user, and is never paced'],
    ]);
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

    const b = k.billing ?? {};
    i.billName = text(b.name ?? '', { placeholder: k.label || 'who the invoice is for' });
    i.billEmail = text(b.email ?? '', { placeholder: 'billing@example.com' });
    i.billAddress = text(b.address ?? '');
    i.billTaxId = text(b.taxId ?? '', { placeholder: 'VAT / NPWP' });
    i.billTax = text(b.taxPercent ?? '', { placeholder: 'inherit the global rate' });
    i.autoInvoice = h('input', { type: 'checkbox', checked: b.autoInvoice === true });

    return h('div', {},
      field('Label', i.label),
      h('label.switch', { style: { marginBottom: '14px' } }, i.enabled, h('span', { text: 'Enabled' })),
      field('Kind', i.kind, 'private keys ignore the caller’s user id and are never throttled'),
      field('Model access', i.models),
      field('Allowed models', i.modelList, 'comma separated public names'),
      h('div.grid.form', {},
        field('Requests / minute', i.rpm, '0 = unlimited'),
        field('Requests / day', i.rpd, '0 = unlimited'),
        field('Tokens / day', i.tpd, '0 = unlimited'),
      ),
      field('Note', i.note),
      h('h3.small', { text: 'Billing', style: { marginTop: '18px' } }),
      h('div.grid.form', {},
        field('Bill to', i.billName, 'blank uses the label'),
        field('Email', i.billEmail),
        field('Tax id', i.billTaxId),
        field('Tax %', i.billTax, 'blank inherits the global rate'),
      ),
      field('Address', i.billAddress),
      h('label.switch', {}, i.autoInvoice, h('span', { text: 'Invoice automatically on the billing cycle' })),
      h('div.row', { style: { marginTop: '14px' } },
        h('button.sm', { onclick: () => ctx.go('billing') }, 'Usage and invoices →'),
      ),
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
        // An empty tax box means "inherit", which is not the same as 0. Only a
        // number that was actually typed is sent.
        const typedTax = i.billTax.value.trim();
        await api.save('keys', {
          id: k.id,
          label: i.label.value,
          enabled: i.enabled.checked,
          kind: i.kind.value,
          models: i.models.value === '*' ? ['*'] : parseList(i.modelList.value),
          quota: {
            requestsPerMinute: Number(i.rpm.value) || 0,
            requestsPerDay: Number(i.rpd.value) || 0,
            tokensPerDay: Number(i.tpd.value) || 0,
          },
          note: i.note.value,
          billing: {
            name: i.billName.value,
            email: i.billEmail.value,
            address: i.billAddress.value,
            taxId: i.billTaxId.value,
            ...(typedTax === '' ? {} : { taxPercent: Number(typedTax) || 0 }),
            autoInvoice: i.autoInvoice.checked,
          },
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
