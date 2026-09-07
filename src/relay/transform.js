import { deepMerge, isPlainObject } from '../util/misc.js';

/**
 * Request and response reshaping.
 *
 * Requests: the public model name is swapped for the backend's real name, the
 * configured system prompt is injected, params are defaulted/forced, and text
 * rules may rewrite what the user sent.
 *
 * Responses: the backend's identity is scrubbed back to the public alias,
 * reasoning traces are kept/stripped/inlined, and text rules rewrite content.
 * The same rules run over streamed deltas so a stream and a buffered response
 * come out identically shaped.
 */

/* ---------------------------------------------------------- text rules -- */

/** Compile `[{pattern, flags, replacement, literal}]` into one function. */
export function compileTextRules(rules) {
  const compiled = [];
  for (const rule of rules ?? []) {
    if (!rule || !rule.pattern) continue;
    try {
      const flags = rule.literal ? 'g' : (rule.flags ?? 'g');
      const source = rule.literal ? escapeRe(rule.pattern) : rule.pattern;
      compiled.push({
        re: new RegExp(source, flags.includes('g') ? flags : `${flags}g`),
        replacement: rule.replacement ?? '',
      });
    } catch { /* an invalid rule must not take the relay down */ }
  }
  if (!compiled.length) return null;
  const apply = (text) => {
    let out = String(text ?? '');
    for (const { re, replacement } of compiled) {
      re.lastIndex = 0;
      out = out.replace(re, replacement);
    }
    return out;
  };
  // The streaming rewriter needs the raw patterns to find a safe cut point.
  apply.patterns = compiled.map((c) => c.re);
  return apply;
}

/** How far a streamed rewrite must look back to catch a straddling match. */
export function rulesLookbehind(rules) {
  let max = 16;
  for (const rule of rules ?? []) {
    const len = String(rule?.pattern ?? '').length + String(rule?.replacement ?? '').length;
    if (len > max) max = len;
  }
  return Math.min(512, max * 2);
}

function escapeRe(s) {
  return String(s).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/* ------------------------------------------------------------- request -- */

/**
 * Build the body actually sent upstream.
 * `route` is the resolved model route; `defaults` is config.defaults.
 */
export function transformRequest(body, route, defaults, promptLibrary = []) {
  const out = structuredClone(body ?? {});
  const rt = deepMerge(defaults.requestTransform ?? {}, route.requestTransform ?? {});

  // 5. name translation: what the caller asked for -> what the backend knows
  out.model = route.upstreamModel;

  // parameter defaults, then hard overrides that a caller cannot beat
  for (const [k, v] of Object.entries(route.params ?? {})) {
    if (out[k] === undefined) out[k] = v;
  }
  Object.assign(out, route.forceParams ?? {});

  for (const key of rt.dropParams ?? []) delete out[key];
  for (const [from, to] of Object.entries(rt.renameParams ?? {})) {
    if (out[from] !== undefined) { out[to] = out[from]; delete out[from]; }
  }

  if (Array.isArray(out.messages)) {
    out.messages = injectSystemPrompt(out.messages, route.systemPrompt, defaults.systemPrompt, promptLibrary);
    const rewrite = compileTextRules(rt.replace);
    if (rewrite) out.messages = out.messages.map((m) => rewriteMessage(m, rewrite));
  }

  if (Array.isArray(rt.injectStop) && rt.injectStop.length) {
    const existing = Array.isArray(out.stop) ? out.stop : (out.stop ? [out.stop] : []);
    out.stop = [...new Set([...existing, ...rt.injectStop])].slice(0, 4);
  }

  const maxOut = route.limits?.maxOutputTokens ?? 0;
  if (maxOut > 0) {
    const key = out.max_completion_tokens !== undefined ? 'max_completion_tokens' : 'max_tokens';
    out[key] = out[key] ? Math.min(Number(out[key]), maxOut) : maxOut;
  }

  return out;
}

/** Resolve the prompt text for a route, allowing a shared library entry. */
export function resolveSystemPrompt(spec, defaults, library) {
  const merged = spec && spec.mode && spec.mode !== 'inherit' ? spec : (defaults ?? {});
  const mode = merged.mode ?? 'none';
  if (mode === 'none') return { mode, text: '' };
  let text = merged.text ?? '';
  if (merged.promptId) {
    const entry = (library ?? []).find((p) => p.id === merged.promptId);
    if (entry) text = entry.text;
  }
  return { mode, text: String(text ?? '') };
}

/**
 * Inject the configured system prompt.
 *   prepend - our text first, then the caller's own system message
 *   append  - the caller's first, ours after
 *   replace - ours only; the caller's system message is dropped
 *   merge   - joined into a single system message
 */
export function injectSystemPrompt(messages, spec, defaults, library) {
  const { mode, text } = resolveSystemPrompt(spec, defaults, library);
  if (mode === 'none' || !text.trim()) return messages;

  const rest = messages.filter((m) => m?.role !== 'system');
  const theirs = messages.filter((m) => m?.role === 'system');
  const theirText = theirs.map((m) => flattenContent(m.content)).filter(Boolean).join('\n\n');

  switch (mode) {
    case 'replace':
      return [{ role: 'system', content: text }, ...rest];
    case 'append':
      return theirText
        ? [{ role: 'system', content: theirText }, { role: 'system', content: text }, ...rest]
        : [{ role: 'system', content: text }, ...rest];
    case 'merge':
      return [{ role: 'system', content: theirText ? `${text}\n\n${theirText}` : text }, ...rest];
    case 'prepend':
    default:
      return theirText
        ? [{ role: 'system', content: text }, { role: 'system', content: theirText }, ...rest]
        : [{ role: 'system', content: text }, ...rest];
  }
}

export function flattenContent(content) {
  if (content == null) return '';
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content.map((p) => (typeof p === 'string' ? p : (p?.text ?? ''))).join('');
  }
  return String(content);
}

function rewriteMessage(msg, rewrite) {
  if (!msg || msg.role === 'system') return msg;
  if (typeof msg.content === 'string') return { ...msg, content: rewrite(msg.content) };
  if (Array.isArray(msg.content)) {
    return {
      ...msg,
      content: msg.content.map((p) => (p?.type === 'text' ? { ...p, text: rewrite(p.text ?? '') } : p)),
    };
  }
  return msg;
}

/* ------------------------------------------------------------ response -- */

export function resolveResponseTransform(route, defaults) {
  return deepMerge(defaults.responseTransform ?? {}, route.responseTransform ?? {});
}

/**
 * Reshape a complete (non-streamed) chat completion.
 * `publicModel` is the alias the caller used, restored over the backend's name.
 */
export function transformResponse(body, { publicModel, transform }) {
  if (!isPlainObject(body)) return body;
  const out = structuredClone(body);
  const rewrite = compileTextRules(transform.replace);

  if (transform.renameModel !== false && publicModel) out.model = publicModel;

  for (const field of transform.stripFields ?? []) delete out[field];

  if (Array.isArray(out.choices)) {
    out.choices = out.choices.map((choice) => {
      const c = { ...choice };
      if (c.message) c.message = transformMessage(c.message, transform, rewrite);
      if (c.text !== undefined) c.text = applyText(c.text, transform, rewrite);
      return c;
    });
  }

  for (const [k, v] of Object.entries(transform.setFields ?? {})) out[k] = v;
  return out;
}

function transformMessage(message, transform, rewrite) {
  const m = { ...message };
  const reasoning = m.reasoning_content ?? m.reasoning ?? null;

  switch (transform.reasoning) {
    case 'strip':
      delete m.reasoning_content;
      delete m.reasoning;
      break;
    case 'inline': {
      const [open, close] = transform.reasoningTags ?? ['<think>', '</think>'];
      if (reasoning) m.content = `${open}${reasoning}${close}${m.content ?? ''}`;
      delete m.reasoning_content;
      delete m.reasoning;
      break;
    }
    case 'field':
      if (reasoning) m.reasoning = reasoning;
      delete m.reasoning_content;
      break;
    case 'keep':
    default:
      break;
  }

  if (typeof m.content === 'string') m.content = applyText(m.content, transform, rewrite);
  return m;
}

function applyText(text, transform, rewrite) {
  let out = String(text ?? '');
  if (rewrite) out = rewrite(out);
  if (transform.prefix) out = transform.prefix + out;
  if (transform.suffix) out += transform.suffix;
  return out;
}

/**
 * Reshape one streamed chunk. Text rewriting is handled by the caller through a
 * StreamRewriter so patterns can span chunk boundaries; this only handles the
 * structural parts (model name, reasoning routing, stripped fields).
 */
export function transformChunk(chunk, { publicModel, transform, reasoningState }) {
  if (!isPlainObject(chunk)) return chunk;
  const out = { ...chunk };
  if (transform.renameModel !== false && publicModel) out.model = publicModel;
  for (const field of transform.stripFields ?? []) delete out[field];

  if (Array.isArray(out.choices)) {
    out.choices = out.choices.map((choice) => {
      const c = { ...choice };
      if (!c.delta) return c;
      const d = { ...c.delta };
      const reasoning = d.reasoning_content ?? d.reasoning ?? null;

      switch (transform.reasoning) {
        case 'strip':
          delete d.reasoning_content;
          delete d.reasoning;
          break;
        case 'inline': {
          const [open, close] = transform.reasoningTags ?? ['<think>', '</think>'];
          if (reasoning) {
            const prefix = reasoningState.open ? '' : open;
            reasoningState.open = true;
            d.content = `${prefix}${reasoning}${d.content ?? ''}`;
          } else if (reasoningState.open && (d.content || c.finish_reason)) {
            reasoningState.open = false;
            d.content = `${close}${d.content ?? ''}`;
          }
          delete d.reasoning_content;
          delete d.reasoning;
          break;
        }
        case 'field':
          if (reasoning) d.reasoning = reasoning;
          delete d.reasoning_content;
          break;
        case 'keep':
        default:
          break;
      }
      c.delta = d;
      return c;
    });
  }

  for (const [k, v] of Object.entries(transform.setFields ?? {})) out[k] = v;
  return out;
}
