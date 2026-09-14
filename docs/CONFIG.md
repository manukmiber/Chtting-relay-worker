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
| `pricing` | see below | what a request costs and what it sells for |
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
| `wakeLock` | `true` | hold Android's wake lock so the screen going off does not suspend the relay |
| `sseKeepaliveMs` | `15000` | how often a quiet stream gets a keep-alive comment of ours; `0` switches it off |
| `sseKeepaliveText` | `Zeiko is still here, Just be patience` | the text of that comment |
| `rotateHours` | `1` | replace this process with a fresh one every N hours; `0` never |
| `rotateMinutes` | `0` | the same in minutes, when a finer interval is wanted; wins over `rotateHours` |
| `rotateDrainTimeoutMs` | `600000` | how long a retiring instance waits for its last requests |

There is no relay-side request timeout: a long generation must not be cut off.
Upstream timeouts are per backend.

### Keep-alive

While a backend is thinking, nothing crosses the wire, and cloudflared or a
carrier NAT will eventually decide the connection is dead. The relay fills that
silence with an SSE comment of its own:

```
: Zeiko is still here, Just be patience
```

A comment is not an event, so no client parses it as one — it is only traffic,
which is the entire point. Whatever the backend sends to hold *its* connection
open is parsed and dropped rather than forwarded: the shape of a keep-alive is
a fingerprint of which backend is upstream.

### Replacing itself on a clock

Android's low-memory killer goes after whatever has been resident longest, so a
relay that stays up for days climbs that list until the phone kills it —
usually overnight. `rotateHours` has it retire on purpose before that happens.

The handover drops nothing:

1. The running instance starts a fresh copy of itself.
2. The new one binds the same ports — `SO_REUSEPORT` makes that legal — and
   writes a readiness marker under `data/run/`.
3. Only once that marker appears does the old one stop accepting. New
   connections now reach the new instance; the old one finishes the requests it
   already has, up to `rotateDrainTimeoutMs`.
4. The old one hands over the tunnel and exits.

At no point is nothing listening, so no connection is refused. If the successor
fails to start — a missing binary, a port it cannot take — the old instance
keeps running exactly as it was and tries again at the next tick. A missed
rotation is a non-event; a rotation that took the relay down would not be.

The same `SO_REUSEPORT` that makes step 2 legal means a second relay started by
hand does **not** fail to bind: it listens on the same port and the kernel
splits new connections between the two. The older process then answers out of
the config it started with, which is why a key minted after it started comes
back as `invalid API key` on some requests and works on others. `start`
therefore claims `data/run/serving.pid` and refuses to run while a live relay
holds it. A rotation successor carries a generation number and is exempt;
anything started by hand needs `--replace` to take the ports over.

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
| `dashboardOriginGuard` | `true` | refuse dashboard requests from another origin or host |
| `privateUserId` | `fingerprint` | what a private key's user id looks like upstream: `fingerprint`, `keyId` or `secret` |

`trustProxyHeaders` is honoured only when the connection itself came from this
machine, which is where cloudflared runs. A forwarded address that arrived off
the network is a string the caller typed, and `blockedIps` is checked against
whatever the relay decides the address is — so believing one would turn the
block list into a suggestion.

`dashboardOriginGuard` is what keeps the loopback dashboard from being driven
by a web page. Loopback is not private on Android: any app on the phone can
reach `127.0.0.1:8788`, and a page in the browser can *send* requests to it even
though CORS stops it reading the answers — and adding a backend or switching
`requireClientKey` off needs no answer to be useful. The guard refuses a request
whose `Origin` is not this server, and one whose `Host` is a name that is not
this machine, which is what DNS rebinding relies on. It is not a substitute for
`dashboard.password`: the guard is about browsers, and the password is about
everything else.

`privateUserId` decides what a private key sends upstream as its user id.
`fingerprint`, the default, is a truncated SHA-256 of the key — stable, unique
per key, and reveals nothing. `keyId` sends the key's own `key_...` id.

`secret` sends the key itself. It exists for the backend that genuinely
requires it, and it costs two things: the credential lands in somebody else's
request log, and — because the relay records the id it sent — it is also
written in the clear into `relay.db` and shown on the Usage screen. That makes
the metrics database as sensitive as `config.json`, which matters the moment
anyone exports it. Leave this alone unless a backend forces it.

---

## `backends[]`

The real providers. Their API keys never leave the device.

| Key | Default | Meaning |
|---|---|---|
| `id` | generated | referenced by `models[].backend` |
| `name` | | label shown in the dashboard |
| `baseUrl` | | e.g. `https://api.deepseek.com/v1`; a trailing `/vN` is respected |
| `apiKey` | `""` | a single key |
| `apiKeys` | `[]` | a pool of keys, used round-robin; wins over `apiKey` when non-empty |
| `type` | `openai` | `openai` sends `Authorization: Bearer`, `anthropic` sends `x-api-key` |
| `enabled` | `true` | |
| `timeoutMs` | `600000` | per attempt |
| `maxRetries` | `1` | retries on 429, 5xx and dropped connections, with backoff |
| `streamOptions` | `true` | ask for `stream_options.include_usage` while streaming |
| `forwardUserId` | `true` | pass the caller's id upstream, for prompt-cache isolation |
| `userIdHeader` | `x-user-id` | the header it travels in; empty sends none |
| `userIdField` | `user_id` | a second body field it is copied into, beside `user`; empty sends only `user` |
| `note` | `""` | free text for your own reference |
| `headers` | `{}` | extra headers, e.g. OpenRouter's `HTTP-Referer` |

### Key pools

`apiKeys` exists because a provider's rate limit is usually per key, not per
account. Requests are spread over the pool in turn rather than at random:
random is only even on average, and a phone's traffic is bursty enough for the
difference to matter.

### Caller ids and prompt caches

A backend that caches prompts usually keys that cache by user. With one API key
fronting many callers, that would mean one caller's cached prefix serving
another's request — so the caller's own id travels upstream, in `userIdHeader`
and in the body under **two** names: OpenAI's `user`, and whatever
`userIdField` says (`user_id` unless you change it). Two names because the
backends disagree on one, and a backend reading only its own spelling would
pool every caller behind this relay into a single cache — the exact leak the id
exists to prevent. Clear `userIdField` for a backend that rejects body fields it
does not recognise.

The id comes from whichever the caller sent: `user` or `user_id` in the body,
or an `x-user-id`, `x-user`, `x-openai-user` or `x-kv-user` header. It is
recorded on the request row either way, so the Requests screen can tell two
callers behind one key apart. Set `forwardUserId` to `false` to record it
without passing it on.

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
| `owner` | `ZeikoAI` | published as `owned_by` in `/v1/models` |
| `backend` | | backend id |
| `upstreamModel` | | the real name sent upstream — never exposed |
| `fallbacks` | `[]` | backend ids tried in order when the main one fails |
| `contextLength` | `0` | advertised in `/v1/models` |
| `tokenizer` | `""` | pin a vocabulary; empty means match by rules |
| `chatProfile` | `""` | pin a chat template profile |
| `systemPrompt` | | `{ mode, text, promptId }` |
| `systemPrompts` | `[]` | a prompt per thinking effort; see below |
| `maxTokensPerSecond` | `0` | hold the reply to this many tokens a second; `0` is full speed |
| `pricing` | | this model's own price list, layered over the global one |
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

`promptId` points at a top-level `systemPrompts[]` entry and wins over `text`.

### Two prompts: Default and No thinking

A model is written to two audiences and, most of the time, no more: the caller
who asked it to think, and the caller who did not. So the dashboard offers two
boxes rather than a rule editor.

**Default** is the model's plain `systemPrompt`. It answers for every caller who
asked the model to think — `low`, `medium`, `high`, `max` — and for everyone at
all while the second box is left at `none`.

**No thinking** is one `systemPrompts[]` rule, written under the reserved id
`sp-non-thinking`, with `efforts: ["none", "minimal", "default"]`:

```json
"systemPrompts": [
  { "id": "sp-non-thinking", "name": "No thinking", "enabled": true,
    "efforts": ["none", "minimal", "default"],
    "prompt": { "mode": "replace", "text": "Answer directly." } }
]
```

Those are the same three efforts the **no-thinking price band** covers, and they
are meant to stay that way: silence is not a choice to think, so it should
neither be answered as one nor billed as one. A request told one thing and
billed as another is the one bug nobody reading either screen can see.

Leave the mode at `none` and the rule is not written at all, rather than written
as a rule that matches and injects nothing — those callers then fall through to
Default, which is what "I did not fill this in" should mean.

### A prompt per thinking effort, for anything narrower

The two boxes are the common case; `systemPrompts` underneath them is a general
list of rules. Each one names the efforts it answers for and the prompt to
inject when it does; the first match wins, and a caller who matches nothing
falls through to the model's plain `systemPrompt`. The dashboard keeps your own
rules **before** the No-thinking rule, so a narrower one can still win.

```json
"systemPrompts": [
  { "id": "thinking", "minEffort": "high",
    "prompt": { "mode": "replace", "text": "Take your time. Work it through." } },
  { "id": "fast", "efforts": ["none", "minimal", "low"],
    "prompt": { "mode": "replace", "promptId": "sp_direct" } }
]
```

| Key | Meaning |
|---|---|
| `efforts` | the efforts this rule answers for; empty matches every effort |
| `minEffort`, `maxEffort` | inclusive ranked bounds, as an alternative to listing them |
| `enabled` | `false` skips the rule |
| `prompt` | the same `{ mode, text, promptId }` as `systemPrompt` |

The effort vocabulary is `none`, `minimal`, `low`, `medium`, `high`, `max`, and
`default` for a caller who named none. A ranked bound (`minEffort`) never
matches `default`: it is a statement about callers who chose, and silence is not
a choice. An `efforts` list containing `"default"` does match them.

The effort is read from the caller's own body, before anything is injected, and
understands every spelling in circulation: OpenAI's `reasoning_effort`,
OpenRouter's `reasoning.effort`, `reasoning.enabled`, Qwen's `enable_thinking`,
and Anthropic's `thinking.budget_tokens` (mapped to a level by size). An
explicit level always beats a budget sitting beside it.

### Holding the stream back

`maxTokensPerSecond` caps how fast the reply leaves the relay, whatever speed
the backend produced it at. A backend running at 170 tokens a second pushes 170
a second down the tunnel, and on a phone's uplink that is where the strain
lands. Holding it to 35 costs the reader nothing they notice — it is still far
faster than anyone reads — and leaves the link room for everyone else.

It works by not reading from the backend faster than it writes to the caller, so
the pause travels back up the TCP window rather than piling tokens up in memory.
The first delta is never delayed, so time to first token stays the backend's
number rather than something the throttle invented.

### `requestTransform`

| Key | Default | Meaning |
|---|---|---|
| `forceStream` | `null` | `null` follows the caller; `true` always streams upstream (so TTFT and tokens/sec are measured even for buffered replies); `false` never does |
| `dropParams` | `[]` | parameters the backend rejects |
| `renameParams` | `{}` | e.g. `{"max_completion_tokens": "max_tokens"}` |
| `injectStop` | `[]` | extra stop sequences (max 4 total) |
| `replace` | `[]` | rewrite rules applied to non-system message text |

### `responseTransform`

Every reply is rebuilt before any of this runs. The relay does not filter the
backend's JSON — it starts an envelope of its own and copies a named handful of
fields across:

```json
{ "id": "<our uuid v4>", "object": "chat.completion.chunk",
  "created": <when we took the request>, "model": "<the public alias>",
  "choices": [{ "index": 0, "delta": { "content": "…" }, "finish_reason": null }] }
```

Reshaping by deletion is the wrong way round: it removes only what somebody
thought to name, so the day a backend adds a field, that field ships.
`system_fingerprint`, the backend's request id, its `created` stamp, its
`service_tier`, per-choice `logprobs` and anything it invents tomorrow are not
removed — they are simply never copied. The backend's `usage` block does not
travel either; the relay sends its own, with its own input count and the price.

`stripFields` and `setFields` still run, on top of the rebuilt envelope.

| Key | Default | Meaning |
|---|---|---|
| `renameModel` | `true` | report the public alias as `model`; `false` passes the backend's own name through, which is the only way it ever leaves this process |
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
| `kind` | `company` | `company` or `private` — see below |
| `models` | `["*"]` | `*` or a list of public model ids |
| `quota.requestsPerMinute` | `0` | sliding window; 0 = unlimited |
| `quota.requestsPerDay` | `0` | counted in `timezone` days |
| `quota.tokensPerDay` | `0` | total tokens, counted in `timezone` days |
| `billing.name` | | who the invoice is addressed to; blank uses `label` |
| `billing.email` | | |
| `billing.address` | | |
| `billing.taxId` | | VAT, NPWP, whatever the jurisdiction calls it |
| `billing.taxPercent` | *absent* | overrides `billing.taxPercent`; an explicit `0` means tax-exempt |
| `billing.autoInvoice` | `false` | include this key in the automatic cycle |

### Company keys and private keys

The kind decides one thing, and everything else follows from it: **who the
request is on behalf of.**

|  | `company` | `private` |
|---|---|---|
| Who is behind the key | many end users | one holder |
| The user id sent upstream | the one the caller sent, in `user` or `x-user-id` | the key's own identity |
| A caller-supplied `user` | honoured | **ignored** |
| Usage breaks down by | end user | the key |
| `maxTokensPerSecond` | applies | **never applies** |

A **company** key is a reseller. Every call should carry its own end user, and
that id is what the backend sees, what isolates their prompt cache, and what an
invoice breaks its usage down by. This is what every key did before there were
two kinds, so an existing config keeps behaving exactly as it did — `company` is
the default for a key that does not say.

A **private** key is one person, and the key *is* the user. Whatever the caller
puts in `user` is not honoured: they cannot file their spend under somebody
else's name, and they cannot reach another caller's cache partition. What
travels upstream instead is set by `security.privateUserId`, and defaults to a
fingerprint of the key rather than the key itself.

A private key is also never paced. The route's `maxTokensPerSecond` exists so
one reseller's traffic does not fill the phone's uplink at everybody else's
expense; a key with a single holder behind it is the case that costs nobody
else anything, so its replies leave at whatever speed the backend manages. The
request row records `targetTps` as `0` for those, rather than claiming a ceiling
that was never applied.

Each key is one "daily user" in the stats. A key fronting many callers is told
apart by the caller id — see *Caller ids and prompt caches* above.

Keys minted by the relay are shaped `Kunci-Zeiko-` followed by a version-4
UUID — `Kunci-Zeiko-3f2b9c41-7d6a-4e0b-9a55-c1d8e2f40b73` — for company and
private keys alike. Nothing but hex digits and hyphens, so a key survives a
shell, an `.env` line, a YAML file and a query string without a character being
eaten or reinterpreted, which is what earlier keys full of symbols did not.
Keys of any other shape keep working, including every key minted before this;
nothing checks the format on the way in.

---

## `billing`

Turning recorded usage into an invoice, and starting the next period.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | off records usage as before but issues nothing |
| `currency` | `USD` | printed on the invoice; the arithmetic stays in USD |
| `numberPrefix` | `INV` | gives `INV-2026-0001`, counted per year |
| `taxPercent` | `0` | added to the subtotal unless the key overrides it |
| `minimumUsd` | `0` | under this the period stays open instead of being billed |
| `cycleDay` | `1` | day of the month the automatic cycle runs, 1–28 |
| `autoIssue` | `false` | run that cycle |
| `issuer.name` / `.email` / `.address` / `.taxId` | | who the invoice is from |
| `issuer.paymentTerms` | | free text under the totals |

### What "reset the usage" actually does

The usage ledger takes appends only — SQLite refuses to change or remove a row,
and each row carries the hash of the one before it. So an invoice does not clear
anything. It draws a line:

```
  usage_ledger   ─── seq ───────────────────────────────────────────────►
    … 41  42  43 │ 44  45  46  47 │ 48  49  50 …
                 │                │
           invoice #1        invoice #2         "unbilled"
           toSeq = 43        toSeq = 47         = everything past 47
```

A key's **unbilled** total is everything it has run past the line its last
invoice drew. Issuing an invoice moves that line forward, which is why the
number reads zero afterwards — without a single recorded figure being deleted,
and with every past period still reconstructible from the same rows months
later.

Consequences worth knowing:

* An invoice cannot be un-issued. **Voiding** one marks it void and hands its
  period back, so the next invoice covers both.
* **Only the newest invoice may be voided.** A period is a range of `seq`, and
  where the next one starts is read off the newest invoice that still stands.
  Voiding one from the middle would leave its range covered by nothing — never
  billed, and never showing as unbilled either, so the money would simply leave
  the books. Void newest first, then the one before it, then re-issue; the
  refusal names which invoice to void first.
* A request that was in flight when the invoice was issued lands on the next
  one. Its price is only known when the answer completes, and nothing had been
  charged for it yet.
* `minimumUsd` holds a small period **open** rather than throwing it away: the
  usage rolls into the next invoice.
* An issued invoice's figures are hashed with it and SQLite refuses an update
  that touches one. Only `status`, `settledAt` and the note may change.
* **No `paid` flag is written onto a request.** Which invoice covers a ledger
  row is worked out when it is read — an invoice covers a range of `seq`, and a
  key has dozens of invoices against millions of rows, so the join is small.
  Writing the answer onto each row instead would mean rewriting every unbilled
  row of a key on every issue: at a few hundred thousand requests a week, that
  is millions of rows rewritten while the relay is trying to record live
  traffic, and the writer queue is what would give first. The Usage screen
  shows each row's invoice number and whether it is `unbilled`, `issued` or
  `paid` all the same.

Automatic issue only touches keys with `billing.autoInvoice`, and only on
`cycleDay`. Everything else waits for someone to press the button on the
Billing screen, because closing a billing period is a decision.

---

## `pricing`

What a request costs and what it sells for: what the provider charges us, a
rate card for what we charge, and a list of rules that move the card.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | off means no money is reported at all |
| `currency` | `USD` | a label; every figure here is USD |
| `backendInputUsdPerM` | `0` | what the provider charges **us**, per million input tokens |
| `backendOutputUsdPerM` | `0` | …per million output tokens |
| `backendCachedInputUsdPerM` | `0` | the cache-hit rate; `0` means no discount |
| `backendReasoningUsdPerM` | `0` | `0` bills reasoning tokens at the output rate |
| `inputUsdPerM` | `0` | what **we** charge on the standard band; `0` derives it from the backend rate plus the margin |
| `outputUsdPerM` | `0` | likewise |
| `cachedInputUsdPerM`, `reasoningUsdPerM` | `0` | likewise |
| `maxThinking` | `{}` | the rate card at maximum thinking effort; see below |
| `nonThinking` | `{}` | the rate card with thinking off |
| `marginPercent` | `0` | markup over the backend rate, for every rate left at 0 |
| `requestUsd` | `0` | a flat fee per request |
| `refusalUsd` | `0` | what a refused answer costs instead of its tokens; `0` bills it like any other reply |
| `refusalPhrases` | built-in | the wording that marks a reply as a refusal |
| `tiers` | `[]` | conditional price changes; see below |

Off by default on purpose: a relay nobody has priced should report nothing
rather than a column of zeroes, which reads as free service.

A model's own `pricing` is layered over this one field by field — a non-zero
rate there wins, band by band — and its tiers are appended after the global
ones, so a model's rules get the last word.

### Thinking bands

These models are sold at three prices, not one: a standard rate, a higher one
when the caller asks for maximum thinking, a lower one when thinking is off.
So the sell side is a rate card of three bands, and the band is chosen by the
effort the caller asked for and nothing else:

| Band | Chosen by | Priced by |
|---|---|---|
| standard | `low`, `medium`, `high` | `inputUsdPerM`, `cachedInputUsdPerM`, `outputUsdPerM` |
| max thinking | `max` (and a thinking budget over 32K) | `maxThinking` |
| no thinking | `none`, `minimal`, **and a caller who said nothing** | `nonThinking` |

```json
"inputUsdPerM": 0.35,
"cachedInputUsdPerM": 0.10,
"outputUsdPerM": 1.5,
"maxThinking":  { "inputUsdPerM": 0.35, "cachedInputUsdPerM": 0.10, "outputUsdPerM": 2.0 },
"nonThinking":  { "inputUsdPerM": 0.35, "cachedInputUsdPerM": 0.10, "outputUsdPerM": 1.2 }
```

A band takes `inputUsdPerM`, `cachedInputUsdPerM`, `outputUsdPerM` and
`reasoningUsdPerM`, and a rate it leaves at `0` charges the standard rate rather
than nothing — so a band that only moves output says so in one number. Reasoning
tokens are output tokens at the band's own output rate unless the band prices
them apart.

Silence is billed as no thinking on purpose: a caller who never mentioned
thinking did not choose to buy it, and should not pay for it.

The band is settled before any tier is read, so a tier can never stop the chain
early and leave a maximum-effort request paying the standard rate. The band that
applied is recorded on the request row beside the tiers, as `max thinking` or
`no thinking`; the standard band is the rates themselves and records nothing.

### Refusals

A model that will not answer still had to read the prompt to decide that, so the
request is not free — and it is not worth the price of an answer either. Set
`refusalUsd` and a refused request costs that flat amount instead of its tokens:
no band or tier applies, `price_tiers` reads `refusal`, and the request row still carries
the backend's own charge for the prompt it read, so the cost of saying no is
visible rather than hidden.

A refusal is recognised from the completion itself — a refusal is a perfectly
successful `200` — by matching `refusalPhrases` anywhere in the reply, ignoring
case and collapsing whitespace. Left empty with a price set, it falls back to the
sentence the models are told to refuse with:

```json
"refusalUsd": 0.05,
"refusalPhrases": ["I cannot do that. I only provide AI roleplay."]
```

A model's own list replaces the global one outright rather than adding to it:
wording, unlike a rate, is all-or-nothing.

### Tiers

A tier is a condition and a price change, for what a rate card cannot say: the
hour, the size of the prompt, a weekend deal. Tiers apply on top of whichever
band the request is on, and **every tier that matches applies**, in order, which
is the whole point: a 300k-token prompt during a busy hour pays both, rather
than the relay having to pick one reason to charge more. There is no limit on
how many you write.

Thinking effort does not belong here — it is the card. A tier written as a
`stop` on `efforts: ["max"]` would also hide every tier below it from exactly
the requests that pay the most, which is why bands are not tiers.

```json
"tiers": [
  { "name": "busy hours",   "inputMultiplier": 1.25, "outputMultiplier": 1.25,
    "when": { "hours": [{ "from": 19, "to": 23 }] } },
  { "name": "over 256K",    "inputMultiplier": 2,
    "when": { "minInputTokens": 256000 } },
  { "name": "weekend rate", "inputUsdPerM": 0.14,
    "when": { "weekdays": [5, 6] }, "stop": true }
]
```

| Key | Default | Meaning |
|---|---|---|
| `id`, `name` | generated | `name` is what the request row and the log line record |
| `enabled` | `true` | |
| `when` | `{}` | the conditions; an empty one matches every request |
| `inputMultiplier` | `1` | scales the input rate (and the cached rate with it) |
| `outputMultiplier` | `1` | scales the output rate, and reasoning with it |
| `reasoningMultiplier` | `1` | stacks on top of `outputMultiplier` |
| `inputUsdPerM`, `cachedInputUsdPerM`, `outputUsdPerM`, `reasoningUsdPerM` | unset | absolute rates; set, they replace rather than scale |
| `surchargeUsd` | `0` | a flat amount added when this tier matches |
| `stop` | `false` | apply this tier and leave the rest of the list unread |

Conditions inside `when`. Every one that is set must hold; one left unset is not
a condition at all.

| Key | Meaning |
|---|---|
| `models` | public model ids, globbed |
| `efforts` | thinking efforts this applies to |
| `minEffort`, `maxEffort` | inclusive ranked bounds; neither matches a caller who named no effort |
| `hours` | `[{from, to}]` in the configured timezone, inclusive, wrapping past midnight (`{from: 22, to: 5}` is the night shift) |
| `weekdays` | Monday is `0`, Sunday is `6` |
| `minInputTokens`, `maxInputTokens` | `0` on a max means no ceiling |
| `minOutputTokens`, `maxOutputTokens` | |
| `minTotalTokens`, `maxTotalTokens` | input plus output |
| `streamed` | `true` only for streamed calls, `false` only for buffered |
| `cacheHit` | `true` only when the backend served part of the prompt from cache |

### What lands where

Tiers never touch the backend figure: a markup of ours cannot change somebody
else's invoice. So each request row carries three numbers —
`backend_usd` (the backend's rates over the body the backend actually received),
`proxy_usd` (our rates, after every matching tier, over the caller's own token
count) and `profit_usd` — plus `price_tiers`, the names of the tiers that
applied, or `refusal` when the reply was one.

`proxy_usd` also goes back to the caller, rounded, inside the `usage` block:

```json
"usage": { "prompt_tokens": 1284, "completion_tokens": 909, "total_tokens": 2193,
           "completion_tokens_details": { "reasoning_tokens": 629 },
           "usage": 0.001291 }
```

Nothing about the backend's own prices is ever visible there.

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
| `verboseRequests` | `true` | trace every request phase by phase; see below |

Prompts are stored on the device only. Set `storeBodies` to `none` to keep none
of them. What is kept is what the *caller* wrote — the preview is taken from
their own body, before injection, so a system prompt of yours is never filed
under their words.

### The request trace

With `verboseRequests` on, each request writes a trail of lines under one uuid:

```
req 32dadfde in    2026-09-13T14:41:41.383+07:00 model=Wissangeni-512B-V1 key=hp user=tenant-42 effort=high stream=true bytes=147 ip=127.0.0.1
req 32dadfde inj         0.1ms  rule=spr_think mode=replace
req 32dadfde tok       811.1ms  9 caller / 9 upstream  o200k_base exact
req 32dadfde ttft     2014.5ms
req 32dadfde done     2766.8ms  status=200 stop
req 32dadfde sum   uid=32dadfde-f3dd-4ad8-acc5-387b72415f46 model=Wissangeni-512B-V1 key=hp user=tenant-42 effort=high | ram=82.6MB net=147B in/1 437B out/1 889B up | tok=9 in (0 cached, 0% hit) 909 out (629 reasoning) | backend=$0.000391 proxy=$0.001291 profit=$0.000900 [jam padat, thinking effort tinggi] | latency=2766.8ms ttft=2014.5ms tps=2171.84 (held to 6)
```

| Line | What it says |
|---|---|
| `in` | the request arrived: uuid, local timestamp, model, key, caller, effort, body size |
| `inj` | how long injecting the system prompt took, and which rule answered |
| `tok` | how long counting took, the caller's count against the upstream one, and the vocabulary |
| `ttft` | time to first token |
| `done` | end to end, with the status and finish reason |
| `sum` | everything at once: memory, network in/out/upstream, tokens with cache rate, what it cost and made, and latency, TTFT and throughput |

The uuid on these lines is the same one the caller got back as the reply's `id`
and in `x-relay-request-id`, and the same one the request is filed under in the
database — so a complaint quoting a response id leads straight to its row.

All of it goes through the one logger, which writes to stderr and to
`relay.log` together: the dashboard's Logs tab shows exactly what a Termux
session shows. Turn it off and the relay falls back to one line per request.

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
| `autoStart` | `true` | start with the relay, keep trying until it is up, and reconnect if it drops |
| `token` | `""` | named mode, from Cloudflare Zero Trust |
| `configFile` | `""` | named mode alternative to a token |
| `extraArgs` | `[]` | passed through to cloudflared |

Only `server.port` is published. The dashboard is never routed through it.

`autoStart` is on by default, and it means more than one attempt. A phone that
has just rebooted may have no network for a while, and cloudflared may not be
installed yet — so the relay keeps trying with a widening backoff rather than
giving up on the first failure, and the supervisor inside brings cloudflared
back if it dies later. Between them, the tunnel is up after a restart without
anyone opening a terminal.

During an instance rotation the retiring copy hands the tunnel over rather than
leaving two cloudflared processes fighting over one quick-tunnel URL: the
successor waits for the lock under `data/run/` before starting its own.

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
| `deprecationDate` | `""` | `YYYY-MM-DD`; published as `expiration_date` on the public listing |

The rest of this block is read by the public listing at `/v1/models` as well:

| Key | Default | Meaning |
|---|---|---|
| `canonicalSlug` | `""` | the dated, never-reused name of this exact snapshot; empty publishes `id` |
| `outputModalities` | `["text"]` | what the model answers in |
| `instructType` | `""` | a base model's prompt format, e.g. `chatml`; empty publishes `null`, which is what an instruct-tuned model reports |
| `isModerated` | `false` | a moderation pass sits in front of this model |
| `knowledgeCutoff` | `""` | `YYYY-MM-DD`; empty publishes `null` |
| `supportedParameters` | `[]` | empty works the list out from what this model actually accepts |
| `defaultParameters` | `{}` | values a caller gets without asking; empty publishes the model's own `params` |
| `reasoning.mandatory` | `false` | the model always reasons and cannot be asked not to |
| `reasoning.defaultEnabled` | `false` | it reasons for a caller who never mentioned it |
| `reasoning.supportedEfforts` | `[]` | empty publishes the levels the relay prices |
| `reasoning.defaultEffort` | `high` | what a caller who asked for reasoning without naming a level gets |

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
| `pricing.overrides` | what the price becomes at certain hours of certain days |

### Price by hour and day

These models are not sold at one rate; they are sold at a rate that moves with
the clock, and a listing that publishes only the off-peak number is one nobody
can reconcile an invoice against. `pricing.overrides` is that list:

```jsonc
"overrides": [
  { "utcDays": ["saturday", "sunday"] },
  { "utcDays": ["monday", "tuesday", "wednesday", "thursday", "friday"],
    "utcStart": 100, "utcEnd": 400,
    "promptUsd": "0.0000003", "completionUsd": "0.0000012",
    "cachedPromptUsd": "0.000000006" }
]
```

| Key | Meaning |
|---|---|
| `utcDays` | lower-case weekday names; empty is every day |
| `utcStart`, `utcEnd` | the window as `HHMM` — `0` is midnight, `100` is 01:00, `1730` is 17:30 |
| the price keys | the same names as above, for this window only |

UTC and not the relay's timezone, because that is the clock the listing is read
on and the only one a caller on the other side of the world can check a bill
against. The start is inclusive and the end exclusive; an end below the start
wraps past midnight, and a window of `0` to `0` (or no window at all) is the
whole day. The **first** window that covers a moment wins, so the narrow ones
go first. A price a window leaves out keeps the standing one rather than
becoming free — and every window is published complete, so a client reading the
third override does not have to walk back up the list.

Unlike `pricing.tiers`, which are the relay's own internal rate rules on a
local clock, these are a published promise: what `usage.usage` charges when the
relay's own rate card is switched off is exactly what this says.

They are strings and not numbers because `0.0000006` loses its last digits
through an `f64`, and OpenRouter compares them as decimals. A price left empty
is **not published at all** rather than published as zero — a wrong price is
worse than a missing one, so a listed model with no price and no `isFree` is
rejected by validation.

One exception, and it is arithmetic rather than invention: on the public
listing at `/v1/models`, a price left empty here is filled in from the sell side
of this model's own rate card when `pricing.enabled` is on — the same figure
divided by a million. An operator who priced the model once does not have to
type it again per token.

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
