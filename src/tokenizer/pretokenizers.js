/**
 * HuggingFace pre-tokenizer pipeline.
 *
 * `tokenizer.json` describes pre-tokenization as an ordered sequence of rules,
 * each of which further subdivides the pieces produced by the previous one.
 * DeepSeek, for instance, chains three Split rules (digits, CJK, then a
 * GPT-style word pattern) before ByteLevel. Honouring the whole chain — not
 * just the first rule — is what makes the counts match the reference library.
 */

const GPT2_SPLIT = /'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+/gu;

/** Build `(text) => string[]` from a pre_tokenizer node. */
export function buildPreTokenizer(node) {
  const stages = [];
  collect(node, stages);
  if (!stages.length) stages.push((pieces) => pieces);
  return (text) => stages.reduce((pieces, stage) => stage(pieces), [text]).filter((p) => p !== '');
}

function collect(node, stages) {
  if (!node || typeof node !== 'object') return;
  switch (node.type) {
    case 'Sequence':
      for (const child of node.pretokenizers ?? []) collect(child, stages);
      return;
    case 'Split': {
      const re = compileRegex(node.pattern);
      if (!re) return;
      const behavior = node.behavior ?? 'Isolated';
      const invert = Boolean(node.invert);
      stages.push(perPiece((p) => splitWithBehavior(p, re, behavior, invert)));
      return;
    }
    case 'ByteLevel': {
      if (node.add_prefix_space) {
        stages.push(perPiece((p) => [p && !p.startsWith(' ') ? ` ${p}` : p]));
      }
      if (node.use_regex !== false) {
        stages.push(perPiece((p) => splitWithBehavior(p, GPT2_SPLIT, 'Isolated', false)));
      }
      return;
    }
    case 'Metaspace': {
      const replacement = node.replacement ?? '\u2581';
      const scheme = node.prepend_scheme ?? (node.add_prefix_space === false ? 'never' : 'always');
      stages.push(perPiece((p) => {
        let s = p.split(' ').join(replacement);
        if (scheme !== 'never' && s && !s.startsWith(replacement)) s = replacement + s;
        return [s];
      }));
      if (node.split !== false) {
        // sentencepiece keeps the marker attached to the word that follows it
        const re = new RegExp(`${escapeRe(replacement)}?[^${escapeRe(replacement)}]+`, 'gu');
        stages.push(perPiece((p) => p.match(re) ?? []));
      }
      return;
    }
    case 'Whitespace':
      stages.push(perPiece((p) => p.match(/\w+|[^\w\s]+/gu) ?? []));
      return;
    case 'WhitespaceSplit':
      stages.push(perPiece((p) => p.split(/\s+/u)));
      return;
    case 'Punctuation':
      stages.push(perPiece((p) => splitWithBehavior(p, /\p{P}/gu, node.behavior ?? 'Isolated', false)));
      return;
    case 'Digits':
      stages.push(perPiece((p) => splitWithBehavior(
        p,
        node.individual_digits ? /\p{N}/gu : /\p{N}+/gu,
        'Isolated',
        false,
      )));
      return;
    case 'BertPreTokenizer':
      // BERT splits on whitespace, then isolates punctuation only. Symbols
      // (math, currency, emoji) stay glued to the word, as in the reference.
      stages.push(perPiece((p) => p.split(/\s+/u)));
      stages.push(perPiece((p) => splitWithBehavior(p, /[\p{P}!-/:-@[-`{-~]/gu, 'Isolated', false)));
      return;
    case 'CharDelimiterSplit':
      stages.push(perPiece((p) => p.split(node.delimiter ?? ' ')));
      return;
    case 'FixedLength': {
      const len = Math.max(1, Number(node.length) || 1);
      stages.push(perPiece((p) => p.match(new RegExp(`.{1,${len}}`, 'gsu')) ?? []));
      return;
    }
    default:
      // Unknown rule: leave pieces untouched rather than silently mangling them.
  }
}

function perPiece(fn) {
  return (pieces) => {
    const out = [];
    for (const p of pieces) {
      if (p === '') continue;
      for (const q of fn(p)) if (q !== '') out.push(q);
    }
    return out;
  };
}

/**
 * Split `text` at the matches of `re`, honouring the HF `behavior` flag.
 * With `invert`, the roles of match and gap are swapped.
 */
export function splitWithBehavior(text, re, behavior = 'Isolated', invert = false) {
  if (!text) return [];
  const spans = [];
  re.lastIndex = 0;
  let last = 0;
  let m;
  while ((m = re.exec(text)) !== null) {
    if (m[0] === '') { re.lastIndex += 1; continue; }
    if (m.index > last) spans.push({ text: text.slice(last, m.index), match: false });
    spans.push({ text: m[0], match: true });
    last = m.index + m[0].length;
  }
  if (last < text.length) spans.push({ text: text.slice(last), match: false });
  if (invert) for (const s of spans) s.match = !s.match;

  switch (behavior) {
    case 'Removed':
      return spans.filter((s) => !s.match).map((s) => s.text);
    case 'MergedWithPrevious': {
      const out = [];
      for (const s of spans) {
        if (s.match && out.length) out[out.length - 1] += s.text;
        else out.push(s.text);
      }
      return out;
    }
    case 'MergedWithNext': {
      const out = [];
      let pending = '';
      for (const s of spans) {
        if (s.match) pending += s.text;
        else { out.push(pending + s.text); pending = ''; }
      }
      if (pending) out.push(pending);
      return out;
    }
    case 'Contiguous': {
      const out = [];
      let prev = null;
      for (const s of spans) {
        if (prev !== null && prev === s.match) out[out.length - 1] += s.text;
        else out.push(s.text);
        prev = s.match;
      }
      return out;
    }
    case 'Isolated':
    default:
      return spans.map((s) => s.text);
  }
}

/** Port a Rust `regex` crate pattern to JS, or return null if impossible. */
export function compileRegex(pattern) {
  const src = typeof pattern === 'string'
    ? escapeRe(pattern)
    : pattern?.Regex ?? (typeof pattern?.String === 'string' ? escapeRe(pattern.String) : null);
  if (!src) return null;
  const converted = src
    .replace(/\(\?i:([^)]*)\)/g, (_, body) => `(?:${body.replace(/[A-Za-z]/g, (c) => `[${c.toLowerCase()}${c.toUpperCase()}]`)})`)
    .replace(/\(\?s:/g, '(?:')
    .replace(/\(\?m:/g, '(?:');
  for (const flags of ['gu', 'g']) {
    try {
      return new RegExp(converted, flags);
    } catch { /* try the next flag set */ }
  }
  return null;
}

export function escapeRe(s) {
  return String(s).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}
