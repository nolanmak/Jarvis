#!/bin/bash
# Vendor whisper.cpp + the ggml-medium.en model into vendor/whisper/ for the
# voice-capture channel (#80).
#
# Idempotent: re-running skips the clone/build/download when the artifacts
# already exist. The voice listener resolves
#   <repo>/vendor/whisper/main
#   <repo>/vendor/whisper/models/ggml-medium.en.bin
# (see WhisperCppTranscriber::from_repo_root).
#
# Not run by the auto-updater — voice capture is opt-in; run this once by hand
# on a host that wants it.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="$REPO_ROOT/vendor/whisper"
SRC="$VENDOR/src"
MODEL_DIR="$VENDOR/models"
MODEL="$MODEL_DIR/ggml-medium.en.bin"
WHISPER_REPO="https://github.com/ggerganov/whisper.cpp.git"

mkdir -p "$VENDOR" "$MODEL_DIR"

if [ -x "$VENDOR/main" ] && [ -f "$MODEL" ]; then
  echo "[build-whisper] already vendored ($VENDOR/main + model); nothing to do"
  exit 0
fi

if [ ! -d "$SRC/.git" ]; then
  echo "[build-whisper] cloning whisper.cpp"
  git clone --depth 1 "$WHISPER_REPO" "$SRC"
fi

echo "[build-whisper] building (this takes a few minutes)"
make -C "$SRC" -j"$(nproc)"

# whisper.cpp's binary has been `main` historically and `whisper-cli` in
# newer trees. Vendor whichever exists as a stable `main`.
if [ -x "$SRC/main" ]; then
  cp "$SRC/main" "$VENDOR/main"
elif [ -x "$SRC/build/bin/whisper-cli" ]; then
  cp "$SRC/build/bin/whisper-cli" "$VENDOR/main"
else
  echo "[build-whisper] ERROR: no whisper binary produced" >&2
  exit 1
fi
chmod +x "$VENDOR/main"

if [ ! -f "$MODEL" ]; then
  echo "[build-whisper] downloading ggml-medium.en model (~1.5GB)"
  if [ -x "$SRC/models/download-ggml-model.sh" ]; then
    (cd "$SRC" && bash ./models/download-ggml-model.sh medium.en)
    cp "$SRC/models/ggml-medium.en.bin" "$MODEL"
  else
    curl -L -o "$MODEL" \
      "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.en.bin"
  fi
fi

echo "[build-whisper] done: $VENDOR/main + $MODEL"
