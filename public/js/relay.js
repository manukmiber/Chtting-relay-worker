import { api } from './api.js';
import { h, card, clear, mount, toast } from './ui.js';

/**
 * Ending this process and coming back, from any screen that offers it.
 *
 * The two ways out are here together because they differ by one step and are
 * easy to confuse otherwise:
 *
 * * **Restart** re-executes the binary that is already on disk. Seconds.
 * * **Update & restart** runs `git pull`, builds what it pulled, and restarts
 *   into the result. Minutes, and the only one that changes what the relay
 *   actually does — the dashboard's own HTML and JavaScript are compiled into
 *   the binary, so a pull nobody built is a pull nobody is running.
 *
 * Both end with the page waiting for the relay to answer again and reloading
 * itself, because a restart is an exec: same port, same address, a gap in the
 * middle.
 */

/** Poll until the relay answers again, then reload. */
export async function waitForRelay(holder, { tries = 40, every = 750 } = {}) {
  for (let i = 0; i < tries; i += 1) {
    // eslint-disable-next-line no-await-in-loop
    await new Promise((r) => { setTimeout(r, every); });
    try {
      // eslint-disable-next-line no-await-in-loop
      await api.session();
      location.reload();
      return true;
    } catch {
      /* still down — that is what we are waiting for */
    }
  }
  if (holder) {
    mount(clear(holder), card('Still down', h('div', {},
      h('p.muted', { text: `The relay has not come back after ${Math.round((tries * every) / 1000)} seconds.` }),
      h('p.small.muted', {
        text: 'Check Termux, or start it from the chtting-relay-start shortcut.',
      }),
      h('button', { onclick: () => location.reload() }, 'Reload'),
    )));
  }
  return false;
}

/** Restart into the binary that is already built. */
export async function restartRelay(holder) {
  const res = await api.serviceAction('restart');
  toast(res.message, 'ok');
  if (holder) {
    mount(clear(holder), card('Restarting', h('p.muted', { text: 'Waiting for the relay to come back…' })));
  }
  await waitForRelay(holder);
}

/**
 * `git pull`, build, restart.
 *
 * The request itself answers immediately — a phone takes five to fifteen
 * minutes to build — so the work is followed through `GET /api/update`, whose
 * log is shown as it arrives. Two things end the wait: the relay stops
 * answering, which means it is restarting into the new binary, or the update
 * reports that it failed, which means the relay is still up and running the
 * code it started with.
 */
export async function updateRelay(holder) {
  const res = await api.serviceAction('update');
  toast(res.message, 'ok');

  const logBox = h('pre.log', { text: 'starting…' });
  const stepLine = h('p.small.muted', { text: 'pulling…' });
  if (holder) {
    mount(clear(holder), card('Updating', h('div', {},
      h('p.muted', {
        text: 'Pulling the latest commits and building them. On a phone this takes '
          + 'five to fifteen minutes; the relay keeps serving until the new binary is '
          + 'ready, and this page reloads itself when it restarts.',
      }),
      stepLine,
      logBox,
    )));
  }

  for (let i = 0; i < 1600; i += 1) {
    // eslint-disable-next-line no-await-in-loop
    await new Promise((r) => { setTimeout(r, 1500); });
    let status;
    try {
      // eslint-disable-next-line no-await-in-loop
      status = await api.update();
    } catch {
      // The relay stopped answering: it is restarting into what it just built.
      stepLine.textContent = 'restarting into the new build…';
      await waitForRelay(holder, { tries: 120 });
      return;
    }

    stepLine.textContent = describe(status);
    logBox.textContent = (status.logs ?? []).join('\n') || 'no output yet';
    logBox.scrollTop = logBox.scrollHeight;

    if (status.step === 'failed') {
      toast(status.error || 'Update failed', 'err');
      stepLine.textContent = `failed: ${status.error || 'see the log below'}`;
      if (holder) {
        holder.prepend(card('The relay is still running', h('p.small.muted', {
          text: 'Nothing was replaced: the update stops before touching the running '
            + 'relay if the pull or the build fails, so what is serving now is what '
            + 'was serving before.',
        })));
      }
      return;
    }
    if (status.step === 'restarting') {
      stepLine.textContent = 'built — restarting into it…';
      await waitForRelay(holder, { tries: 120 });
      return;
    }
  }
}

function describe(status) {
  const at = status.after && status.before && status.after !== status.before
    ? ` (${status.before} → ${status.after})`
    : '';
  switch (status.step) {
    case 'pulling': return `pulling${at}…`;
    case 'building': return `building${at}… this is the slow part`;
    case 'up-to-date': return 'already up to date — restarting anyway';
    case 'restarting': return `restarting${at}…`;
    default: return status.step ?? 'working…';
  }
}
