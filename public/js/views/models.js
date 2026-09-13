import { api } from '../api.js';
import {
  h, card, table, pill, drawer, field, text, number, textarea, select, toggle,
  toast, confirmDialog, parseKeyValues, stringifyKeyValues, parseList, parseLines, copy,
} from '../ui.js';

/**
 * Model aliases: the public name a caller sends, and the real backend name it
 * becomes. This is where "manukmiberai/creative-writer" is mapped onto
 * "Deepseek-v4-flash-0731", along with the prompt and reshaping that go with it.
 */
export async function modelsView(ctx) {
  const cfg = ctx.state.config;
  const models = cfg.models ?? [];
  const backends = cfg.backends ?? [];

  const root = h('div');

  if (!backends.length) {
    root.append(card('No backend yet', h('div', {},
      h('p.muted', { text: 'A model alias points at a backend. Add a backend first, then come back here.' }),
      h('button.primary', { onclick: () => ctx.go('backends') }, 'Go to backends'),
    )));
  }

  root.append(card('Model aliases', table(
    [{ label: 'Public name (what callers send)' }, { label: '' }, { label: 'Backend model (what we send)' },
      { label: 'Backend' }, { label: 'Prompt' }, { label: '' }],
    models,
    (m) => h('tr.clickable', { onclick: () => editModel(ctx, m) },
      h('td', {},
        h('div.mono', { text: m.id }),
        m.displayName && m.displayName !== m.id ? h('div.small.muted', { text: m.displayName }) : null,
        m.enabled ? null : pill('disabled', 'warn'),
      ),
      h('td.arrow', { text: '→' }),
      h('td.mono', { text: m.upstreamModel || '—' }),
      h('td', { text: backends.find((b) => b.id === m.backend)?.name ?? m.backend ?? '—' }),
      h('td', {},
        m.systemPrompt?.mode && m.systemPrompt.mode !== 'none'
          ? pill(m.systemPrompt.mode, 'accent')
          : h('span.muted', { text: '—' }),
        m.systemPrompts?.length ? pill(`+${m.systemPrompts.length} by effort`, 'accent') : null,
        m.maxTokensPerSecond ? pill(`${m.maxTokensPerSecond} tok/s`, 'warn') : null,
        m.pricing?.enabled ? pill('priced', 'ok') : null,
        m.openrouter?.listed ? pill('OpenRouter', 'ok') : null),
      h('td', {}, h('button.ghost.sm', {
        onclick: (e) => { e.stopPropagation(); copy(m.id, `Copied "${m.id}"`); },
        title: 'Copy the public model name',
      }, '⧉')),
    ),
  ), [
    h('button.primary.sm', { onclick: () => editModel(ctx, null) }, '+ Add model'),
  ]));

  return root;
}

function editModel(ctx, existing) {
  const cfg = ctx.state.config;
  const isNew = !existing;
  const m = structuredClone(existing ?? {
    id: '',
    enabled: true,
    backend: cfg.backends[0]?.id ?? '',
    upstreamModel: '',
    displayName: '',
    description: '',
    aliases: [],
    systemPrompt: { mode: 'none', text: '', promptId: '' },
    systemPrompts: [],
    owner: '',
    maxTokensPerSecond: 0,
    pricing: {},
    params: {},
    forceParams: {},
    limits: { maxInputTokens: 0, maxOutputTokens: 0 },
    tokenizer: '',
    chatProfile: '',
    fallbacks: [],
    requestTransform: {},
    responseTransform: {},
    contextLength: 0,
    openrouter: {},
  });

  const inputs = {};
  const rt = m.responseTransform ?? {};
  const qt = m.requestTransform ?? {};

  drawer(isNew ? 'New model alias' : m.id, () => {
    const body = h('div');

    /* ------------------------------------------------- name translation */
    inputs.id = text(m.id, { placeholder: 'manukmiberai/creative-writer', class: 'mono' });
    inputs.upstreamModel = text(m.upstreamModel, { placeholder: 'Deepseek-v4-flash-0731', class: 'mono' });
    inputs.backend = select(m.backend, cfg.backends.map((b) => [b.id, `${b.name} (${b.baseUrl})`]));
    inputs.enabled = h('input', { type: 'checkbox', checked: m.enabled !== false });
    inputs.displayName = text(m.displayName, { placeholder: 'Creative Writer' });
    inputs.description = text(m.description, { placeholder: 'Shown in the dashboard only' });
    inputs.owner = text(m.owner ?? '', { placeholder: 'ZeikoAI' });
    inputs.aliases = text((m.aliases ?? []).join(', '), { placeholder: 'extra names callers may use' });

    body.append(
      field('Public model name', inputs.id, 'what callers put in "model"'),
      field('Backend model name', inputs.upstreamModel, 'what the relay actually sends upstream'),
      field('Backend', inputs.backend),
      h('div.grid.form', {},
        field('Display name', inputs.displayName),
        field('Extra aliases', inputs.aliases),
      ),
      h('div.grid.form', {},
        field('Owner', inputs.owner, 'published as "owned_by"; blank means ZeikoAI'),
        field('Description', inputs.description),
      ),
      h('label.switch', { style: { marginBottom: '14px' } }, inputs.enabled, h('span', { text: 'Published in /v1/models' })),
    );

    /* --------------------------------------------------- system prompt */
    inputs.spMode = select(m.systemPrompt?.mode ?? 'none', [
      ['none', 'none — pass through untouched'],
      ['prepend', 'prepend — ours first, then theirs'],
      ['append', 'append — theirs first, then ours'],
      ['replace', 'replace — drop whatever the caller sent'],
      ['merge', 'merge — one system message, ours on top'],
    ]);
    inputs.spPromptId = select(m.systemPrompt?.promptId ?? '', [
      ['', '— write it inline below —'],
      ...(cfg.systemPrompts ?? []).map((p) => [p.id, p.name]),
    ]);
    inputs.spText = textarea(m.systemPrompt?.text ?? '', {
      placeholder: 'You are Creative Writer, a careful and vivid fiction assistant…',
      rows: 8,
    });

    // Picking a saved prompt makes the inline box inert; showing the library
    // text (read-only) stops anyone typing into a field that will be ignored.
    const spTextField = field('Prompt text', inputs.spText);
    const syncPromptSource = () => {
      const saved = (cfg.systemPrompts ?? []).find((p) => p.id === inputs.spPromptId.value);
      inputs.spText.disabled = Boolean(saved);
      inputs.spText.value = saved ? saved.text : (m.systemPrompt?.text ?? '');
      spTextField.querySelector('span').textContent = saved
        ? `Prompt text — from "${saved.name}", edit it in the Prompts tab`
        : 'Prompt text';
    };
    inputs.spPromptId.addEventListener('change', syncPromptSource);

    // One model, several prompts, chosen by how hard the caller asked the model
    // to think. A non-reasoning call and a maximum-effort call want different
    // instructions, and the rule that matches first wins.
    inputs.promptRules = textarea(JSON.stringify(m.systemPrompts ?? [], null, 1), { rows: 8 });

    body.append(section('System prompt injection', [
      field('Mode', inputs.spMode),
      field('Use a saved prompt', inputs.spPromptId, 'from the Prompts tab; overrides the text below'),
      spTextField,
      h('hr'),
      h('p.small.muted', {
        text: 'A prompt per thinking effort. The first rule whose effort matches wins; '
          + 'a caller who named no effort falls through to the prompt above. Efforts are '
          + 'none, minimal, low, medium, high, max and default.',
      }),
      field('Prompts by effort', inputs.promptRules,
        'JSON: [{"efforts":["none","low"],"prompt":{"mode":"replace","text":"…"}}, '
        + '{"minEffort":"high","prompt":{"mode":"replace","promptId":"sp_…"}}]'),
    ], true));
    syncPromptSource();

    /* --------------------------------------------------------- params */
    inputs.params = textarea(stringifyKeyValues(m.params), { placeholder: 'temperature=1.1\ntop_p=0.95', rows: 4 });
    inputs.forceParams = textarea(stringifyKeyValues(m.forceParams), { placeholder: 'temperature=0.7', rows: 3 });
    inputs.maxIn = number(m.limits?.maxInputTokens ?? 0, { min: 0 });
    inputs.maxOut = number(m.limits?.maxOutputTokens ?? 0, { min: 0 });
    inputs.contextLength = number(m.contextLength ?? 0, { min: 0 });

    body.append(section('Parameters and limits', [
      field('Default params', inputs.params, 'used when the caller does not set them'),
      field('Forced params', inputs.forceParams, 'always override whatever the caller sent'),
      h('div.grid.form', {},
        field('Max input tokens', inputs.maxIn, '0 = no limit'),
        field('Max output tokens', inputs.maxOut, '0 = no limit'),
        field('Context length', inputs.contextLength, 'shown in /v1/models'),
      ),
    ]));

    /* --------------------------------------------------------- pricing */
    const pr = m.pricing ?? {};
    inputs.maxTps = number(m.maxTokensPerSecond ?? 0, { min: 0, step: 1 });
    inputs.prEnabled = h('input', { type: 'checkbox', checked: pr.enabled === true });
    inputs.prBackendIn = number(pr.backendInputUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prBackendOut = number(pr.backendOutputUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prBackendCached = number(pr.backendCachedInputUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prBackendReasoning = number(pr.backendReasoningUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prIn = number(pr.inputUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prOut = number(pr.outputUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prCached = number(pr.cachedInputUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prReasoning = number(pr.reasoningUsdPerM ?? 0, { min: 0, step: 0.01 });
    inputs.prMargin = number(pr.marginPercent ?? 0, { min: 0, step: 1 });
    inputs.prRequest = number(pr.requestUsd ?? 0, { min: 0, step: 0.0001 });
    inputs.prRefusal = number(pr.refusalUsd ?? 0, { min: 0, step: 0.01 });
    inputs.prRefusalPhrases = textarea((pr.refusalPhrases ?? []).join('\n'), { rows: 2 });
    inputs.prTiers = textarea(JSON.stringify(pr.tiers ?? [], null, 1), { rows: 10 });

    body.append(section('Speed and price', [
      h('p.small.muted', {
        text: 'A ceiling on how fast the reply leaves the relay. A backend running at '
          + '170 tokens a second pushes 170 a second down the tunnel; holding it to 35 '
          + 'costs the reader nothing they notice and leaves the uplink room to breathe. '
          + '0 means full speed.',
      }),
      field('Tokens per second out', inputs.maxTps, '0 = as fast as the backend manages'),
      h('hr'),
      h('label.switch', { style: { marginBottom: '12px' } }, inputs.prEnabled,
        h('span', { text: 'Price this model (leave off to inherit the global price list)' })),
      h('p.small.muted', { text: 'Rates are USD per million tokens. Left at 0, each one falls back to the global list.' }),
      h('div.grid.form', {},
        field('Backend input', inputs.prBackendIn, 'what we are charged'),
        field('Backend output', inputs.prBackendOut),
        field('Backend cached input', inputs.prBackendCached, '0 = no cache discount'),
        field('Backend reasoning', inputs.prBackendReasoning, '0 = billed as output'),
      ),
      h('div.grid.form', {},
        field('Our input', inputs.prIn, '0 = backend rate + margin'),
        field('Our output', inputs.prOut, '0 = backend rate + margin'),
        field('Our cached input', inputs.prCached, 'what a cache read costs the caller'),
        field('Our reasoning', inputs.prReasoning, '0 = billed as output'),
      ),
      h('div.grid.form', {},
        field('Margin %', inputs.prMargin),
        field('Per-request fee', inputs.prRequest),
        field('Refused answer', inputs.prRefusal, 'flat price instead of tokens; 0 = off'),
      ),
      field('Refusal wording', inputs.prRefusalPhrases,
        'one per line; a reply carrying any of them is billed as a refusal. '
        + 'Blank inherits the global list.'),
      h('p.small.muted', {
        text: 'Tiers change the price per request, and every tier that matches applies — '
          + 'a long prompt during a busy hour at maximum thinking effort pays all three. '
          + 'There is no limit on how many you add.',
      }),
      field('Price tiers', inputs.prTiers,
        'JSON: [{"name":"busy hours","inputMultiplier":1.25,"when":{"hours":[{"from":19,"to":23}]}}, '
        + '{"name":"over 256K","inputMultiplier":2,"when":{"minInputTokens":256000}}, '
        + '{"name":"hard thinking","reasoningMultiplier":1.5,"when":{"minEffort":"high"}}]'),
    ]));

    /* ------------------------------------------------------- tokenizer */
    const installed = (ctx.state.tokenizers?.installed ?? []).map((t) => t.name);
    inputs.tokenizer = select(m.tokenizer ?? '', [
      ['', 'auto — match by model name rules'],
      ...installed.map((n) => [n, n]),
    ]);
    inputs.chatProfile = select(m.chatProfile ?? '', [
      ['', 'auto — from the tokenizer rules'],
      ...(ctx.state.profiles ?? []).map((p) => [p, p]),
    ]);

    body.append(section('Token counting', [
      field('Tokenizer', inputs.tokenizer, 'pin the exact vocabulary this backend model uses'),
      field('Chat template profile', inputs.chatProfile, 'per-message overhead of the backend chat template'),
    ]));

    /* -------------------------------------------------- request shape */
    inputs.forceStream = select(
      qt.forceStream === true ? 'true' : qt.forceStream === false ? 'false' : '',
      [
        ['', 'follow the caller'],
        ['true', 'always stream upstream (measures TTFT even for buffered replies)'],
        ['false', 'never stream upstream'],
      ],
    );
    inputs.dropParams = text((qt.dropParams ?? []).join(', '), { placeholder: 'logit_bias, seed' });
    inputs.renameParams = textarea(stringifyKeyValues(qt.renameParams), { placeholder: 'max_completion_tokens=max_tokens', rows: 2 });
    inputs.injectStop = text((qt.injectStop ?? []).join(', '), { placeholder: '</end>' });
    inputs.reqReplace = textarea(JSON.stringify(qt.replace ?? [], null, 1), { rows: 4 });

    body.append(section('Request shaping', [
      field('Upstream streaming', inputs.forceStream),
      field('Drop params', inputs.dropParams, 'parameters this backend rejects'),
      field('Rename params', inputs.renameParams),
      field('Extra stop sequences', inputs.injectStop),
      field('Rewrite user text', inputs.reqReplace, 'JSON: [{"pattern":"…","flags":"gi","replacement":"…"}]'),
    ]));

    /* ------------------------------------------------- response shape */
    inputs.renameModel = h('input', { type: 'checkbox', checked: rt.renameModel !== false });
    inputs.reasoning = select(rt.reasoning ?? 'keep', [
      ['keep', 'keep — pass reasoning_content through'],
      ['strip', 'strip — remove the reasoning entirely'],
      ['inline', 'inline — wrap it into the content with tags'],
      ['field', 'field — move it to "reasoning"'],
    ]);
    inputs.reasoningOpen = text((rt.reasoningTags ?? ['<think>', '</think>'])[0]);
    inputs.reasoningClose = text((rt.reasoningTags ?? ['<think>', '</think>'])[1]);
    inputs.stripFields = text((rt.stripFields ?? []).join(', '), { placeholder: 'system_fingerprint' });
    inputs.setFields = textarea(stringifyKeyValues(rt.setFields), { placeholder: 'owned_by=manukmiber', rows: 2 });
    inputs.prefix = text(rt.prefix ?? '');
    inputs.suffix = text(rt.suffix ?? '');
    inputs.resReplace = textarea(JSON.stringify(rt.replace ?? [], null, 1), { rows: 5 });

    body.append(section('Response shaping', [
      h('label.switch', { style: { marginBottom: '12px' } }, inputs.renameModel,
        h('span', { text: 'Report the public alias as "model" in the response' })),
      field('Reasoning traces', inputs.reasoning),
      h('div.grid.form', {},
        field('Open tag', inputs.reasoningOpen),
        field('Close tag', inputs.reasoningClose),
      ),
      field('Strip fields', inputs.stripFields, 'top-level fields to delete from the response'),
      field('Set fields', inputs.setFields, 'top-level fields to add'),
      h('div.grid.form', {},
        field('Prefix', inputs.prefix),
        field('Suffix', inputs.suffix),
      ),
      field('Rewrite reply text', inputs.resReplace,
        'JSON rules, applied to buffered and streamed text alike'),
    ], true));

    /* ---------------------------------------------------- openrouter */
    // What OpenRouter is told about this model. Prices are text, not numbers:
    // 0.0000006 loses digits through a float, and OpenRouter reads them as
    // decimals.
    const o = m.openrouter ?? {};
    const price = (v, ph) => text(v ?? '', { class: 'mono', placeholder: ph, inputmode: 'decimal' });
    inputs.orListed = h('input', { type: 'checkbox', checked: o.listed === true });
    inputs.orSlug = text(o.slug ?? '', { class: 'mono', placeholder: 'derived from the provider slug' });
    inputs.orHf = text(o.huggingFaceId ?? '', { class: 'mono', placeholder: 'deepseek-ai/DeepSeek-V3' });
    inputs.orQuant = select(o.quantization ?? '', [
      ['', 'unspecified'],
      ...['int4', 'int8', 'fp4', 'mxfp4', 'nvfp4', 'fp6', 'fp8', 'mxfp8', 'fp16', 'bf16', 'fp32'].map((q) => [q, q]),
    ]);
    inputs.orTokFamily = text(o.tokenizerFamily ?? '', { placeholder: 'auto — from the tokenizer in use' });
    inputs.orModalities = text((o.inputModalities ?? ['text']).join(', '), { class: 'mono', placeholder: 'text, image' });
    inputs.orPromptPrice = price(o.pricing?.promptUsd, '0.0000006');
    inputs.orCompletionPrice = price(o.pricing?.completionUsd, '0.0000018');
    inputs.orCachedPrice = price(o.pricing?.cachedPromptUsd, '0.00000015');
    inputs.orCacheWritePrice = price(o.pricing?.cacheWriteUsd, '');
    inputs.orReasoningPrice = price(o.pricing?.internalReasoningUsd, '');
    inputs.orRequestPrice = price(o.pricing?.requestUsd, 'flat fee per request');
    inputs.orCacheTtl = number(o.pricing?.cacheTtlSeconds ?? 0, { min: 0 });
    inputs.orCacheImplicit = h('input', { type: 'checkbox', checked: o.pricing?.cacheImplicit === true });
    inputs.orMaxPrompt = number(o.maxPromptTokens ?? 0, { min: 0 });
    inputs.orMaxOutput = number(o.maxOutputTokens ?? 0, { min: 0 });
    inputs.orTempMax = number(o.temperatureMax ?? 2, { min: 0, step: 0.1 });
    inputs.orStreaming = h('input', { type: 'checkbox', checked: o.streaming !== false });
    inputs.orTools = h('input', { type: 'checkbox', checked: o.supportsTools !== false });
    inputs.orStructured = h('input', { type: 'checkbox', checked: o.supportsStructuredOutputs === true });
    inputs.orReasoning = h('input', { type: 'checkbox', checked: o.supportsReasoning === true });
    inputs.orFree = h('input', { type: 'checkbox', checked: o.isFree === true });
    inputs.orDiscount = number(o.discountToUser ?? 0, { min: 0, max: 0.99, step: 0.01 });
    inputs.orDeprecation = text(o.deprecationDate ?? '', { placeholder: 'YYYY-MM-DD' });
    inputs.orTpmIn = number(o.capacity?.promptTokensPerMinute ?? 0, { min: 0 });
    inputs.orTpmOut = number(o.capacity?.completionTokensPerMinute ?? 0, { min: 0 });
    inputs.orRpm = number(o.capacity?.requestsPerMinute ?? 0, { min: 0 });
    inputs.orConcurrency = number(o.capacity?.concurrency ?? 0, { min: 0 });

    body.append(section('OpenRouter', [
      h('label.switch', { style: { marginBottom: '12px' } }, inputs.orListed,
        h('span', { text: 'Offer this model to OpenRouter' })),
      h('p.small.muted', {
        text: 'Prices are US dollars for a single token. A field left empty is not '
          + 'published at all, which is safer than publishing a zero.',
      }),
      h('div.grid.form', {},
        field('Prompt', inputs.orPromptPrice, 'USD per input token'),
        field('Completion', inputs.orCompletionPrice, 'USD per output token'),
        field('Cached prompt', inputs.orCachedPrice, 'USD per cached input token'),
      ),
      h('div.grid.form', {},
        field('Cache write', inputs.orCacheWritePrice),
        field('Internal reasoning', inputs.orReasoningPrice),
        field('Per request', inputs.orRequestPrice),
      ),
      h('div.grid.form', {},
        field('Cache lifetime (s)', inputs.orCacheTtl, '0 = do not publish one'),
        field('Discount to user', inputs.orDiscount, '0 to 0.99'),
        field('Deprecation date', inputs.orDeprecation),
      ),
      h('div.row', { style: { marginBottom: '12px' } },
        h('label.switch', {}, inputs.orCacheImplicit, h('span', { text: 'Caching is automatic' })),
        h('label.switch', {}, inputs.orFree, h('span', { text: 'Free model' })),
      ),
      h('div.grid.form', {},
        field('Slug', inputs.orSlug),
        field('HuggingFace id', inputs.orHf, 'required if the model is on HuggingFace'),
        field('Quantization', inputs.orQuant),
      ),
      h('div.grid.form', {},
        field('Tokenizer family', inputs.orTokFamily),
        field('Input modalities', inputs.orModalities, 'text, image, audio, video, file'),
        field('Max temperature', inputs.orTempMax),
      ),
      h('div.grid.form', {},
        field('Max prompt tokens', inputs.orMaxPrompt, '0 = use the limits above'),
        field('Max output tokens', inputs.orMaxOutput, '0 = use the limits above'),
      ),
      h('div.row', { style: { marginBottom: '12px' } },
        h('label.switch', {}, inputs.orStreaming, h('span', { text: 'Streaming' })),
        h('label.switch', {}, inputs.orTools, h('span', { text: 'Tools' })),
        h('label.switch', {}, inputs.orStructured, h('span', { text: 'Structured outputs' })),
        h('label.switch', {}, inputs.orReasoning, h('span', { text: 'Reasoning' })),
      ),
      h('p.small.muted', {
        text: 'Capacity is what this relay can actually sustain. Publishing an honest '
          + 'number is what stops OpenRouter sending more than the phone can take; '
          + '0 concurrency publishes the relay\'s own limit.',
      }),
      h('div.grid.form', {},
        field('Input tokens / minute', inputs.orTpmIn),
        field('Output tokens / minute', inputs.orTpmOut),
        field('Requests / minute', inputs.orRpm),
        field('Concurrent requests', inputs.orConcurrency),
      ),
    ]));

    /* ---------------------------------------------------- reliability */
    inputs.fallbacks = text((m.fallbacks ?? []).join(', '), { placeholder: 'backend ids, tried in order' });
    body.append(section('Fallbacks', [
      field('Fallback backends', inputs.fallbacks, 'used when the main backend is down or rate-limited'),
    ]));

    return body;
  }, {
    saveLabel: isNew ? 'Create' : 'Save',
    extra: isNew ? [] : [h('button.danger', {
      onclick: async () => {
        if (!confirmDialog(`Delete model "${m.id}"?`)) return;
        await api.remove('models', m.id);
        toast('Model deleted', 'ok');
        await ctx.reload();
      },
    }, 'Delete')],
    onSave: async (close) => {
      try {
        const payload = {
          id: inputs.id.value.trim(),
          enabled: inputs.enabled.checked,
          backend: inputs.backend.value,
          upstreamModel: inputs.upstreamModel.value.trim(),
          displayName: inputs.displayName.value.trim(),
          description: inputs.description.value.trim(),
          owner: inputs.owner.value.trim(),
          aliases: parseList(inputs.aliases.value),
          systemPrompt: {
            mode: inputs.spMode.value,
            // when a saved prompt is in use the box mirrors it, so keep the
            // model's own inline text instead of overwriting it with the copy
            text: inputs.spPromptId.value ? (m.systemPrompt?.text ?? '') : inputs.spText.value,
            promptId: inputs.spPromptId.value,
          },
          systemPrompts: JSON.parse(inputs.promptRules.value || '[]'),
          maxTokensPerSecond: Number(inputs.maxTps.value) || 0,
          pricing: {
            enabled: inputs.prEnabled.checked,
            backendInputUsdPerM: Number(inputs.prBackendIn.value) || 0,
            backendOutputUsdPerM: Number(inputs.prBackendOut.value) || 0,
            backendCachedInputUsdPerM: Number(inputs.prBackendCached.value) || 0,
            backendReasoningUsdPerM: Number(inputs.prBackendReasoning.value) || 0,
            inputUsdPerM: Number(inputs.prIn.value) || 0,
            outputUsdPerM: Number(inputs.prOut.value) || 0,
            cachedInputUsdPerM: Number(inputs.prCached.value) || 0,
            reasoningUsdPerM: Number(inputs.prReasoning.value) || 0,
            marginPercent: Number(inputs.prMargin.value) || 0,
            requestUsd: Number(inputs.prRequest.value) || 0,
            refusalUsd: Number(inputs.prRefusal.value) || 0,
            refusalPhrases: parseLines(inputs.prRefusalPhrases.value),
            tiers: JSON.parse(inputs.prTiers.value || '[]'),
          },
          params: parseKeyValues(inputs.params.value),
          forceParams: parseKeyValues(inputs.forceParams.value),
          limits: {
            maxInputTokens: Number(inputs.maxIn.value) || 0,
            maxOutputTokens: Number(inputs.maxOut.value) || 0,
          },
          contextLength: Number(inputs.contextLength.value) || 0,
          tokenizer: inputs.tokenizer.value,
          chatProfile: inputs.chatProfile.value,
          fallbacks: parseList(inputs.fallbacks.value),
          requestTransform: {
            forceStream: inputs.forceStream.value === '' ? null : inputs.forceStream.value === 'true',
            dropParams: parseList(inputs.dropParams.value),
            renameParams: parseKeyValues(inputs.renameParams.value),
            injectStop: parseList(inputs.injectStop.value),
            replace: JSON.parse(inputs.reqReplace.value || '[]'),
          },
          openrouter: {
            listed: inputs.orListed.checked,
            slug: inputs.orSlug.value.trim(),
            huggingFaceId: inputs.orHf.value.trim(),
            quantization: inputs.orQuant.value,
            tokenizerFamily: inputs.orTokFamily.value.trim(),
            inputModalities: parseList(inputs.orModalities.value),
            maxPromptTokens: Number(inputs.orMaxPrompt.value) || 0,
            maxOutputTokens: Number(inputs.orMaxOutput.value) || 0,
            temperatureMax: Number(inputs.orTempMax.value) || 2,
            streaming: inputs.orStreaming.checked,
            supportsTools: inputs.orTools.checked,
            supportsStructuredOutputs: inputs.orStructured.checked,
            supportsReasoning: inputs.orReasoning.checked,
            isFree: inputs.orFree.checked,
            discountToUser: Number(inputs.orDiscount.value) || 0,
            deprecationDate: inputs.orDeprecation.value.trim(),
            pricing: {
              promptUsd: inputs.orPromptPrice.value.trim(),
              completionUsd: inputs.orCompletionPrice.value.trim(),
              cachedPromptUsd: inputs.orCachedPrice.value.trim(),
              cacheWriteUsd: inputs.orCacheWritePrice.value.trim(),
              internalReasoningUsd: inputs.orReasoningPrice.value.trim(),
              requestUsd: inputs.orRequestPrice.value.trim(),
              cacheTtlSeconds: Number(inputs.orCacheTtl.value) || 0,
              cacheImplicit: inputs.orCacheImplicit.checked,
            },
            capacity: {
              promptTokensPerMinute: Number(inputs.orTpmIn.value) || 0,
              completionTokensPerMinute: Number(inputs.orTpmOut.value) || 0,
              requestsPerMinute: Number(inputs.orRpm.value) || 0,
              concurrency: Number(inputs.orConcurrency.value) || 0,
            },
          },
          responseTransform: {
            renameModel: inputs.renameModel.checked,
            reasoning: inputs.reasoning.value,
            reasoningTags: [inputs.reasoningOpen.value, inputs.reasoningClose.value],
            stripFields: parseList(inputs.stripFields.value),
            setFields: parseKeyValues(inputs.setFields.value),
            prefix: inputs.prefix.value,
            suffix: inputs.suffix.value,
            replace: JSON.parse(inputs.resReplace.value || '[]'),
          },
        };
        if (!payload.id) return toast('The public model name is required', 'err');
        if (!payload.upstreamModel) return toast('The backend model name is required', 'err');

        // Renaming a model means removing the old entry, not leaving both.
        if (!isNew && existing.id !== payload.id) await api.remove('models', existing.id);
        await api.save('models', payload);
        toast(`Saved ${payload.id}`, 'ok');
        close();
        await ctx.reload();
      } catch (err) {
        toast(err.message, 'err');
      }
      return undefined;
    },
  });
}

function section(title, children, open = false) {
  return h('details.section', { open }, h('summary', { text: title }), h('div', {}, ...children));
}
