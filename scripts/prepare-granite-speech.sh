#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE_ROOT="${FRACTAL_GRANITE_CACHE_DIR:-$HOME/.fractal/models/granite-speech-4.1-2b-q4}"
LLAMA_ROOT="${FRACTAL_LLAMA_SOURCE_DIR:-$HOME/.cache/fractal-build/llama.cpp}"
LLAMA_COMMIT="42fc243060709331ff9b158a9ed2cbe37219ae83"
MODEL_REPO="https://huggingface.co/ibm-granite/granite-speech-4.1-2b-GGUF/resolve/8267dad2adc84209b0efd2702ec68a98356125eb"

mkdir -p "$CACHE_ROOT" "$(dirname "$LLAMA_ROOT")"

download() {
  local filename="$1"
  if [[ ! -f "$CACHE_ROOT/$filename" ]]; then
    curl \
      --fail \
      --location \
      --retry 3 \
      --continue-at - \
      --progress-bar \
      --output "$CACHE_ROOT/$filename" \
      "$MODEL_REPO/$filename"
  fi
}

download "granite-speech-4.1-2b-Q4_K_M.gguf"
download "mmproj-model-f16.gguf"

(cd "$CACHE_ROOT" && shasum -a 256 -c "$ROOT/macos/FractalVoice/GRANITE_MODEL_SHA256SUMS")

if [[ ! -d "$LLAMA_ROOT/.git" ]]; then
  git clone --filter=blob:none https://github.com/ggml-org/llama.cpp.git "$LLAMA_ROOT"
fi
git -C "$LLAMA_ROOT" fetch --depth 1 origin "$LLAMA_COMMIT"
git -C "$LLAMA_ROOT" checkout --detach "$LLAMA_COMMIT"

cmake \
  -S "$LLAMA_ROOT" \
  -B "$LLAMA_ROOT/build-fractal" \
  -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_SHARED_LIBS=OFF \
  -DLLAMA_CURL=OFF \
  -DLLAMA_OPENSSL=OFF \
  -DGGML_METAL=ON \
  -DGGML_ACCELERATE=ON \
  -DLLAMA_BUILD_TESTS=OFF \
  -DLLAMA_BUILD_EXAMPLES=OFF \
  -DLLAMA_BUILD_SERVER=ON \
  -DLLAMA_BUILD_APP=OFF \
  -DLLAMA_BUILD_UI=OFF \
  -DLLAMA_BUILD_TOOLS=ON
cmake --build \
  "$LLAMA_ROOT/build-fractal" \
  --config Release \
  --target llama-cli llama-server \
  -j

echo "$CACHE_ROOT"
echo "$LLAMA_ROOT/build-fractal/bin/llama-cli"
echo "$LLAMA_ROOT/build-fractal/bin/llama-server"
