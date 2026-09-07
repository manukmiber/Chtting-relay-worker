import { readFile } from 'node:fs/promises';
import { bpeCount, bpeSplit, makePairRanker } from './bpe.js';
import { byteLevelUnitsToBytes, fromByteLevelUnits, toByteLevelUnits } from './encodings.js';
import { buildPreTokenizer, escapeRe } from './pretokenizers.js';

/**
 * Loader for HuggingFace `tokenizer.json` files, which is how every open model
 * (DeepSeek, Qwen, Llama, Mistral, Gemma, GLM, Kimi...) ships its exact
 * tokenizer. Three model families are implemented:
 *
 *   BPE       - byte-level (Llama-3/Qwen/DeepSeek) and metaspace (Llama-2/Mistral)
 *   Unigram   - sentencepiece Viterbi (Gemma/T5-style)
 *   WordPiece - greedy longest-match (BERT-family embedding models)
 *
 * Added/special tokens are matched verbatim before pre-tokenization, exactly
 * like `tokenizers` does, so chat-template control tokens cost one token.
 */
export async function loadHfTokenizer(name, filePath) {
  const json = JSON.parse(await readFile(filePath, 'utf8'));
  return buildHfTokenizer(name, json);
}

export function buildHfTokenizer(name, json) {
  const model = json.model ?? {};
  const added = (json.added_tokens ?? []).map((t) => t.content).filter(Boolean);
  const addedRe = added.length
    ? new RegExp(`(${added.slice().sort((a, b) => b.length - a.length).map(escapeRe).join('|')})`)
    : null;
  const addedSet = new Set(added);
  const normalize = buildNormalizer(json.normalizer);
  const type = detectModelType(model);

  let core;
  if (type === 'BPE') core = buildBpe(model, json, normalize);
  else if (type === 'Unigram') core = buildUnigram(model, json, normalize);
  else if (type === 'WordPiece') core = buildWordPiece(model, json, normalize);
  else throw new Error(`unsupported HF tokenizer model type: ${type}`);

  const splitOnAdded = (text) => (addedRe ? text.split(addedRe).filter((s) => s !== '') : [text]);

  return {
    kind: `hf:${type.toLowerCase()}`,
    name,
    exact: true,
    vocabSize: core.vocabSize,
    specialTokens: added,

    count(text) {
      if (!text) return 0;
      let total = 0;
      for (const chunk of splitOnAdded(String(text))) {
        total += addedSet.has(chunk) ? 1 : core.count(chunk);
      }
      return total;
    },

    pieces(text) {
      const out = [];
      for (const chunk of splitOnAdded(String(text ?? ''))) {
        if (addedSet.has(chunk)) out.push({ text: chunk, id: -1, special: true });
        else out.push(...core.pieces(chunk));
      }
      return out;
    },
  };
}

/* ------------------------------------------------------------------ BPE -- */

function buildBpe(model, json, normalize) {
  const pairRanks = new Map();
  const merges = model.merges ?? [];
  for (let i = 0; i < merges.length; i++) {
    const m = merges[i];
    const key = Array.isArray(m) ? `${m[0]} ${m[1]}` : m;
    if (!pairRanks.has(key)) pairRanks.set(key, i);
  }
  const rankOf = makePairRanker(pairRanks);
  const vocab = model.vocab ?? {};
  const byteLevel = usesByteLevel(json);
  const preTokenize = buildPreTokenizer(json.pre_tokenizer);
  const cache = new Map();
  const alias = byteLevel ? null : new SymbolAliaser(Object.keys(vocab));
  // sentencepiece-style vocabs resolve out-of-vocabulary symbols to <0xXX>
  // byte tokens; without that flag they collapse into a single <unk> instead.
  const byteFallback = Boolean(model.byte_fallback);
  const fuseUnk = model.fuse_unk !== false;

  const toUnits = (piece) => (byteLevel ? toByteLevelUnits(piece) : alias.encode(piece));
  const fromUnits = (units) => (byteLevel ? fromByteLevelUnits(units) : alias.decode(units));

  // Byte-level vocabs cover all 256 bytes, so every symbol is in-vocabulary and
  // the unknown-token bookkeeping below can be skipped entirely.
  const fastPath = byteLevel;

  const symbolsOf = (piece) => {
    const units = toUnits(piece);
    return vocab[units] !== undefined ? [units] : bpeSplit(units, rankOf);
  };

  return {
    vocabSize: Object.keys(vocab).length,

    count(text) {
      let total = 0;
      for (const piece of preTokenize(normalize(text))) {
        if (fastPath) {
          const units = toUnits(piece);
          total += vocab[units] !== undefined ? 1 : bpeCount(units, rankOf, cache);
          continue;
        }
        let pendingUnk = false;
        for (const sym of symbolsOf(piece)) {
          if (vocab[sym] !== undefined) { total += 1; pendingUnk = false; continue; }
          if (byteFallback) {
            total += Buffer.byteLength(fromUnits(sym), 'utf8');
            pendingUnk = false;
          } else if (!fuseUnk || !pendingUnk) {
            total += 1;
            pendingUnk = true;
          }
        }
      }
      return total;
    },

    pieces(text) {
      const out = [];
      // A byte-level vocabulary can split one character across two tokens, so
      // its bytes go through a single streaming decoder rather than being
      // decoded per token (which would yield replacement characters).
      const decoder = byteLevel ? new TextDecoder('utf-8') : null;
      const show = (units) => (decoder
        ? decoder.decode(byteLevelUnitsToBytes(units), { stream: true })
        : fromUnits(units));
      for (const piece of preTokenize(normalize(text))) {
        let pendingUnk = false;
        for (const sym of symbolsOf(piece)) {
          const id = vocab[sym];
          if (id !== undefined) { out.push({ text: show(sym), id }); pendingUnk = false; continue; }
          const decoded = show(sym);
          if (!fastPath && byteFallback) {
            for (const b of Buffer.from(decoded, 'utf8')) {
              const tok = `<0x${b.toString(16).toUpperCase().padStart(2, '0')}>`;
              out.push({ text: tok, id: vocab[tok] ?? -1 });
            }
            pendingUnk = false;
          } else if (fastPath || !fuseUnk || !pendingUnk) {
            out.push({ text: decoded, id: -1 });
            pendingUnk = true;
          }
        }
      }
      return out;
    },
  };
}

function detectModelType(model) {
  if (model.type) return model.type;
  if (Array.isArray(model.vocab)) return 'Unigram';
  if (model.merges) return 'BPE';
  if (model.continuing_subword_prefix !== undefined || model.max_input_chars_per_word !== undefined) {
    return 'WordPiece';
  }
  return 'Unigram';
}

/* -------------------------------------------------------------- Unigram -- */

function buildUnigram(model, json, normalize) {
  // model.vocab is [[piece, logprob], ...]; Viterbi picks the highest-scoring
  // segmentation, which is what sentencepiece does at inference time.
  const scores = new Map();
  let maxLen = 1;
  for (const entry of model.vocab ?? []) {
    const [piece, score] = entry;
    if (!scores.has(piece)) scores.set(piece, score);
    if (piece.length > maxLen) maxLen = piece.length;
  }
  const unkScore = -1e4;
  const preTokenize = buildPreTokenizer(json.pre_tokenizer);

  const segment = (piece) => {
    const n = piece.length;
    if (!n) return [];
    const best = new Float64Array(n + 1).fill(-Infinity);
    const from = new Int32Array(n + 1).fill(-1);
    best[0] = 0;
    for (let i = 0; i < n; i++) {
      if (best[i] === -Infinity) continue;
      const limit = Math.min(n, i + maxLen);
      let matched = false;
      for (let j = i + 1; j <= limit; j++) {
        const sc = scores.get(piece.slice(i, j));
        if (sc === undefined) continue;
        matched = true;
        const cand = best[i] + sc;
        if (cand > best[j]) { best[j] = cand; from[j] = i; }
      }
      if (!matched) {
        const cand = best[i] + unkScore; // byte fallback / unk, still one token
        if (cand > best[i + 1]) { best[i + 1] = cand; from[i + 1] = i; }
      }
    }
    const out = [];
    for (let i = n; i > 0; i = from[i]) {
      if (from[i] < 0) break;
      out.push(piece.slice(from[i], i));
    }
    return out.reverse();
  };

  // sentencepiece fuses a run of unknown symbols into a single <unk>, unless a
  // byte fallback vocabulary is present to spell them out instead.
  const byteFallback = Boolean(model.byte_fallback);
  const fuseUnk = model.fuse_unk !== false;

  const run = (text) => {
    const out = [];
    for (const piece of preTokenize(normalize(text))) {
      let pendingUnk = false;
      for (const sym of segment(piece)) {
        if (scores.has(sym)) { out.push({ text: sym, unk: false }); pendingUnk = false; continue; }
        if (byteFallback) {
          for (const b of Buffer.from(sym, 'utf8')) {
            out.push({ text: `<0x${b.toString(16).toUpperCase().padStart(2, '0')}>`, unk: false });
          }
          pendingUnk = false;
        } else if (!fuseUnk || !pendingUnk) {
          out.push({ text: sym, unk: true });
          pendingUnk = true;
        }
      }
    }
    return out;
  };

  return {
    vocabSize: scores.size,
    count: (text) => (text ? run(text).length : 0),
    pieces: (text) => run(text).map((p) => ({
      text: p.text.split('\u2581').join(' '),
      id: p.unk ? -1 : 0,
    })),
  };
}

/* ------------------------------------------------------------ WordPiece -- */

function buildWordPiece(model, json, normalize) {
  const vocab = model.vocab ?? {};
  const prefix = model.continuing_subword_prefix ?? '##';
  const maxChars = model.max_input_chars_per_word ?? 100;
  const unk = model.unk_token ?? '[UNK]';
  const preTokenize = buildPreTokenizer(json.pre_tokenizer ?? { type: 'BertPreTokenizer' });

  const run = (text) => {
    const out = [];
    for (const word of preTokenize(normalize(text))) {
      if (word.length > maxChars) { out.push(unk); continue; }
      let start = 0;
      const sub = [];
      let ok = true;
      while (start < word.length) {
        let end = word.length;
        let found = null;
        while (start < end) {
          const cand = (start > 0 ? prefix : '') + word.slice(start, end);
          if (vocab[cand] !== undefined) { found = cand; break; }
          end -= 1;
        }
        if (found === null) { ok = false; break; }
        sub.push(found);
        start = end;
      }
      if (ok) out.push(...sub);
      else out.push(unk);
    }
    return out;
  };

  return {
    vocabSize: Object.keys(vocab).length,
    count: (text) => (text ? run(text).length : 0),
    pieces: (text) => run(text).map((p) => ({ text: p, id: vocab[p] ?? -1 })),
  };
}

/* ---------------------------------------------------------------- utils -- */

/**
 * The BPE core treats one UTF-16 code unit as one symbol. Metaspace vocabs can
 * contain astral characters (emoji), so those are aliased onto private-use
 * code points consistently across the vocab and the input text.
 */
class SymbolAliaser {
  constructor(vocabKeys) {
    this.map = new Map();
    this.rev = new Map();
    this.next = 0xe000;
    for (const key of vocabKeys) {
      for (const cp of key) if (cp.length > 1) this.alias(cp);
    }
  }

  alias(cp) {
    let a = this.map.get(cp);
    if (a === undefined) {
      a = String.fromCharCode(this.next++);
      this.map.set(cp, a);
      this.rev.set(a, cp);
    }
    return a;
  }

  encode(s) {
    if (!/[\uD800-\uDBFF]/.test(s)) return s;
    let out = '';
    for (const cp of s) out += cp.length > 1 ? this.alias(cp) : cp;
    return out;
  }

  decode(s) {
    let out = '';
    for (const ch of s) out += this.rev.get(ch) ?? ch;
    return out;
  }
}

function usesByteLevel(json) {
  return JSON.stringify(json.pre_tokenizer ?? {}).includes('ByteLevel')
    || JSON.stringify(json.decoder ?? {}).includes('ByteLevel');
}

function buildNormalizer(node) {
  const ops = [];
  const walk = (n) => {
    if (!n || typeof n !== 'object') return;
    switch (n.type) {
      case 'Replace': {
        const from = n.pattern?.String;
        if (typeof from === 'string') {
          const to = n.content ?? '';
          ops.push((s) => s.split(from).join(to));
        }
        break;
      }
      case 'Prepend':
        ops.push((s) => (s ? (n.prepend ?? '') + s : s));
        break;
      case 'NFKC': ops.push((s) => s.normalize('NFKC')); break;
      case 'NFC': ops.push((s) => s.normalize('NFC')); break;
      case 'NFD': ops.push((s) => s.normalize('NFD')); break;
      case 'NFKD': ops.push((s) => s.normalize('NFKD')); break;
      case 'Lowercase': ops.push((s) => s.toLowerCase()); break;
      case 'StripAccents':
        ops.push((s) => s.normalize('NFD').replace(/\p{M}/gu, ''));
        break;
      case 'Strip':
        ops.push((s) => (n.strip_left === false ? s : s.replace(/^\s+/, ''))
          .replace(n.strip_right === false ? /$^/ : /\s+$/, ''));
        break;
      case 'BertNormalizer': {
        const lower = n.lowercase !== false;
        const stripAccents = n.strip_accents ?? lower;
        const chineseChars = n.handle_chinese_chars !== false;
        ops.push((s) => {
          let out = s;
          if (n.clean_text !== false) {
            // drop nulls, replacement chars and control/format codepoints, then
            // fold every remaining whitespace character to a plain space
            out = Array.from(out)
              .filter((c) => c !== '\u0000' && c !== '\ufffd'
                && (/\s/u.test(c) || !/[\p{C}\p{Z}]/u.test(c)))
              .map((c) => (/\s/u.test(c) ? ' ' : c))
              .join('');
          }
          // BERT isolates every CJK ideograph so it becomes its own word
          if (chineseChars) out = out.replace(/[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]/g, (c) => ` ${c} `);
          if (lower) out = out.toLowerCase();
          if (stripAccents) out = out.normalize('NFD').replace(/\p{M}/gu, '');
          return out;
        });
        break;
      }
      case 'Precompiled':
        // sentencepiece ships a compiled charsmap blob; NFKC is its dominant
        // effect. Counts stay exact for ordinary text, but rare legacy
        // codepoints can differ by a token on Unigram vocabularies.
        ops.push((s) => s.normalize('NFKC'));
        break;
      default: break;
    }
    if (Array.isArray(n.normalizers)) n.normalizers.forEach(walk);
  };
  walk(node);
  if (!ops.length) return (s) => String(s ?? '');
  return (s) => ops.reduce((acc, f) => f(acc), String(s ?? ''));
}
