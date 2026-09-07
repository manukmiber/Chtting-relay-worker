#!/data/data/com.termux/files/usr/bin/bash
#
# One-shot setup for chtting-relay on Termux.
#
#   bash scripts/install-termux.sh
#
# Installs Node and cloudflared, downloads the tokenizer vocabularies you
# choose, writes a starting config and prints your first client key.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

bold()  { printf '\033[1m%s\033[0m\n' "$*"; }
info()  { printf '  %s\n' "$*"; }
ok()    { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '  \033[33m!\033[0m %s\n' "$*"; }
fail()  { printf '  \033[31m✗\033[0m %s\n' "$*"; exit 1; }

echo
bold "chtting-relay — Termux setup"
echo

if [ -z "${PREFIX:-}" ] || [ "${PREFIX#*com.termux}" = "$PREFIX" ]; then
  warn "This does not look like Termux. Continuing anyway — nothing here is destructive."
fi

# ---------------------------------------------------------------- packages --
bold "1. Packages"
if command -v pkg >/dev/null 2>&1; then
  pkg update -y >/dev/null 2>&1 || warn "pkg update failed; continuing with what is installed"
fi

if ! command -v node >/dev/null 2>&1; then
  info "installing nodejs…"
  pkg install -y nodejs || pkg install -y nodejs-lts || fail "could not install nodejs"
fi

NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
if [ "$NODE_MAJOR" -lt 20 ]; then
  fail "Node $NODE_MAJOR is too old — chtting-relay needs Node 20 or newer (22+ recommended for the built-in SQLite)."
fi
ok "node $(node -v)"

if node -e "require('node:sqlite')" >/dev/null 2>&1; then
  ok "node:sqlite available — metrics go into a real database"
else
  warn "node:sqlite missing (Node < 22) — metrics fall back to an append-only JSONL file"
fi

if command -v cloudflared >/dev/null 2>&1; then
  ok "cloudflared $(cloudflared --version 2>/dev/null | head -1)"
else
  info "installing cloudflared…"
  if pkg install -y cloudflared >/dev/null 2>&1; then
    ok "cloudflared installed"
  else
    warn "cloudflared is not in your Termux repo — install it manually if you want a public URL:"
    warn "  https://github.com/cloudflare/cloudflared/releases (pick the linux-arm64 build)"
  fi
fi

# There are no runtime dependencies, but running install keeps npm happy if
# you later add any of your own.
if [ -f package.json ] && command -v npm >/dev/null 2>&1; then
  npm install --no-audit --no-fund >/dev/null 2>&1 || true
fi

# -------------------------------------------------------------- tokenizers --
echo
bold "2. Tokenizer vocabularies"
info "Exact token counts need the vocabulary your backend model actually uses."
info "Without one the relay still works, but counts are estimates."
echo
info "  1) cl100k_base + o200k_base   (GPT-family, ~5 MB)"
info "  2) the above + DeepSeek + Qwen (~19 MB)"
info "  3) skip for now"
echo
read -r -p "  choice [1/2/3] (default 2): " TOKCHOICE || TOKCHOICE=""
TOKCHOICE="${TOKCHOICE:-2}"

case "$TOKCHOICE" in
  1) node scripts/fetch-tokenizer.mjs cl100k_base o200k_base || warn "download failed — you can retry from the dashboard" ;;
  2) node scripts/fetch-tokenizer.mjs cl100k_base o200k_base deepseek qwen || warn "download failed — you can retry from the dashboard" ;;
  *) info "skipped — install them later from the dashboard's Tokenizer tab" ;;
esac

# ------------------------------------------------------------------ config --
echo
bold "3. Configuration"
CONFIG="$ROOT/config/config.json"
if [ -f "$CONFIG" ]; then
  ok "config already exists at $CONFIG (left untouched)"
else
  node src/cli.js config path >/dev/null   # creates the default file
  ok "wrote $CONFIG"
fi

if [ "$(node -p "require('$CONFIG').keys.length")" = "0" ]; then
  node src/cli.js key new "termux" || warn "could not create a client key"
else
  ok "client key(s) already configured"
fi

# ------------------------------------------------------------------- boot --
echo
bold "4. Autostart (optional)"
BOOT_DIR="$HOME/.termux/boot"
if [ -d "$BOOT_DIR" ]; then
  cat > "$BOOT_DIR/chtting-relay.sh" <<BOOTEOF
#!/data/data/com.termux/files/usr/bin/sh
termux-wake-lock
exec node "$ROOT/src/cli.js" start
BOOTEOF
  chmod +x "$BOOT_DIR/chtting-relay.sh"
  ok "installed $BOOT_DIR/chtting-relay.sh — the relay starts when your phone boots"
else
  info "install the Termux:Boot app and run this script again to start the relay on boot"
fi

# ------------------------------------------------------------------- done --
echo
node src/cli.js doctor
bold "Next steps"
info "start it:      bash scripts/start-termux.sh"
info "dashboard:     http://127.0.0.1:8788   (open it in your phone's browser)"
info "relay API:     http://127.0.0.1:8787/v1"
echo
info "Then, in the dashboard: add a Backend, add a Model alias, and press Start"
info "on the Tunnel tab to get a public https URL."
echo
