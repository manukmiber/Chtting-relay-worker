#!/data/data/com.termux/files/usr/bin/bash
#
# One-shot setup for Termux. Three commands, then everything else is buttons:
#
#   pkg install git
#   git clone https://github.com/manukmiber/Chtting-relay-worker
#   cd Chtting-relay-worker && bash scripts/install-termux.sh
#
# Downloads a prebuilt binary when one exists for your device, otherwise builds
# from source. Then writes a config, mints your first client key, wires the
# relay into the phone and starts it. Backends, models, tokenizers, the tunnel,
# start, stop and restart all live in the dashboard.
#
#   bash scripts/install-termux.sh --build      # always build, never download
#   bash scripts/install-termux.sh --no-start   # set up but do not launch
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BIN="$ROOT/target/release/chtting-relay"
REPO="manukmiber/Chtting-relay-worker"
FORCE_BUILD=0
START=1
for arg in "$@"; do
  case "$arg" in
    --build)    FORCE_BUILD=1 ;;
    --no-start) START=0 ;;
  esac
done

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

if [ -z "${PREFIX:-}" ] || [[ "$PREFIX" != *com.termux* ]]; then
  echo "This script is for Termux. On a desktop just run: cargo build --release" >&2
  exit 1
fi

# The Rust target triple matching this device.
case "$(uname -m)" in
  aarch64|arm64)   TARGET="aarch64-linux-android" ;;
  armv7l|armv8l|arm) TARGET="armv7-linux-androideabi" ;;
  x86_64)          TARGET="x86_64-linux-android" ;;
  *)               TARGET="" ;;
esac

say "1/4  binary"

# --- try a prebuilt release first ------------------------------------------
download_prebuilt() {
  [ "$FORCE_BUILD" = 1 ] && return 1
  [ -n "$TARGET" ] || { echo "unrecognised CPU $(uname -m); building instead"; return 1; }
  command -v curl >/dev/null 2>&1 || { pkg install -y curl >/dev/null 2>&1 || return 1; }

  echo "looking for a prebuilt binary for $TARGET..."
  local api="https://api.github.com/repos/$REPO/releases/latest"
  local listing
  listing="$(curl -sSL --max-time 60 "$api" 2>/dev/null)" || return 1

  local url sums_url
  url="$(printf '%s' "$listing" | grep -o "https://[^\"]*${TARGET}\.tar\.gz" | head -1)" || true
  sums_url="$(printf '%s' "$listing" | grep -o 'https://[^"]*SHA256SUMS' | head -1)" || true
  [ -n "$url" ] || { echo "no release asset for $TARGET yet"; return 1; }

  # Called from an `if`, so errexit is suspended inside this function: every
  # step that matters has to check for itself.
  local tmp
  tmp="$(mktemp -d)" || return 1
  [ -n "$tmp" ] || return 1
  trap 'rm -rf "$tmp"' RETURN

  echo "downloading $(basename "$url")"
  curl -sSL --max-time 600 -o "$tmp/pkg.tar.gz" "$url" || return 1

  # Verify against the published checksums when they are available. A missing
  # SHA256SUMS is not fatal, but a mismatch always is.
  if [ -n "$sums_url" ] && curl -sSL --max-time 60 -o "$tmp/SHA256SUMS" "$sums_url"; then
    local want got
    want="$(grep -F "$(basename "$url")" "$tmp/SHA256SUMS" | awk '{print $1}' | head -1)"
    got="$(sha256sum "$tmp/pkg.tar.gz" | awk '{print $1}')"
    if [ -n "$want" ] && [ "$want" != "$got" ]; then
      echo "checksum mismatch — refusing this download" >&2
      return 1
    fi
    if [ -n "$want" ]; then echo "checksum verified"; fi
  else
    echo "no checksum file published; continuing without verification"
  fi

  tar -xzf "$tmp/pkg.tar.gz" -C "$tmp" || return 1
  local found
  found="$(find "$tmp" -name chtting-relay -type f | head -1)"
  [ -n "$found" ] || return 1

  mkdir -p "$(dirname "$BIN")" || return 1
  install -m 755 "$found" "$BIN" || return 1
  return 0
}

build_from_source() {
  say "building from source (5 to 15 minutes on a phone)"
  # rust brings cargo and rustc; clang is what `ring` and the bundled SQLite
  # compile with.
  pkg install -y rust clang binutils pkg-config

  # A phone has limited RAM; one codegen job at a time is slower but survives.
  if [ "$(nproc)" -le 4 ]; then
    echo "few cores detected — building with a single job to stay within memory"
    cargo build --release -j1
  else
    cargo build --release
  fi
}

if download_prebuilt; then
  echo "installed a prebuilt binary — no compiling needed"
else
  build_from_source
fi

if [ ! -x "$BIN" ]; then
  echo "no binary at $BIN" >&2
  exit 1
fi
say "ready: $(du -h "$BIN" | cut -f1) at $BIN"
"$BIN" --version

say "2/4  config and your first client key"
"$BIN" config path >/dev/null   # creates it on first run
if [ "$("$BIN" key list | grep -c . || true)" -le 1 ]; then
  echo "your first client key (save it — it is shown once):"
  "$BIN" key new --label "first key"
else
  echo "keys already exist; the dashboard lists them under Keys"
fi

say "3/4  wiring it into the phone"
# The runit service, the home-screen shortcuts and the boot hook. All three are
# also buttons on the dashboard's Setup screen, so nothing here is a one-way
# door.
"$BIN" setup || echo "  (setup skipped — do it from the dashboard's Setup tab)"
pkg install -y termux-services >/dev/null 2>&1 \
  && echo "  termux-services installed" \
  || echo "  termux-services not installed — the Setup tab can do it later"

# The default; if you have moved the dashboard you already know where it is.
DASH="http://127.0.0.1:8788"

cat <<EOF

Done. Everything else is in the dashboard:

  $DASH

  Setup        backends, models, tokenizers, the tunnel, start/stop/restart
  Termux:Widget  put Start / Stop / Restart / Open on your home screen
  Termux:Boot    start the relay when the phone does

The only command you still need is the one below, and only if you skipped the
shortcuts:

  bash scripts/start-termux.sh
EOF

if [ "$START" = 1 ]; then
  say "4/4  starting"
  echo "open $DASH — Ctrl-C here stops the relay"
  exec bash "$ROOT/scripts/start-termux.sh"
fi
