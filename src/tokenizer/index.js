import { TokenizerRegistry, DEFAULT_TOKENIZER_RULES } from './registry.js';
import { countChatRequest, countCompletion, resolveProfile, CHAT_PROFILES } from './chat.js';

export { TokenizerRegistry, DEFAULT_TOKENIZER_RULES, CHAT_PROFILES, resolveProfile };

/**
 * The facade the relay uses. It answers three questions:
 *   how many tokens went in, how many came out, and how much to trust either.
 *
 * When the backend reports `usage`, that number is authoritative and the local
 * count is kept beside it as a calibration signal. When the backend reports
 * nothing (many OpenAI-compatible gateways omit usage while streaming), the
 * local count is what gets recorded.
 */
export class TokenCounter {
  constructor(registry) {
    this.registry = registry;
  }

  static create(opts) {
    return new TokenCounter(new TokenizerRegistry({
      rules: DEFAULT_TOKENIZER_RULES,
      ...opts,
    }));
  }

  /**
   * Tokenizer chosen for the *backend* model, since that is what bills.
   * `override.tokenizer` names a vocabulary directly and bypasses the model
   * matching rules, which is how a route pins an exact vocabulary.
   */
  async resolve(model, override = {}) {
    const matched = this.registry.match(model);
    const name = override.tokenizer || matched.tokenizer;
    return {
      ...matched,
      tokenizer: name,
      profile: override.profile || matched.profile,
      tokenizerImpl: await this.registry.get(name),
    };
  }

  async countRequest(body, model, imageOpts, override) {
    const { tokenizerImpl, profile, profileOverride } = await this.resolve(model, override);
    return countChatRequest(body, tokenizerImpl, { profile, profileOverride, image: imageOpts });
  }

  async countText(text, model, override) {
    const { tokenizerImpl } = await this.resolve(model, override);
    return { total: tokenizerImpl.count(text ?? ''), exact: tokenizerImpl.exact !== false, tokenizer: tokenizerImpl.name };
  }

  async countOutput(text, model, extra, override) {
    const { tokenizerImpl } = await this.resolve(model, override);
    return {
      total: countCompletion(text, tokenizerImpl, extra),
      exact: tokenizerImpl.exact !== false,
      tokenizer: tokenizerImpl.name,
    };
  }

  async pieces(text, model, limit = 4000, override) {
    const { tokenizerImpl } = await this.resolve(model, override);
    const all = tokenizerImpl.pieces(String(text ?? ''));
    return {
      tokenizer: tokenizerImpl.name,
      kind: tokenizerImpl.kind,
      exact: tokenizerImpl.exact !== false,
      count: all.length,
      pieces: all.slice(0, limit),
      truncated: all.length > limit,
    };
  }
}

/**
 * Merge local counts with whatever the backend reported.
 * `source` records which number won, so the dashboard can show drift between
 * the relay's tokenizer and the provider's billing.
 */
export function reconcileUsage({ local, upstream, preferUpstream = true }) {
  const up = normalizeUsage(upstream);
  const hasUp = up && (up.prompt_tokens > 0 || up.completion_tokens > 0);
  const useUp = preferUpstream && hasUp;

  const promptTokens = useUp && up.prompt_tokens > 0 ? up.prompt_tokens : local.prompt;
  const completionTokens = useUp && up.completion_tokens > 0 ? up.completion_tokens : local.completion;

  return {
    prompt_tokens: promptTokens,
    completion_tokens: completionTokens,
    total_tokens: promptTokens + completionTokens,
    source: useUp ? 'upstream' : 'local',
    exact: useUp ? true : local.exact !== false,
    local: { prompt_tokens: local.prompt, completion_tokens: local.completion },
    upstream: hasUp ? up : null,
    drift: hasUp
      ? { prompt: local.prompt - up.prompt_tokens, completion: local.completion - up.completion_tokens }
      : null,
    cached_tokens: up?.cached_tokens ?? 0,
    reasoning_tokens: up?.reasoning_tokens ?? local.reasoning ?? 0,
  };
}

export function normalizeUsage(usage) {
  if (!usage || typeof usage !== 'object') return null;
  const prompt = num(usage.prompt_tokens ?? usage.input_tokens ?? usage.promptTokens);
  const completion = num(usage.completion_tokens ?? usage.output_tokens ?? usage.completionTokens);
  const cached = num(
    usage.prompt_tokens_details?.cached_tokens
    ?? usage.prompt_cache_hit_tokens
    ?? usage.cache_read_input_tokens,
  );
  const reasoning = num(usage.completion_tokens_details?.reasoning_tokens ?? usage.reasoning_tokens);
  return {
    prompt_tokens: prompt,
    completion_tokens: completion,
    total_tokens: num(usage.total_tokens) || prompt + completion,
    cached_tokens: cached,
    reasoning_tokens: reasoning,
  };
}

function num(v) {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? Math.round(n) : 0;
}
