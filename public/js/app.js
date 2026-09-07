import { api } from './api.js';
import { h, clear, toast, card, field, toggle } from './ui.js';
import { overviewView } from './views/overview.js';
import { modelsView } from './views/models.js';
import { backendsView } from './views/backends.js';
import { keysView } from './views/keys.js';
import { promptsView } from './views/prompts.js';
import { requestsView } from './views/requests.js';
import { tokenizerView } from './views/tokenizer.js';
import { tunnelView } from './views/tunnel.js';
import { playgroundView } from './views/playground.js';
import { settingsView } from './views/settings.js';
import { logsView } from './views/logs.js';

const TABS = [
  ['overview', 'Overview', overviewView],
  ['models', 'Models', modelsView],
  ['backends', 'Backends', backendsView],
  ['prompts', 'Prompts', promptsView],
  ['keys', 'Keys', keysView],
  ['requests', 'Requests', requestsView],
  ['tokenizer', 'Tokenizer', tokenizerView],
  ['playground', 'Playground', playgroundView],
  ['tunnel', 'Tunnel', tunnelView],
  ['settings', 'Settings', settingsView],
  ['logs', 'Logs', logsView],
];

const view = document.getElementById('view');
const tabsEl = document.getElementById('tabs');
const liveDot = document.getElementById('live-dot');
const relayPort = document.getElementById('relay-port');

/** Per-tab scratch space that survives re-renders but not reloads. */
const store = {};
let leaveHooks = [];
let state = null;
let current = location.hash.replace('#', '') || 'overview';

const ctx = {
  store,
  get state() { return state; },
  go(tab) {
    location.hash = tab;
  },
  onLeave(fn) {
    leaveHooks.push(fn);
  },
  async refreshState() {
    state = await api.state();
    paintHeader();
  },
  async reload() {
    await ctx.refreshState();
    await render();
  },
  rerender: () => render(),
};

/* ------------------------------------------------------------- chrome -- */

function paintHeader() {
  if (!state) return;
  liveDot.classList.toggle('live', Boolean(state.relay?.listening));
  const tunnelUrl = state.tunnel?.url;
  relayPort.textContent = tunnelUrl
    ? `:${state.relay.port} · tunnel up`
    : `:${state.relay.port}`;
  relayPort.title = tunnelUrl ? `Public URL: ${tunnelUrl}` : 'No tunnel running';
}

function paintTabs() {
  clear(tabsEl);
  for (const [id, label] of TABS) {
    tabsEl.append(h('button', {
      role: 'tab',
      'aria-selected': String(id === current),
      onclick: () => ctx.go(id),
    }, label));
  }
}

async function render() {
  for (const fn of leaveHooks) {
    try { fn(); } catch { /* a stale timer must not block navigation */ }
  }
  leaveHooks = [];

  paintTabs();
  const entry = TABS.find(([id]) => id === current) ?? TABS[0];
  clear(view).append(h('div.empty', { text: 'Loading…' }));

  try {
    const node = await entry[2](ctx);
    clear(view).append(node);
  } catch (err) {
    if (err.status === 401) return showLogin();
    clear(view).append(card('Something went wrong', h('div', {},
      h('pre.log', { text: err.stack ?? err.message }),
      h('button', { onclick: () => render() }, 'Retry'),
    )));
  }
  return undefined;
}

/* -------------------------------------------------------------- login -- */

function showLogin(message) {
  clear(tabsEl);
  const password = h('input', { type: 'password', placeholder: 'dashboard password', autofocus: true });
  const submit = async () => {
    try {
      await api.login(password.value);
      await boot();
    } catch (err) {
      toast(err.message, 'err');
    }
  };
  password.addEventListener('keydown', (e) => { if (e.key === 'Enter') submit(); });

  clear(view).append(h('div.login', {}, card('Sign in', h('div', {},
    message ? h('p.small.muted', { text: message }) : null,
    field('Password', password),
    h('button.primary', { onclick: submit, style: { width: '100%' } }, 'Sign in'),
  ))));
}

/* --------------------------------------------------------------- boot -- */

async function boot() {
  try {
    const session = await api.session();
    if (!session.authenticated) return showLogin();
    document.getElementById('logout').hidden = !session.passwordSet;
    state = await api.state();
    paintHeader();
    await render();
  } catch (err) {
    clear(view).append(card('Cannot reach the relay', h('div', {},
      h('p.muted', { text: err.message }),
      h('p.small.muted', { text: 'Is the relay still running in Termux? Start it with: npm start' }),
      h('button', { onclick: boot }, 'Retry'),
    )));
  }
  return undefined;
}

window.addEventListener('hashchange', () => {
  current = location.hash.replace('#', '') || 'overview';
  render();
});

document.getElementById('refresh').addEventListener('click', () => ctx.reload());
document.getElementById('logout').addEventListener('click', async () => {
  await api.logout();
  showLogin('Signed out.');
});

/* Theme: follow the system unless the user picks one. */
const THEME_KEY = 'chtting-theme';
const applyTheme = (value) => {
  document.documentElement.dataset.theme = value === 'system' ? '' : value;
};
applyTheme(localStorage.getItem(THEME_KEY) ?? 'system');
document.getElementById('theme-toggle').addEventListener('click', () => {
  const order = ['system', 'light', 'dark'];
  const next = order[(order.indexOf(localStorage.getItem(THEME_KEY) ?? 'system') + 1) % order.length];
  localStorage.setItem(THEME_KEY, next);
  applyTheme(next);
  toast(`Theme: ${next}`);
});

boot();
