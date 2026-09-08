import { api } from '../api.js';
import {
  h, card, field, text, number, textarea, select, toast, parseList,
  fmtNum, fmtMs, fmtBytes, confirmDialog, copy, stat,
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

  /* -------------------------------------------------------------- queue */
  // How many requests the phone runs at once, and what happens to the rest.
  const queue = await api.queue();
  i.maxConcurrent = number(queue.configured.maxConcurrentRequests, { min: 1, max: 4096 });
  i.queueCapacity = number(queue.configured.queueCapacity, { min: 0, max: 100000 });
  i.queueTimeout = number(queue.configured.queueTimeoutMs, { min: 100, step: 500 });

  const live = queue.live;
  const queueStats = h('div.grid.stats', {},
    stat('Running now', String(live.inFlight), `of ${live.limit} slots`),
    stat('Waiting', String(live.waiting), `peak ${live.peakWaiting}`),
    stat('Average wait', fmtMs(live.avgWaitMs), `${fmtNum(live.admittedAfterWait)} queued so far`),
    stat('Turned away', fmtNum(live.refusedQueueFull + live.refusedTimeout),
      `${fmtNum(live.refusedQueueFull)} full · ${fmtNum(live.refusedTimeout)} timed out`),
  );

  root.append(card('Concurrency and queue', h('div', {},
    queueStats,
    h('p.small.muted', {
      text: 'Requests over the limit wait in line rather than being refused. '
        + 'Only a full queue, or a wait past the deadline, gets a 503.',
    }),
    h('div.grid.form', {},
      field('Run at once', i.maxConcurrent, 'takes effect immediately'),
      field('Queue capacity', i.queueCapacity, 'how many may wait'),
      field('Give up after (ms)', i.queueTimeout, 'keep it under your client\'s own timeout'),
    ),
  ), [h('button.ghost.sm', { onclick: () => ctx.rerender() }, '↻ Refresh')]));

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
  i.billSystem = h('input', { type: 'checkbox', checked: cfg.tokenizer.billSystemPromptToUser === true });
  i.rules = textarea(JSON.stringify(cfg.tokenizer.rules ?? [], null, 1), { rows: 12 });

  root.append(card('Token counting', h('div', {},
    h('label.switch', { style: { marginBottom: '12px' } }, i.preferUpstream,
      h('span', { text: 'Trust the usage the backend reports, and keep the local count as a check' })),
    h('label.switch', { style: { marginBottom: '12px' } }, i.billSystem,
      h('span', { text: 'Charge callers for the system prompt this relay injects' })),
    h('p.small.muted', {
      text: 'Off by default: the caller did not write that prompt and cannot see it. '
        + 'Either way both numbers are recorded, so the difference stays visible in Usage.',
    }),
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

  /* -------------------------------------------------------- openrouter */
  // Everything OpenRouter reads about this relay. All of it is a commercial
  // decision, so all of it lives here rather than in the code.
  const or = cfg.openrouter ?? {};
  i.orEnabled = h('input', { type: 'checkbox', checked: or.enabled === true });
  i.orReady = h('input', { type: 'checkbox', checked: or.isReady !== false });
  i.orPath = text(or.path ?? '/provider/models', { class: 'mono' });
  i.orToken = h('input', { type: 'password', value: '', placeholder: or.token ? '•••••• (unchanged)' : 'no token — the listing is public' });
  i.orSlug = text(or.providerSlug ?? '', { class: 'mono', placeholder: 'chtting' });
  i.orRegion = text(or.deploymentRegion ?? '', { placeholder: 'ID', maxlength: 2 });
  i.orZdr = h('input', { type: 'checkbox', checked: or.compliance?.zdr === true });
  i.orHipaa = h('input', { type: 'checkbox', checked: or.compliance?.hipaa === true });
  i.orConcurrency = number(or.maxConcurrentRequests ?? 0, { min: 0 });
  i.orRpm = number(or.requestsPerMinute ?? 0, { min: 0 });
  i.orDatacenters = textarea(JSON.stringify(or.datacenters ?? [], null, 1), { rows: 4 });

  const preview = h('pre.log', { style: { maxHeight: '360px' }, hidden: true });
  root.append(card('OpenRouter', h('div', {},
    h('p.small.muted', {
      text: 'OpenRouter polls one URL to learn what this relay serves and what it costs. '
        + 'Per-model prices and limits live on each model, under its OpenRouter section.',
    }),
    h('label.switch', { style: { marginBottom: '12px' } }, i.orEnabled,
      h('span', { text: 'Publish the provider model document' })),
    h('label.switch', { style: { marginBottom: '12px' } }, i.orReady,
      h('span', { text: 'Ready for traffic — clear this to be listed without being routed to' })),
    h('div.grid.form', {},
      field('Listing path', i.orPath, 'a new path needs a restart; the default stays mounted'),
      field('Provider slug', i.orSlug, 'prefixes each model slug'),
      field('Deployment region', i.orRegion, 'ISO country code the traffic is served from'),
    ),
    field('Listing token', i.orToken, 'optional; leave blank to keep the current one'),
    h('div.grid.form', {},
      field('Concurrent requests', i.orConcurrency, '0 publishes the relay\'s own limit'),
      field('Requests per minute', i.orRpm, '0 = do not publish a limit'),
    ),
    field('Datacenters', i.orDatacenters, '[{"countryCode":"ID","region":"jakarta"}]'),
    h('div.row', { style: { marginBottom: '10px' } },
      h('label.switch', {}, i.orZdr, h('span', { text: 'Zero data retention' })),
      h('label.switch', {}, i.orHipaa, h('span', { text: 'HIPAA' })),
    ),
    h('p.small.muted', {
      text: 'Zero data retention is refused while request bodies are being stored — '
        + 'publishing it then would be a false claim.',
    }),
    h('div.row', {},
      h('button.sm', {
        onclick: async () => {
          try {
            const doc = await api.openrouterPreview();
            preview.textContent = JSON.stringify(doc.document, null, 2);
            preview.hidden = false;
            if (!doc.document.data?.length) {
              toast('No models are offered to OpenRouter yet — set one up under Models', 'err');
            }
          } catch (err) {
            toast(err.message, 'err');
          }
        },
      }, 'Preview what OpenRouter sees'),
      h('button.sm.ghost', { onclick: () => { preview.hidden = true; } }, 'Hide'),
    ),
    preview,
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
              maxConcurrentRequests: Number(i.maxConcurrent.value),
              queueCapacity: Number(i.queueCapacity.value),
              queueTimeoutMs: Number(i.queueTimeout.value),
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
              billSystemPromptToUser: i.billSystem.checked,
              rules: JSON.parse(i.rules.value || '[]'),
            },
            openrouter: {
              enabled: i.orEnabled.checked,
              isReady: i.orReady.checked,
              path: i.orPath.value.trim() || '/provider/models',
              providerSlug: i.orSlug.value.trim(),
              deploymentRegion: i.orRegion.value.trim().toUpperCase(),
              maxConcurrentRequests: Number(i.orConcurrency.value),
              requestsPerMinute: Number(i.orRpm.value),
              datacenters: JSON.parse(i.orDatacenters.value || '[]'),
              compliance: { zdr: i.orZdr.checked, hipaa: i.orHipaa.checked },
              ...(i.orToken.value ? { token: i.orToken.value } : {}),
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
      h('div.stat', {}, h('div.label', { text: 'Runtime' }), h('div.value', { style: { fontSize: '16px' }, text: `${rt.runtime} ${rt.version}` })),
      h('div.stat', {}, h('div.label', { text: 'Platform' }), h('div.value', { style: { fontSize: '16px' }, text: `${rt.platform}/${rt.arch}` }), rt.termux ? h('div.sub', { text: 'Termux detected' }) : null),
      h('div.stat', {}, h('div.label', { text: 'Workers' }), h('div.value', { style: { fontSize: '16px' }, text: rt.workers ? String(rt.workers) : `${rt.cores} (one per core)` })),
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
