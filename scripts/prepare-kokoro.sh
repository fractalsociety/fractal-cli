#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${FRACTAL_KOKORO_MODEL_DIR:-$HOME/.fractal/models/kokoro-82m-bf16}"
REVISION="a71e4d38b236d968966a2002c4c895dbd12b1c3c"
BASE="https://huggingface.co/mlx-community/Kokoro-82M-bf16/resolve/$REVISION"

mkdir -p "$DEST"
curl -L --fail --retry 3 --continue-at - \
  -o "$DEST/kokoro-v1_0.safetensors" \
  "$BASE/kokoro-v1_0.safetensors"
curl -L --fail --retry 3 \
  -o "$DEST/af_heart.safetensors" \
  "$BASE/voices/af_heart.safetensors"

(cd "$DEST" && shasum -a 256 -c "$ROOT/macos/FractalVoice/KOKORO_MODEL_SHA256SUMS")
echo "Kokoro 82M and af_heart are ready at $DEST"
