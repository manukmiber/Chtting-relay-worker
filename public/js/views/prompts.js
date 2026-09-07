import { api } from '../api.js';
import {
  h, card, table, drawer, field, text, textarea, toast, confirmDialog, fmtTime, fmtNum,
} from '../ui.js';

/** A reusable library of system prompts that model aliases can point at. */
export async function promptsView(ctx) {
  const prompts = ctx.state.config.systemPrompts ?? [];
  const models = ctx.state.config.models ?? [];

  return h('div', {},
    card('System prompts', table(
      [{ label: 'Name' }, { label: 'Preview' }, { label: 'Chars', num: true }, { label: 'Used by', num: true }, { label: 'Updated' }],
      prompts,
      (p) => h('tr.clickable', { onclick: () => editPrompt(ctx, p) },
        h('td', { text: p.name }),
        h('td.small.muted', {}, h('span.truncate', { text: p.text.replace(/\s+/g, ' ').slice(0, 120) })),
        h('td.num', { text: fmtNum(p.text.length) }),
        h('td.num', { text: String(models.filter((m) => m.systemPrompt?.promptId === p.id).length) }),
        h('td.small.muted', { text: fmtTime(p.updatedAt) }),
      ),
    ), [h('button.primary.sm', { onclick: () => editPrompt(ctx, null) }, '+ New prompt')]),

    card('How injection works', h('div.small.muted', {}, h('p', {}, 'A model alias picks one of these prompts and a mode:'), h('ul', {},
      h('li', {}, h('b', {}, 'prepend'), ' — your prompt first, then whatever system message the caller sent.'),
      h('li', {}, h('b', {}, 'append'), ' — the caller\'s system message first, yours after it.'),
      h('li', {}, h('b', {}, 'replace'), ' — only yours; the caller\'s system message is dropped.'),
      h('li', {}, h('b', {}, 'merge'), ' — a single system message with yours on top.'),
    ))),
  );
}

function editPrompt(ctx, existing) {
  const isNew = !existing;
  const p = structuredClone(existing ?? { name: '', text: '' });
  const i = {};

  drawer(isNew ? 'New system prompt' : p.name, () => {
    i.name = text(p.name, { placeholder: 'Creative Writer persona' });
    i.text = textarea(p.text, { rows: 18, placeholder: 'You are Creative Writer…' });
    const counter = h('p.small.muted', { text: `${p.text.length} characters` });
    i.text.addEventListener('input', () => { counter.textContent = `${i.text.value.length} characters`; });
    return h('div', {}, field('Name', i.name), field('Prompt', i.text), counter);
  }, {
    saveLabel: isNew ? 'Create' : 'Save',
    extra: isNew ? [] : [h('button.danger', {
      onclick: async () => {
        const used = (ctx.state.config.models ?? []).filter((m) => m.systemPrompt?.promptId === p.id);
        const msg = used.length
          ? `${used.length} model(s) use this prompt and will fall back to their inline text. Delete anyway?`
          : `Delete prompt "${p.name}"?`;
        if (!confirmDialog(msg)) return;
        await api.remove('systemPrompts', p.id);
        toast('Prompt deleted', 'ok');
        await ctx.reload();
      },
    }, 'Delete')],
    onSave: async (close) => {
      try {
        await api.save('systemPrompts', {
          ...(isNew ? {} : { id: p.id }),
          name: i.name.value.trim() || 'prompt',
          text: i.text.value,
          updatedAt: Date.now(),
        });
        toast('Prompt saved', 'ok');
        close();
        await ctx.reload();
      } catch (err) {
        toast(err.message, 'err');
      }
    },
  });
}
