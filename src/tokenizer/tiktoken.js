import { readFile } from 'node:fs/promises';
import { bpeCount, bpeSplit, makeResultRanker } from './bpe.js';
import { PATTERNS, pretokenize, toLatin1Units } from './encodings.js';

/**
 * A tiktoken rank file: one `<base64 token bytes> <rank>` per line.
 * Loading yields an encoder that is byte-exact with OpenAI's tiktoken for
 * cl100k_base / o200k_base / p50k_base / r50k_base.
 */
export async function loadTiktoken(name, filePath) {
  const raw = await readFile(filePath, 'utf8');
  return parseTiktoken(name, raw);
}

export function parseTiktoken(name, raw) {
  const ranks = new Map();
  let vocabSize = 0;
  for (const line of raw.split('\n')) {
    if (!line) continue;
    const sp = line.indexOf(' ');
    if (sp < 0) continue;
    const token = Buffer.from(line.slice(0, sp), 'base64').toString('latin1');
    const rank = Number(line.slice(sp + 1));
    if (!Number.isFinite(rank)) continue;
    ranks.set(token, rank);
    if (rank + 1 > vocabSize) vocabSize = rank + 1;
  }
  if (!ranks.size) throw new Error(`tiktoken file for "${name}" contained no ranks`);

  const pattern = PATTERNS[name] ?? PATTERNS.cl100k_base;
  const rankOf = makeResultRanker(ranks);
  const cache = new Map();

  return {
    kind: 'tiktoken',
    name,
    vocabSize,
    exact: true,
    /** Special tokens are counted as a single token when present verbatim. */
    specialTokens: SPECIALS[name] ?? SPECIALS.cl100k_base,

    count(text) {
      if (!text) return 0;
      let total = 0;
      for (const piece of pretokenize(text, pattern)) {
        const units = toLatin1Units(piece);
        const whole = ranks.get(units);
        total += whole !== undefined ? 1 : bpeCount(units, rankOf, cache);
      }
      return total;
    },

    encode(text) {
      const ids = [];
      for (const piece of pretokenize(text, pattern)) {
        const units = toLatin1Units(piece);
        const whole = ranks.get(units);
        if (whole !== undefined) { ids.push(whole); continue; }
        for (const sym of bpeSplit(units, rankOf)) {
          const id = ranks.get(sym);
          if (id !== undefined) ids.push(id);
        }
      }
      return ids;
    },

    /** Human-readable pieces, for the dashboard's tokenizer playground. */
    pieces(text) {
      const out = [];
      // A single character can straddle two tokens, so bytes are fed through
      // one streaming decoder; the piece that completes a character shows it.
      const decoder = new TextDecoder('utf-8');
      for (const piece of pretokenize(text, pattern)) {
        const units = toLatin1Units(piece);
        const whole = ranks.get(units);
        const syms = whole !== undefined ? [units] : bpeSplit(units, rankOf);
        for (const sym of syms) {
          out.push({
            text: decoder.decode(Buffer.from(sym, 'latin1'), { stream: true }),
            id: ranks.get(sym) ?? -1,
          });
        }
      }
      return out;
    },
  };
}

const SPECIALS = {
  cl100k_base: ['<|endoftext|>', '<|fim_prefix|>', '<|fim_middle|>', '<|fim_suffix|>', '<|endofprompt|>'],
  o200k_base: ['<|endoftext|>', '<|endofprompt|>'],
  p50k_base: ['<|endoftext|>'],
  r50k_base: ['<|endoftext|>'],
};
