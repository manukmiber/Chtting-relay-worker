# CLAUDE.md

Repo-specific notes for Claude Code sessions working on this project.

## GitHub Actions CI is broken at the account/repo-settings level — do not chase it

Every workflow run in this repository's history concludes `failure`, on
every branch, including runs on `main` with no code change that could
plausibly break a Rust build (merge commits, doc-only commits). The jobs
never execute: a typical run is created and completed within 4-6 seconds
for three Rust jobs (`test, clippy, fmt`, `tokenizer accuracy vs the
reference implementations`, `cross-compile for Termux`), the logs return
HTTP 404, and `started_at` equals `created_at` on every job — meaning no
step ever ran. Re-running failed jobs reproduces the same thing.

This is the signature of GitHub Actions refusing to *dispatch* the jobs —
a spending limit, Actions disabled for the account/org, or a runner the
repository can no longer use — not a code problem. There is no commit that
fixes this; it needs a change in repository or organization settings
(Settings → Actions, or the account's billing/usage page) that a coding
session has no access to. Confirmed dead as of 2026-09-14 across at least
27 workflow runs going back through `main`.

**What this means for a session working here:**

- Do not spend time debugging "CI failure" on a PR before checking whether
  the failing runs are also failing on `main` with unrelated commits — if
  so, it is this account-level issue, not your change.
- Do not repeatedly re-run the workflow hoping it starts working. One
  re-run is enough to confirm the same 4-second no-op failure; further
  re-runs waste the re-run budget these rules allow for genuine flakes.
- Instead, run the workflow's own commands locally and report those
  results in the PR: `cargo fmt --all --check`, `cargo clippy --all-targets
  -- -D warnings`, `cargo test --all-targets`, and (with HuggingFace
  vocabularies fetched into `data/tokenizers/`) `cargo test --test
  tokenizer -- --nocapture`. See `.github/workflows/ci.yml` for the exact
  commands and flags (note: CI runs in **debug**, not `--release`).
- The one check that cannot be reproduced locally in a typical sandboxed
  session is `cross-compile for Termux` (needs the Android NDK for
  `cc-rs`/`ring`). Say so explicitly rather than claiming it passed.
- Post one comment on the PR stating the account-level CI outage, the
  local verification results, and which check (if any) could not be
  checked — then treat the PR as blocked on that external fix, not on
  your code. Do not leave a PR silently "waiting for CI" forever; say
  what's actually blocking it.
- If someone reports CI is fixed, re-verify by triggering a run rather
  than assuming — this note may go stale.

## Operational hazard: two relays on one port

`SO_REUSEPORT` makes the hourly self-rotation (`src/rotate.rs`) seamless,
but it also means a second `chtting-relay start` does **not** fail to
bind — both processes listen on the same port and the kernel splits new
connections between them at random. The older process keeps answering out
of whichever config it started with, so a freshly minted API key looks
like it "doesn't work" on a random fraction of requests while working
fine on others. This class of bug reads exactly like a broken key; it is
not one.

The guard is `src/lock.rs`: an **abstract unix socket named after the
port**, bound before anything else happens at startup. Binding it is one
atomic syscall, the name is kernel-global (so `--home`, `$TMPDIR` and the
working directory have no say in it), and the kernel releases it when the
holder dies however it dies. There is no stale lock to recover from and
no "allow on doubt" path: every uncertainty resolves to not starting.

- A duplicate start exits with **code 3** (`lock::EXIT_PORT_BUSY`) and
  says which port and who has it. The keeper script treats 3 as "wait,
  do not restart" — anything else is still a crash to restart from.
- `--replace` genuinely evicts: it asks the lease who holds it (the
  holder listens on its own lease and answers with its pid), SIGTERMs,
  waits for the lease to come free, then SIGKILLs.
- A rotation successor is the one case where two may serve at once. It
  proves it is one with a token its predecessor minted in
  `data/run/handover-<gen>` — `CHTTING_GENERATION` alone is **not**
  proof, since an inherited env var used to be enough to skip the check
  entirely — and it is not the relay until it holds the lease, which it
  can only take once the predecessor is gone.
- `data/run/serving.pid` still exists but is only a **hint** for error
  messages. It is keyed on the data directory, which is exactly why it
  could never catch two copies started with different `--home` values.

`chtting-relay doctor` prints whether the port is free and who has it.
Start there when debugging "the key doesn't work". End-to-end coverage is
in `tests/single_instance.rs`, which starts the real binary twice.

## The dashboard can now be published, which changes what "loopback" bought

`dashboard.tunnel` is a second cloudflared publishing `dashboard.port`. Before
it existed, several things were true because only this device could reach the
panel; they are not true any more, and the code that depends on them is worth
knowing before changing it:

- **`tunnel::TunnelManager` has a `Scope`.** Each manager reads its port from
  its own scope, never from an argument, so no configuration can make one
  tunnel publish the other's port. Keep it that way.
- **`TunnelManager::start` refuses the dashboard scope without a password of
  `config::MIN_REMOTE_PASSWORD` characters.** The check is at start rather than
  at save, because a config that *describes* a dashboard tunnel is fine to
  store; the moment that matters is the moment a process begins accepting from
  the internet.
- **The origin guard is widened, not disabled.** `Dashboard::answers_to` accepts
  this machine's names, the live tunnel's hostname, and
  `security.dashboardAllowedHosts`. Do not "fix" a remote-access problem by
  turning `dashboardOriginGuard` off.
- **The session cookie is `Secure` only for a non-local `Host`.** Setting it
  unconditionally breaks sign-in over plain `http://127.0.0.1` in browsers that
  do not treat loopback as a secure context. `logout` has to clear it with the
  same attributes or the cookie survives the sign-out.
- **`security_headers` uses `entry().or_insert()` for `cache-control`, not
  `insert`.** An `insert` there overwrites the static handler's validator with
  `no-store`, which re-sends the whole frontend over the tunnel on every
  navigation. API responses get `no-store`; assets get an ETag and a 304.

## Langfuse goes over OTLP, and the ingestion endpoint is a trap

`src/langfuse.rs` posts to `POST /api/public/otel/v1/traces`. Do not "simplify"
it to `/api/public/ingestion`: Langfuse's own OpenAPI document marks that
deprecated, and on Langfuse Cloud it rejects trace and observation events once
v4-only write mode begins on 2026-11-16. The JSON-protobuf OTLP body is also
why this needs no `prost` and no build-time codegen, which matters on a device
that compiles its own binary.

Two invariants the tests pin:

- `Langfuse::begin` decides sampling at the *start* of a request and holds
  bodies by `Arc`; nothing is serialised until `record` is certain a span is
  going out. Moving that work earlier puts it on the hot path.
- A span never carries a credential. A private key with
  `security.privateUserId: "secret"` is fingerprinted, and `redact_body` strips
  the fields a client key can be written into.

## Invoice hashing is versioned — do not add a field to `canonical()` blindly

`Invoice::canonical()` branches on `hash_version`. Version 1 is the byte-for-byte
form used before the payment and due-date fields existed, and it is reproduced
exactly so those invoices still verify. Hashing new fields unconditionally would
report every invoice already on disk as tampered with. New fields go behind a
new version, and `invoice::ADDED_COLUMNS` carries the defaults that make an old
row read correctly.

## Repo conventions worth knowing

- Rust workspace, single binary `chtting-relay` (`src/main.rs`) +
  library crate `chtting_relay` (`src/lib.rs`).
- CI runs in **debug** profile (`cargo test --all-targets`, not
  `--release`), so verify locally in debug to match CI exactly, not just
  in release.
- Three release profiles: `release` (default), `release-small` (a phone that
  would be OOM-killed building anything larger) and `release-fast` (fat LTO,
  one codegen unit — for a tablet with cores and RAM to spare). `Updater::
  profile()` reads which one is running off the *directory* the executable sits
  in, so adding a profile means adding it to that list too, or "Update &
  restart" rebuilds into a directory nothing starts from.
- Answers that are a pure function of the config are resolved in
  `config::normalize` and cached on the struct — `parsed_tz`,
  `resolved_tokenizer`, `resolved_profile`, `resolved_pricing`. Each is
  `#[serde(skip)]` and each has a live fallback for a `Config` built by hand in
  a test. When you find per-request work whose inputs only change on save, this
  is where it goes.
- `cargo clippy --all-targets -- -D warnings` — clippy warnings are hard
  failures in CI (once CI runs at all).
- No `.claude/skills/steward/SKILL.md` or `.claude/skills/babysit/SKILL.md`
  exist in this repo as of 2026-09-14 (checked when handling PR events).
- No PR template exists (`.github/pull_request_template.md` etc. — none
  found).

## The three lists: QoL, features, optimisation

What follows is standing work, written down so a session that finishes early
has somewhere useful to look instead of inventing a task. Every entry was
checked against the tree on 2026-09-16, and each one names the file it lives
in and the reason it is here — an entry with no reason is a wish, and wishes
belong in an issue, not in this file.

Three rules for keeping these lists honest:

- **Nothing here is a commitment.** The operator's call still outranks the
  list. `DEEPSEEK_WITHHELD` is on it precisely *because* switching those
  parameters on was somebody's decision, not because the decision was wrong.
- **An entry is deleted when it ships.** Do not leave it with a tick beside
  it. If shipping it taught something a future session would otherwise learn
  the hard way, that lesson goes in one of the prose sections above — those
  are about traps, these are about intent, and the two should not blur.
- **Check before you believe.** Several obvious-sounding gaps are already
  closed and are not listed for that reason: the Requests tab does have
  search and model/status filters, the dashboard does have a dark theme and
  a 640px breakpoint, the SSE decoder does handle a multi-byte character
  split across two network chunks (`Utf8Decoder`), and the tokenizer already
  memoises per message and already runs on a blocking thread. Confirm an
  entry still stands before acting on it.

### 1. QoL — the same relay, less friction around it

**To update**

- **`relay.log` grows until the disk does not.** `src/logging.rs` appends
  through a background channel and nothing ever rolls or truncates the file.
  On a phone left at `info` for months that file is what fills the storage,
  and the dashboard's Logs tab has to read the tail of it. Size-based rolling
  (`relay.log` → `relay.log.1`, keep a handful) belongs in that same writer
  task: it is the only thing that touches the file, so nothing else changes.
- **`logging.retentionDays` is only honoured when a human presses a button.**
  The default is 30 days, but the only caller of the prune is
  `POST /api/maintenance/prune` from Settings (`server::dashboard::prune`). A
  relay nobody visits keeps every request row forever. It wants a daily ticker
  beside the two that already exist in `src/main.rs` — the rate-limit sweeper
  and `billing_cycle` — and the dashboard should say when it last ran.
- **The CLI can create but never remove.** `src/main.rs` offers `key
  new|list`, `backend add|list`, `model add|list`, `config path|show`. There
  is no `key disable`, no `... rm`, no `config set`, no `config check`, no
  `usage` and no `invoice`. Everything destructive is dashboard-only, which
  is the wrong way round on a device where the dashboard is sometimes the
  thing that is broken. `ConfigStore::upsert` / `replace_list` / the delete
  path already do the work; the subcommands are a thin wrapper over them.
- **`doctor` stops one question short.** It reports version, platform, port
  and lease holder, tokenizer inventory, cloudflared and `config::validate`.
  It does not answer what an operator asks next: can each backend actually be
  reached (the dashboard has `POST /api/backends/{id}/test`), is the ledger
  chain intact (`GET /api/usage/verify`), how large is `relay.db` and how much
  disk is left, and does every enabled model resolve to a vocabulary that is
  installed rather than one that is merely named.
- **Two Settings tabs silently overwrite each other.** `PUT /api/config` deep-
  merges whatever arrives (`ConfigStore::update`) with no notion of what the
  browser was looking at when it started editing. Serve a config version with
  the GET and refuse a PUT carrying a stale one; on a relay where the config
  *is* the product, last-write-wins is the wrong default.

**To add**

- **Config history and a revert.** `write_atomic` replaces `config.json` and
  the previous bytes are gone. Keeping the last N revisions under
  `data/config-history/` costs one extra write on a path that already has the
  old file in hand, and turns "I changed pricing and now everything 400s" into
  one button.
- **One page per key.** `public/js/views/keys.js` lists keys; that key's
  quota, live usage, recent requests and invoices live in three other tabs.
  When a customer asks a question, the operator wants one screen, not four.
- **Copy as curl**, on a request row and in the Playground. The base URL, the
  public model alias and the headers are otherwise retyped by hand every time
  a caller's request has to be reproduced.
- **A warning before the wall.** There is no notification of any kind in the
  tree (see the features list). The cheapest first step is in-dashboard: a
  banner when a key passes ~80% of its daily quota, when the last prune is
  overdue, or when a backend's most recent test failed — the numbers are
  already in `QuotaTracker` and on the request rows.

### 2. Features — things the relay cannot do yet

**To update**

- **Tool calling and JSON mode are switched off by a `const`, not by config.**
  `DEEPSEEK_WITHHELD` in `src/relay/transform.rs` withholds `tools`,
  `tool_choice`, `response_format`, `structured_outputs`, `logprobs` and
  `top_logprobs`, and `deepseek_takes` keeps them out of the published
  listing so nobody integrates against them. That is the right *behaviour*,
  but it means reversing the operator's decision needs a recompile on a
  phone — ten minutes on `release-small`. It belongs in the config, per
  backend or per model, defaulting exactly as it does today. Note that the
  reply half already exists: `collect_tool_calls` reassembles streamed
  `tool_calls` deltas.
- **The inbound surface is OpenAI-shaped only.** `src/server/public.rs`
  serves `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings` and
  `/v1/models`. Outbound already knows `kind == "anthropic"` (it sets
  `anthropic-version` in `src/relay/upstream.rs`), and `src/pricing.rs`
  already reads `thinking.budget_tokens` from Anthropic-shaped clients. An
  inbound `/v1/messages` would let those clients point straight at the relay,
  and the translation is the same reshaping `transform.rs` already does in
  one direction.
- **Keys have no lifetime and no ceiling of their own.** `ClientKey` is
  enabled or disabled; there is no `expiresAt`, and no rotation in the sense
  that matters to a customer — mint a successor, keep both live for N days,
  retire the old one. `Quota` is per-day and per-minute only: no monthly cap,
  and no per-key concurrency limit, so a single key can hold every slot in
  `relay::gate` while the queue in front of it is global FIFO. Per-key
  fairness is the feature; the counters it needs are already in `Gate`.
- **A dead backend costs full price on every request.** `Upstream` retries
  with backoff and then falls through to the route's fallbacks, but remembers
  nothing between requests: while a backend is down, every caller pays the
  whole retry budget to discover it again. A circuit breaker — open after N
  consecutive failures, one half-open probe, closed on success — belongs
  beside the round-robin `turns` map it already keeps.

**To add**

- **`/metrics` in Prometheus text format.** Nothing in this relay can be
  scraped. The numbers already exist in memory: `Gate`'s admitted / refused /
  waited counters, the store writer's queue depth, and the per-request rows.
  This is an encoder over bookkeeping that is already paid for, not new
  bookkeeping.
- **Anything that reaches the operator when they are not looking.** There is
  no webhook, no Termux notification, no email — grep the tree and nothing
  notifies. Backend down, tunnel down, key over quota, disk nearly full,
  invoice overdue: all of it is visible only to somebody who happens to open
  the dashboard. A single outbound webhook plus `termux-notification` covers
  most of it.
- **Backup and restore as one command.** Nothing bundles `config.json`,
  `relay.db` and the installed tokenizer vocabularies into an archive, and
  nothing reads one back. The device is a phone: it gets dropped, wiped and
  replaced, and the ledger and the invoices are the parts nobody can
  reconstruct.
- **An audit trail for the dashboard.** One shared password, one session
  cookie, and full write access to keys, pricing and backends — and no record
  of who changed what or when. `src/store/ledger.rs` already has the hash-
  chained, append-only shape to copy, and the same argument for it: a number
  somebody edited should be something you can prove.
- **A read-only dashboard account.** Same reason. Somebody who should be able
  to look at usage should not thereby be able to mint keys.

### 3. Optimisation — measured work, on a device that feels every millisecond

**To update**

- **Overview re-scans the whole request table every five seconds.**
  `views/overview.js` refreshes on a 5s timer, and `stats_summary` answers it
  with three full aggregates (`all` is `since = 0, until = i64::MAX`) plus
  three 20,000-row percentile scans. On a phone with months of rows, that is
  the single most expensive thing the dashboard does, and it runs whether or
  not anything changed. Either roll per-day totals into a table the store's
  writer maintains, or memoise the answer for a few seconds beside the
  counters — the all-time aggregate in particular cannot change faster than
  the writer commits.
- **Statements are re-prepared almost everywhere.** Two call sites use
  `prepare_cached`; sixteen use `prepare`. The writer re-prepares its INSERTs
  once per batch and the dashboard's stats queries re-prepare per call. This
  is a mechanical change with a measurable payoff on SQLite.
- **The pragmas stop at three.** `journal_mode = WAL`, `synchronous =
  NORMAL`, `busy_timeout = 5000` (`src/store/mod.rs`). No `mmap_size`, no
  `cache_size`, and nothing ever runs `PRAGMA optimize` or `ANALYZE` — so as
  `requests` and `usage_ledger` grow, the planner keeps working from
  statistics gathered when the tables were empty. Nothing checkpoints the WAL
  on a schedule either.
- **Prune never gives the space back.** The prune deletes rows and no
  `VACUUM` follows, so `relay.db` only ever grows on disk however much is
  deleted inside it. On a phone that is the whole point of pruning.
- **`SseParser::push` allocates per event.** It copies the front of the
  buffer into a fresh `String` and drains the buffer once per event — O(buffer)
  work and one allocation for every delta of every stream. Scanning by byte
  offset and draining once per batch produces exactly the same events.

**To add**

- **A benchmark, so this list stops being an argument from reading.** There
  is no `cargo bench`, no criterion, no load-test script. Two harnesses would
  make every entry above decidable: a fixed transcript through
  `count_chat_request`, and a mock backend streaming N deltas through
  `transform` and `pace`. Verify in **debug** as well as release — CI runs
  debug, and the tokenizer paths are where the difference is largest.
- **A ceiling on memory, not just on one body.** `maxBodyBytes` is 20 MB and
  `maxConcurrentRequests` is 512. Nothing tracks the product, and the
  reshaping in `transform.rs` holds more than one copy of a body at a time.
  Measure it first — the number may be fine — but right now nobody knows what
  512 large concurrent requests costs, and the phone finds out by being
  OOM-killed.
- **Record what the binary costs.** `release-small` exists because a 4 GB
  phone cannot build anything larger, and nothing tracks binary size or peak
  build RSS across commits. A dependency that pushes the build past the
  device's memory is currently discovered by the build being killed halfway
  through, on the device, by the person who least wants to debug it.
