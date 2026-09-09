import { api } from '../api.js';
import {
  h, card, pill, field, text, textarea, select, toggle, toast, clear, copy, fmtMs, mount,
} from '../ui.js';

/**
 * cloudflared control. The tunnel publishes the relay port only — this
 * dashboard stays bound to localhost and is never routed through it.
 */
export async function tunnelView(ctx) {
  const root = h('div');
  const statusBox = h('div');
  const logBox = h('pre.log', { text: '' });
  let timer = null;

  async function refresh() {
    let s;
    try {
      s = await api.tunnel();
    } catch (err) {
      clear(statusBox).append(h('div.empty', { text: err.message }));
      return;
    }

    const kind = s.state === 'running' ? 'ok' : s.state === 'failed' ? 'err' : s.state === 'starting' ? 'warn' : '';
    mount(clear(statusBox),
      h('div.row', {},
        pill(s.state, kind),
        pill(`mode: ${s.mode}`),
        s.pid ? pill(`pid ${s.pid}`) : null,
        s.uptime_s ? pill(`up ${formatUptime(s.uptime_s)}`) : null,
        s.restarts ? pill(`${s.restarts} restarts`, 'warn') : null,
        s.cloudflared?.installed ? pill(s.cloudflared.version.split(' ')[0] ?? 'cloudflared') : pill('cloudflared not installed', 'err'),
      ),
      s.url
        ? h('div', { style: { marginTop: '12px' } },
          h('div.row', {},
            h('a', { href: s.url, target: '_blank', rel: 'noreferrer', class: 'mono' }, s.url),
            h('button.ghost.sm', { onclick: () => copy(`${s.url}/v1`, 'Base URL copied') }, '⧉ copy /v1'),
          ),
          h('p.small.muted', {
            text: 'Point any OpenAI-compatible client at this URL with one of your client keys.',
          }))
        : null,
      s.lastError ? h('p.small', { style: { color: 'var(--err)' }, text: s.lastError } ) : null,
      !s.cloudflared?.installed
        ? h('div', { style: { marginTop: '10px' } },
          h('p.small.muted', { text: 'cloudflared has to be installed before a tunnel can start.' }),
          h('button.sm.primary', {
            onclick: async (ev) => {
              const btn = ev.currentTarget;
              btn.disabled = true;
              btn.textContent = 'Installing…';
              try {
                const res = await api.installPackage('cloudflared');
                toast(res.ok ? 'cloudflared installed' : 'Install failed', res.ok ? 'ok' : 'err');
                await refresh();
              } catch (err) {
                toast(err.message, 'err');
              } finally {
                btn.disabled = false;
                btn.textContent = 'Install cloudflared';
              }
            },
          }, 'Install cloudflared'))
        : null,
    );

    logBox.textContent = (s.logs ?? []).join('\n') || 'no output yet';
    logBox.scrollTop = logBox.scrollHeight;
  }

  const act = async (action) => {
    try {
      await api.tunnelAction(action);
      toast(`Tunnel ${action}ed`, 'ok');
      await refresh();
    } catch (err) {
      toast(err.message, 'err');
    }
  };

  root.append(card('Cloudflare tunnel', statusBox, [
    h('button.primary.sm', { onclick: () => act('start') }, 'Start'),
    h('button.sm', { onclick: () => act('restart') }, 'Restart'),
    h('button.sm', { onclick: () => act('stop') }, 'Stop'),
    h('button.ghost.sm', { onclick: refresh }, '↻'),
  ]));

  /* ------------------------------------------------------------ config */
  const t = ctx.state.config.tunnel ?? {};
  const i = {};
  i.mode = select(t.mode ?? 'quick', [
    ['quick', 'quick — free *.trycloudflare.com URL, no account'],
    ['named', 'named — your own hostname via a tunnel token'],
    ['off', 'off'],
  ]);
  i.token = h('input', { type: 'text', value: t.token ?? '', class: 'mono', placeholder: 'eyJhIjoi…' });
  i.hostname = text(t.hostname ?? '', { placeholder: 'api.example.com', class: 'mono' });
  i.configFile = text(t.configFile ?? '', { placeholder: '~/.cloudflared/config.yml', class: 'mono' });
  i.binary = text(t.binary ?? 'cloudflared', { class: 'mono' });
  i.autoStart = h('input', { type: 'checkbox', checked: Boolean(t.autoStart) });
  i.extraArgs = text((t.extraArgs ?? []).join(' '), { placeholder: '--loglevel info' });

  root.append(card('Tunnel settings', h('div', {},
    field('Mode', i.mode),
    field('Tunnel token', i.token, 'named mode: from the Cloudflare Zero Trust dashboard'),
    field('Hostname', i.hostname, 'informational; routing is configured in Cloudflare'),
    field('Config file', i.configFile, 'named mode alternative to a token'),
    h('div.grid.form', {},
      field('Binary', i.binary),
      field('Extra arguments', i.extraArgs),
    ),
    h('label.switch', { style: { marginBottom: '14px' } }, i.autoStart,
      h('span', { text: 'Start the tunnel with the relay, and restart it if it drops' })),
    h('div.row', {}, h('button.primary', {
      onclick: async () => {
        try {
          await api.saveConfig({
            tunnel: {
              mode: i.mode.value,
              token: i.token.value,
              hostname: i.hostname.value.trim(),
              configFile: i.configFile.value.trim(),
              binary: i.binary.value.trim() || 'cloudflared',
              autoStart: i.autoStart.checked,
              extraArgs: i.extraArgs.value.split(/\s+/).filter(Boolean),
            },
          });
          toast('Tunnel settings saved — restart the tunnel to apply', 'ok');
          await ctx.reload();
        } catch (err) {
          toast(err.message, 'err');
        }
      },
    }, 'Save')),
  )));

  root.append(card('cloudflared output', logBox));

  await refresh();
  timer = setInterval(refresh, 4000);
  ctx.onLeave(() => clearInterval(timer));
  return root;
}

function formatUptime(seconds) {
  const s = Number(seconds) || 0;
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}
