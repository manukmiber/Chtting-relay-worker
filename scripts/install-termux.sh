#!/data/data/com.termux/files/usr/bin/bash
#
# One-shot setup for Termux.
#
#   pkg install git
#   git clone https://github.com/manukmiber/Chtting-relay-worker
#   cd Chtting-relay-worker && bash scripts/install-termux.sh
#
# Downloads a prebuilt binary when one exists for your device, otherwise builds
# from source. Then offers cloudflared and a tokenizer or two, writes a config
# and prints your first client key.
#
#   bash scripts/install-termux.sh --build   # always build, never download
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BIN="$ROOT/target/release/chtting-relay"
REPO="manukmiber/Chtting-relay-worker"
FORCE_BUILD=0
# An `if` rather than `[ ... ] && FORCE_BUILD=1`, whose status would be the
# script's own if it ever ended up as the last statement.
if [ "${1:-}" = "--build" ]; then
  FORCE_BUILD=1
fi

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
ask() {
  local prompt="$1" reply
  read -r -p "$prompt [y/N] " reply || reply=n
  [[ "$reply" =~ ^[Yy]$ ]]
}

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

say "1/5  binary"

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

say "2/5  cloudflared"
if command -v cloudflared >/dev/null 2>&1; then
  echo "already installed: $(cloudflared --version 2>&1 | head -1)"
elif ask "Install cloudflared for the public tunnel?"; then
  pkg install -y cloudflared || echo "could not install cloudflared; the relay still works locally"
fi

say "3/5  tokenizers"
echo "OpenAI vocabularies (cl100k_base, o200k_base, ...) are built into the binary."
echo "Open-model vocabularies are a download each:"
"$BIN" tokenizer list | sed -n '/downloadable presets/,/installed in/p' | head -12
if ask "Download the DeepSeek and Qwen vocabularies now (~15 MB)?"; then
  "$BIN" tokenizer install deepseek || true
  "$BIN" tokenizer install qwen || true
fi

say "4/5  config"
"$BIN" config path >/dev/null   # creates it on first run

say "5/5  client key"
if [ "$("$BIN" key list | grep -c . || true)" -le 1 ]; then
  echo "your first client key (save it — it is shown once):"
  "$BIN" key new --label "first key"
else
  echo "keys already exist; run \`$BIN key list\` to see them"
fi

cat <<EOF

Done.

  start it            bash scripts/start-termux.sh
  dashboard           http://127.0.0.1:8788
  check the install   $BIN doctor
  add a backend       $BIN backend add --name deepseek \\
                        --base-url https://api.deepseek.com/v1 --api-key sk-...
  add a model alias   $BIN model add --id manukmiberai/creative-writer \\
                        --backend <id> --upstream Deepseek-v4-flash-0731

To keep it running in the background:
  pkg install termux-services
  ln -s "$ROOT/scripts/service" \$PREFIX/var/service/chtting-relay
  sv up chtting-relay
EOF
