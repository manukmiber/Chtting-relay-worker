import { api } from '../api.js';
import {
  h, card, pill, toast, clear, copy, mount, confirmDialog,
} from '../ui.js';

/**
 * The first screen, and the one that replaces the Termux session.
 *
 * Everything the install used to print as a list of commands is a button here:
 * what is still missing, what supervises the relay, and how to start, stop or
 * restart it. The one thing no web page can do is start a relay that is not
 * running — hence the shortcuts and the boot hook, which start it without
 * typing either.
 */
export async function setupView(ctx) {
  const root = h('div');
  const body = h('div');
  root.append(body);

  const busy = async (button, fn) => {
    const label = button.textContent;
    button.disabled = true;
    button.textContent = 'Working…';
    try {
      await fn();
    } catch (err) {
      toast(err.message, 'err');
    } finally {
      button.disabled = false;
      button.textContent = label;
    }
  };

  /** A button that runs an action and repaints. */
  const action = (label, fn, kind = 'sm') => h(`button.${kind}`, {
    onclick: (ev) => busy(ev.currentTarget, async () => {
      await fn();
      await paint();
    }),
  }, label);

  async function paint() {
    const s = await api.setup();
    clear(body);

    /* ------------------------------------------------------ the checklist */
    const done = s.steps.filter((x) => x.ok).length;
    const list = h('div.checklist');
    for (const st of s.steps) {
      list.append(h('div.check', {},
        h('span.mark', { text: st.ok ? '✓' : '•', class: st.ok ? 'ok' : 'todo' }),
        h('div', {},
          h('div.check-label', { text: st.label }),
          h('div.small.muted', { text: st.detail }),
        ),
        h('div.spacer'),
        // A step whose fix is on this very screen needs no jump button.
        st.ok || !st.fix || st.fix === 'setup'
          ? null
          : h('button.sm', { onclick: () => ctx.go(st.fix) }, 'Fix'),
      ));
    }

    body.append(card(`Checklist — ${done} of ${s.steps.length} done`, h('div', {},
      list,
      s.problems?.length
        ? h('div', { style: { marginTop: '12px' } },
          h('p.small', { style: { color: 'var(--err)' }, text: 'Config problems:' }),
          h('ul.small.muted', {}, ...s.problems.map((p) => h('li', { text: p }))))
        : null,
    ), [h('button.ghost.sm', { onclick: () => paint() }, '↻')]));

    /* ------------------------------------------------------------ running */
    const svc = s.service;
    body.append(card('Running the relay', h('div', {},
      h('div.row', {},
        pill(svc.supervised ? 'supervised' : svc.installed ? 'service installed' : 'not supervised',
          svc.supervised ? 'ok' : svc.installed ? 'warn' : ''),
        pill(`v${s.version}`),
        s.termux ? pill('Termux') : pill('not Termux', 'warn'),
        s.wakeLock.held ? pill('wake lock held', 'ok') : null,
      ),
      h('p.small.muted', { style: { marginTop: '10px' }, text: svc.state }),

      /* ---- the buttons that end this process ---- */
      h('div.row', { style: { marginTop: '12px' } },
        h('button.primary.sm', {
          onclick: (ev) => busy(ev.currentTarget, async () => {
            const res = await api.serviceAction('restart');
            toast(res.message, 'ok');
            await waitForRelay();
          }),
        }, 'Restart relay'),
        h('button.sm', {
          onclick: (ev) => busy(ev.currentTarget, async () => {
            if (!confirmDialog(
              'Stop the relay? This page goes with it — you will need a home-screen '
              + 'shortcut or Termux to start it again.',
            )) return;
            const res = await api.serviceAction('stop');
            toast(res.message, 'ok');
            body.prepend(card('Stopped', h('p.muted', {
              text: 'The relay has shut down. Start it from the chtting-relay-start '
                + 'shortcut, or run: sv up chtting-relay',
            })));
          }),
        }, 'Stop relay'),
      ),
      h('p.small.muted', {
        text: 'Restarting keeps the same address, so this page reloads itself when the '
          + 'relay is back. Stopping cannot be undone from here: nothing served by the '
          + 'relay can start the relay.',
      }),

      /* ---- the supervisor ---- */
      h('hr'),
      h('h3.small', { text: 'Supervisor' }),
      h('p.small.muted', {
        text: 'With termux-services the relay is restarted whenever it dies and starts '
          + 'again with Termux, so a closed session no longer takes it down.',
      }),
      h('div.row', {},
        svc.installed
          ? action('Remove service', async () => {
            if (!confirmDialog('Remove the runit service?')) return;
            await api.serviceAction('uninstall');
            toast('Service removed', 'ok');
          })
          : action('Install service', async () => {
            const res = await api.serviceAction('install');
            toast(res.note ?? 'Service installed', 'ok');
          }, 'sm primary'),
        svc.installed && !svc.supervised && svc.svAvailable
          ? h('button.sm.primary', {
            onclick: (ev) => busy(ev.currentTarget, async () => {
              const res = await api.serviceAction('hand-over');
              toast(res.message, 'ok');
              await waitForRelay();
            }),
          }, 'Hand over to the service')
          : null,
      ),
      svc.path ? h('p.small.muted.mono.path', { text: svc.path }) : null,
      !svc.svAvailable
        ? h('p.small.muted', { text: 'The termux-services package is not installed yet — see Packages below.' })
        : null,
    )));

    /* ------------------------------------------- start without typing */
    const sc = s.shortcuts;
    const boot = s.boot;
    body.append(card('Starting it without Termux', h('div', {},
      h('div.row', {},
        pill(sc.installed ? 'shortcuts installed' : 'no shortcuts', sc.installed ? 'ok' : ''),
        pill(boot.installed ? 'starts on boot' : 'no boot hook', boot.installed ? 'ok' : ''),
      ),
      h('p.small.muted', { style: { marginTop: '10px' },
        text: 'Home-screen shortcuts give you Start, Stop, Restart and Open dashboard as '
          + 'taps. Add the Termux:Widget app from F-Droid, then long-press the home '
          + 'screen and place the widget.' }),
      h('div.row', {},
        sc.installed
          ? action('Remove shortcuts', async () => {
            await api.serviceAction('shortcuts-off');
            toast('Shortcuts removed', 'ok');
          })
          : action('Create shortcuts', async () => {
            await api.serviceAction('shortcuts-on');
            toast('Shortcuts written', 'ok');
          }, 'sm primary'),
      ),
      sc.path ? h('p.small.muted.mono.path', { text: sc.path }) : null,
      sc.files?.length
        ? h('div.row', {}, ...sc.files.map((f) => pill(f.name, f.installed ? 'ok' : '')))
        : null,

      h('hr'),
      h('h3.small', { text: 'Start when the phone boots' }),
      h('p.small.muted', {
        text: 'Needs the Termux:Boot app from F-Droid, opened once so Android grants it '
          + 'permission. After that the relay is up before you unlock the phone.',
      }),
      h('div.row', {},
        boot.installed
          ? action('Remove boot hook', async () => {
            await api.serviceAction('boot-off');
            toast('Boot hook removed', 'ok');
          })
          : action('Start on boot', async () => {
            await api.serviceAction('boot-on');
            toast('Boot hook written', 'ok');
          }, 'sm primary'),
      ),
      boot.path ? h('p.small.muted.mono.path', { text: boot.path }) : null,
    )));

    /* ---------------------------------------------------------- wake lock */
    body.append(card('Screen off', h('div', {},
      h('p.small.muted', {
        text: 'Android suspends Termux seconds after the screen goes off, which cuts every '
          + 'request in flight. The wake lock stops that; it costs battery and nothing else.',
      }),
      h('div.row', {},
        s.wakeLock.held
          ? action('Release wake lock', async () => {
            await api.wakeLock(false);
            toast('Wake lock released', 'ok');
          })
          : action('Hold wake lock', async () => {
            await api.wakeLock(true);
            toast('Wake lock held', 'ok');
          }, 'sm primary'),
        pill(s.wantsWakeLock ? 'taken at startup' : 'not taken at startup',
          s.wantsWakeLock ? 'ok' : ''),
      ),
      !s.wakeLock.supported
        ? h('p.small.muted', { text: 'termux-wake-lock is not on the path — this only works inside Termux.' })
        : null,
    )));

    /* ----------------------------------------------------------- packages */
    body.append(card('Packages', h('div', {},
      h('p.small.muted', {
        text: 'Installed with pkg, straight from here. Both are optional: the relay works '
          + 'on your LAN without either.',
      }),
      ...s.packages.map((p) => h('div.check', {},
        h('span.mark', { text: p.installed ? '✓' : '•', class: p.installed ? 'ok' : 'todo' }),
        h('div', {},
          h('div.check-label', { text: p.name }),
          h('div.small.muted', { text: p.why }),
        ),
        h('div.spacer'),
        p.installed
          ? pill('installed', 'ok')
          : action(`Install ${p.name}`, async () => {
            toast(`Installing ${p.name} — this can take a minute`, '');
            const res = await api.installPackage(p.name);
            toast(res.ok ? `${p.name} installed` : `${p.name} did not install`,
              res.ok ? 'ok' : 'err');
            if (!res.ok && res.output) {
              body.prepend(card(`pkg install ${p.name}`, h('pre.log', { text: res.output })));
            }
          }),
      )),
    )));

    /* -------------------------------------------------------------- paths */
    body.append(card('Where everything lives', h('div', {},
      ...[
        ['Binary', s.binary],
        ['Working directory', s.root],
        ['State', s.stateHome],
        ['Config', ctx.state.paths.config],
        ['Database', ctx.state.store.file],
        ['Dashboard', s.dashboardUrl],
      ].map(([label, value]) => h('div.pathrow', {},
        h('div.small.muted', { text: label }),
        h('div.row', {},
          h('code.small.mono.path', { text: value || '—' }),
          value ? h('button.ghost.sm', { onclick: () => copy(value, `${label} copied`) }, '⧉') : null,
        ),
      )),
    )));
  }

  /**
   * Poll until the relay answers again, then reload the page.
   *
   * A restart is an exec: same port, same address, a second or two of nothing.
   * Reloading is simpler than rebuilding the view against a state that changed
   * underneath it.
   */
  async function waitForRelay() {
    const notice = card('Restarting', h('p.muted', { text: 'Waiting for the relay to come back…' }));
    clear(body).append(notice);
    for (let i = 0; i < 40; i += 1) {
      // eslint-disable-next-line no-await-in-loop
      await new Promise((r) => { setTimeout(r, 750); });
      try {
        // eslint-disable-next-line no-await-in-loop
        await api.session();
        location.reload();
        return;
      } catch {
        /* still down — that is what we are waiting for */
      }
    }
    mount(clear(body), card('Still down', h('div', {},
      h('p.muted', { text: 'The relay has not come back after 30 seconds.' }),
      h('p.small.muted', {
        text: 'Check Termux, or start it from the chtting-relay-start shortcut.',
      }),
      h('button', { onclick: () => location.reload() }, 'Reload'),
    )));
  }

  await paint();
  return root;
}
