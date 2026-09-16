import { api } from '../api.js';
import {
  h, card, stat, table, pill, drawer, field, text, textarea, toast,
  confirmDialog, copy, fmtNum, fmtUsd, fmtTime, fmtDate, fmtIdr, fmtUsdt,
} from '../ui.js';

/**
 * What every key owes, and the invoices that closed the periods before it.
 *
 * The number that matters on this screen is **unbilled**: everything a key has
 * run since its last invoice. Issuing an invoice is what moves that number to
 * zero, and it does so without deleting anything — the usage ledger takes
 * appends only, so an invoice records where the line was drawn rather than
 * clearing the rows behind it. Every past period stays reconstructible, which
 * is the whole reason the ledger refuses to be edited in the first place.
 */
export async function billingView(ctx) {
  const [rows, invoices, chain] = await Promise.all([
    api.usageByKey(),
    api.invoices({ limit: 100 }),
    api.verifyInvoices().catch(() => null),
  ]);

  const billing = ctx.state.config.billing ?? {};
  const root = h('div');

  /* ------------------------------------------------------------- totals */
  const unbilled = rows.reduce((sum, r) => sum + (r.current?.subtotalUsd ?? 0), 0);
  const cost = rows.reduce((sum, r) => sum + (r.current?.backendUsd ?? 0), 0);
  const outstanding = invoices
    .filter((i) => i.status === 'issued')
    .reduce((sum, i) => sum + i.totalUsd, 0);

  root.append(card('Billing', h('div', {},
    h('div.grid.stats', {},
      stat('Unbilled', fmtUsd(unbilled), 'across every key, since each last invoice'),
      stat('Cost of it', fmtUsd(cost), 'what the backends charge for the same traffic'),
      stat('Margin', fmtUsd(unbilled - cost),
        unbilled ? `${(((unbilled - cost) / unbilled) * 100).toFixed(1)}% of the sell side` : ''),
      stat('Awaiting payment', fmtUsd(outstanding),
        `${invoices.filter((i) => i.status === 'issued').length} issued, not yet paid`),
    ),
    billing.enabled
      ? h('div', {},
        h('p.small.muted', {
          text: `Currency ${billing.currency ?? 'USD'} · tax ${billing.taxPercent ?? 0}%`
            + ` · due ${billing.dueDays ?? 3} day(s) after issue`
            + (billing.autoIssue
              ? ` · issued automatically on day ${billing.cycleDay} of each month, for keys that opted in`
              : ' · issued by hand'),
        }),
        billing.payment?.idrPerUsdt
          ? h('p.small.muted', {
            text: `Paid in USDT on ${billing.payment.usdtNetwork || 'an unspecified network'}`
              + ` at ${fmtIdr(billing.payment.idrPerUsdt)} per USDT`
              + (billing.payment.rateSource ? ` (${billing.payment.rateSource})` : '')
              + '. The rate and the wallet are frozen onto each invoice as it is issued.',
          })
          : h('p.small', {
            style: { color: 'var(--warn)' },
            text: 'No IDR/USDT rate is set, so invoices will carry no rupiah figure. '
              + 'Set one under Settings → Billing.',
          }),
        !billing.payment?.usdtAddress
          ? h('p.small', {
            style: { color: 'var(--warn)' },
            text: 'No USDT address is set — invoices will not say where to send the money.',
          })
          : null,
      )
      : h('p.small.muted', {
        text: 'Billing is switched off under Settings, so nothing is issued automatically. '
          + 'Usage is still recorded, and you can still issue an invoice by hand.',
      }),
  )));

  /* -------------------------------------------------------- per-key rows */
  root.append(card('Per key', table(
    ['Key', 'Kind', { label: 'Requests', num: true }, { label: 'Tokens', num: true },
      { label: 'Unbilled', num: true }, { label: 'Lifetime', num: true }, 'Last invoice', ''],
    rows,
    (r) => h('tr.clickable', { onclick: () => openKey(ctx, r.keyId) },
      h('td', {},
        h('div', { text: r.label }),
        r.autoInvoice ? pill('auto', 'ok') : null,
      ),
      h('td', {}, r.kind === 'private' ? pill('private', 'ok') : pill('company', '')),
      h('td.num', { text: fmtNum(r.current?.requests ?? 0) }),
      h('td.num', {
        text: fmtNum((r.current?.inputTokens ?? 0) + (r.current?.outputTokens ?? 0)),
      }),
      h('td.num', { text: fmtUsd(r.current?.subtotalUsd ?? 0) }),
      h('td.num.muted', { text: fmtUsd(r.lifetime?.subtotalUsd ?? 0) }),
      h('td.small', {}, r.lastInvoice
        ? h('span', {}, h('span.mono', { text: r.lastInvoice.number }), ' ',
          statusPill(r.lastInvoice.status))
        : h('span.muted', { text: 'never' })),
      h('td', {}, h('button.primary.sm', {
        onclick: (ev) => {
          ev.stopPropagation();
          issue(ctx, r);
        },
      }, 'Invoice')),
    ),
  )));

  /* ------------------------------------------------------------ history */
  const now = Date.now();
  root.append(card('Invoices', table(
    ['Number', 'Billed to', 'Issued', 'Pay by', { label: 'Requests', num: true },
      { label: 'Total', num: true }, { label: 'USDT', num: true },
      { label: 'IDR', num: true }, 'Status'],
    invoices,
    (inv) => h('tr.clickable', { onclick: () => openInvoice(ctx, inv.id) },
      h('td.mono.small', { text: inv.number }),
      h('td.small', { text: (inv.billTo?.company) || inv.keyLabel }),
      h('td.small.muted', { text: fmtDate(inv.issuedAt) }),
      h('td.small', {},
        inv.dueAt
          ? h('span', {
            // A deadline that has passed on an unpaid invoice is the one thing
            // on this screen somebody has to act on today.
            style: (inv.status === 'issued' && inv.dueAt < now) ? { color: 'var(--err)' } : {},
            text: fmtDate(inv.dueAt),
          })
          : h('span.muted', { text: '—' })),
      h('td.num', { text: fmtNum(inv.requests) }),
      h('td.num', { text: fmtUsd(inv.totalUsd) }),
      h('td.num.muted', { text: inv.totalUsdt ? fmtUsdt(inv.totalUsdt) : '—' }),
      h('td.num.muted', { text: inv.totalIdr ? fmtIdr(inv.totalIdr) : '—' }),
      h('td', {}, statusPill(inv.status)),
    ),
  )));

  /* ---------------------------------------------------------- integrity */
  if (chain) {
    root.append(card('Invoice integrity', h('div', {},
      h('div.row', { style: { alignItems: 'center', gap: '10px' } },
        pill(chain.ok ? 'intact' : 'broken', chain.ok ? 'ok' : 'err'),
        h('span.small', { text: chain.message }),
      ),
      h('p.small.muted', {
        text: 'Every figure on an issued invoice is hashed with it, and SQLite refuses '
          + 'an update that touches one. Only paid, void and the note may change '
          + 'afterwards — an invoice that needs different numbers is voided and '
          + 'issued again, which hands its period back to the next one.',
      }),
    )));
  }

  return root;
}

function statusPill(status) {
  if (status === 'paid') return pill('paid', 'ok');
  if (status === 'void') return pill('void', 'warn');
  return pill('issued', '');
}

/* ------------------------------------------------------------- one key -- */

/** The open period in full: every model it used, and what each came to. */
async function openKey(ctx, keyId) {
  let detail;
  try {
    detail = await api.keyUsage(keyId);
  } catch (err) {
    toast(err.message, 'err');
    return;
  }
  const cur = detail.current ?? {};

  drawer(`${detail.label} · unbilled`, () => h('div', {},
    h('div.grid.stats', {},
      stat('Unbilled', fmtUsd(cur.subtotalUsd ?? 0),
        cur.startTs ? `since ${fmtTime(cur.startTs)}` : 'nothing yet'),
      stat('Requests', fmtNum(cur.requests ?? 0)),
      stat('Tokens', fmtNum((cur.inputTokens ?? 0) + (cur.outputTokens ?? 0)),
        `${fmtNum(cur.inputTokens ?? 0)} in · ${fmtNum(cur.outputTokens ?? 0)} out`),
      stat(detail.kind === 'private' ? 'Holder' : 'End users',
        detail.kind === 'private' ? 'one' : fmtNum(cur.users ?? 0)),
    ),
    (cur.lines ?? []).length
      ? h('div', {},
        h('h3.small', { text: 'By model' }),
        table(
          ['Model', { label: 'Requests', num: true }, { label: 'In', num: true },
            { label: 'Out', num: true }, { label: 'Amount', num: true }],
          cur.lines,
          (l) => h('tr', {},
            h('td.mono.small', { text: l.model }),
            h('td.num', { text: fmtNum(l.requests) }),
            h('td.num', { text: fmtNum(l.inputTokens) }),
            h('td.num', { text: fmtNum(l.outputTokens) }),
            h('td.num', { text: fmtUsd(l.amountUsd) }),
          ),
        ),
      )
      : h('p.muted', { text: 'Nothing used since the last invoice.' }),
    h('h3.small', { text: 'Lifetime', style: { marginTop: '18px' } }),
    h('p.small.muted', {
      text: `${fmtNum(detail.lifetime?.requests ?? 0)} requests · `
        + `${fmtUsd(detail.lifetime?.subtotalUsd ?? 0)} charged · `
        + `${fmtUsd(detail.lifetime?.backendUsd ?? 0)} cost · `
        + `${(detail.invoices ?? []).length} invoice(s)`,
    }),
  ), {
    extra: [h('button.primary', {
      onclick: () => issue(ctx, { keyId, label: detail.label, current: cur }),
    }, 'Issue invoice')],
  });
}

/* ------------------------------------------------------------- issuing -- */

/**
 * Issue an invoice, which closes the key's current period and opens the next.
 *
 * Confirmed rather than one-click, because it is the one action here that
 * cannot be taken back: an invoice can be voided, but voiding it hands its
 * period to the next invoice rather than un-issuing it.
 */
function issue(ctx, row) {
  const i = {};
  drawer(`Invoice ${row.label}`, () => {
    i.tax = text('', { placeholder: 'leave blank for the configured rate' });
    i.note = textarea('', { rows: 3, placeholder: 'shown on the invoice' });
    i.force = h('input', { type: 'checkbox' });
    return h('div', {},
      h('p', {
        text: `${fmtNum(row.current?.requests ?? 0)} request(s) come to `
          + `${fmtUsd(row.current?.subtotalUsd ?? 0)} before tax.`,
      }),
      h('p.small.muted', {
        text: 'Issuing draws a line under everything used so far. The key’s unbilled '
          + 'total goes to zero and the next period starts from here — no usage is '
          + 'deleted, and the ledger rows behind this invoice stay exactly where '
          + 'they are.',
      }),
      field('Tax %', i.tax, 'overrides both the key’s rate and the global one'),
      field('Note', i.note),
      h('label.switch', {}, i.force,
        h('span', { text: 'Issue even if the period is under the minimum' })),
    );
  }, {
    saveLabel: 'Issue invoice',
    onSave: async (close) => {
      if (!confirmDialog(`Issue an invoice for "${row.label}"? This closes the current period.`)) return;
      try {
        const typedTax = i.tax.value.trim();
        const result = await api.issueInvoice({
          keyId: row.keyId,
          note: i.note.value,
          force: i.force.checked,
          ...(typedTax === '' ? {} : { taxPercent: Number(typedTax) || 0 }),
        });
        if (!result.ok) {
          toast(result.message, 'warn');
          return;
        }
        toast(`${result.invoice.number} issued · ${fmtUsd(result.invoice.totalUsd)}`, 'ok');
        close();
        ctx.rerender();
      } catch (err) {
        toast(err.message, 'err');
      }
    },
  });
}

/* -------------------------------------------------------- one invoice -- */

async function openInvoice(ctx, id) {
  let payload;
  try {
    payload = await api.invoice(id);
  } catch (err) {
    toast(err.message, 'err');
    return;
  }
  const inv = payload.invoice;
  const issuer = payload.issuer ?? {};
  const to = inv.billTo ?? {};
  const overdue = payload.overdue === true;

  const mark = async (status) => {
    try {
      await api.setInvoiceStatus(inv.id, status);
      toast(`Marked ${status}`, 'ok');
      ctx.rerender();
    } catch (err) {
      toast(err.message, 'err');
    }
  };

  // Every figure below is read off the invoice rather than recomputed from
  // today's settings: the rate, the wallet and the deadline were frozen when
  // it was issued, and recomputing them here would quietly contradict the
  // document the customer is holding.
  const rated = Number(inv.idrPerUsdt) > 0;
  const lineIdr = (usd) => (Number(inv.usdPerUsdt) > 0
    ? (usd / inv.usdPerUsdt) * inv.idrPerUsdt
    : 0);

  const totalTokens = (inv.inputTokens ?? 0) + (inv.outputTokens ?? 0);

  drawer(inv.number, () => h('div', {},
    /* --------------------------------------------------------- status */
    h('div.row', { style: { alignItems: 'center', gap: '10px' } },
      statusPill(inv.status),
      overdue ? pill('overdue', 'err') : null,
      h('span.small.muted', {
        text: inv.settledAt ? `settled ${fmtTime(inv.settledAt)}` : '',
      }),
    ),

    /* ------------------------------------------- 1. who, both parties */
    h('div.grid.form', { style: { marginTop: '14px' } },
      h('div', {},
        h('div.label', { text: 'From' }),
        h('p.small', { style: { fontWeight: '600' }, text: issuer.name || '—' }),
        issuer.address ? h('p.small.muted', { text: issuer.address }) : null,
        issuer.email ? h('p.small.muted', { text: issuer.email }) : null,
        issuer.taxId ? h('p.small.muted', { text: `Tax id ${issuer.taxId}` }) : null,
      ),
      h('div', {},
        h('div.label', { text: 'Billed to' }),
        h('p.small', { style: { fontWeight: '600' }, text: to.company || to.name || inv.keyLabel }),
        to.company && to.name ? h('p.small', { text: to.name }) : null,
        to.email ? h('p.small.muted', { text: to.email }) : null,
        to.address ? h('p.small.muted', { text: to.address }) : null,
        to.taxId ? h('p.small.muted', { text: `Tax id ${to.taxId}` }) : null,
      ),
    ),

    /* ------------------------------- 2. the two dates that matter */
    h('div.grid.stats', { style: { marginTop: '14px' } },
      stat('Invoice date', fmtDate(inv.issuedAt), fmtTime(inv.issuedAt)),
      stat('Pay by', inv.dueAt ? fmtDate(inv.dueAt) : '—',
        inv.dueAt
          ? (overdue ? 'past due' : `${daysLeft(inv.dueAt)} left`)
          : 'no terms were set when this was issued'),
      stat('Period', fmtDate(inv.periodStart), `to ${fmtDate(inv.periodEnd)}`),
    ),

    /* ----------------------------------- 3. what was used, per model */
    h('h3.small', { text: 'Models used', style: { marginTop: '18px' } }),
    table(
      ['Model', { label: 'Requests', num: true }, { label: 'In', num: true },
        { label: 'Out', num: true }, { label: 'Cached', num: true },
        { label: 'Reasoning', num: true }, { label: 'Amount', num: true },
        { label: 'IDR', num: true }],
      inv.lines ?? [],
      (l) => h('tr', {},
        h('td.mono.small', { text: l.model }),
        h('td.num', { text: fmtNum(l.requests) }),
        h('td.num', { text: fmtNum(l.inputTokens) }),
        h('td.num', { text: fmtNum(l.outputTokens) }),
        h('td.num.muted', { text: fmtNum(l.cachedTokens ?? 0) }),
        h('td.num.muted', { text: fmtNum(l.reasoningTokens ?? 0) }),
        h('td.num', { text: fmtUsd(l.amountUsd) }),
        h('td.num.muted', { text: rated ? fmtIdr(lineIdr(l.amountUsd)) : '—' }),
      ),
    ),

    /* ------------------------------------------ 4. total consumption */
    h('h3.small', { text: 'Total usage', style: { marginTop: '18px' } }),
    h('div.grid.stats', {},
      stat('Requests', fmtNum(inv.requests)),
      stat('Tokens', fmtNum(totalTokens),
        `${fmtNum(inv.inputTokens)} in · ${fmtNum(inv.outputTokens)} out`),
      stat('Cached', fmtNum(inv.cachedTokens ?? 0),
        totalTokens ? `${(((inv.cachedTokens ?? 0) / totalTokens) * 100).toFixed(1)}% of the total` : ''),
      stat('Reasoning', fmtNum(inv.reasoningTokens ?? 0)),
    ),
    h('p.small.muted', {
      text: `Ledger rows ${inv.fromSeq + 1}–${inv.toSeq}, which is what makes this `
        + 'invoice reconstructible from the usage ledger months later.',
    }),

    /* ---------------------------------------------------- the totals */
    h('div.grid.stats', { style: { marginTop: '14px' } },
      stat('Subtotal', fmtUsd(inv.subtotalUsd)),
      stat(`Tax ${inv.taxPercent}%`, inv.taxUsd ? fmtUsd(inv.taxUsd) : '—'),
      stat('Total', fmtUsd(inv.totalUsd), inv.currency),
    ),

    /* ------------------------ 5 and 6. the rate, and where it goes */
    h('h3.small', { text: 'Payment', style: { marginTop: '18px' } }),
    rated
      ? h('div.grid.stats', {},
        stat('Amount due', fmtUsdt(inv.totalUsdt), `${fmtUsd(inv.totalUsd)} at the rate below`),
        stat('In rupiah', fmtIdr(inv.totalIdr), 'for reference only — pay in USDT'),
        stat('Rate', fmtIdr(inv.idrPerUsdt), `per USDT${inv.rateSource ? ` · ${inv.rateSource}` : ''}`),
      )
      : h('p.small.muted', {
        text: 'No IDR/USDT rate was set when this invoice was issued, so it carries '
          + 'no rupiah figure. Set one under Settings → Billing before issuing the next.',
      }),
    Number(inv.usdPerUsdt) && Number(inv.usdPerUsdt) !== 1
      ? h('p.small.muted', { text: `Converted at ${inv.usdPerUsdt} USD per USDT.` })
      : null,
    inv.usdtAddress
      ? h('div', { style: { marginTop: '10px' } },
        h('div.label', { text: `USDT address (${inv.usdtNetwork || 'network not recorded'})` }),
        h('div.row', { style: { alignItems: 'center', gap: '8px' } },
          h('code.mono', { style: { wordBreak: 'break-all' }, text: inv.usdtAddress }),
          h('button.ghost.sm', {
            onclick: () => copy(inv.usdtAddress, 'Address copied'),
          }, '⧉'),
        ),
        inv.usdtNetwork
          ? h('p.small', {
            style: { color: 'var(--warn)' },
            text: `Send only over ${inv.usdtNetwork}. USDT sent to this address over `
              + 'another chain cannot be recovered.',
          })
          : null,
      )
      : h('p.small.muted', { text: 'No USDT address was recorded on this invoice.' }),
    inv.paymentInstructions ? h('p.small', { text: inv.paymentInstructions }) : null,
    issuer.paymentTerms ? h('p.small.muted', { text: issuer.paymentTerms }) : null,
    inv.note ? h('p.small', { style: { marginTop: '10px' }, text: inv.note }) : null,

    /* ------------------------------------------------------ integrity */
    h('p.small.muted', { style: { marginTop: '14px' } },
      h('span', { text: 'Content hash ' }),
      h('code.mono', { text: (inv.contentHash ?? '').slice(0, 16) }),
      h('span', { text: ` · v${inv.hashVersion ?? 1}` }),
    ),
  ), {
    extra: [
      inv.status === 'paid'
        ? null
        : h('button.primary', { onclick: () => mark('paid') }, 'Mark paid'),
      inv.status === 'void'
        ? null
        : h('button.danger', {
          onclick: () => {
            if (!confirmDialog(
              `Void ${inv.number}? The period it covered becomes unbilled again and `
              + 'lands on the next invoice.',
            )) return;
            mark('void');
          },
        }, 'Void'),
    ].filter(Boolean),
  });
}

/** How long the customer still has, in the words a person would use. */
function daysLeft(dueAt) {
  const ms = Number(dueAt) - Date.now();
  if (ms <= 0) return 'nothing';
  const days = Math.floor(ms / 86400000);
  if (days >= 1) return `${days} day${days === 1 ? '' : 's'}`;
  const hours = Math.max(1, Math.floor(ms / 3600000));
  return `${hours} hour${hours === 1 ? '' : 's'}`;
}
