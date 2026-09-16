import { api } from '../api.js';
import {
  h, card, pill, field, text, select, toast, clear, copy, mount,
} from '../ui.js';

/**
 * cloudflared control.
 *
 * There are two ways to reach this panel from another device, and the first one
 * is usually the answer:
 *
 * * **On the relay tunnel**, at `<relay URL>/dashboard/`. One cloudflared for
 *   both — on a phone the second process is another 40 MB and another URL to
 *   keep track of. The cost is that the panel's sign-in page is on the URL you
 *   hand to callers, so it will not answer there without a 16-character
 *   password.
 * * **On a tunnel of its own**, which gives the panel a separate URL that can
 *   be stopped without taking the relay off the air. Also off until you turn it
 *   on, and it refuses to start without the same password.
 *
 * The two tunnels are separate cloudflared processes with separate settings and
 * separate URLs, and neither can publish the other's port.
 */
export async function tunnelView(ctx) {
  const root = h('div');
  const relay = section({
    title: 'Relay tunnel',
    read: () => api.tunnel(),
    act: (action) => api.tunnelAction(action),
    hint: 'Point any OpenAI-compatible client at this URL with one of your client keys.',
    suffix: '/v1',
    extra: (s) => panelOnRelay(ctx, s.dashboard ?? {}),
  });
  const dash = section({
    title: 'Dashboard tunnel (a second one, optional)',
    read: () => api.dashboardTunnel(),
    act: (action) => api.dashboardTunnelAction(action),
    hint: 'Open this on your other device and sign in with the dashboard password. '
      + 'Anyone who has this URL and the password has the whole panel.',
    suffix: '',
    dangerous: true,
    note: 'Only needed if you want the panel on a URL of its own. To use the tunnel that '
      + 'is already up, switch on "Open this panel on the relay tunnel" above instead — '
      + 'that runs one cloudflared rather than two.',
  });

  root.append(relay.card, dash.card);
  root.append(settingsCard(ctx, 'Relay tunnel settings', ctx.state.config.tunnel ?? {}, (tunnel) => ({ tunnel })));
  root.append(settingsCard(
    ctx,
    'Dashboard tunnel settings',
    ctx.state.config.dashboard?.tunnel ?? {},
    (tunnel) => ({ dashboard: { tunnel } }),
    'Off by default. It will not start until the dashboard password is at least 16 '
      + 'characters — behind a tunnel that password is the only thing between the '
      + 'internet and a panel that reveals every client key.',
  ));

  const refreshAll = () => Promise.all([relay.refresh(), dash.refresh()]);
  await refreshAll();
  const timer = setInterval(refreshAll, 4000);
  ctx.onLeave(() => clearInterval(timer));
  return root;
}

/* ---------------------------------------------------------- one tunnel -- */

function section({ title, read, act, hint, suffix, dangerous = false, note, extra }) {
  const statusBox = h('div');
  const logBox = h('pre.log', { text: '' });

  async function refresh() {
    let s;
    try {
      s = await read();
    } catch (err) {
      clear(statusBox).append(h('div.empty', { text: err.message }));
      return;
    }

    const kind = s.state === 'running' ? 'ok'
      : s.state === 'failed' ? 'err'
        : s.state === 'starting' ? 'warn' : '';

    mount(clear(statusBox),
      note ? h('p.small.muted', { style: { marginTop: '0' }, text: note }) : null,
      h('div.row', {},
        pill(s.state, kind),
        pill(`mode: ${s.mode}`),
        s.publishes ? pill(`port ${s.publishes}`) : null,
        s.pid ? pill(`pid ${s.pid}`) : null,
        s.uptime_s ? pill(`up ${formatUptime(s.uptime_s)}`) : null,
        s.restarts ? pill(`${s.restarts} restarts`, 'warn') : null,
        s.cloudflared?.installed
          ? pill(s.cloudflared.version.split(' ')[0] ?? 'cloudflared')
          : pill('cloudflared not installed', 'err'),
      ),

      // Why Start would refuse, said before anybody presses it.
      s.blocked
        ? h('div', { style: { marginTop: '10px' } },
          h('p.small', { style: { color: 'var(--err)' }, text: s.blocked }),
          typeof s.passwordLength === 'number'
            ? h('p.small.muted', {
              text: `The dashboard password is ${s.passwordLength} character(s); `
                + `${s.minPasswordLength} are needed. Set it under Settings → Server.`,
            })
            : null,
        )
        : null,

      s.url
        ? h('div', { style: { marginTop: '12px' } },
          h('div.row', {},
            h('a', { href: s.url, target: '_blank', rel: 'noreferrer', class: 'mono' }, s.url),
            h('button.ghost.sm', {
              onclick: () => copy(`${s.url}${suffix}`, 'URL copied'),
            }, suffix ? `⧉ copy ${suffix}` : '⧉ copy'),
          ),
          h('p.small.muted', { text: hint }),
          dangerous
            ? h('p.small', {
              style: { color: 'var(--warn)' },
              text: 'This URL is a capability. Anyone who learns it is one password away '
                + 'from your keys — stop the tunnel when you are done with it.',
            })
            : null,
        )
        : null,

      s.lastError ? h('p.small', { style: { color: 'var(--err)' }, text: s.lastError }) : null,

      extra ? extra(s) : null,

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

  const run = async (action) => {
    try {
      await act(action);
      toast(`Tunnel ${action}ed`, 'ok');
      await refresh();
    } catch (err) {
      toast(err.message, 'err');
    }
  };

  const box = card(title, h('div', {}, statusBox, h('details', { style: { marginTop: '12px' } },
    h('summary.small.muted', { text: 'cloudflared output' }),
    logBox,
  )), [
    h('button.primary.sm', { onclick: () => run('start') }, 'Start'),
    h('button.sm', { onclick: () => run('restart') }, 'Restart'),
    h('button.sm', { onclick: () => run('stop') }, 'Stop'),
    h('button.ghost.sm', { onclick: refresh }, '↻'),
  ]);

  return { card: box, refresh };
}

/* ------------------------------------------- the panel on this tunnel -- */

/**
 * Publish this panel under `/dashboard` on the relay tunnel.
 *
 * Inside the relay tunnel's card on purpose: it is not a second tunnel to start
 * and stop, it is one more path on the one already running. The switch takes
 * effect on the next request — nothing restarts — so the copy can promise that.
 */
function panelOnRelay(ctx, d) {
  const on = Boolean(d.published);
  const short = typeof d.passwordLength === 'number' && d.passwordLength < d.minPasswordLength;

  const box = h('div', { style: { marginTop: '14px', borderTop: '1px solid var(--border)', paddingTop: '12px' } });
  const toggle = h('input', { type: 'checkbox', checked: on, disabled: !on && short });

  toggle.addEventListener('change', async () => {
    toggle.disabled = true;
    try {
      await api.saveConfig({ dashboard: { publishOnRelay: toggle.checked } });
      toast(toggle.checked
        ? 'The panel now answers at /dashboard on the relay tunnel'
        : 'The panel is off the relay tunnel again', 'ok');
      await ctx.refreshState();
    } catch (err) {
      toast(err.message, 'err');
      toggle.checked = !toggle.checked;
    } finally {
      toggle.disabled = false;
    }
  });

  return mount(box,
    h('label.switch', {}, toggle, h('span', { text: 'Open this panel on the relay tunnel' })),
    h('p.small.muted', {
      text: 'One cloudflared for both: the panel answers under '
        + `${d.path ?? '/dashboard/'} on the URL above. Nothing restarts — it starts and stops answering `
        + 'with this switch.',
    }),
    // Said before the switch is flipped rather than after.
    d.blocked ? h('p.small', { style: { color: 'var(--err)' }, text: d.blocked }) : null,
    on && d.url
      ? h('div.row', {},
        h('a', { href: d.url, target: '_blank', rel: 'noreferrer', class: 'mono' }, d.url),
        h('button.ghost.sm', { onclick: () => copy(d.url, 'Panel URL copied') }, '⧉ copy'),
      )
      : null,
    on
      ? h('p.small', {
        style: { color: 'var(--warn)' },
        text: 'The relay URL is the one your callers have, and it now shows them a sign-in '
          + 'page. The password is what stands behind it — and the panel still refuses any '
          + 'other hostname, including this phone\'s Wi-Fi address.',
      })
      : null,
  );
}

/* -------------------------------------------------------- one settings -- */

function settingsCard(ctx, title, t, wrap, note) {
  const i = {};
  i.mode = select(t.mode ?? 'off', [
    ['quick', 'quick — free *.trycloudflare.com URL, no account'],
    ['named', 'named — your own hostname via a tunnel token'],
    ['off', 'off'],
  ]);
  i.token = h('input', { type: 'password', value: '', class: 'mono', placeholder: t.token ? '•••••• (unchanged)' : 'eyJhIjoi…' });
  i.hostname = text(t.hostname ?? '', { placeholder: 'api.example.com', class: 'mono' });
  i.configFile = text(t.configFile ?? '', { placeholder: '~/.cloudflared/config.yml', class: 'mono' });
  i.binary = text(t.binary ?? 'cloudflared', { class: 'mono' });
  i.autoStart = h('input', { type: 'checkbox', checked: Boolean(t.autoStart) });
  i.extraArgs = text((t.extraArgs ?? []).join(' '), { placeholder: '--loglevel info' });

  return card(title, h('div', {},
    note ? h('p.small.muted', { text: note }) : null,
    field('Mode', i.mode),
    field('Tunnel token', i.token, 'named mode: from the Cloudflare Zero Trust dashboard'),
    field('Hostname', i.hostname, 'named mode: also what the origin guard recognises as this panel'),
    field('Config file', i.configFile, 'named mode alternative to a token'),
    h('div.grid.form', {},
      field('Binary', i.binary),
      field('Extra arguments', i.extraArgs),
    ),
    h('label.switch', { style: { marginBottom: '14px' } }, i.autoStart,
      h('span', { text: 'Start this tunnel with the relay, and restart it if it drops' })),
    h('div.row', {}, h('button.primary', {
      onclick: async () => {
        try {
          await api.saveConfig(wrap({
            mode: i.mode.value,
            // Blank means "keep the one that is there"; sending an empty
            // string would silently delete a working tunnel's credentials.
            ...(i.token.value ? { token: i.token.value } : {}),
            hostname: i.hostname.value.trim(),
            configFile: i.configFile.value.trim(),
            binary: i.binary.value.trim() || 'cloudflared',
            autoStart: i.autoStart.checked,
            extraArgs: i.extraArgs.value.split(/\s+/).filter(Boolean),
          }));
          toast('Saved — restart the tunnel to apply', 'ok');
          await ctx.reload();
        } catch (err) {
          toast(err.message, 'err');
        }
      },
    }, 'Save')),
  ));
}

function formatUptime(seconds) {
  const s = Number(seconds) || 0;
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}
