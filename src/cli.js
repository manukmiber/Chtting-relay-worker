#!/usr/bin/env node
import './util/quiet-warnings.js';
import { randomBytes } from 'node:crypto';
import { createApp, resolvePaths } from './index.js';
import { Config } from './config.js';
import { newId, maskSecret } from './util/misc.js';

const HELP = `chtting-relay - OpenAI-compatible LLM relay for Termux

usage: chtting <command> [options]

  start                 run the relay + dashboard (default)
  doctor                check the environment and configuration
  config path           print the config file location
  config show           print the config with secrets masked
  key new [label]       create a client API key and print it once
  key list              list client keys
  backend add           add an upstream backend via flags
  model add             add a model alias interactively via flags
  tokenizer list        show installed tokenizer vocabularies
  tunnel                start only the cloudflared tunnel

options:
  --port <n>            override server.port for this run
  --dashboard-port <n>  override dashboard.port for this run
  --no-dashboard        do not start the dashboard
  --config <file>       use a specific config file

backend add flags:
  --id <id>                 short id you reference from a model
  --name <label>
  --url <base url>          e.g. https://api.deepseek.com/v1
  --key <api key>

model add flags:
  --id <public name>        e.g. manukmiberai/creative-writer
  --backend <backend id>
  --upstream <real name>    e.g. Deepseek-v4-flash-0731
  --system <text>           system prompt to inject
  --tokenizer <name>        vocabulary to count with

examples:
  chtting backend add --id ds --name DeepSeek --url https://api.deepseek.com/v1 --key sk-...
  chtting model add --id manukmiberai/creative-writer --backend ds \
      --upstream deepseek-chat --tokenizer deepseek --system "You are Creative Writer."
`;

async function main() {
  const argv = process.argv.slice(2);
  const flags = parseFlags(argv);
  const [command = 'start', sub, ...rest] = argv.filter((a) => !a.startsWith('--') && !isFlagValue(argv, a));

  if (flags.help || command === 'help') return console.log(HELP);

  const paths = resolvePaths({ config: flags.config });

  switch (command) {
    case 'start':
      return startRelay(flags);
    case 'doctor':
      return doctor(paths);
    case 'config':
      return configCommand(sub, paths);
    case 'key':
      return keyCommand(sub, rest, paths);
    case 'backend':
      return backendCommand(sub, flags, paths);
    case 'model':
      return modelCommand(sub, flags, paths);
    case 'tokenizer':
      return tokenizerCommand(sub, paths);
    case 'tunnel':
      return tunnelCommand(flags);
    default:
      console.error(`unknown command "${command}"\n`);
      console.log(HELP);
      process.exitCode = 1;
      return undefined;
  }
}

async function startRelay(flags) {
  const app = await createApp({ config: flags.config });
  const patch = {};
  if (flags.port) patch.server = { port: Number(flags.port) };
  if (flags['dashboard-port']) patch.dashboard = { port: Number(flags['dashboard-port']) };
  if (flags['no-dashboard']) patch.dashboard = { ...(patch.dashboard ?? {}), enabled: false };
  if (Object.keys(patch).length) {
    // applied in memory only, so a one-off port override is not persisted
    Object.assign(app.config.data, mergeShallow(app.config.data, patch));
  }

  await app.start();
  banner(app);

  let shuttingDown = false;
  const shutdown = async (signal) => {
    if (shuttingDown) return;
    shuttingDown = true;
    app.logger.info(`${signal} received, shutting down`);
    await app.stop();
    process.exit(0);
  };
  process.on('SIGINT', () => shutdown('SIGINT'));
  process.on('SIGTERM', () => shutdown('SIGTERM'));
  process.on('unhandledRejection', (err) => app.logger.error('unhandled rejection:', err));
  return undefined;
}

function banner(app) {
  const c = app.config.get();
  const lines = [
    '',
    '  chtting-relay',
    `  relay      http://${c.server.host}:${c.server.port}/v1`,
    c.dashboard.enabled ? `  dashboard  http://127.0.0.1:${c.dashboard.port}` : '  dashboard  disabled',
    `  models     ${c.models.filter((m) => m.enabled).length} published, ${c.backends.filter((b) => b.enabled).length} backend(s)`,
    `  store      ${app.store.kind}`,
    '',
  ];
  if (!c.models.length) {
    lines.push('  no models yet - open the dashboard and add a backend, then a model alias', '');
  }
  if (c.security.requireClientKey && !c.keys.length) {
    lines.push('  no client keys yet - run: node src/cli.js key new "my phone"', '');
  }
  process.stdout.write(`${lines.join('\n')}\n`);
}

async function doctor(paths) {
  const checks = [];
  const node = process.versions.node.split('.').map(Number);
  checks.push(['node', `${process.version} (${process.arch})`, node[0] >= 20]);

  let sqlite = false;
  try {
    await import('node:sqlite');
    sqlite = true;
  } catch { /* falls back to JSONL */ }
  checks.push(['node:sqlite', sqlite ? 'available' : 'missing (JSONL fallback will be used)', true]);
  checks.push(['termux', process.env.PREFIX?.includes('com.termux') ? 'yes' : 'no', true]);

  const config = await Config.load(paths.config);
  const cfg = config.get();
  checks.push(['config', paths.config, true]);
  checks.push(['backends', String(cfg.backends.length), cfg.backends.length > 0]);
  checks.push(['models', String(cfg.models.length), cfg.models.length > 0]);
  checks.push(['client keys', String(cfg.keys.length), !cfg.security.requireClientKey || cfg.keys.length > 0]);

  const { TokenizerRegistry } = await import('./tokenizer/registry.js');
  const reg = new TokenizerRegistry({ dir: paths.tokenizers, rules: cfg.tokenizer.rules, fallback: cfg.tokenizer.fallback });
  const inv = await reg.inventory();
  checks.push(['tokenizers', inv.installed.length ? inv.installed.map((i) => i.name).join(', ') : 'none installed (estimates only)', inv.installed.length > 0]);

  const { TunnelManager } = await import('./tunnel/cloudflared.js');
  const tunnel = new TunnelManager({ config, logger: console });
  const v = await tunnel.version();
  checks.push(['cloudflared', v.installed ? v.version.split('\n')[0] : 'not installed (pkg install cloudflared)', v.installed]);

  process.stdout.write('\n');
  for (const [name, value, ok] of checks) {
    process.stdout.write(`  ${ok ? '✓' : '!'} ${name.padEnd(14)} ${value}\n`);
  }
  process.stdout.write('\n');
  return undefined;
}

async function configCommand(sub, paths) {
  if (sub === 'path') return console.log(paths.config);
  const config = await Config.load(paths.config);
  return console.log(JSON.stringify(config.redacted(), null, 2));
}

async function keyCommand(sub, rest, paths) {
  const config = await Config.load(paths.config);
  if (sub === 'list') {
    for (const k of config.get().keys) {
      console.log(`${k.enabled ? '●' : '○'} ${k.id}  ${maskSecret(k.key)}  ${k.label}`);
    }
    return undefined;
  }
  if (sub === 'new') {
    const key = `sk-relay-${randomBytes(24).toString('base64url')}`;
    await config.upsert('keys', {
      id: newId('key'),
      label: rest.join(' ') || 'cli key',
      key,
      enabled: true,
      models: ['*'],
    });
    console.log(`\n  ${key}\n\n  Saved. This is the only time it is shown in full.\n`);
    return undefined;
  }
  console.error('usage: chtting key <new|list> [label]');
  process.exitCode = 1;
  return undefined;
}

async function backendCommand(sub, flags, paths) {
  const config = await Config.load(paths.config);
  if (sub === 'list') {
    for (const b of config.get().backends) {
      console.log(`${b.enabled ? '●' : '○'} ${b.id.padEnd(12)} ${b.baseUrl}  ${maskSecret(b.apiKey)}`);
    }
    return undefined;
  }
  if (sub !== 'add') {
    console.error('usage: chtting backend <add|list> --id <id> --url <base url> [--key <api key>]');
    process.exitCode = 1;
    return undefined;
  }
  if (!flags.url) {
    console.error('backend add needs --url');
    process.exitCode = 1;
    return undefined;
  }
  const saved = await config.upsert('backends', {
    id: flags.id || newId('be'),
    name: flags.name ?? flags.id ?? 'backend',
    baseUrl: flags.url,
    apiKey: flags.key ?? '',
    enabled: true,
  });
  console.log(`added backend ${saved.id} -> ${saved.baseUrl}`);
  return undefined;
}

async function modelCommand(sub, flags, paths) {
  if (sub !== 'add') {
    console.error('usage: chtting model add --id <alias> --backend <id> --upstream <real name>');
    process.exitCode = 1;
    return undefined;
  }
  const config = await Config.load(paths.config);
  if (!flags.id || !flags.backend || !flags.upstream) {
    console.error('model add needs --id, --backend and --upstream');
    process.exitCode = 1;
    return undefined;
  }
  const saved = await config.upsert('models', {
    id: flags.id,
    backend: flags.backend,
    upstreamModel: flags.upstream,
    displayName: flags.name ?? flags.id,
    tokenizer: flags.tokenizer ?? '',
    enabled: true,
    systemPrompt: flags.system ? { mode: 'prepend', text: flags.system } : { mode: 'none', text: '' },
  });
  console.log(`added ${saved.id} -> ${saved.upstreamModel} via ${saved.backend}`);
  return undefined;
}

async function tokenizerCommand(sub, paths) {
  const { TokenizerRegistry } = await import('./tokenizer/registry.js');
  const config = await Config.load(paths.config);
  const reg = new TokenizerRegistry({
    dir: paths.tokenizers,
    rules: config.get().tokenizer.rules,
    fallback: config.get().tokenizer.fallback,
  });
  const inv = await reg.inventory();
  if (sub === 'list' || !sub) {
    console.log(`\ninstalled in ${inv.dir}:`);
    if (!inv.installed.length) console.log('  (none - run: npm run tokenizer:fetch cl100k_base o200k_base)');
    for (const t of inv.installed) {
      console.log(`  ${t.name.padEnd(18)} ${t.kind.padEnd(12)} ${(t.size / 1048576).toFixed(2)} MB`);
    }
    console.log('');
  }
  return undefined;
}

async function tunnelCommand(flags) {
  const { TunnelManager } = await import('./tunnel/cloudflared.js');
  const { Logger } = await import('./logger.js');
  const paths = resolvePaths({ config: flags.config });
  const config = await Config.load(paths.config);
  const logger = new Logger({ level: 'info', dir: paths.logs });
  const tunnel = new TunnelManager({ config, logger });
  const status = await tunnel.start();
  console.log(status.url ? `\n  ${status.url}\n` : '\n  tunnel starting; watch the log above\n');
  process.on('SIGINT', async () => {
    await tunnel.stop();
    process.exit(0);
  });
  return undefined;
}

/* ------------------------------------------------------------- parsing -- */

function parseFlags(argv) {
  const flags = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) continue;
    const name = a.slice(2);
    const next = argv[i + 1];
    if (next === undefined || next.startsWith('--')) flags[name] = true;
    else flags[name] = next;
  }
  return flags;
}

function isFlagValue(argv, token) {
  const idx = argv.indexOf(token);
  return idx > 0 && argv[idx - 1].startsWith('--');
}

function mergeShallow(base, patch) {
  const out = { ...base };
  for (const [k, v] of Object.entries(patch)) {
    out[k] = v && typeof v === 'object' && !Array.isArray(v) ? { ...base[k], ...v } : v;
  }
  return out;
}

main().catch((err) => {
  console.error(`\nerror: ${err.message}\n`);
  process.exit(1);
});
