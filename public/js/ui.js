/** Tiny DOM helpers. No framework: the dashboard has to work offline. */

/** h('div.card', {onclick}, child, child) */
export function h(spec, props = {}, ...children) {
  const [tag, ...classes] = String(spec).split('.');
  const el = document.createElement(tag || 'div');
  if (classes.length) el.className = classes.join(' ');

  for (const [k, v] of Object.entries(props ?? {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.className = `${el.className} ${v}`.trim();
    else if (k === 'html') el.innerHTML = v;
    else if (k === 'text') el.textContent = v;
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2).toLowerCase(), v);
    else if (k === 'value' || k === 'checked' || k === 'disabled' || k === 'hidden' || k === 'selected') el[k] = v;
    else el.setAttribute(k, v === true ? '' : v);
  }

  for (const child of children.flat(4)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

export function clear(el) {
  while (el.firstChild) el.removeChild(el.firstChild);
  return el;
}

/**
 * Append children, skipping null/undefined/false.
 * `Element.append(null)` would render the literal text "null", so conditional
 * children must go through here rather than straight into append().
 */
export function mount(el, ...children) {
  for (const child of children.flat(4)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

export function toast(message, kind = '') {
  const root = document.getElementById('toasts');
  const el = h(`div.toast${kind ? `.${kind}` : ''}`, { text: message });
  root.append(el);
  setTimeout(() => {
    el.style.transition = 'opacity .3s';
    el.style.opacity = '0';
    setTimeout(() => el.remove(), 320);
  }, kind === 'err' ? 6000 : 3000);
}

/* -------------------------------------------------------------- inputs -- */

export function field(label, input, hint) {
  return h('label.field', {}, h('span', {}, label, hint ? h('span.hint', { text: ` — ${hint}` }) : null), input);
}

export function text(value, props = {}) {
  return h('input', { type: 'text', value: value ?? '', ...props });
}

export function number(value, props = {}) {
  return h('input', { type: 'number', value: value ?? 0, ...props });
}

export function textarea(value, props = {}) {
  return h('textarea', { ...props }, value ?? '');
}

export function select(value, options, props = {}) {
  const el = h('select', props);
  for (const opt of options) {
    const [val, label] = Array.isArray(opt) ? opt : [opt, opt];
    el.append(h('option', { value: val, selected: String(val) === String(value) }, label));
  }
  el.value = value ?? '';
  return el;
}

export function toggle(checked, label, props = {}) {
  return h('label.switch', {}, h('input', { type: 'checkbox', checked: Boolean(checked), ...props }), h('span', { text: label }));
}

/* -------------------------------------------------------------- layout -- */

export function card(title, body, actions = []) {
  const head = title || actions.length
    ? h('header', {}, h('h2', { text: title ?? '' }), h('div.spacer'), ...actions)
    : null;
  return h('section.card', {}, head, h('div.body', {}, body));
}

export function stat(label, value, sub) {
  return h('div.stat', {}, h('div.label', { text: label }), h('div.value', { text: value }), sub ? h('div.sub', { text: sub }) : null);
}

export function table(columns, rows, renderRow) {
  if (!rows.length) return h('div.empty', { text: 'Nothing here yet.' });
  const thead = h('thead', {}, h('tr', {}, ...columns.map((c) => h(
    c.num ? 'th.num' : 'th',
    { text: typeof c === 'string' ? c : c.label },
  ))));
  const tbody = h('tbody', {}, ...rows.map(renderRow));
  return h('div.table-wrap', {}, h('table', {}, thead, tbody));
}

export function pill(text2, kind = '') {
  return h(`span.pill${kind ? `.${kind}` : ''}`, { text: text2 });
}

/**
 * The three thinking bands a model is sold in, as one editable rate card.
 *
 * A price list is a table — a rate per band, read down a column — so this is a
 * table rather than a dozen loose number boxes. The standard row is the whole
 * card on its own: a band left blank charges the standard rate rather than
 * nothing, which is why the lower rows read as empty until somebody prices
 * them.
 *
 * `rates` is the pricing object being edited. Returns `{ el, inputs }`, where
 * `inputs` is keyed `standard|maxThinking|nonThinking` then `input|cached|output`.
 */
export function rateCard(rates = {}) {
  const bands = [
    ['standard', 'Standard', 'low, medium and high thinking', rates],
    ['maxThinking', 'Max thinking', 'reasoning_effort "max", or a budget that large', rates.maxThinking ?? {}],
    ['nonThinking', 'No thinking', 'thinking off, minimal — and callers who said nothing', rates.nonThinking ?? {}],
  ];

  const inputs = {};
  const totals = {};
  const box = (value) => number(value ?? 0, { min: 0, step: 0.01, class: 'rate' });

  // A million in and a million out, so the card can be checked at a glance
  // against the price list it is meant to reproduce.
  const retotal = () => {
    for (const [key] of bands) {
      const at = (name) => {
        const own = Number(inputs[key][name].value) || 0;
        return own > 0 ? own : Number(inputs.standard[name].value) || 0;
      };
      totals[key].textContent = `$${(at('input') + at('output')).toFixed(2)}`;
    }
  };

  const rows = bands.map(([key, label, hint, src]) => {
    inputs[key] = {
      input: box(src.inputUsdPerM),
      cached: box(src.cachedInputUsdPerM),
      output: box(src.outputUsdPerM),
    };
    for (const el of Object.values(inputs[key])) el.addEventListener('input', retotal);
    totals[key] = h('span.mono', { text: '$0.00' });
    return h('tr', {},
      h('td', {}, h('div', { text: label }), h('div.small.muted', { text: hint })),
      h('td', {}, inputs[key].input),
      h('td', {}, inputs[key].cached),
      h('td', {}, inputs[key].output),
      h('td.num', {}, totals[key]),
    );
  });

  const el = h('div.table-wrap.rate-card', {}, h('table', {},
    h('thead', {}, h('tr', {},
      h('th', { text: 'Thinking band' }),
      h('th', { text: 'Input' }),
      h('th', { text: 'Cache read' }),
      h('th', { text: 'Output' }),
      h('th.num', { text: '1M + 1M' }),
    )),
    h('tbody', {}, ...rows),
  ));
  retotal();
  return { el, inputs };
}

/** Read a rate card back out, in the shape the config stores it in. */
export function rateCardValues(inputs) {
  const band = (key) => ({
    inputUsdPerM: Number(inputs[key].input.value) || 0,
    cachedInputUsdPerM: Number(inputs[key].cached.value) || 0,
    outputUsdPerM: Number(inputs[key].output.value) || 0,
  });
  return {
    ...band('standard'),
    maxThinking: band('maxThinking'),
    nonThinking: band('nonThinking'),
  };
}

/** A right-hand editor panel. `render(close)` returns the body element. */
export function drawer(title, render, { onSave, saveLabel = 'Save', extra = [] } = {}) {
  const root = document.getElementById('drawer-root');
  const close = () => clear(root);

  const body = h('div.body');
  const footer = onSave
    ? h('footer', {}, ...extra, h('button.ghost', { onclick: close }, 'Cancel'), h('button.primary', {
      onclick: async (ev) => {
        const btn = ev.currentTarget;
        btn.disabled = true;
        try {
          await onSave(close);
        } finally {
          btn.disabled = false;
        }
      },
    }, saveLabel))
    : h('footer', {}, ...extra, h('button', { onclick: close }, 'Close'));

  const panel = h('div.drawer', {}, h('header', {}, h('h2', { text: title }), h('div.spacer'), h('button.ghost.sm', { onclick: close, title: 'Close' }, '✕')), body, footer);
  const bg = h('div.drawer-bg', {
    onclick: (ev) => { if (ev.target === bg) close(); },
  }, panel);

  body.append(render(close));
  clear(root).append(bg);
  return close;
}

export function confirmDialog(message) {
  // eslint-disable-next-line no-alert
  return window.confirm(message);
}

/* ------------------------------------------------------------ format -- */

export function fmtNum(n) {
  const v = Number(n) || 0;
  if (Math.abs(v) >= 1e9) return `${(v / 1e9).toFixed(2)}B`;
  if (Math.abs(v) >= 1e6) return `${(v / 1e6).toFixed(2)}M`;
  if (Math.abs(v) >= 1e4) return `${(v / 1e3).toFixed(1)}k`;
  return v.toLocaleString();
}

export function fmtMs(ms) {
  const v = Number(ms) || 0;
  if (!v) return '—';
  if (v < 1000) return `${Math.round(v)} ms`;
  return `${(v / 1000).toFixed(2)} s`;
}

export function fmtTime(ts) {
  if (!ts) return '—';
  const d = new Date(Number(ts));
  return d.toLocaleString(undefined, { month: 'short', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

/**
 * A USD amount, at the precision the number actually has.
 *
 * A relay's per-request prices are fractions of a cent, and rounding them to
 * two places would show every one of them as $0.00 — which reads as free. So
 * small amounts keep their digits and large ones do not carry pointless ones.
 */
export function fmtUsd(n) {
  const v = Number(n) || 0;
  if (v === 0) return '$0';
  const abs = Math.abs(v);
  if (abs >= 1) return `$${v.toFixed(2)}`;
  if (abs >= 0.01) return `$${v.toFixed(4)}`;
  return `$${v.toFixed(6)}`;
}

export function fmtBytes(n) {
  const v = Number(n) || 0;
  if (v > 1048576) return `${(v / 1048576).toFixed(2)} MB`;
  if (v > 1024) return `${(v / 1024).toFixed(1)} kB`;
  return `${v} B`;
}

/**
 * Re-run `tick` every `ms`, and stop when the view is left.
 *
 * Requirement 4: the live screens refresh themselves rather than waiting to be
 * asked. The timer is registered with the view's `onLeave` hook so navigating
 * away cancels it — otherwise every tab visited in a session would keep polling
 * in the background, which on a phone is battery spent on a screen nobody is
 * looking at.
 *
 * A tick that is still in flight is never overlapped, and one that throws is
 * swallowed: a blip must not leave the page with a stopped clock.
 */
export function live(ctx, ms, tick) {
  let running = false;
  let stopped = false;
  const timer = setInterval(async () => {
    if (running || stopped || document.hidden) return;
    running = true;
    try {
      await tick();
    } catch {
      /* a failed refresh is not worth a toast every few seconds */
    } finally {
      running = false;
    }
  }, ms);
  ctx.onLeave(() => {
    stopped = true;
    clearInterval(timer);
  });
  return () => {
    stopped = true;
    clearInterval(timer);
  };
}

export function copy(value, label = 'Copied') {
  const done = () => toast(label, 'ok');
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(value).then(done, () => fallbackCopy(value, done));
  } else {
    fallbackCopy(value, done);
  }
}

function fallbackCopy(value, done) {
  const ta = h('textarea', { style: { position: 'fixed', opacity: '0' } }, value);
  document.body.append(ta);
  ta.select();
  try {
    document.execCommand('copy');
    done();
  } catch {
    toast('Could not copy — select the text manually', 'err');
  }
  ta.remove();
}

/** Parse a textarea of `key=value` or JSON into an object. */
export function parseKeyValues(raw) {
  const trimmed = String(raw ?? '').trim();
  if (!trimmed) return {};
  if (trimmed.startsWith('{')) return JSON.parse(trimmed);
  const out = {};
  for (const line of trimmed.split('\n')) {
    const eq = line.indexOf('=');
    if (eq < 0) continue;
    const key = line.slice(0, eq).trim();
    const value = line.slice(eq + 1).trim();
    if (!key) continue;
    out[key] = /^-?\d+(\.\d+)?$/.test(value) ? Number(value)
      : value === 'true' ? true
        : value === 'false' ? false
          : value;
  }
  return out;
}

export function stringifyKeyValues(obj) {
  return Object.entries(obj ?? {}).map(([k, v]) => `${k}=${typeof v === 'object' ? JSON.stringify(v) : v}`).join('\n');
}

export function parseList(raw) {
  return String(raw ?? '').split(/[\n,]/).map((s) => s.trim()).filter(Boolean);
}

// One entry per line and nothing else, for values that may hold a comma of
// their own — a sentence, say.
export function parseLines(raw) {
  return String(raw ?? '').split('\n').map((s) => s.trim()).filter(Boolean);
}
