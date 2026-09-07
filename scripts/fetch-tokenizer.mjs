#!/usr/bin/env node
/**
 * Download tokenizer vocabularies into data/tokenizers/ so the relay can count
 * tokens exactly and then run fully offline.
 *
 *   node scripts/fetch-tokenizer.mjs cl100k_base o200k_base
 *   node scripts/fetch-tokenizer.mjs --hf deepseek-ai/DeepSeek-V3 --as deepseek
 *   node scripts/fetch-tokenizer.mjs --url https://host/tokenizer.json --as custom
 *   node scripts/fetch-tokenizer.mjs --list
 */
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { TIKTOKEN_SOURCES } from '../src/tokenizer/encodings.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const OUT_DIR = process.env.CHTTING_TOKENIZER_DIR || path.join(ROOT, 'data', 'tokenizers');

/** Popular open models and the repo that carries their tokenizer.json. */
const HF_PRESETS = {
  deepseek: 'deepseek-ai/DeepSeek-V3',
  'deepseek-r1': 'deepseek-ai/DeepSeek-R1',
  qwen: 'Qwen/Qwen2.5-7B-Instruct',
  qwen3: 'Qwen/Qwen3-8B',
  llama3: 'meta-llama/Meta-Llama-3-8B-Instruct',
  mistral: 'mistralai/Mistral-7B-Instruct-v0.3',
  gemma: 'google/gemma-2-9b-it',
  glm: 'THUDM/glm-4-9b-chat',
};

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--list') || argv.length === 0) return printHelp();

  await mkdir(OUT_DIR, { recursive: true });

  const asIdx = argv.indexOf('--as');
  const alias = asIdx >= 0 ? argv[asIdx + 1] : null;

  const urlIdx = argv.indexOf('--url');
  if (urlIdx >= 0) {
    const url = argv[urlIdx + 1];
    if (!url || !alias) throw new Error('--url requires --as <name>');
    return saveHf(alias, url);
  }

  const hfIdx = argv.indexOf('--hf');
  if (hfIdx >= 0) {
    const repo = argv[hfIdx + 1];
    if (!repo) throw new Error('--hf requires a repo id, e.g. deepseek-ai/DeepSeek-V3');
    const name = alias ?? repo.split('/').pop().toLowerCase();
    return saveHf(name, hfUrl(repo));
  }

  const names = argv.filter((a) => !a.startsWith('--') && a !== alias);
  for (const name of names) {
    if (TIKTOKEN_SOURCES[name]) await saveTiktoken(name, TIKTOKEN_SOURCES[name]);
    else if (HF_PRESETS[name]) await saveHf(name, hfUrl(HF_PRESETS[name]));
    else console.error(`unknown tokenizer "${name}" - run with --list to see the options`);
  }
}

function hfUrl(repo) {
  const base = process.env.HF_ENDPOINT || 'https://huggingface.co';
  return `${base}/${repo}/resolve/main/tokenizer.json`;
}

async function download(url) {
  const headers = { 'user-agent': 'chtting-relay/1.0' };
  if (process.env.HF_TOKEN && url.includes('huggingface.co')) {
    headers.authorization = `Bearer ${process.env.HF_TOKEN}`;
  }
  const res = await fetch(url, { headers, redirect: 'follow' });
  if (!res.ok) {
    const hint = res.status === 401 || res.status === 403
      ? ' (gated repo - export HF_TOKEN=hf_... and retry)'
      : '';
    throw new Error(`GET ${url} -> ${res.status} ${res.statusText}${hint}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

async function saveTiktoken(name, url) {
  process.stdout.write(`fetching ${name} ... `);
  const buf = await download(url);
  const file = path.join(OUT_DIR, `${name}.tiktoken`);
  await writeFile(file, buf);
  console.log(`${(buf.length / 1024 / 1024).toFixed(2)} MB -> ${path.relative(ROOT, file)}`);
}

async function saveHf(name, url) {
  process.stdout.write(`fetching ${name} from ${url} ... `);
  const buf = await download(url);
  JSON.parse(buf.toString('utf8')); // fail fast on an HTML error page
  const file = path.join(OUT_DIR, `${name}.tokenizer.json`);
  await writeFile(file, buf);
  console.log(`${(buf.length / 1024 / 1024).toFixed(2)} MB -> ${path.relative(ROOT, file)}`);
}

function printHelp() {
  console.log(`Download tokenizer vocabularies into ${path.relative(ROOT, OUT_DIR)}/\n`);
  console.log('OpenAI rank files (exact for GPT / Claude-ish estimates):');
  for (const n of Object.keys(TIKTOKEN_SOURCES)) console.log(`  ${n}`);
  console.log('\nHuggingFace presets (exact for open models):');
  for (const [n, repo] of Object.entries(HF_PRESETS)) console.log(`  ${n.padEnd(12)} ${repo}`);
  console.log('\nExamples:');
  console.log('  node scripts/fetch-tokenizer.mjs cl100k_base o200k_base deepseek');
  console.log('  node scripts/fetch-tokenizer.mjs --hf Qwen/Qwen3-8B --as qwen3');
  console.log('  HF_TOKEN=hf_xxx node scripts/fetch-tokenizer.mjs llama3');
}

main().catch((err) => {
  console.error(`\nerror: ${err.message}`);
  process.exit(1);
});
