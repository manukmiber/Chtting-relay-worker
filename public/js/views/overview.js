import { api } from '../api.js';
import { h, card, stat, table, pill, fmtNum, fmtMs, fmtUsd, live, clear, toast } from '../ui.js';
import { lineChart, barChart } from '../charts.js';
import { restartRelay, updateRelay } from '../relay.js';

const RANGES = [['24h', 'Last 24h'], ['7d', 'Last 7 days'], ['30d', 'Last 30 days'], ['365d', 'All time']];

export async function overviewView(ctx) {
  const range = ctx.store.range ?? '7d';
  const days = range === '24h' ? 2 : range === '7d' ? 7 : range === '30d' ? 30 : 90;

  const [summary, daily, byModel, byKey] = await Promise.all([
    api.summary(range),
    api.daily(days),
    api.groupBy('model', range),
    api.groupBy('key', range),
  ]);

  const w = summary.window;
  const today = summary.today;

  const rangeSelect = h('select', {
    style: { width: 'auto' },
    onchange: (e) => { ctx.store.range = e.target.value; ctx.rerender(); },
  }, ...RANGES.map(([v, l]) => h('option', { value: v, selected: v === range }, l)));

  const root = h('div');

  root.append(h('div.grid.stats', {},
    stat('Requests', fmtNum(w.requests), `${fmtNum(today.requests)} today`),
    stat('Daily users', fmtNum(today.users), `${fmtNum(w.users)} in range`),
    stat('Tokens in', fmtNum(w.prompt_tokens), w.cached_tokens ? `${fmtNum(w.cached_tokens)} cached` : 'prompt tokens'),
    stat('Tokens out', fmtNum(w.completion_tokens), w.reasoning_tokens ? `${fmtNum(w.reasoning_tokens)} reasoning` : 'completion tokens'),
    stat('Avg TTFT', fmtMs(w.avg_ttft_ms), `p95 ${fmtMs(w.p95_ttft_ms)}`),
    stat('Avg TPS', `${w.avg_tps || 0}`, `p95 ${w.p95_tps || 0} tok/s`),
    stat('Avg latency', fmtMs(w.avg_total_ms), `p95 ${fmtMs(w.p95_total_ms)}`),
    stat('Errors', `${w.error_rate}%`, `${fmtNum(w.errors)} of ${fmtNum(w.requests)}`),
  ));

  // Money, but only once there is a price list to report it from. Three zeroes
  // would read as free service rather than as "nobody has set a price".
  if (w.proxy_usd || w.backend_usd) {
    root.append(h('div.grid.stats', {},
      stat('Backend cost', fmtUsd(w.backend_usd), 'what we were charged'),
      stat('Proxy revenue', fmtUsd(w.proxy_usd), 'what callers were charged'),
      stat('Profit', fmtUsd(w.profit_usd), `${w.margin_percent}% margin`),
      stat('Callers', fmtNum(w.callers), 'distinct user ids'),
    ));
  }

  const chartHolder = h('div');
  const metrics = [
    ['requests', 'Requests / day', (d) => ({ label: shortDay(d.day), value: d.requests })],
    ['tokens', 'Tokens / day', null],
    ['ttft', 'Avg TTFT / day (ms)', (d) => ({ label: shortDay(d.day), value: Math.round(d.avg_ttft ?? 0) })],
    ['tps', 'Avg tokens/sec per day', (d) => ({ label: shortDay(d.day), value: Number((d.avg_tps ?? 0).toFixed(2)) })],
    ['users', 'Daily users', (d) => ({ label: shortDay(d.day), value: d.users })],
  ];

  const drawChart = (kind) => {
    clear(chartHolder);
    if (kind === 'tokens') {
      chartHolder.append(
        barChart(
          daily.map((d) => ({ label: shortDay(d.day), in: d.prompt_tokens ?? 0, out: d.completion_tokens ?? 0 })),
          { series: ['in', 'out'], label: 'tokens in and out per day' },
        ),
        h('div.legend', {},
          h('span', { html: '<i style="background:var(--accent)"></i>' }, 'input tokens'),
          h('span', { html: '<i style="background:var(--ok)"></i>' }, 'output tokens'),
        ),
      );
      return;
    }
    const spec = metrics.find((m) => m[0] === kind);
    chartHolder.append(lineChart(daily.map(spec[2]), { label: spec[1] }));
  };

  const buttons = h('div.row', {}, ...metrics.map(([key, label]) => h('button.sm', {
    class: key === (ctx.store.metric ?? 'requests') ? 'primary' : '',
    onclick: (e) => {
      ctx.store.metric = key;
      for (const b of buttons.children) b.className = 'sm';
      e.currentTarget.className = 'sm primary';
      drawChart(key);
    },
  }, label)));

  root.append(card('Trend', h('div', {}, buttons, chartHolder), [rangeSelect]));
  drawChart(ctx.store.metric ?? 'requests');

  // Requirement 4: the numbers keep themselves current.
  const stopLive = live(ctx, 5000, () => ctx.rerender());

  root.append(relayControls(root, stopLive));

  root.append(card('Per model', table(
    [{ label: 'Alias' }, { label: 'Requests', num: true }, { label: 'Users', num: true },
      { label: 'In', num: true }, { label: 'Out', num: true }, { label: 'TTFT', num: true },
      { label: 'TPS', num: true }, { label: 'Profit', num: true }, { label: 'Errors', num: true }],
    byModel,
    (r) => h('tr', {},
      h('td.mono', { text: r.name || '—' }),
      h('td.num', { text: fmtNum(r.requests) }),
      h('td.num', { text: fmtNum(r.users) }),
      h('td.num', { text: fmtNum(r.prompt_tokens) }),
      h('td.num', { text: fmtNum(r.completion_tokens) }),
      h('td.num', { text: fmtMs(r.avg_ttft) }),
      h('td.num', { text: (r.avg_tps ?? 0).toFixed(1) }),
      h('td.num', { text: r.proxy_usd ? fmtUsd(r.profit_usd) : '—' }),
      h('td.num', {}, r.errors ? pill(String(r.errors), 'err') : h('span.muted', { text: '0' })),
    ),
  )));

  root.append(card('Per client key', table(
    [{ label: 'Key' }, { label: 'Requests', num: true }, { label: 'In', num: true },
      { label: 'Out', num: true }, { label: 'Total tokens', num: true }, { label: 'Errors', num: true }],
    byKey,
    (r) => h('tr', {},
      h('td', { text: r.label ?? r.name ?? '—' }),
      h('td.num', { text: fmtNum(r.requests) }),
      h('td.num', { text: fmtNum(r.prompt_tokens) }),
      h('td.num', { text: fmtNum(r.completion_tokens) }),
      h('td.num', { text: fmtNum(r.total_tokens) }),
      h('td.num', {}, r.errors ? pill(String(r.errors), 'err') : h('span.muted', { text: '0' })),
    ),
  )));

  return root;
}

/**
 * Ending this process from the screen everyone lands on.
 *
 * The same two buttons as the Setup tab, put where they are actually reached
 * for: restarting the relay after a settings change, or moving it onto the
 * commits that were pushed since it started, should not need three taps and a
 * scroll past the install checklist.
 *
 * Both take over the page while they run, because neither leaves anything else
 * on it worth looking at.
 */
function relayControls(root, stopLive) {
  const run = (fn) => async (ev) => {
    const button = ev.currentTarget;
    const label = button.textContent;
    button.disabled = true;
    button.textContent = 'Working…';
    // The five-second refresh would rebuild this page out from under the
    // progress log the moment it appeared.
    stopLive();
    try {
      await fn(root);
    } catch (err) {
      toast(err.message, 'err');
      button.disabled = false;
      button.textContent = label;
    }
  };

  return card('Relay', h('div', {},
    h('p.small.muted', {
      text: 'Update & restart pulls the latest commits, builds them and restarts into '
        + 'the result \u2014 minutes on a phone, and the relay keeps answering until the '
        + 'new binary is ready. Restart re-runs the binary already on disk, which takes '
        + 'seconds and changes no code.',
    }),
    h('div.row', {},
      h('button.primary.sm', { onclick: run(updateRelay) }, 'Update & restart'),
      h('button.sm', { onclick: run(restartRelay) }, 'Restart'),
    ),
  ));
}

function shortDay(day) {
  return String(day ?? '').slice(5);
}
