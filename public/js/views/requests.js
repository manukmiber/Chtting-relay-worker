import { api } from '../api.js';
import {
  h, card, table, pill, drawer, select, clear, toast,
  fmtNum, fmtMs, fmtTime, copy,
} from '../ui.js';

/** The request log: one row per relayed call, with the full timing breakdown. */
export async function requestsView(ctx) {
  const state = ctx.store.requests ?? (ctx.store.requests = { offset: 0, limit: 50, model: '', status: '', q: '' });
  const root = h('div');
  const body = h('div');

  const modelFilter = select(state.model, [
    ['', 'all models'],
    ...(ctx.state.config.models ?? []).map((m) => [m.id, m.id]),
  ], {
    style: { width: 'auto' },
    onchange: (e) => { state.model = e.target.value; state.offset = 0; load(); },
  });

  const statusFilter = select(state.status, [
    ['', 'all statuses'], ['ok', 'succeeded'], ['error', 'failed'],
  ], {
    style: { width: 'auto' },
    onchange: (e) => { state.status = e.target.value; state.offset = 0; load(); },
  });

  const search = h('input', {
    type: 'search',
    value: state.q,
    placeholder: 'search prompt, reply or error',
    style: { width: 'auto', minWidth: '180px' },
    onchange: (e) => { state.q = e.target.value; state.offset = 0; load(); },
  });

  async function load() {
    clear(body).append(h('div.empty', { text: 'Loading…' }));
    try {
      const { rows, total } = await api.requests({
        limit: state.limit,
        offset: state.offset,
        model: state.model,
        status: state.status,
        q: state.q,
      });
      clear(body).append(renderTable(rows), pager(total));
    } catch (err) {
      clear(body).append(h('div.empty', { text: err.message }));
    }
  }

  function pager(total) {
    const from = total === 0 ? 0 : state.offset + 1;
    const to = Math.min(total, state.offset + state.limit);
    return h('div.row', { style: { padding: '10px 14px', borderTop: '1px solid var(--border)' } },
      h('span.small.muted', { text: `${from}–${to} of ${fmtNum(total)}` }),
      h('div.spacer', { style: { flex: 1 } }),
      h('button.sm', {
        disabled: state.offset === 0,
        onclick: () => { state.offset = Math.max(0, state.offset - state.limit); load(); },
      }, '← Newer'),
      h('button.sm', {
        disabled: to >= total,
        onclick: () => { state.offset += state.limit; load(); },
      }, 'Older →'),
    );
  }

  function renderTable(rows) {
    return table(
      [{ label: 'When' }, { label: 'Model' }, { label: 'Key' }, { label: 'In', num: true },
        { label: 'Out', num: true }, { label: 'TTFT', num: true }, { label: 'TPS', num: true },
        { label: 'Total', num: true }, { label: 'Status' }],
      rows,
      (r) => h('tr.clickable', { onclick: () => showDetail(r.id) },
        h('td.small', { text: fmtTime(r.ts) }),
        h('td.mono.small', {}, h('span.truncate', { text: r.public_model, title: `${r.public_model} → ${r.upstream_model}` })),
        h('td.small', { text: r.key_label || '—' }),
        h('td.num', { text: fmtNum(r.prompt_tokens) }),
        h('td.num', { text: fmtNum(r.completion_tokens) }),
        h('td.num', { text: r.ttft_ms ? fmtMs(r.ttft_ms) : '—' }),
        h('td.num', { text: r.tokens_per_sec ? r.tokens_per_sec.toFixed(1) : '—' }),
        h('td.num', { text: fmtMs(r.total_ms) }),
        h('td', {}, statusPill(r)),
      ),
    );
  }

  root.append(card('Requests', body, [modelFilter, statusFilter, search,
    h('button.sm', { onclick: load }, '↻')]));
  await load();
  return root;
}

function statusPill(r) {
  if (r.status === 0) return pill('failed', 'err');
  if (r.status >= 400) return pill(String(r.status), 'err');
  if (r.stream) return pill('stream', 'ok');
  return pill(String(r.status), 'ok');
}

async function showDetail(id) {
  let r;
  try {
    r = await api.requestDetail(id);
  } catch (err) {
    return toast(err.message, 'err');
  }

  return drawer(`Request ${r.id}`, () => h('div', {},
    h('div.row', { style: { marginBottom: '14px' } },
      statusPill(r),
      pill(r.stream ? 'streamed' : 'buffered'),
      pill(r.usage_source === 'upstream' ? 'usage from backend' : 'usage counted locally',
        r.usage_source === 'upstream' ? 'accent' : ''),
      r.exact ? null : pill('estimated tokens', 'warn'),
    ),

    kv('Time', fmtTime(r.ts)),
    kv('Public model', r.public_model, true),
    kv('Backend model', r.upstream_model, true),
    kv('Backend', r.backend_id),
    kv('Endpoint', r.endpoint),
    kv('Client key', r.key_label),
    kv('IP', r.ip),
    kv('User agent', r.user_agent),
    kv('Finish reason', r.finish_reason || '—'),
    r.retries ? kv('Retries', String(r.retries)) : null,

    h('h3', { style: { fontSize: '13px', marginTop: '18px' }, text: 'Timing' }),
    h('div.grid.stats', {},
      miniStat('TTFT', r.ttft_ms ? fmtMs(r.ttft_ms) : '—', 'to first token'),
      miniStat('Generation', r.gen_ms ? fmtMs(r.gen_ms) : '—', 'first to last token'),
      miniStat('Total', fmtMs(r.total_ms), 'end to end'),
      miniStat('Throughput', r.tokens_per_sec ? `${r.tokens_per_sec}` : '—', 'tokens / second'),
    ),

    h('h3', { style: { fontSize: '13px', marginTop: '18px' }, text: 'Tokens' }),
    h('div.grid.stats', {},
      miniStat('Input', fmtNum(r.prompt_tokens), r.cached_tokens ? `${fmtNum(r.cached_tokens)} cached` : ''),
      miniStat('Output', fmtNum(r.completion_tokens), r.reasoning_tokens ? `${fmtNum(r.reasoning_tokens)} reasoning` : ''),
      miniStat('Total', fmtNum(r.total_tokens), r.tokenizer),
    ),
    (r.drift_prompt || r.drift_completion)
      ? h('p.small.muted', {
        text: `Local tokenizer counted ${fmtNum(r.local_prompt)} in / ${fmtNum(r.local_completion)} out — `
          + `a drift of ${signed(r.drift_prompt)} / ${signed(r.drift_completion)} against the backend.`,
      })
      : null,

    r.error ? h('div', {}, h('h3', { style: { fontSize: '13px', marginTop: '18px' }, text: 'Error' }), h('pre.log', { text: r.error })) : null,
    r.req_preview ? h('div', {}, h('h3', { style: { fontSize: '13px', marginTop: '18px' }, text: 'Prompt' }), h('pre.log', { text: r.req_preview })) : null,
    r.res_preview ? h('div', {}, h('h3', { style: { fontSize: '13px', marginTop: '18px' }, text: 'Reply' }), h('pre.log', { text: r.res_preview })) : null,
  ), {
    extra: [h('button', { onclick: () => copy(JSON.stringify(r, null, 2), 'Record copied as JSON') }, 'Copy JSON')],
  });
}

function kv(label, value, mono = false) {
  return h('div.row', { style: { gap: '10px', padding: '3px 0' } },
    h('span.small.muted', { style: { minWidth: '120px' }, text: label }),
    h(mono ? 'span.mono.small' : 'span.small', { text: value || '—' }),
  );
}

function miniStat(label, value, sub) {
  return h('div.stat', {},
    h('div.label', { text: label }),
    h('div.value', { style: { fontSize: '17px' }, text: value }),
    sub ? h('div.sub', { text: sub }) : null,
  );
}

function signed(n) {
  const v = Number(n) || 0;
  return v > 0 ? `+${v}` : String(v);
}
