#!/usr/bin/env bash
#
# Cross-compile the relay for Android/Termux from a desktop, so a phone does
# not have to spend 5-15 minutes compiling it.
#
#   ANDROID_NDK_HOME=~/android-ndk-r27c bash scripts/build-android.sh
#   bash scripts/build-android.sh armv7            # 32-bit device
#   bash scripts/build-android.sh aarch64 armv7    # both
#
# Add --tune to build for one specific chip instead of the baseline every
# arm64 Android device shares:
#
#   bash scripts/build-android.sh --tune cortex-x925     # Dimensity 9400/9400+
#   CHTTING_PROFILE=release-fast bash scripts/build-android.sh --tune cortex-x925
#
# What that buys, and what it costs:
#
#   The default arm64 target is ARMv8.0 with nothing optional assumed, because
#   the binary has to run on whatever the person copying it owns. Naming the CPU
#   lets the compiler use what that chip actually has — on an ARMv9.2 core that
#   means the crypto extensions (the relay SHA-256s every presented client key,
#   chains every ledger row and hashes every invoice), the dot-product and
#   8-bit matrix instructions, and a scheduling model for the right pipeline.
#
#   The cost is that the result runs on that chip and chips like it, and nothing
#   older. A binary built with --tune and copied onto a device without those
#   instructions does not fail gracefully — it takes SIGILL. So it is opt-in, and
#   the untuned build stays the default.
#
#   On a big.LITTLE part, name any core in the cluster set: every core in a given
#   SoC implements the same architecture version, so the choice changes the
#   scheduling model rather than the instructions available. The Dimensity 9400+
#   is 1x Cortex-X925 + 3x Cortex-X4 + 4x Cortex-A720, all ARMv9.2-A, so
#   `cortex-x925` is safe across all eight.
#
#   `rustc --print target-cpus --target aarch64-linux-android` lists the names
#   this toolchain knows.
#
# Copy the result to the phone and run it — it needs no Rust toolchain there:
#   adb push target/aarch64-linux-android/release/chtting-relay /sdcard/
#   # then in Termux: cp /sdcard/chtting-relay . && chmod +x chtting-relay
set -euo pipefail
cd "$(dirname "$0")/.."

# Termux itself requires Android 7.0, so there is no reason to build for older.
API="${ANDROID_API_LEVEL:-24}"

# --tune <cpu>, anywhere in the arguments.
TUNE=""
ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --tune)
      TUNE="${2:-}"
      [ -n "$TUNE" ] || { echo "--tune needs a CPU name, e.g. --tune cortex-x925" >&2; exit 1; }
      shift 2
      ;;
    --tune=*) TUNE="${1#--tune=}"; shift ;;
    *) ARGS+=("$1"); shift ;;
  esac
done
set -- "${ARGS[@]+"${ARGS[@]}"}"

# `release` unless asked otherwise. `release-fast` is fat LTO and one codegen
# unit — a few minutes and several gigabytes, for a device that has both.
PROFILE="${CHTTING_PROFILE:-release}"

find_ndk() {
  for candidate in \
    "${ANDROID_NDK_HOME:-}" \
    "${ANDROID_NDK_ROOT:-}" \
    "${ANDROID_NDK_LATEST_HOME:-}" \
    "$HOME/Android/Sdk/ndk"/* \
    "$HOME/Library/Android/sdk/ndk"/* \
    /usr/local/lib/android/sdk/ndk/*
  do
    [ -n "$candidate" ] && [ -d "$candidate/toolchains/llvm/prebuilt" ] && {
      echo "$candidate"
      return 0
    }
  done
  return 1
}

NDK="$(find_ndk)" || {
  cat >&2 <<'EOF'
Android NDK not found.

Set ANDROID_NDK_HOME to an unpacked NDK, or install one:
  https://developer.android.com/ndk/downloads

  curl -LO https://dl.google.com/android/repository/android-ndk-r27c-linux.zip
  unzip -q android-ndk-r27c-linux.zip
  export ANDROID_NDK_HOME="$PWD/android-ndk-r27c"
EOF
  exit 1
}

# The prebuilt directory is named for the *host*, not the target.
case "$(uname -s)" in
  Darwin) HOST_TAG="darwin-x86_64" ;;
  *)      HOST_TAG="linux-x86_64" ;;
esac
TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG"
BIN="$TOOLCHAIN/bin"
[ -d "$BIN" ] || { echo "no toolchain at $BIN" >&2; exit 1; }

echo "NDK        $NDK"
echo "API level  $API"
echo "profile    $PROFILE"
[ -n "$TUNE" ] && echo "target-cpu $TUNE"

build_one() {
  local arch="$1" target clang_prefix
  case "$arch" in
    aarch64|arm64) target="aarch64-linux-android";     clang_prefix="aarch64-linux-android" ;;
    armv7|arm)     target="armv7-linux-androideabi";   clang_prefix="armv7a-linux-androideabi" ;;
    x86_64)        target="x86_64-linux-android";      clang_prefix="x86_64-linux-android" ;;
    *) echo "unknown architecture \"$arch\" (try aarch64, armv7 or x86_64)" >&2; return 1 ;;
  esac

  local cc="$BIN/${clang_prefix}${API}-clang"
  [ -x "$cc" ] || { echo "no compiler at $cc — is API level $API available in this NDK?" >&2; return 1; }

  echo
  echo "=== $target ==="
  rustup target add "$target" >/dev/null 2>&1 || true

  # Cargo wants the linker under a target-shaped variable name; the `cc` crate
  # (which builds ring and the bundled SQLite) wants CC/AR for the same target.
  local upper
  upper="$(echo "$target" | tr '[:lower:]-' '[:upper:]_')"

  # Appended to whatever the caller already set rather than replacing it, so
  # RUSTFLAGS from the environment still applies.
  local rustflags="${RUSTFLAGS:-}"
  if [ -n "$TUNE" ]; then
    case "$arch" in
      aarch64|arm64) ;;
      *) echo "--tune is only meaningful for aarch64; ignoring it for $target" >&2; TUNE="" ;;
    esac
  fi
  if [ -n "$TUNE" ]; then
    rustc --print target-cpus --target "$target" 2>/dev/null | grep -qx "[[:space:]]*$TUNE" || {
      echo "this toolchain does not know a CPU called \"$TUNE\"." >&2
      echo "run: rustc --print target-cpus --target $target" >&2
      return 1
    }
    rustflags="$rustflags -C target-cpu=$TUNE"
    echo "tuning for $TUNE — the result will NOT run on an older arm64 device"
  fi

  env \
    "CARGO_TARGET_${upper}_LINKER=$cc" \
    "CC_${target}=$cc" \
    "CXX_${target}=$BIN/${clang_prefix}${API}-clang++" \
    "AR_${target}=$BIN/llvm-ar" \
    "RANLIB_${target}=$BIN/llvm-ranlib" \
    "RUSTFLAGS=$rustflags" \
    cargo build --profile "$PROFILE" --target "$target"

  local out="target/$target/$PROFILE/chtting-relay"
  "$BIN/llvm-strip" "$out" 2>/dev/null || true
  echo "built $out  ($(du -h "$out" | cut -f1))"
  file "$out" 2>/dev/null || true
}

ARCHES=("$@")
[ ${#ARCHES[@]} -eq 0 ] && ARCHES=(aarch64)
for arch in "${ARCHES[@]}"; do
  build_one "$arch"
done
