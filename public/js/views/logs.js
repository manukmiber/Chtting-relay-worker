import { api } from '../api.js';
import { h, card, select, toast, clear } from '../ui.js';

/** Tail of relay.log, for when something failed and the row is not enough. */
export async function logsView(ctx) {
  const box = h('pre.log', { style: { maxHeight: '70vh' }, text: 'Loading…' });
  let lines = ctx.store.logLines ?? 300;
  let timer = null;

  async function load() {
    try {
      const raw = await api.logs(lines);
      const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
      box.textContent = raw || '(empty)';
      if (atBottom) box.scrollTop = box.scrollHeight;
    } catch (err) {
      box.textContent = err.message;
    }
  }

  const linesSelect = select(String(lines), [['100', '100 lines'], ['300', '300 lines'], ['1000', '1000 lines'], ['5000', '5000 lines']], {
    style: { width: 'auto' },
    onchange: (e) => { lines = Number(e.target.value); ctx.store.logLines = lines; load(); },
  });

  const follow = h('input', { type: 'checkbox', checked: ctx.store.followLogs !== false });
  follow.addEventListener('change', () => {
    ctx.store.followLogs = follow.checked;
    if (follow.checked) timer = setInterval(load, 3000);
    else clearInterval(timer);
  });

  const root = h('div', {}, card('Relay log', box, [
    h('label.switch', {}, follow, h('span', { text: 'Follow' })),
    linesSelect,
    h('button.ghost.sm', { onclick: load }, '↻'),
  ]));

  await load();
  if (follow.checked) timer = setInterval(load, 3000);
  ctx.onLeave(() => clearInterval(timer));
  return root;
}
