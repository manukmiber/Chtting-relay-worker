# Configuration reference

Everything lives in one file, `config/config.json`, written atomically on every
save so a phone that loses power mid-write cannot corrupt it. The dashboard
edits the same file; there is no second source of truth.

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

---

## `server`

| Key | Default | Meaning |
|---|---|---|
| `host` | `0.0.0.0` | `0.0.0.0` lets the tunnel and your LAN reach it |
| `port` | `8787` | relay port — the only one published |
| `maxBodyBytes` | `20971520` | request body ceiling |
| `keepAliveTimeoutMs` | `75000` | idle keep-alive |

There is no relay-side request timeout: a long generation must not be cut off.
Upstream timeouts are per backend.

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
| `rules` | built-in list | first match wins |
| `imageDefaults` | | assumed detail/size when a caller sends no dimensions |

A rule:

```json
{ "match": "Deepseek*", "tokenizer": "deepseek", "profile": "deepseek" }
```

`match` is a case-insensitive glob against the **backend** model name.
`tokenizer` names a file in `data/tokenizers/` (`<name>.tiktoken` or
`<name>.tokenizer.json`). A model's own `tokenizer` field bypasses these rules.

### Chat profiles

`openai`, `chatml`, `llama3`, `deepseek`, `mistral`, `gemma`, `raw`. The profile
sets the per-message, per-name and priming overhead of the backend's chat
template — the tokens billed on top of your message text.

---

## `logging`

| Key | Default | Meaning |
|---|---|---|
| `level` | `info` | `debug`, `info`, `warn`, `error`, `silent` |
| `retentionDays` | `30` | request rows older than this are pruned; 0 = keep forever |
| `storeBodies` | `preview` | `none`, `preview`, or `full` |
| `previewChars` | `800` | preview length |
| `fileEnabled` | `true` | also write `data/logs/relay.log` |

Prompts are stored on the device only. Set `storeBodies` to `none` to keep none
of them.

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

## `defaults`

Same shape as a model's `systemPrompt`, `requestTransform` and
`responseTransform`. Every model inherits these and may override any part.
