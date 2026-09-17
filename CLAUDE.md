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

## One panel, two mounts: `/dashboard` on the relay port

`dashboard.publishOnRelay` answers the panel under `RELAY_MOUNT` on the **relay's**
port, so the cloudflared already publishing the relay carries both and there is
no second process. `server::public::router` always `nest_service`s it;
`dashboard::publish_gate` decides per request whether it answers at all.

- **The gate is two checks, and they are not interchangeable.**
  `blocked_on_relay` is the switch, `dashboard.enabled`, `--no-dashboard` and the
  16-character password; the second is the `Host`, which must be one of this
  machine's names, a live tunnel's hostname, or `dashboardAllowedHosts`. The
  relay port is bound to `0.0.0.0`, so without the host check the panel would be
  served over plain HTTP on the phone's Wi-Fi address. That check deliberately
  does **not** consult `dashboardOriginGuard` — the operator may switch that off.
- **A refusal is the relay's own 404, word for word** (`no route for that path`).
  This path sits on the URL handed to callers; "there is a panel here, it is just
  off" is not something to publish. The reason goes to the log at `debug` and to
  the Tunnel tab, not into the response.
- **Sessions, the login throttle and the reset code live on `AppState` as
  `Panel`,** not on the router, precisely because there are two routers now. Two
  copies would mean a sign-out that does not sign out, twice the guessing budget,
  and a reset code the other door has never heard of.
- **The frontend is mount-relative and must stay that way.** `index.html` links
  `css/app.css` and `js/app.js` without a leading slash, and `api.js` resolves
  every path against `new URL('.', location.href)`. The one redirect in
  `static_files` (`/dashboard` → `/dashboard/`, mount root only) is what gives
  those relative URLs a directory to resolve against. An absolute `/api/...`
  anywhere in `public/` works on the loopback port and breaks on the relay's.
- **The session cookie's `Path` is the mount**, derived from `OriginalUri` (the
  axum `original-uri` feature is enabled for exactly this). `Path=/` would attach
  the operator's session to every API call sharing that origin. `logout` must
  clear it with the same `Path` or the cookie outlives the sign-out.
- `openrouter.path` is refused when it collides with the mount: two routes on one
  path is a startup panic, and no config value may be able to cause one.

## The dashboard can now be published, which changes what "loopback" bought

`dashboard.tunnel` is a second cloudflared publishing `dashboard.port`. Before
it existed, several things were true because only this device could reach the
panel; they are not true any more, and the code that depends on them is worth
knowing before changing it:

- **`tunnel::TunnelManager` has a `Scope`.** Each manager reads its port from
  its own scope, never from an argument, so no configuration can make one
  tunnel publish the other's port. Keep it that way. (`publishOnRelay` does not
  bend this: it adds a path to the relay's own server, and no tunnel changes
  which port it publishes.)
- **`TunnelManager::start` refuses the dashboard scope without a password of
  `config::MIN_REMOTE_PASSWORD` characters.** The check is at start rather than
  at save, because a config that *describes* a dashboard tunnel is fine to
  store; the moment that matters is the moment a process begins accepting from
  the internet.
- **The origin guard is widened, not disabled.** `answers_to` accepts this
  machine's names, **either** tunnel's live hostname, and
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

## The password reset proves the phone, and may never prove anything else

`src/reset.rs` mints a six-digit code and shows it **only on the device**: the
relay's stderr banner and `chtting-relay reset-code`. The answer to
`POST /api/password-reset` carries the challenge id and never the code. The
moment it carries anything a browser could answer with, this stops being a way
back in and becomes a way in.

- **Six digits is safe because of the guessing budget, not the length.** One
  live challenge at a time, ten minutes, five wrong answers and the code is
  destroyed rather than merely refused. Loosening any of those three — an
  attempt counter that resets, a challenge per browser, a longer TTL — is what
  turns a million-to-five into a login somebody can guess.
- **Starting twice hands back the live code instead of minting a new one**, and
  a genuinely new code is rate-limited to one per 30s. That is the only thing
  stopping a stranger from filling the operator's terminal with reset banners,
  which is both noise and a way to hide the real one.
- **The banner goes to stderr with `eprint!`, not through the logger.** The
  logger writes `relay.log`, which the dashboard hands to anyone who is signed
  in, and it obeys `logging.level` — a reset code must be neither hidden by
  `silent` nor filed where a session can read it.
- `data/run/password-reset.json` (0600) exists because the keeper starts the
  relay with stderr pointed at `/dev/null`, so the banner reaches nobody. It is
  not a new exposure: `config/config.json` in the same directory holds the
  dashboard password itself in the clear. It is cleared at startup, because the
  challenge only ever lived in the memory of the process that minted it.
- **Confirming enforces `config::MIN_REMOTE_PASSWORD` when the dashboard tunnel
  is up or the request arrived under a non-local `Host`.** `TunnelManager::
  start` checks that at start only, so without the same check here the reset is
  the way around it.
- A successful reset clears **every session and every login lockout**. Sessions
  were opened under the old password, and being locked out by the sign-in
  throttle is half the reason to be on that screen.
- Both routes are in `guard`'s `open` list, like sign-in. They have to be: not
  being able to sign in is the premise.

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

## `install.sh` is at the repo root, rewrites itself, and always compiles

One script installs and updates: `bash install.sh` does `git pull --ff-only`
→ `pkg update` + `pkg upgrade` + build deps → `cargo build` → config, key,
`setup`, start. Re-running it *is* the update, so there is no second command
and nothing to keep in sync with one.

- **It re-execs itself after a pull that changed it.** Bash reads a script in
  blocks rather than all at once, so a `git pull` that rewrites the file the
  interpreter is still reading runs nonsense from the middle of the new copy.
  The script sha256s itself before the pull, compares after, and hands over
  with `CHTTING_INSTALL_REEXEC=1 exec bash "$SELF" "$@"` when the hash moved —
  which is also what stops that handover being a loop. `$SELF` is resolved to
  an absolute path *before* the `cd`, because a relative `$0` stops meaning
  anything once the working directory changes. Do not "simplify" any of those
  three away.
- **A failed pull is not a failed install.** A checkout with local commits
  cannot fast-forward; the script says so and builds what is there. Only the
  build-dependency install is allowed to end the run — `pkg update` and
  `pkg upgrade` are best-effort, because a bad mirror is not a reason to refuse
  to build with the toolchain already on the device.
- **There is no prebuilt-download path any more, and its absence is deliberate.**
  The old script asked the GitHub releases API for an asset matching the
  device's triple, verified a checksum and untarred it. That was the bulk of the
  script and it is gone: installing compiles. The release workflow still
  publishes the tarballs, for dropping a binary in by hand — the release notes
  say so, and they must not go back to claiming the installer uses them.
- **The profile the script picks is the profile the device keeps.**
  `release-small` with `-j1` under 4 cores or 4 GB, `release` otherwise. The
  dashboard's update button rebuilds with whatever profile the *running* binary
  sits in (`Updater::profile()`), so changing this heuristic changes which
  directory a phone rebuilds into from then on.
- Runtime state is what makes the pull safe: `config/config.json`, `data/` and
  the logs are gitignored, so no pull can touch the live config, the database
  or the keys.

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
