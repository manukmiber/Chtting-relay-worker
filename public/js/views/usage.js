import { api } from '../api.js';
import {
  h, card, stat, table, pill, toast, select,
  fmtNum, fmtMs, fmtTime,
} from '../ui.js';
import { barChart } from '../charts.js';

/**
 * The accounting view.
 *
 * Everything here comes from the usage ledger, not from the browsable request
 * log — the ledger takes appends only, is never pruned, and each row is
 * chained to the one before it, so these are the numbers that can be relied on
 * even after old request rows have been cleared away.
 */
export async function usageView(ctx) {
  const range = ctx.store.usageRange ?? '30d';
  const [summary, daily, rows, chain] = await Promise.all([
    api.usageSummary(range),
    api.usageDaily(30),
    api.usageLedger(100),
    api.verifyLedger(),
  ]);

  const root = h('div');
  const w = summary.window;
  const t = summary.today;

  /* ------------------------------------------------------ the six numbers */
  const ranges = select(range, [
    ['24h', 'last 24 hours'], ['7d', 'last 7 days'],
    ['30d', 'last 30 days'], ['365d', 'last year'],
  ], {
    onchange: (ev) => {
      ctx.store.usageRange = ev.target.value;
      ctx.rerender();
    },
  });

  root.append(card('Usage', h('div', {},
    h('div.grid.stats', {},
      stat('Requests', fmtNum(w.requests), `${fmtNum(t.requests)} today`),
      stat('Input tokens', fmtNum(w.inputTokens), `${fmtNum(w.billedInputTokens)} billed upstream`),
      stat('Output tokens', fmtNum(w.outputTokens), `${fmtNum(t.outputTokens)} today`),
      // Both are measured between the first and last token, so a window with
      // no streamed replies in it has nothing to average — say so rather than
      // showing a zero that reads like a performance problem.
      stat('Average TTFT', fmtMs(w.avgTtftMs),
        w.avgTtftMs ? 'time to the first token' : 'streamed replies only'),
      stat('Tokens per second', w.avgTokensPerSec ? String(w.avgTokensPerSec) : '—',
        w.avgTokensPerSec ? 'while generating' : 'streamed replies only'),
      stat('Cache hits', `${w.cacheHitRate ?? 0}%`, `${fmtNum(w.cachedTokens)} tokens served from cache`),
    ),
    h('p.small.muted', {
      text: `${fmtNum(w.users)} distinct keys · ${fmtNum(w.errors)} errors · `
        + `${fmtNum(w.inFlight)} still open · average wait ${fmtMs(w.avgQueuedMs)}`,
    }),
  ), [ranges]));

  /* --------------------------------------------------- what the relay pays */
  const margin = w.billedInputTokens - w.inputTokens;
  if (margin !== 0) {
    root.append(card('System prompt overhead', h('div', {},
      h('p.small.muted', {
        text: 'What the backend charged for prompts, against what callers were '
          + 'accounted for. The difference is the system prompt this relay injects '
          + 'on their behalf.',
      }),
      h('div.grid.stats', {},
        stat('Charged to callers', fmtNum(w.inputTokens)),
        stat('Charged by backends', fmtNum(w.billedInputTokens)),
        stat('Injected by this relay', fmtNum(margin),
          w.billedInputTokens ? `${((margin / w.billedInputTokens) * 100).toFixed(1)}% of input` : ''),
      ),
    )));
  }

  /* --------------------------------------------------------------- by day */
  if (daily.length) {
    root.append(card('Daily', h('div', {},
      barChart(
        daily.map((d) => ({
          label: d.day.slice(5),
          inputTokens: d.inputTokens,
          outputTokens: d.outputTokens,
        })),
        { series: ['inputTokens', 'outputTokens'], label: 'Input and output tokens per day' },
      ),
      table(
        ['Day', { label: 'Requests', num: true }, { label: 'In', num: true },
          { label: 'Out', num: true }, { label: 'Cache hits', num: true },
          { label: 'TTFT', num: true }, { label: 'Tok/s', num: true }],
        [...daily].reverse(),
        (d) => h('tr', {},
          h('td.mono', { text: d.day }),
          h('td.num', { text: fmtNum(d.requests) }),
          h('td.num', { text: fmtNum(d.inputTokens) }),
          h('td.num', { text: fmtNum(d.outputTokens) }),
          h('td.num', { text: fmtNum(d.cacheHits) }),
          h('td.num', { text: fmtMs(d.avgTtftMs) }),
          h('td.num', { text: String(d.avgTokensPerSec) }),
        ),
      ),
    )));
  }

  /* ------------------------------------------------------------ the chain */
  root.append(card('Ledger integrity', h('div', {},
    h('div.row', { style: { alignItems: 'center', gap: '10px' } },
      pill(chain.ok ? 'intact' : 'broken', chain.ok ? 'ok' : 'err'),
      h('span.small', { text: chain.message }),
    ),
    h('p.small.muted', {
      text: 'The ledger accepts new rows only: SQLite refuses to change or remove one. '
        + 'Each row also carries the hash of the row before it, so an edit made around '
        + 'the database — by dropping the guard, or touching the file directly — shows '
        + 'up here as a break.',
    }),
    h('button.sm', {
      onclick: async () => {
        const again = await api.verifyLedger();
        toast(again.message, again.ok ? 'ok' : 'err');
      },
    }, 'Re-check now'),
  )));

  /* -------------------------------------------------------- recent rows */
  root.append(card(`Last ${rows.length} ledger rows`, table(
    ['When', 'Phase', 'Model', { label: 'In', num: true }, { label: 'Out', num: true },
      { label: 'TTFT', num: true }, { label: 'Tok/s', num: true }, 'Cache', 'Hash'],
    rows,
    (r) => h('tr', {},
      h('td', { text: fmtTime(r.ts) }),
      h('td', {}, pill(r.phase, r.phase === 'input' ? '' : (r.status === 200 ? 'ok' : 'err'))),
      h('td.mono.small', { text: r.model || '—' }),
      h('td.num', { text: r.inputTokens ? fmtNum(r.inputTokens) : '—' }),
      h('td.num', { text: r.outputTokens ? fmtNum(r.outputTokens) : '—' }),
      h('td.num', { text: r.ttftMs ? fmtMs(r.ttftMs) : '—' }),
      h('td.num', { text: r.tokensPerSec || '—' }),
      h('td', { text: r.cacheHit ? `${fmtNum(r.cachedTokens)}` : '—' }),
      h('td.mono.small.muted', { text: r.hash }),
    ),
  )));

  return root;
}
