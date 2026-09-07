/**
 * Script-aware estimator used only when no exact vocabulary is installed.
 * It is deliberately conservative (slightly over-counts rather than under),
 * and every count it produces is flagged `exact: false` so the dashboard can
 * mark the number as an estimate instead of quietly billing against it.
 */

const W_ASCII_LETTER = 0.25; // ~4 chars/token for latin prose
const W_DIGIT = 0.34; // \p{N}{1,3} => up to 3 digits per token
const W_SPACE = 0.06; // whitespace almost always merges into the next word
const W_PUNCT = 0.5;
const W_CJK = 1.0; // one token per han/kana/hangul char
const W_OTHER_BMP = 0.55;
const W_ASTRAL = 2.0; // emoji and friends

function isCjk(cp) {
  return (cp >= 0x3040 && cp <= 0x30ff) // kana
    || (cp >= 0x3400 && cp <= 0x4dbf)
    || (cp >= 0x4e00 && cp <= 0x9fff) // han
    || (cp >= 0xac00 && cp <= 0xd7af) // hangul
    || (cp >= 0xf900 && cp <= 0xfaff);
}

export function approxCount(text) {
  if (!text) return 0;
  let score = 0;
  for (const ch of String(text)) {
    const cp = ch.codePointAt(0);
    if (cp > 0xffff) score += W_ASTRAL;
    else if (cp < 128) {
      if ((cp >= 65 && cp <= 90) || (cp >= 97 && cp <= 122)) score += W_ASCII_LETTER;
      else if (cp >= 48 && cp <= 57) score += W_DIGIT;
      else if (cp === 32 || cp === 9 || cp === 10 || cp === 13) score += W_SPACE;
      else score += W_PUNCT;
    } else if (isCjk(cp)) score += W_CJK;
    else score += W_OTHER_BMP;
  }
  return Math.max(1, Math.ceil(score));
}

export function approxTokenizer(name = 'approx') {
  return {
    kind: 'approx',
    name,
    exact: false,
    vocabSize: 0,
    specialTokens: [],
    count: approxCount,
    pieces(text) {
      // Coarse preview only: chunk on the pre-tokenizer-ish boundaries.
      return (String(text).match(/\s*\S+|\s+/gu) ?? []).map((t) => ({ text: t, id: -1 }));
    },
  };
}
