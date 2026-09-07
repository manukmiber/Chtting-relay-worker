import { readdir, stat } from 'node:fs/promises';
import path from 'node:path';
import { existsSync } from 'node:fs';
import { loadTiktoken } from './tiktoken.js';
import { loadHfTokenizer } from './hf.js';
import { approxTokenizer } from './approx.js';
import { TIKTOKEN_SOURCES } from './encodings.js';

/**
 * Resolves a model name to the tokenizer that model actually uses, loading the
 * vocabulary from disk on first use and keeping it resident afterwards.
 *
 * Vocabularies live in data/tokenizers/:
 *   <name>.tiktoken       - OpenAI rank file
 *   <name>.tokenizer.json - HuggingFace tokenizer.json
 *
 * Nothing is required to be installed: a missing vocabulary degrades to the
 * estimator, and every count carries an `exact` flag so the difference is
 * visible rather than silent.
 */
export class TokenizerRegistry {
  constructor({ dir, rules = [], fallback = 'o200k_base', logger = console } = {}) {
    this.dir = dir;
    this.rules = rules;
    this.fallback = fallback;
    this.logger = logger;
    this.loaded = new Map(); // name -> tokenizer
    this.loading = new Map(); // name -> promise
    this.approx = approxTokenizer();
    this.misses = new Set();
  }

  setRules(rules, fallback) {
    this.rules = rules ?? this.rules;
    if (fallback) this.fallback = fallback;
  }

  /** Which tokenizer name and chat profile a model should use. */
  match(model) {
    const name = String(model ?? '');
    for (const rule of this.rules) {
      if (!rule?.match) continue;
      if (globMatch(name, rule.match)) {
        return {
          tokenizer: rule.tokenizer ?? this.fallback,
          profile: rule.profile ?? 'openai',
          profileOverride: rule.profileOverride ?? null,
        };
      }
    }
    return { tokenizer: this.fallback, profile: 'openai', profileOverride: null };
  }

  /** Tokenizer for a model, falling back to the estimator when uninstalled. */
  async forModel(model) {
    const m = this.match(model);
    const tokenizer = await this.get(m.tokenizer);
    return { ...m, tokenizerImpl: tokenizer };
  }

  async get(name) {
    if (!name) return this.approx;
    if (this.loaded.has(name)) return this.loaded.get(name);
    if (this.loading.has(name)) return this.loading.get(name);

    const p = this.#load(name).then((tok) => {
      this.loaded.set(name, tok);
      this.loading.delete(name);
      return tok;
    }).catch((err) => {
      this.loading.delete(name);
      if (!this.misses.has(name)) {
        this.misses.add(name);
        this.logger.warn?.(`tokenizer "${name}" unavailable (${err.message}); using estimator`);
      }
      this.loaded.set(name, this.approx);
      return this.approx;
    });
    this.loading.set(name, p);
    return p;
  }

  async #load(name) {
    const tiktokenPath = path.join(this.dir, `${name}.tiktoken`);
    if (existsSync(tiktokenPath)) return loadTiktoken(name, tiktokenPath);

    for (const candidate of [`${name}.tokenizer.json`, `${name}.json`, path.join(name, 'tokenizer.json')]) {
      const p = path.join(this.dir, candidate);
      if (existsSync(p)) return loadHfTokenizer(name, p);
    }
    throw new Error(`no vocabulary file found in ${this.dir}`);
  }

  /** Drop a cached vocabulary so a freshly downloaded file is picked up. */
  invalidate(name) {
    if (name) {
      this.loaded.delete(name);
      this.misses.delete(name);
    } else {
      this.loaded.clear();
      this.misses.clear();
    }
  }

  /** Everything installed on disk, plus the well-known downloadable names. */
  async inventory() {
    const installed = [];
    let entries = [];
    try {
      entries = await readdir(this.dir);
    } catch { /* directory may not exist yet */ }

    for (const entry of entries) {
      const full = path.join(this.dir, entry);
      let size = 0;
      try {
        size = (await stat(full)).size;
      } catch { continue; }
      if (entry.endsWith('.tiktoken')) {
        installed.push({ name: entry.slice(0, -'.tiktoken'.length), kind: 'tiktoken', size, file: entry });
      } else if (entry.endsWith('.tokenizer.json')) {
        installed.push({ name: entry.slice(0, -'.tokenizer.json'.length), kind: 'huggingface', size, file: entry });
      }
    }

    const available = Object.keys(TIKTOKEN_SOURCES).map((name) => ({
      name,
      kind: 'tiktoken',
      installed: installed.some((i) => i.name === name),
      url: TIKTOKEN_SOURCES[name],
    }));

    return {
      dir: this.dir,
      installed: installed.sort((a, b) => a.name.localeCompare(b.name)),
      available,
      loaded: [...this.loaded.entries()].map(([name, t]) => ({
        name, kind: t.kind, exact: t.exact !== false, vocabSize: t.vocabSize ?? 0,
      })),
    };
  }
}

/** `deepseek-*`, `*-chat`, `gpt-4o` style matching (case-insensitive). */
export function globMatch(value, pattern) {
  if (pattern === '*') return true;
  const re = new RegExp(`^${String(pattern)
    .replace(/[.+^${}()|[\]\\]/g, '\\$&')
    .replace(/\*/g, '.*')
    .replace(/\?/g, '.')}$`, 'i');
  return re.test(value);
}

/** Sensible defaults covering the model families a relay usually fronts. */
export const DEFAULT_TOKENIZER_RULES = [
  { match: 'gpt-4o*', tokenizer: 'o200k_base', profile: 'openai' },
  { match: 'gpt-5*', tokenizer: 'o200k_base', profile: 'openai' },
  { match: 'o1*', tokenizer: 'o200k_base', profile: 'openai' },
  { match: 'o3*', tokenizer: 'o200k_base', profile: 'openai' },
  { match: 'gpt-4*', tokenizer: 'cl100k_base', profile: 'openai' },
  { match: 'gpt-3.5*', tokenizer: 'cl100k_base', profile: 'openai' },
  { match: 'text-embedding-*', tokenizer: 'cl100k_base', profile: 'raw' },
  { match: 'deepseek*', tokenizer: 'deepseek', profile: 'deepseek' },
  { match: 'qwen*', tokenizer: 'qwen', profile: 'chatml' },
  { match: 'llama-3*', tokenizer: 'llama3', profile: 'llama3' },
  { match: 'mistral*', tokenizer: 'mistral', profile: 'mistral' },
  { match: 'gemma*', tokenizer: 'gemma', profile: 'gemma' },
  { match: 'claude*', tokenizer: 'cl100k_base', profile: 'openai' },
  { match: '*', tokenizer: 'o200k_base', profile: 'openai' },
];
