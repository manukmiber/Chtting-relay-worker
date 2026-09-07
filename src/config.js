import { readFile, writeFile, rename, mkdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { EventEmitter } from 'node:events';
import { newId, deepMerge, maskSecret, isPlainObject } from './util/misc.js';
import { DEFAULT_TOKENIZER_RULES } from './tokenizer/registry.js';

/**
 * The single source of truth for the relay. Everything the dashboard edits
 * lives here, is validated on write, and is saved atomically so a half-written
 * file can never brick a running relay on a phone that lost power mid-save.
 */

export const DEFAULT_CONFIG = {
  version: 1,
  timezone: 'Asia/Jakarta',

  server: {
    host: '0.0.0.0',
    port: 8787,
    requestTimeoutMs: 600000,
    maxBodyBytes: 20 * 1024 * 1024,
    keepAliveTimeoutMs: 75000,
  },

  dashboard: {
    enabled: true,
    host: '127.0.0.1', // never expose the dashboard through the tunnel
    port: 8788,
    password: '',
    sessionTtlMs: 7 * 24 * 60 * 60 * 1000,
  },

  security: {
    requireClientKey: true,
    corsOrigins: ['*'],
    trustProxyHeaders: true,
    blockedIps: [],
  },

  backends: [],
  models: [],
  keys: [],
  systemPrompts: [],

  defaults: {
    systemPrompt: { mode: 'none', text: '', promptId: '' },
    params: {},
    requestTransform: {
      dropParams: [],
      renameParams: {},
      replace: [],
      injectStop: [],
      forceStream: null,
    },
    responseTransform: {
      renameModel: true,
      reasoning: 'keep',
      reasoningTags: ['<think>', '</think>'],
      stripFields: [],
      replace: [],
      prefix: '',
      suffix: '',
      setFields: {},
    },
  },

  tokenizer: {
    fallback: 'o200k_base',
    preferUpstreamUsage: true,
    rules: DEFAULT_TOKENIZER_RULES,
    imageDefaults: { defaultDetail: 'auto', defaultWidth: 1024, defaultHeight: 1024 },
  },

  logging: {
    level: 'info',
    retentionDays: 30,
    storeBodies: 'preview', // 'none' | 'preview' | 'full'
    previewChars: 800,
    fileEnabled: true,
  },

  tunnel: {
    mode: 'quick', // 'quick' | 'named' | 'off'
    binary: 'cloudflared',
    autoStart: false,
    token: '',
    hostname: '',
    configFile: '',
    extraArgs: [],
  },
};

const SECRET_PATHS = new Set(['apiKey', 'key', 'password', 'token']);

export class Config extends EventEmitter {
  constructor(file, data) {
    super();
    this.file = file;
    this.data = data;
  }

  static async load(file) {
    let data = structuredClone(DEFAULT_CONFIG);
    if (existsSync(file)) {
      const raw = await readFile(file, 'utf8');
      try {
        data = normalize(deepMerge(data, JSON.parse(raw)));
      } catch (err) {
        throw new Error(`config at ${file} is not valid JSON: ${err.message}`);
      }
    } else {
      data = normalize(data);
      await mkdir(path.dirname(file), { recursive: true });
      await writeAtomic(file, data);
    }
    return new Config(file, data);
  }

  get() {
    return this.data;
  }

  /** Merge a patch, validate, persist, and notify listeners. */
  async update(patch) {
    const next = normalize(deepMerge(this.data, patch));
    const errors = validate(next);
    if (errors.length) throw Object.assign(new Error(errors[0]), { status: 400, errors });
    this.data = next;
    await writeAtomic(this.file, next);
    this.emit('change', next);
    return next;
  }

  /** Replace a whole collection (models / backends / keys / systemPrompts). */
  async replaceList(name, items) {
    if (!Array.isArray(items)) throw Object.assign(new Error(`${name} must be an array`), { status: 400 });
    return this.update({ [name]: items });
  }

  /** Insert or update one item of a collection by id. */
  async upsert(name, item) {
    const list = [...(this.data[name] ?? [])];
    const id = item.id ?? newId(name.slice(0, 3));
    const idx = list.findIndex((x) => x.id === id);
    const merged = idx >= 0 ? deepMerge(list[idx], { ...item, id }) : { ...item, id };
    if (idx >= 0) list[idx] = merged;
    else list.push(merged);
    await this.replaceList(name, list);
    return merged;
  }

  async remove(name, id) {
    const list = (this.data[name] ?? []).filter((x) => x.id !== id);
    return this.replaceList(name, list);
  }

  findModel(publicName) {
    const want = String(publicName ?? '').trim();
    if (!want) return null;
    return this.data.models.find((m) => m.id === want)
      ?? this.data.models.find((m) => m.aliases?.includes(want))
      ?? null;
  }

  findBackend(id) {
    return this.data.backends.find((b) => b.id === id) ?? null;
  }

  findKeyBySecret(secret) {
    if (!secret) return null;
    return this.data.keys.find((k) => k.key === secret) ?? null;
  }

  /** Config with every secret masked, for the dashboard. */
  redacted() {
    return redact(this.data);
  }
}

/* ------------------------------------------------------------ normalize -- */

function normalize(cfg) {
  const out = { ...cfg };
  out.backends = (cfg.backends ?? []).map((b) => ({
    id: b.id || newId('be'),
    name: b.name || b.id || 'backend',
    type: b.type || 'openai',
    baseUrl: stripTrailingSlash(b.baseUrl || ''),
    apiKey: b.apiKey ?? '',
    enabled: b.enabled !== false,
    timeoutMs: numOr(b.timeoutMs, 600000),
    headers: isPlainObject(b.headers) ? b.headers : {},
    maxRetries: numOr(b.maxRetries, 1),
    note: b.note ?? '',
  }));

  out.models = (cfg.models ?? []).map((m) => ({
    id: String(m.id ?? '').trim(),
    aliases: Array.isArray(m.aliases) ? m.aliases.filter(Boolean) : [],
    enabled: m.enabled !== false,
    displayName: m.displayName ?? m.id ?? '',
    description: m.description ?? '',
    backend: m.backend ?? '',
    upstreamModel: m.upstreamModel ?? '',
    fallbacks: Array.isArray(m.fallbacks) ? m.fallbacks : [],
    systemPrompt: {
      mode: m.systemPrompt?.mode ?? 'none',
      text: m.systemPrompt?.text ?? '',
      promptId: m.systemPrompt?.promptId ?? '',
    },
    params: isPlainObject(m.params) ? m.params : {},
    forceParams: isPlainObject(m.forceParams) ? m.forceParams : {},
    limits: {
      maxInputTokens: numOr(m.limits?.maxInputTokens, 0),
      maxOutputTokens: numOr(m.limits?.maxOutputTokens, 0),
    },
    tokenizer: m.tokenizer ?? '',
    chatProfile: m.chatProfile ?? '',
    requestTransform: isPlainObject(m.requestTransform) ? m.requestTransform : {},
    responseTransform: isPlainObject(m.responseTransform) ? m.responseTransform : {},
    contextLength: numOr(m.contextLength, 0),
    createdAt: m.createdAt ?? Date.now(),
  })).filter((m) => m.id);

  out.keys = (cfg.keys ?? []).map((k) => ({
    id: k.id || newId('key'),
    label: k.label ?? '',
    key: k.key ?? '',
    enabled: k.enabled !== false,
    models: Array.isArray(k.models) && k.models.length ? k.models : ['*'],
    quota: {
      requestsPerDay: numOr(k.quota?.requestsPerDay, 0),
      tokensPerDay: numOr(k.quota?.tokensPerDay, 0),
      requestsPerMinute: numOr(k.quota?.requestsPerMinute, 0),
    },
    note: k.note ?? '',
    createdAt: k.createdAt ?? Date.now(),
  }));

  out.systemPrompts = (cfg.systemPrompts ?? []).map((p) => ({
    id: p.id || newId('sp'),
    name: p.name ?? 'prompt',
    text: p.text ?? '',
    updatedAt: p.updatedAt ?? Date.now(),
  }));

  return out;
}

function validate(cfg) {
  const errors = [];
  const backendIds = new Set(cfg.backends.map((b) => b.id));

  for (const b of cfg.backends) {
    if (!b.baseUrl) errors.push(`backend "${b.name}" needs a baseUrl`);
    else if (!/^https?:\/\//i.test(b.baseUrl)) errors.push(`backend "${b.name}" baseUrl must start with http:// or https://`);
  }

  const seen = new Set();
  for (const m of cfg.models) {
    if (seen.has(m.id)) errors.push(`duplicate model id "${m.id}"`);
    seen.add(m.id);
    if (!m.upstreamModel) errors.push(`model "${m.id}" needs an upstreamModel (the name sent to the backend)`);
    if (!m.backend) errors.push(`model "${m.id}" needs a backend`);
    else if (!backendIds.has(m.backend)) errors.push(`model "${m.id}" points at unknown backend "${m.backend}"`);
    for (const fb of m.fallbacks) {
      if (!backendIds.has(fb)) errors.push(`model "${m.id}" has unknown fallback backend "${fb}"`);
    }
  }

  const keys = new Set();
  for (const k of cfg.keys) {
    if (!k.key) errors.push(`key "${k.label || k.id}" is empty`);
    else if (keys.has(k.key)) errors.push('two client keys share the same secret');
    keys.add(k.key);
  }

  // port 0 means "pick any free port", so two zeroes are not a conflict
  if (cfg.server.port && cfg.server.port === cfg.dashboard.port) {
    errors.push('server.port and dashboard.port must differ');
  }
  return errors;
}

/* ------------------------------------------------------------------ io -- */

async function writeAtomic(file, data) {
  await mkdir(path.dirname(file), { recursive: true });
  const tmp = `${file}.tmp`;
  await writeFile(tmp, `${JSON.stringify(data, null, 2)}\n`, { mode: 0o600 });
  await rename(tmp, file);
}

export function redact(value, key = '') {
  if (Array.isArray(value)) return value.map((v) => redact(v));
  if (isPlainObject(value)) {
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = redact(v, k);
    return out;
  }
  if (SECRET_PATHS.has(key) && typeof value === 'string' && value) return maskSecret(value);
  return value;
}

function stripTrailingSlash(s) {
  return String(s).replace(/\/+$/, '');
}

function numOr(v, fallback) {
  const n = Number(v);
  return Number.isFinite(n) ? n : fallback;
}
