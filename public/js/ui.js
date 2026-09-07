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

export function fmtBytes(n) {
  const v = Number(n) || 0;
  if (v > 1048576) return `${(v / 1048576).toFixed(2)} MB`;
  if (v > 1024) return `${(v / 1024).toFixed(1)} kB`;
  return `${v} B`;
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
