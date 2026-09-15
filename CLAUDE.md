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

## Repo conventions worth knowing

- Rust workspace, single binary `chtting-relay` (`src/main.rs`) +
  library crate `chtting_relay` (`src/lib.rs`).
- CI runs in **debug** profile (`cargo test --all-targets`, not
  `--release`), so verify locally in debug to match CI exactly, not just
  in release.
- `cargo clippy --all-targets -- -D warnings` — clippy warnings are hard
  failures in CI (once CI runs at all).
- No `.claude/skills/steward/SKILL.md` or `.claude/skills/babysit/SKILL.md`
  exist in this repo as of 2026-09-14 (checked when handling PR events).
- No PR template exists (`.github/pull_request_template.md` etc. — none
  found).
