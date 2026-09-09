#!/data/data/com.termux/files/usr/bin/bash
#
# Start the relay. Ctrl-C stops it.
#
# The wake lock that keeps Android from suspending the relay when the screen
# goes off is the relay's own job now — it takes one at startup and releases it
# on the way out, and the dashboard's Setup tab turns that off. This script is
# only here for the case where you would rather type than tap.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="./target/release/chtting-relay"
if [ ! -x "$BIN" ]; then
  echo "not built yet — run: bash scripts/install-termux.sh" >&2
  exit 1
fi

exec "$BIN" start "$@"
