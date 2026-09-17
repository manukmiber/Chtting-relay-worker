#!/data/data/com.termux/files/usr/bin/bash
#
# Install the relay on a phone, and update it, with the same one line:
#
#   pkg install git
#   git clone https://github.com/manukmiber/Chtting-relay-worker
#   cd Chtting-relay-worker && bash install.sh
#
# Every run does the same four things in the same order — pull the newest
# commits, upgrade Termux's packages, build, then start — so there is no
# separate update command to remember. Re-running it is the update.
#
#   bash install.sh --no-start   # set up but do not launch
#   bash install.sh --no-pull    # build what is checked out; touch no network
set -euo pipefail

# Resolved before the `cd`, because the re-exec below needs a path that still
# means something from the new working directory.
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
cd "$(dirname "$SELF")"
ROOT="$(pwd)"

PULL=1
START=1
for arg in "$@"; do
  case "$arg" in
    --no-start) START=0 ;;
    --no-pull)  PULL=0 ;;
    # There is no prebuilt shortcut any more: building is what this does.
    --build)    ;;
  esac
done

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

if [ -z "${PREFIX:-}" ] || [[ "$PREFIX" != *com.termux* ]]; then
  echo "This script is for Termux. On a desktop just run: cargo build --release" >&2
  exit 1
fi

# Where a build puts the binary depends on which profile the device could
# manage, so all three are looked for and the newest one wins — the same rule
# start-termux.sh and the keeper use.
BIN="$ROOT/target/release/chtting-relay"
resolve_bin() {
  local newest=""
  for candidate in \
    "$ROOT/target/release-fast/chtting-relay" \
    "$ROOT/target/release/chtting-relay" \
    "$ROOT/target/release-small/chtting-relay"
  do
    [ -x "$candidate" ] || continue
    if [ -z "$newest" ] || [ "$candidate" -nt "$newest" ]; then newest="$candidate"; fi
  done
  # Written as an `if` rather than `[ ... ] && BIN=...`: under `set -e` a test
  # that comes out false as the last command in a function ends the script.
  if [ -n "$newest" ]; then BIN="$newest"; fi
}

# --- 1. the newest source ---------------------------------------------------
say "1/4  newest source"

# A pull can rewrite this very file while bash is part-way through reading it —
# bash reads a script in blocks, not all at once, so a file that changes
# underneath it runs nonsense from the middle. Hash the script, pull, and if the
# hash moved, hand over to the copy that was just pulled instead of carrying on
# inside the old one. `CHTTING_INSTALL_REEXEC` makes that a handover and not a
# loop.
self_hash() { sha256sum "$SELF" 2>/dev/null | awk '{print $1}'; }

pull_newest() {
  if [ -n "${CHTTING_INSTALL_REEXEC:-}" ]; then
    echo "already pulled — continuing in the updated installer"
    return 0
  fi
  if [ "$PULL" = 0 ]; then
    echo "--no-pull: building whatever is checked out"
    return 0
  fi
  if [ ! -d "$ROOT/.git" ]; then
    echo "not a git checkout, so there is nothing to pull"
    return 0
  fi
  command -v git >/dev/null 2>&1 || pkg install -y git

  local before after
  before="$(self_hash)"
  # `--ff-only`, the same as the dashboard's own update button: a phone is not
  # the place to invent a merge commit. Nothing of the operator's is at risk —
  # config/config.json, data/ and the logs are all gitignored, so a pull cannot
  # touch the live config, the database or the keys.
  if ! git -C "$ROOT" pull --ff-only; then
    echo
    echo "could not fast-forward — building what is checked out instead." >&2
    echo "  (local commits or edits in the way? 'git status' says which.)" >&2
    return 0
  fi
  after="$(self_hash)"
  if [ -n "$before" ] && [ -n "$after" ] && [ "$before" != "$after" ]; then
    say "the installer itself changed — restarting into the new one"
    CHTTING_INSTALL_REEXEC=1 exec bash "$SELF" "$@"
  fi
}
pull_newest "$@"

# --- 2. Termux's own packages ----------------------------------------------
say "2/4  Termux packages"

# Refreshing and upgrading are best-effort: a mirror having a bad day is not a
# reason to refuse to build with a toolchain that is already on the device.
# Installing the build dependencies is not best-effort — there is no compiling
# without them, so that one is allowed to end the script.
export DEBIAN_FRONTEND=noninteractive
pkg update -y  || echo "  (could not refresh the package lists — continuing)"
pkg upgrade -y || echo "  (upgrade skipped — continuing with what is installed)"

# rust brings cargo and rustc; clang is what `ring` and the bundled SQLite
# compile with. Nothing here needs cmake, Go or a C++ compiler: the TLS stack is
# `ring` rather than aws-lc, and the tokenizer crate is built without its C++
# suffix-array backend.
pkg install -y git rust clang binutils pkg-config

# --- 3. build ---------------------------------------------------------------
say "3/4  building (5 to 15 minutes the first time; less after that)"

# A phone has limited RAM, and the linker is where a build gets killed. Small
# devices get the release-small profile — one codegen unit, no LTO, optimised
# for size — which trades a little speed for a build that finishes. Whichever
# profile is chosen here is the one the dashboard's update button will keep
# rebuilding with, because it reads the profile off the running binary's own
# directory.
if [ "$(nproc)" -le 4 ] || [ "$(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 9999)" -lt 4096 ]; then
  echo "small device — building for size, one job at a time"
  cargo build --profile release-small -j1
else
  cargo build --release
fi
resolve_bin

if [ ! -x "$BIN" ]; then
  echo "the build finished but there is no binary at $BIN" >&2
  exit 1
fi
echo
echo "ready: $(du -h "$BIN" | cut -f1) at $BIN"
"$BIN" --version

# --- 4. config, first key, and the phone -----------------------------------
say "4/4  config, your first client key, and wiring it into the phone"

"$BIN" config path >/dev/null   # creates it on first run
if [ "$("$BIN" key list | grep -c . || true)" -le 1 ]; then
  echo "your first client key (save it — it is shown once):"
  "$BIN" key new --label "first key"
else
  echo "keys already exist; the dashboard lists them under Keys"
fi

# The keeper, the home-screen shortcuts and the boot hook. All three are also
# buttons on the dashboard's Setup screen, so nothing here is a one-way door.
#
# There is no `pkg install termux-services` any more: that package is gone from
# Termux's repositories, so the relay supervises itself with a small shell loop
# the Setup screen writes.
"$BIN" setup || echo "  (setup skipped — do it from the dashboard's Setup tab)"

# The default; if you have moved the dashboard you already know where it is.
DASH="http://127.0.0.1:8788"

cat <<EOF

Done. Everything else is in the dashboard:

  $DASH

  Setup          backends, models, tokenizers, the tunnel, start/stop/restart
  Termux:Widget  put Start / Stop / Restart / Open on your home screen
  Termux:Boot    start the relay when the phone does

To update later, run the same line again — it pulls, upgrades, rebuilds and
restarts:

  bash install.sh

And to start it without the shortcuts:

  bash scripts/start-termux.sh
EOF

if [ "$START" = 1 ]; then
  say "starting"
  echo "open $DASH — Ctrl-C here stops the relay"
  exec bash "$ROOT/scripts/start-termux.sh"
fi
