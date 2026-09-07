import { api } from '../api.js';
import {
  h, card, field, text, number, textarea, select, toast, toggle, parseList,
  fmtNum, fmtBytes, confirmDialog, copy,
} from '../ui.js';

/** Everything else: server, security, logging, tokenizer rules, maintenance. */
export async function settingsView(ctx) {
  const cfg = ctx.state.config;
  const i = {};
  const root = h('div');

  /* ------------------------------------------------------------ server */
  i.host = text(cfg.server.host, { class: 'mono' });
  i.port = number(cfg.server.port, { min: 1, max: 65535 });
  i.timezone = text(cfg.timezone, { placeholder: 'Asia/Jakarta' });
  i.maxBody = number(cfg.server.maxBodyBytes, { min: 65536, step: 1048576 });
  i.dashPort = number(cfg.dashboard.port, { min: 1, max: 65535 });
  i.dashHost = text(cfg.dashboard.host, { class: 'mono' });
  i.dashPassword = h('input', { type: 'password', value: '', placeholder: cfg.dashboard.password ? '•••••• (unchanged)' : 'no password set' });

  root.append(card('Server', h('div', {},
    h('div.grid.form', {},
      field('Relay bind address', i.host, '0.0.0.0 lets the tunnel and your LAN reach it'),
      field('Relay port', i.port),
      field('Timezone', i.timezone, 'used for the daily buckets'),
    ),
    h('div.grid.form', {},
      field('Max request body', i.maxBody, `currently ${fmtBytes(cfg.server.maxBodyBytes)}`),
      field('Dashboard bind address', i.dashHost, 'keep 127.0.0.1 so it stays off the tunnel'),
      field('Dashboard port', i.dashPort),
    ),
    field('Dashboard password', i.dashPassword, 'leave blank to keep the current one'),
    h('p.small.muted', { text: 'Port and bind changes take effect the next time the relay starts.' }),
  )));

  /* ---------------------------------------------------------- security */
  i.requireKey = h('input', { type: 'checkbox', checked: cfg.security.requireClientKey !== false });
  i.trustProxy = h('input', { type: 'checkbox', checked: cfg.security.trustProxyHeaders !== false });
  i.cors = text((cfg.security.corsOrigins ?? ['*']).join(', '), { class: 'mono' });
  i.blockedIps = text((cfg.security.blockedIps ?? []).join(', '), { class: 'mono' });

  root.append(card('Security', h('div', {},
    h('label.switch', { style: { marginBottom: '12px' } }, i.requireKey,
      h('span', { text: 'Require a client key on every request' })),
    h('label.switch', { style: { marginBottom: '12px' } }, i.trustProxy,
      h('span', { text: 'Trust CF-Connecting-IP / X-Forwarded-For (on behind the tunnel)' })),
    field('Allowed CORS origins', i.cors, '* allows browser clients from anywhere'),
    field('Blocked IPs', i.blockedIps),
  )));

  /* ----------------------------------------------------------- logging */
  i.level = select(cfg.logging.level, [['debug', 'debug'], ['info', 'info'], ['warn', 'warn'], ['error', 'error'], ['silent', 'silent']]);
  i.retention = number(cfg.logging.retentionDays, { min: 0 });
  i.storeBodies = select(cfg.logging.storeBodies, [
    ['none', 'none — record metrics only'],
    ['preview', 'preview — first N characters of prompt and reply'],
    ['full', 'full — the whole upstream request body'],
  ]);
  i.previewChars = number(cfg.logging.previewChars, { min: 0, max: 20000 });
  i.fileEnabled = h('input', { type: 'checkbox', checked: cfg.logging.fileEnabled !== false });

  root.append(card('Logging', h('div', {},
    h('div.grid.form', {},
      field('Log level', i.level),
      field('Keep request rows for (days)', i.retention, '0 = forever'),
      field('Preview length', i.previewChars, 'characters'),
    ),
    field('Store request bodies', i.storeBodies, 'prompts are stored on this device only'),
    h('label.switch', {}, i.fileEnabled, h('span', { text: 'Also write relay.log to disk' })),
  )));

  /* --------------------------------------------------------- tokenizer */
  i.fallback = text(cfg.tokenizer.fallback, { class: 'mono' });
  i.preferUpstream = h('input', { type: 'checkbox', checked: cfg.tokenizer.preferUpstreamUsage !== false });
  i.rules = textarea(JSON.stringify(cfg.tokenizer.rules ?? [], null, 1), { rows: 12 });

  root.append(card('Token counting', h('div', {},
    h('label.switch', { style: { marginBottom: '12px' } }, i.preferUpstream,
      h('span', { text: 'Trust the usage the backend reports, and keep the local count as a check' })),
    field('Fallback vocabulary', i.fallback, 'used when no rule matches'),
    field('Model → tokenizer rules', i.rules,
      'first match wins; each rule is {"match":"deepseek*","tokenizer":"deepseek","profile":"deepseek"}'),
    h('p.small.muted', {
      text: `Chat profiles available: ${(ctx.state.profiles ?? []).join(', ')}. `
        + 'The profile sets the per-message overhead of the backend\'s chat template.',
    }),
  )));

  /* ---------------------------------------------------------- defaults */
  const d = cfg.defaults ?? {};
  i.defReasoning = select(d.responseTransform?.reasoning ?? 'keep', [
    ['keep', 'keep'], ['strip', 'strip'], ['inline', 'inline'], ['field', 'field'],
  ]);
  i.defRenameModel = h('input', { type: 'checkbox', checked: d.responseTransform?.renameModel !== false });
  i.defStrip = text((d.responseTransform?.stripFields ?? []).join(', '), { class: 'mono' });
  i.defReplace = textarea(JSON.stringify(d.responseTransform?.replace ?? [], null, 1), { rows: 4 });

  root.append(card('Defaults for every model', h('div', {},
    h('p.small.muted', { text: 'A model alias inherits these and may override any of them.' }),
    h('label.switch', { style: { marginBottom: '12px' } }, i.defRenameModel,
      h('span', { text: 'Report the public alias as "model" in responses' })),
    field('Reasoning traces', i.defReasoning),
    field('Strip fields', i.defStrip),
    field('Rewrite reply text', i.defReplace),
  )));

  root.append(h('div.row.end', { style: { marginBottom: '20px' } },
    h('button.primary', {
      onclick: async () => {
        try {
          const patch = {
            timezone: i.timezone.value.trim() || 'UTC',
            server: {
              host: i.host.value.trim(),
              port: Number(i.port.value),
              maxBodyBytes: Number(i.maxBody.value),
            },
            dashboard: {
              host: i.dashHost.value.trim(),
              port: Number(i.dashPort.value),
              ...(i.dashPassword.value ? { password: i.dashPassword.value } : {}),
            },
            security: {
              requireClientKey: i.requireKey.checked,
              trustProxyHeaders: i.trustProxy.checked,
              corsOrigins: parseList(i.cors.value),
              blockedIps: parseList(i.blockedIps.value),
            },
            logging: {
              level: i.level.value,
              retentionDays: Number(i.retention.value),
              storeBodies: i.storeBodies.value,
              previewChars: Number(i.previewChars.value),
              fileEnabled: i.fileEnabled.checked,
            },
            tokenizer: {
              fallback: i.fallback.value.trim(),
              preferUpstreamUsage: i.preferUpstream.checked,
              rules: JSON.parse(i.rules.value || '[]'),
            },
            defaults: {
              responseTransform: {
                renameModel: i.defRenameModel.checked,
                reasoning: i.defReasoning.value,
                stripFields: parseList(i.defStrip.value),
                replace: JSON.parse(i.defReplace.value || '[]'),
              },
            },
          };
          await api.saveConfig(patch);
          toast('Settings saved', 'ok');
          await ctx.reload();
        } catch (err) {
          toast(err.message, 'err');
        }
      },
    }, 'Save settings'),
  ));

  /* -------------------------------------------------------- system info */
  const rt = ctx.state.runtime;
  root.append(card('System', h('div', {},
    h('div.grid.stats', {},
      h('div.stat', {}, h('div.label', { text: 'Node' }), h('div.value', { style: { fontSize: '16px' }, text: rt.node })),
      h('div.stat', {}, h('div.label', { text: 'Platform' }), h('div.value', { style: { fontSize: '16px' }, text: `${rt.platform}/${rt.arch}` }), rt.termux ? h('div.sub', { text: 'Termux detected' }) : null),
      h('div.stat', {}, h('div.label', { text: 'Memory' }), h('div.value', { style: { fontSize: '16px' }, text: `${rt.rss_mb} MB` })),
      h('div.stat', {}, h('div.label', { text: 'Uptime' }), h('div.value', { style: { fontSize: '16px' }, text: `${Math.floor(rt.uptime_s / 60)} min` })),
      h('div.stat', {}, h('div.label', { text: 'Store' }), h('div.value', { style: { fontSize: '16px' }, text: ctx.state.store.kind }), h('div.sub', { text: `${fmtNum(ctx.state.store.rows)} rows` })),
    ),
    h('div.row', { style: { marginTop: '12px' } },
      h('button.sm', {
        onclick: async () => {
          if (!confirmDialog(`Delete request rows older than ${cfg.logging.retentionDays} days?`)) return;
          const { removed } = await api.prune();
          toast(`Removed ${removed} rows`, 'ok');
          await ctx.reload();
        },
      }, 'Prune old requests'),
      h('button.sm', { onclick: () => copy(ctx.state.paths.config, 'Path copied') }, 'Copy config path'),
    ),
    h('p.small.muted.mono', { style: { marginTop: '10px' }, text: ctx.state.paths.config }),
  )));

  return root;
}
