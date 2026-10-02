#!/usr/bin/env bash
# Provision the local whisper.cpp binary and medium.en model for Telegram
# voice capture. Both artifacts are pinned and verified before use.
set -euo pipefail
umask 077

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="$REPO_ROOT/vendor/whisper"
SRC="$VENDOR/src"
MODEL_DIR="$VENDOR/models"
MODEL="$MODEL_DIR/ggml-medium.en.bin"
BIN="$VENDOR/main"
STAMP="$VENDOR/source-commit"
WHISPER_REPO='https://github.com/ggml-org/whisper.cpp.git'
WHISPER_TAG='v1.9.4'
WHISPER_COMMIT='927cfce34f31707e17f2bff35c349632fb9e2c3a'
MODEL_REV='5359861c739e955e79d9a303bcbc70fb988958b1'
MODEL_SHA256='cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356'

fail() { printf '[build-whisper] verification failed: %s\n' "$*" >&2; exit 1; }

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail 'sha256sum or shasum is required'
  fi
}

verify_runtime() {
  [ -x "$BIN" ] || fail 'whisper binary is missing'
  [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$WHISPER_COMMIT" ] \
    || fail 'whisper binary provenance is missing or mismatched'
  [ -f "$MODEL" ] || fail 'medium.en model is missing'
  [ "$(file_sha256 "$MODEL")" = "$MODEL_SHA256" ] \
    || fail 'medium.en model checksum differs from the pinned model'
  "$BIN" -h >/dev/null 2>&1 || fail 'whisper binary cannot start on this host'
}

case "${1:-}" in
  --verify-only) verify_runtime; printf '[build-whisper] pinned runtime verified\n'; exit 0 ;;
  '') ;;
  *) printf 'usage: %s [--verify-only]\n' "$0" >&2; exit 2 ;;
esac

if [ -x "$BIN" ] && [ -f "$STAMP" ] && [ -f "$MODEL" ]; then
  verify_runtime
  printf '[build-whisper] pinned runtime already installed\n'
  exit 0
fi

for program in git cmake curl; do
  command -v "$program" >/dev/null 2>&1 || fail "$program is required"
done
jobs="${AUGMENTAGENT_WHISPER_BUILD_JOBS:-2}"
[[ "$jobs" =~ ^[1-9][0-9]*$ ]] && [ "$jobs" -le 8 ] \
  || fail 'AUGMENTAGENT_WHISPER_BUILD_JOBS must be between 1 and 8'

mkdir -p "$VENDOR" "$MODEL_DIR"
if [ -e "$SRC" ] && [ ! -d "$SRC/.git" ]; then
  fail "$SRC exists but is not a whisper.cpp checkout"
fi
if [ ! -d "$SRC/.git" ]; then
  git clone --depth 1 --branch "$WHISPER_TAG" "$WHISPER_REPO" "$SRC"
fi
[ "$(git -C "$SRC" rev-parse HEAD)" = "$WHISPER_COMMIT" ] \
  || fail "whisper.cpp checkout is not pinned $WHISPER_TAG ($WHISPER_COMMIT)"

if [ ! -f "$MODEL" ]; then
  partial="$(mktemp "$MODEL_DIR/.ggml-medium.en.bin.XXXXXX")"
  trap 'rm -f "$partial"' EXIT
  curl --fail --location --retry 3 \
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/$MODEL_REV/ggml-medium.en.bin" \
    --output "$partial"
  [ "$(file_sha256 "$partial")" = "$MODEL_SHA256" ] \
    || fail 'downloaded medium.en model checksum differs from the pinned model'
  mv "$partial" "$MODEL"
  trap - EXIT
fi
[ "$(file_sha256 "$MODEL")" = "$MODEL_SHA256" ] \
  || fail 'existing medium.en model checksum differs from the pinned model'

cmake -S "$SRC" -B "$SRC/build" -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF
cmake --build "$SRC/build" --config Release --parallel "$jobs"
built="$SRC/build/bin/whisper-cli"
[ -x "$built" ] || fail 'CMake did not produce whisper-cli'
cp "$built" "$BIN.new"
chmod 0755 "$BIN.new"
"$BIN.new" -h >/dev/null 2>&1 || fail 'built whisper-cli cannot start on this host'
mv "$BIN.new" "$BIN"
printf '%s\n' "$WHISPER_COMMIT" > "$STAMP.new"
mv "$STAMP.new" "$STAMP"
verify_runtime
printf '[build-whisper] pinned runtime ready at %s\n' "$VENDOR"
