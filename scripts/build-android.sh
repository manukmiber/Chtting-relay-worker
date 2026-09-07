#!/usr/bin/env bash
#
# Cross-compile the relay for Android/Termux from a desktop, so a phone does
# not have to spend 5-15 minutes compiling it.
#
#   ANDROID_NDK_HOME=~/android-ndk-r27c bash scripts/build-android.sh
#   bash scripts/build-android.sh armv7            # 32-bit device
#   bash scripts/build-android.sh aarch64 armv7    # both
#
# Copy the result to the phone and run it — it needs no Rust toolchain there:
#   adb push target/aarch64-linux-android/release/chtting-relay /sdcard/
#   # then in Termux: cp /sdcard/chtting-relay . && chmod +x chtting-relay
set -euo pipefail
cd "$(dirname "$0")/.."

# Termux itself requires Android 7.0, so there is no reason to build for older.
API="${ANDROID_API_LEVEL:-24}"

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
  env \
    "CARGO_TARGET_${upper}_LINKER=$cc" \
    "CC_${target}=$cc" \
    "CXX_${target}=$BIN/${clang_prefix}${API}-clang++" \
    "AR_${target}=$BIN/llvm-ar" \
    "RANLIB_${target}=$BIN/llvm-ranlib" \
    cargo build --release --target "$target"

  local out="target/$target/release/chtting-relay"
  "$BIN/llvm-strip" "$out" 2>/dev/null || true
  echo "built $out  ($(du -h "$out" | cut -f1))"
  file "$out" 2>/dev/null || true
}

ARCHES=("$@")
[ ${#ARCHES[@]} -eq 0 ] && ARCHES=(aarch64)
for arch in "${ARCHES[@]}"; do
  build_one "$arch"
done
