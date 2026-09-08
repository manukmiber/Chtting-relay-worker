# Configuration reference

Everything lives in one file, `config/config.json`, written atomically on every
save so a phone that loses power mid-write cannot corrupt it. The dashboard
edits the same file; there is no second source of truth.

Reads happen on every request and take no lock: the live config is published
through an atomic pointer swap, so a save never blocks traffic and traffic never
blocks a save. A saved change is live on the very next request — no restart,
except for `server.workerThreads` and the two listen ports.

Move it with `CHTTING_CONFIG=/path/to/config.json`, or move the whole state
directory with `CHTTING_HOME`. The shipped assets (`public/`) always stay next
to the code.

`config/config.example.json` is a filled-in starting point.

---

## Top level

| Key | Default | Meaning |
|---|---|---|
| `timezone` | `Asia/Jakarta` | IANA zone used for the daily/hourly buckets |
| `server` | see below | the public, tunnel-facing relay |
| `dashboard` | see below | the local control panel |
| `security` | see below | who may call the relay |
| `backends` | `[]` | upstream providers |
| `models` | `[]` | public aliases |
| `keys` | `[]` | client API keys |
| `systemPrompts` | `[]` | reusable prompt library |
| `defaults` | see below | inherited by every model |
| `tokenizer` | see below | counting rules |
| `logging` | see below | what is recorded and for how long |
| `tunnel` | see below | cloudflared |
| `openrouter` | see below | what OpenRouter is told about this relay |

---

## `server`

| Key | Default | Meaning |
|---|---|---|
| `host` | `0.0.0.0` | `0.0.0.0` lets the tunnel and your LAN reach it |
| `port` | `8787` | relay port — the only one published |
| `maxBodyBytes` | `20971520` | request body ceiling |
| `keepAliveTimeoutMs` | `75000` | idle keep-alive |
| `maxConcurrentRequests` | `512` | how many requests are worked on at once |
| `queueCapacity` | `2048` | how many may wait for a slot; `0` refuses instead of queueing |
| `queueTimeoutMs` | `30000` | how long a queued request waits before giving up |
| `workerThreads` | `0` | Tokio worker threads; `0` means one per core |

There is no relay-side request timeout: a long generation must not be cut off.
Upstream timeouts are per backend.

`workerThreads` is read before the async runtime starts, so it only takes effect
on restart. Lower it to leave cores for other Termux processes.

### The queue

Past `maxConcurrentRequests`, requests wait in line rather than being turned
away: a caller who waits 300 ms and then gets an answer is better served than
one who gets a 503 and retries into the same wall. The line is FIFO — the caller
who has waited longest gets the next slot, so nobody starves behind a burst of
newcomers.

A 503 with `Retry-After` is sent only when the line is already `queueCapacity`
long, or when a request has waited `queueTimeoutMs` without reaching the front.
Keep `queueTimeoutMs` below your client's own timeout, or the client gives up
first and the slot is spent producing an answer nobody is listening for.

The line lives in memory. Every request in it is already an open HTTP connection
with a caller on the other end; writing it to disk first would add latency to
exactly the path that is under pressure, and buy nothing — the connection dies
with the process either way.

All three take effect immediately, from the dashboard, with no restart. Lowering
the concurrency never takes a slot from a request already running: the shrink
completes as those finish. The Settings tab shows the live picture — running,
waiting, peak depth, average wait, and how many were turned away.

## `dashboard`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | |
| `host` | `127.0.0.1` | keep this so the dashboard stays off the tunnel |
| `port` | `8788` | must differ from `server.port` |
| `password` | `""` | empty means no sign-in; set one if the phone is shared |
| `sessionTtlMs` | 7 days | how long a sign-in lasts |

## `security`

| Key | Default | Meaning |
|---|---|---|
| `requireClientKey` | `true` | turning this off makes the relay open to anyone who reaches it |
| `corsOrigins` | `["*"]` | allowed browser origins |
| `trustProxyHeaders` | `true` | read `CF-Connecting-IP` / `X-Forwarded-For` — correct behind the tunnel |
| `blockedIps` | `[]` | refused outright |

---

## `backends[]`

The real providers. Their API keys never leave the device.

| Key | Default | Meaning |
|---|---|---|
| `id` | generated | referenced by `models[].backend` |
| `name` | | label shown in the dashboard |
| `baseUrl` | | e.g. `https://api.deepseek.com/v1`; a trailing `/vN` is respected |
| `apiKey` | `""` | |
| `type` | `openai` | `openai` sends `Authorization: Bearer`, `anthropic` sends `x-api-key` |
| `enabled` | `true` | |
| `timeoutMs` | `600000` | per attempt |
| `maxRetries` | `1` | retries on 429, 5xx and dropped connections, with backoff |
| `streamOptions` | `true` | ask for `stream_options.include_usage` while streaming |
| `note` | `""` | free text for your own reference |
| `headers` | `{}` | extra headers, e.g. OpenRouter's `HTTP-Referer` |

---

## `models[]`

One public alias. This is where name translation, prompt injection and
reshaping are configured.

| Key | Default | Meaning |
|---|---|---|
| `id` | | the public name callers send |
| `aliases` | `[]` | extra accepted names |
| `enabled` | `true` | disabled models vanish from `/v1/models` and refuse calls |
| `displayName`, `description` | | shown in the dashboard and `/v1/models` |
| `backend` | | backend id |
| `upstreamModel` | | the real name sent upstream — never exposed |
| `fallbacks` | `[]` | backend ids tried in order when the main one fails |
| `contextLength` | `0` | advertised in `/v1/models` |
| `tokenizer` | `""` | pin a vocabulary; empty means match by rules |
| `chatProfile` | `""` | pin a chat template profile |
| `systemPrompt` | | `{ mode, text, promptId }` |
| `params` | `{}` | defaults a caller may override |
| `forceParams` | `{}` | overrides a caller cannot beat |
| `limits.maxInputTokens` | `0` | reject an over-long prompt with 413; 0 = no limit |
| `limits.maxOutputTokens` | `0` | caps `max_tokens`; 0 = no limit |
| `requestTransform` | | see below |
| `responseTransform` | | see below |
| `openrouter` | | what OpenRouter is told about this model; see below |

### `systemPrompt.mode`

| Mode | Result |
|---|---|
| `none` | pass through untouched |
| `prepend` | yours first, then the caller's system message |
| `append` | the caller's first, yours after |
| `replace` | yours only; the caller's is dropped |
| `merge` | one system message, yours on top |

`promptId` points at a `systemPrompts[]` entry and wins over `text`.

### `requestTransform`

| Key | Default | Meaning |
|---|---|---|
| `forceStream` | `null` | `null` follows the caller; `true` always streams upstream (so TTFT and tokens/sec are measured even for buffered replies); `false` never does |
| `dropParams` | `[]` | parameters the backend rejects |
| `renameParams` | `{}` | e.g. `{"max_completion_tokens": "max_tokens"}` |
| `injectStop` | `[]` | extra stop sequences (max 4 total) |
| `replace` | `[]` | rewrite rules applied to non-system message text |

### `responseTransform`

| Key | Default | Meaning |
|---|---|---|
| `renameModel` | `true` | report the public alias as `model` |
| `reasoning` | `keep` | `keep`, `strip`, `inline` (wrapped in tags), or `field` |
| `reasoningTags` | `["<think>","</think>"]` | used by `inline` |
| `stripFields` | `[]` | top-level fields to delete |
| `setFields` | `{}` | top-level fields to add |
| `prefix`, `suffix` | `""` | wrapped around the reply |
| `replace` | `[]` | rewrite rules over the reply text |

### Rewrite rules

```json
{ "pattern": "DeepSeek", "flags": "gi", "replacement": "Creative Writer" }
{ "pattern": "a.b", "literal": true, "replacement": "X" }
```

`literal: true` escapes the pattern instead of treating it as a regex. An
invalid rule is skipped rather than taking the relay down. The same rules run
over streamed deltas, and a match that straddles two chunks is still caught.

---

## `keys[]`

| Key | Default | Meaning |
|---|---|---|
| `id` | generated | |
| `label` | | shown in stats |
| `key` | | the secret the caller sends as `Authorization: Bearer` |
| `enabled` | `true` | |
| `models` | `["*"]` | `*` or a list of public model ids |
| `quota.requestsPerMinute` | `0` | sliding window; 0 = unlimited |
| `quota.requestsPerDay` | `0` | counted in `timezone` days |
| `quota.tokensPerDay` | `0` | total tokens, counted in `timezone` days |

Each key is one "daily user" in the stats.

---

## `tokenizer`

| Key | Default | Meaning |
|---|---|---|
| `fallback` | `o200k_base` | used when no rule matches |
| `preferUpstreamUsage` | `true` | trust the backend's `usage`, keep the local count as drift |
| `billSystemPromptToUser` | `false` | charge callers for the system prompt the relay injects |
| `rules` | built-in list | first match wins |
| `imageDefaults` | | assumed detail/size when a caller sends no dimensions |

A rule:

```json
{ "match": "Deepseek*", "tokenizer": "deepseek", "profile": "deepseek" }
```

`match` is a case-insensitive glob against the **backend** model name — the one
that actually bills, not the public alias. A model's own `tokenizer` field
bypasses these rules entirely and is used verbatim.

`tokenizer` names either:

* a **built-in** OpenAI vocabulary — `o200k_base`, `cl100k_base`, `p50k_base`,
  `p50k_edit`, `r50k_base`, `o200k_harmony`. These are compiled into the binary
  and need no download.
* a file in `data/tokenizers/`: `<name>.tokenizer.json` (a HuggingFace
  `tokenizer.json`, loaded through HuggingFace's own `tokenizers` crate) or
  `<name>.tiktoken` (an OpenAI rank file).

A `.tiktoken` file carries no pre-tokenizer pattern of its own, so the split is
chosen by name: anything containing `o200k` uses the o200k split, everything
else the cl100k one.

An unknown or missing vocabulary is never fatal — the relay falls back to a
script-aware estimator and marks every count `exact: false`.

### Chat profiles

`openai`, `chatml`, `llama3`, `deepseek`, `mistral`, `gemma`, `raw`. The profile
sets the per-message, per-name and priming overhead of the backend's chat
template — the tokens billed on top of your message text.

---

## `logging`

| Key | Default | Meaning |
|---|---|---|
| `level` | `info` | `debug`, `info`, `warn`, `error`, `silent` |
| `retentionDays` | `30` | request rows older than this are pruned by the dashboard's Prune button; 0 = keep forever |
| `storeBodies` | `preview` | `none`, `preview`, or `full` |
| `previewChars` | `800` | preview length |
| `fileEnabled` | `true` | also write `data/logs/relay.log` |

Prompts are stored on the device only. Set `storeBodies` to `none` to keep none
of them.

### What pruning does not touch

`retentionDays` and the Prune button apply to the browsable `requests` table
only. The `usage_ledger` table — request counts, input and output tokens, TTFT,
tokens per second and cache hits — is never pruned, and cannot be:

- `UPDATE` and `DELETE` on it are refused by SQLite triggers, from this process
  or any other holding the file open.
- Every row carries the hash of the row before it, so an edit made around
  SQLite — dropping the triggers, or touching the file directly — leaves a break
  in the chain. The dashboard's Usage tab recomputes it and names the first row
  that does not match.

Each request appends two rows: `input` once the backend has the request, so
tokens already spent survive a crash or a caller hanging up, and `final` when it
ends. Neither restates the other's figures, so a plain `SUM` over the table is
the right answer.

The rows are small and carry no prompt text, so the table grows by roughly a
couple of hundred bytes per request — a million requests is a few hundred MB.

---

## `tunnel`

| Key | Default | Meaning |
|---|---|---|
| `mode` | `quick` | `quick` (free `*.trycloudflare.com`), `named` (your hostname), `off` |
| `binary` | `cloudflared` | path or command |
| `autoStart` | `false` | start with the relay, and reconnect if it drops |
| `token` | `""` | named mode, from Cloudflare Zero Trust |
| `configFile` | `""` | named mode alternative to a token |
| `extraArgs` | `[]` | passed through to cloudflared |

Only `server.port` is published. The dashboard is never routed through it.

---

## `openrouter`

What OpenRouter is told about this relay. Everything here is a commercial
decision rather than a technical fact, so nothing has a useful default and
nothing is guessed.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | publish the model document at all |
| `path` | `/provider/models` | where OpenRouter polls; a custom path needs a restart, and the default stays mounted either way |
| `token` | `""` | optional bearer OpenRouter must present; empty means the listing is public |
| `providerSlug` | `chtting` | prefixes each model's slug |
| `deploymentRegion` | `""` | ISO country code the traffic is actually served from |
| `datacenters` | `[]` | `[{ "countryCode": "ID", "region": "jakarta" }]` |
| `compliance.zdr` | `false` | zero data retention |
| `compliance.hipaa` | `false` | |
| `isReady` | `true` | clear it to be listed without being routed to |
| `maxConcurrentRequests` | `0` | root-scope concurrency; `0` publishes `server.maxConcurrentRequests` |
| `requestsPerMinute` | `0` | root-scope rate; `0` publishes none |

Turning on `compliance.zdr` while `logging.storeBodies` is anything but `none`
is refused: the relay would be keeping prompt text while telling OpenRouter's
users it keeps nothing.

The document follows `schema_version` 2.4 — input and output modalities,
`supported_parameters`, `pricing` and `capacity` arrays, quantization, tokenizer
family, datacenters and compliance. It never contains `upstreamModel`.

### `models[].openrouter`

| Key | Default | Meaning |
|---|---|---|
| `listed` | `false` | offer this model to OpenRouter |
| `slug` | `""` | `openrouter.slug`; empty derives one from `providerSlug` and the model name |
| `huggingFaceId` | `""` | required by OpenRouter when the model exists on HuggingFace |
| `quantization` | `""` | one of int4, int8, fp4, mxfp4, nvfp4, fp6, fp8, mxfp8, fp16, bf16, fp32; empty publishes `null` |
| `tokenizerFamily` | `""` | e.g. `GPT`; empty reports the vocabulary the relay actually counts with |
| `inputModalities` | `["text"]` | text, image, audio, video, file |
| `maxPromptTokens` | `0` | `0` falls back to `limits.maxInputTokens`, then `contextLength` |
| `maxOutputTokens` | `0` | `0` falls back to `limits.maxOutputTokens` |
| `temperatureMax` | `2` | upper bound published for `temperature` |
| `streaming` | `true` | |
| `supportsTools` | `true` | |
| `supportsStructuredOutputs` | `false` | |
| `supportsReasoning` | `false` | |
| `isFree` | `false` | |
| `discountToUser` | `0` | at least 0 and below 1 |
| `deprecationDate` | `""` | `YYYY-MM-DD` |

Prices are USD for a **single token**, kept as strings:

| Key | Scope |
|---|---|
| `pricing.promptUsd` | input |
| `pricing.cachedPromptUsd` | input, served from the backend's cache |
| `pricing.cacheWriteUsd` | input, writing to that cache |
| `pricing.completionUsd` | output |
| `pricing.internalReasoningUsd` | output, reasoning tokens |
| `pricing.requestUsd` | a flat fee per request |
| `pricing.cacheTtlSeconds` | how long a cache entry lives |
| `pricing.cacheImplicit` | caching happens without the caller asking |

They are strings and not numbers because `0.0000006` loses its last digits
through an `f64`, and OpenRouter compares them as decimals. A price left empty
is **not published at all** rather than published as zero — a wrong price is
worse than a missing one, so a listed model with no price and no `isFree` is
rejected by validation.

Capacity is what the model can actually sustain:

| Key | Meaning |
|---|---|
| `capacity.promptTokensPerMinute` | |
| `capacity.completionTokensPerMinute` | |
| `capacity.requestsPerMinute` | |
| `capacity.concurrency` | `0` publishes `server.maxConcurrentRequests` |

Publishing an honest number here is what stops OpenRouter sending more traffic
than the phone can take.

A parameter the relay is configured to drop or rename is left out of
`supported_parameters`: advertising a parameter that gets discarded on the way
through would be a lie OpenRouter acts on.

---

## `defaults`

Same shape as a model's `systemPrompt`, `requestTransform` and
`responseTransform`. Every model inherits these and may override any part.
