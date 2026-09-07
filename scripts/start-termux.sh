#!/data/data/com.termux/files/usr/bin/bash
#
# Start the relay with a wake lock so Android does not suspend it when the
# screen goes off. Ctrl-C stops the relay and releases the lock.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="./target/release/chtting-relay"
if [ ! -x "$BIN" ]; then
  echo "not built yet — run: bash scripts/install-termux.sh" >&2
  exit 1
fi

if command -v termux-wake-lock >/dev/null 2>&1; then
  termux-wake-lock
  trap 'termux-wake-unlock >/dev/null 2>&1 || true' EXIT
  echo "wake lock acquired — the relay keeps running with the screen off"
fi

exec "$BIN" start "$@"
