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

## Operational hazard: never run a second relay instance by hand

`SO_REUSEPORT` makes the hourly self-rotation (`src/rotate.rs`) seamless,
but it also means a second `chtting-relay start` started by hand does
**not** fail to bind — both processes listen on the same port and the
kernel splits new connections between them at random. The older process
keeps answering out of whichever config it started with, so a freshly
minted API key looks like it "doesn't work" on a random fraction of
requests while working fine on others. This class of bug reads exactly
like a broken key; it is not one — check `pgrep -f chtting-relay` / how
many processes are listening before assuming a key or auth bug.

As of the change that added `data/run/serving.pid` (see PR #11), `start`
refuses to run while a live relay already holds that lock, unless invoked
by the rotation successor (which carries a generation number) or with
`--replace`. If debugging a report of "the key doesn't work," check for
multiple live relay processes / stale `serving.pid` owners first.

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
