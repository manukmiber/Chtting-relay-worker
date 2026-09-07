/**
 * Message-level accounting.
 *
 * Counting raw content is not enough: every backend wraps messages in a chat
 * template whose control tokens are billed too. Each family gets a profile with
 * its per-message overhead, and a route may override the profile in config when
 * a backend does something unusual.
 */

export const CHAT_PROFILES = {
  // gpt-3.5/gpt-4/gpt-4o: <|start|>role<|message|>content<|end|>
  openai: { perMessage: 3, perName: 1, perTool: 0, primer: 3, bos: 0 },
  // <|im_start|>role\n content <|im_end|>\n
  chatml: { perMessage: 4, perName: 1, perTool: 0, primer: 3, bos: 0 },
  // <|start_header_id|>role<|end_header_id|>\n\n content <|eot_id|>
  llama3: { perMessage: 5, perName: 1, perTool: 0, primer: 4, bos: 1 },
  // <|User|> / <|Assistant|> markers, bos once, eos after each assistant turn
  deepseek: { perMessage: 2, perName: 0, perTool: 0, primer: 2, bos: 1 },
  // [INST] ... [/INST]
  mistral: { perMessage: 3, perName: 0, perTool: 0, primer: 2, bos: 1 },
  // <start_of_turn>role\n content <end_of_turn>\n
  gemma: { perMessage: 4, perName: 0, perTool: 0, primer: 3, bos: 1 },
  // plain concatenation, for completion-style backends
  raw: { perMessage: 0, perName: 0, perTool: 0, primer: 0, bos: 0 },
};

export function resolveProfile(name, override) {
  const base = CHAT_PROFILES[name] ?? CHAT_PROFILES.openai;
  return override ? { ...base, ...override } : base;
}

/** Flatten OpenAI content (string | array of parts) into countable pieces. */
export function contentToParts(content) {
  if (content == null) return [];
  if (typeof content === 'string') return [{ type: 'text', text: content }];
  if (!Array.isArray(content)) return [{ type: 'text', text: String(content) }];
  return content.map((p) => {
    if (typeof p === 'string') return { type: 'text', text: p };
    if (p?.type === 'text') return { type: 'text', text: p.text ?? '' };
    if (p?.type === 'input_text') return { type: 'text', text: p.text ?? '' };
    if (p?.type === 'image_url') return { type: 'image', image: p.image_url ?? {} };
    if (p?.type === 'input_image') return { type: 'image', image: p };
    if (p?.type === 'input_audio') return { type: 'audio', audio: p.input_audio ?? {} };
    return { type: 'text', text: typeof p === 'object' ? JSON.stringify(p) : String(p) };
  });
}

/**
 * Vision cost, following OpenAI's published tiling rule. `detail:"low"` is a
 * flat 85; high detail is 85 + 170 per 512px tile of the resized image.
 * Without dimensions we assume a common 1024x1024 upload.
 */
export function imageTokens(image, opts = {}) {
  const detail = image?.detail ?? opts.defaultDetail ?? 'auto';
  if (detail === 'low') return 85;
  const w = Number(image?.width ?? opts.defaultWidth ?? 1024);
  const h = Number(image?.height ?? opts.defaultHeight ?? 1024);
  const scale = Math.min(1, 2048 / Math.max(w, h));
  let sw = w * scale;
  let sh = h * scale;
  const shortest = Math.min(sw, sh);
  if (shortest > 768) {
    const k = 768 / shortest;
    sw *= k;
    sh *= k;
  }
  const tiles = Math.ceil(sw / 512) * Math.ceil(sh / 512);
  return 85 + 170 * tiles;
}

/**
 * Tool/function definitions are injected into the prompt by the backend. They
 * are rendered into the pseudo-TypeScript shape OpenAI documents, then counted
 * with the same tokenizer, which tracks the real cost far better than counting
 * the raw JSON.
 */
export function renderTools(tools) {
  if (!Array.isArray(tools) || !tools.length) return '';
  const lines = ['namespace functions {', ''];
  for (const tool of tools) {
    const fn = tool?.function ?? tool ?? {};
    if (fn.description) lines.push(`// ${fn.description}`);
    const params = fn.parameters ?? {};
    const props = params.properties ?? {};
    const required = new Set(params.required ?? []);
    if (!Object.keys(props).length) {
      lines.push(`type ${fn.name} = () => any;`, '');
      continue;
    }
    lines.push(`type ${fn.name} = (_: {`);
    for (const [key, spec] of Object.entries(props)) {
      if (spec?.description) lines.push(`// ${spec.description}`);
      lines.push(`${key}${required.has(key) ? '' : '?'}: ${schemaType(spec)},`);
    }
    lines.push('}) => any;', '');
  }
  lines.push('} // namespace functions');
  return lines.join('\n');
}

function schemaType(spec) {
  if (!spec || typeof spec !== 'object') return 'any';
  if (Array.isArray(spec.enum)) return spec.enum.map((v) => JSON.stringify(v)).join(' | ');
  if (spec.type === 'array') return `${schemaType(spec.items)}[]`;
  if (spec.type === 'object' && spec.properties) {
    const inner = Object.entries(spec.properties)
      .map(([k, v]) => `${k}: ${schemaType(v)}`)
      .join(', ');
    return `{ ${inner} }`;
  }
  return spec.type ?? 'any';
}

/**
 * Count a whole chat request.
 * Returns a breakdown so the dashboard can show where the prompt budget goes.
 */
export function countChatRequest(body, tokenizer, options = {}) {
  const profile = resolveProfile(options.profile ?? 'openai', options.profileOverride);
  const count = (s) => (s ? tokenizer.count(s) : 0);

  const breakdown = {
    system: 0, user: 0, assistant: 0, tool: 0, images: 0, tools: 0, overhead: 0,
  };

  const messages = Array.isArray(body?.messages) ? body.messages : [];
  for (const msg of messages) {
    breakdown.overhead += profile.perMessage;
    if (msg?.name) breakdown.overhead += profile.perName + count(msg.name);
    breakdown.overhead += count(msg?.role ?? '');

    const bucket = msg?.role === 'system' ? 'system'
      : msg?.role === 'assistant' ? 'assistant'
        : msg?.role === 'tool' || msg?.role === 'function' ? 'tool' : 'user';

    for (const part of contentToParts(msg?.content)) {
      if (part.type === 'text') breakdown[bucket] += count(part.text);
      else if (part.type === 'image') breakdown.images += imageTokens(part.image, options.image);
      else if (part.type === 'audio') breakdown[bucket] += audioTokens(part.audio);
    }

    // Assistant turns replaying tool calls still cost their serialized form.
    if (Array.isArray(msg?.tool_calls)) {
      for (const tc of msg.tool_calls) {
        breakdown.assistant += count(tc?.function?.name ?? '');
        breakdown.assistant += count(tc?.function?.arguments ?? '');
        breakdown.overhead += 3;
      }
    }
    if (msg?.tool_call_id) breakdown.overhead += 2;
  }

  if (Array.isArray(body?.tools) && body.tools.length) {
    breakdown.tools += count(renderTools(body.tools)) + 12;
    if (body.tool_choice && body.tool_choice !== 'auto') breakdown.tools += 4;
  } else if (Array.isArray(body?.functions) && body.functions.length) {
    breakdown.tools += count(renderTools(body.functions.map((f) => ({ function: f })))) + 12;
  }

  if (typeof body?.prompt === 'string') breakdown.user += count(body.prompt);
  else if (Array.isArray(body?.prompt)) for (const p of body.prompt) breakdown.user += count(String(p));

  breakdown.overhead += profile.primer + profile.bos;

  const total = Object.values(breakdown).reduce((a, b) => a + b, 0);
  return { total, breakdown, exact: tokenizer.exact !== false, tokenizer: tokenizer.name };
}

function audioTokens(audio) {
  // ~10 tokens per second of PCM/opus audio; base64 length is the only signal
  // available without decoding, so approximate from payload size.
  const b64 = String(audio?.data ?? '');
  const bytes = Math.floor((b64.length * 3) / 4);
  return Math.ceil(bytes / 3200);
}

/** Count a completion (output side): text, tool calls and reasoning traces. */
export function countCompletion(text, tokenizer, extra = {}) {
  let total = tokenizer.count(text ?? '');
  if (extra.reasoning) total += tokenizer.count(extra.reasoning);
  if (Array.isArray(extra.toolCalls)) {
    for (const tc of extra.toolCalls) {
      total += tokenizer.count(tc?.function?.name ?? '');
      total += tokenizer.count(tc?.function?.arguments ?? '');
      total += 3;
    }
  }
  return total;
}
