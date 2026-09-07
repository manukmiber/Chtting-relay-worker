#!/data/data/com.termux/files/usr/bin/bash
#
# One-shot setup for Termux.
#
#   pkg install git
#   git clone https://github.com/manukmiber/Chtting-relay-worker
#   cd Chtting-relay-worker && bash scripts/install-termux.sh
#
# Installs the Rust toolchain, builds the relay, offers to install cloudflared
# and a tokenizer or two, writes a config and prints your first client key.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BIN="$ROOT/target/release/chtting-relay"

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

say "1/5  packages"
# rust brings cargo and rustc; clang is what `ring` and bundled SQLite compile with.
pkg install -y rust clang binutils pkg-config

say "2/5  building (this takes a while on a phone — 5 to 15 minutes)"
# A phone has limited RAM; one codegen job at a time is slower but survives.
if [ "$(nproc)" -le 4 ]; then
  echo "few cores detected — building with a single job to stay within memory"
  cargo build --release -j1
else
  cargo build --release
fi

if [ ! -x "$BIN" ]; then
  echo "build did not produce $BIN" >&2
  exit 1
fi
say "built $(du -h "$BIN" | cut -f1) binary at $BIN"

say "3/5  cloudflared"
if command -v cloudflared >/dev/null 2>&1; then
  echo "already installed: $(cloudflared --version 2>&1 | head -1)"
elif ask "Install cloudflared for the public tunnel?"; then
  pkg install -y cloudflared || echo "could not install cloudflared; the relay still works locally"
fi

say "4/5  tokenizers"
echo "OpenAI vocabularies (cl100k_base, o200k_base, ...) are built into the binary."
echo "Open-model vocabularies are a download each:"
"$BIN" tokenizer list | sed -n '/downloadable presets/,/installed in/p' | head -12
if ask "Download the DeepSeek and Qwen vocabularies now (~15 MB)?"; then
  "$BIN" tokenizer install deepseek || true
  "$BIN" tokenizer install qwen || true
fi

say "5/5  config"
"$BIN" config path >/dev/null   # creates it on first run
if [ "$("$BIN" key list | grep -c . || true)" -le 1 ]; then
  echo "your first client key (save it — it is shown once):"
  "$BIN" key new --label "first key"
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
