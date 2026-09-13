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

# A small device builds with the release-small profile, which lands elsewhere;
# take whichever of the two was built most recently.
BIN=""
for candidate in ./target/release/chtting-relay ./target/release-small/chtting-relay; do
  [ -x "$candidate" ] || continue
  if [ -z "$BIN" ] || [ "$candidate" -nt "$BIN" ]; then BIN="$candidate"; fi
done
if [ -z "$BIN" ]; then
  echo "not built yet — run: bash scripts/install-termux.sh" >&2
  exit 1
fi

exec "$BIN" start "$@"
