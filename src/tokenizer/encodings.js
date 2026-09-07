/**
 * Pre-tokenizer patterns and the byte alphabets used by the BPE core.
 *
 * JS RegExp has no inline `(?i:...)` group, so the case-insensitive
 * contraction alternation from the upstream patterns is expanded by hand.
 */

const CONTRACTIONS = "'[sS]|'[tT]|'[rR][eE]|'[vV][eE]|'[mM]|'[lL][lL]|'[dD]";

export const PATTERNS = {
  cl100k_base: new RegExp(
    `${CONTRACTIONS}`
    + '|[^\\r\\n\\p{L}\\p{N}]?\\p{L}+'
    + '|\\p{N}{1,3}'
    + '| ?[^\\s\\p{L}\\p{N}]+[\\r\\n]*'
    + '|\\s*[\\r\\n]+'
    + '|\\s+(?!\\S)'
    + '|\\s+',
    'gu',
  ),
  o200k_base: new RegExp(
    '[^\\r\\n\\p{L}\\p{N}]?[\\p{Lu}\\p{Lt}\\p{Lm}\\p{Lo}\\p{M}]*'
    + `[\\p{Ll}\\p{Lm}\\p{Lo}\\p{M}]+(?:${CONTRACTIONS})?`
    + '|[^\\r\\n\\p{L}\\p{N}]?[\\p{Lu}\\p{Lt}\\p{Lm}\\p{Lo}\\p{M}]+'
    + `[\\p{Ll}\\p{Lm}\\p{Lo}\\p{M}]*(?:${CONTRACTIONS})?`
    + '|\\p{N}{1,3}'
    + '| ?[^\\s\\p{L}\\p{N}]+[\\r\\n/]*'
    + '|\\s*[\\r\\n]+'
    + '|\\s+(?!\\S)'
    + '|\\s+',
    'gu',
  ),
  p50k_base: new RegExp(
    `${CONTRACTIONS}`
    + '| ?\\p{L}+'
    + '| ?\\p{N}+'
    + '| ?[^\\s\\p{L}\\p{N}]+'
    + '|\\s+(?!\\S)'
    + '|\\s+',
    'gu',
  ),
};
PATTERNS.r50k_base = PATTERNS.p50k_base;

/**
 * Downloadable rank files. `scripts/fetch-tokenizer.mjs` mirrors these into
 * data/tokenizers/ so Termux can run fully offline afterwards.
 */
export const TIKTOKEN_SOURCES = {
  cl100k_base: 'https://openaipublic.blob.core.windows.net/encodings/cl100k_base.tiktoken',
  o200k_base: 'https://openaipublic.blob.core.windows.net/encodings/o200k_base.tiktoken',
  p50k_base: 'https://openaipublic.blob.core.windows.net/encodings/p50k_base.tiktoken',
  r50k_base: 'https://openaipublic.blob.core.windows.net/encodings/r50k_base.tiktoken',
};

/**
 * GPT-2 byte <-> unicode alphabet. HuggingFace ByteLevel BPE stores its vocab
 * in this alphabet, so bytes must be mapped through it before merging.
 */
function buildByteAlphabet() {
  const bs = [];
  for (let i = 0x21; i <= 0x7e; i++) bs.push(i);
  for (let i = 0xa1; i <= 0xac; i++) bs.push(i);
  for (let i = 0xae; i <= 0xff; i++) bs.push(i);
  const cs = bs.slice();
  let n = 0;
  for (let b = 0; b < 256; b++) {
    if (!bs.includes(b)) {
      bs.push(b);
      cs.push(256 + n);
      n += 1;
    }
  }
  const byteToUnicode = new Array(256);
  const unicodeToByte = new Map();
  for (let i = 0; i < bs.length; i++) {
    byteToUnicode[bs[i]] = String.fromCodePoint(cs[i]);
    unicodeToByte.set(String.fromCodePoint(cs[i]), bs[i]);
  }
  return { byteToUnicode, unicodeToByte };
}

export const { byteToUnicode: BYTE_TO_UNICODE, unicodeToByte: UNICODE_TO_BYTE } = buildByteAlphabet();

/** UTF-8 bytes as a latin-1 unit string (1 char == 1 byte). */
export function toLatin1Units(text) {
  return Buffer.from(text, 'utf8').toString('latin1');
}

/** UTF-8 bytes as a GPT-2 byte-level unit string. */
export function toByteLevelUnits(text) {
  const bytes = Buffer.from(text, 'utf8');
  let out = '';
  for (let i = 0; i < bytes.length; i++) out += BYTE_TO_UNICODE[bytes[i]];
  return out;
}

/** Raw bytes behind a byte-level unit string. */
export function byteLevelUnitsToBytes(units) {
  const bytes = [];
  for (const ch of units) {
    const b = UNICODE_TO_BYTE.get(ch);
    bytes.push(b === undefined ? 0x3f : b);
  }
  return Buffer.from(bytes);
}

/** Inverse of toByteLevelUnits, for rendering token previews. */
export function fromByteLevelUnits(units) {
  return byteLevelUnitsToBytes(units).toString('utf8');
}

/** Split text with a pre-tokenizer pattern, always returning a fresh lastIndex. */
export function pretokenize(text, pattern) {
  pattern.lastIndex = 0;
  return text.match(pattern) ?? [];
}
