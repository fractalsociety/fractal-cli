#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PACKAGE="$ROOT/macos/FractalVoice"
DIST="$ROOT/dist"
APP="$DIST/Fractal Voice.app"
CONTENTS="$APP/Contents"
DEFAULT_MODEL_DIR="$HOME/.fractal/models/granite-speech-4.1-2b-q4"
MODEL_DIR="${FRACTAL_GRANITE_MODEL_DIR:-$DEFAULT_MODEL_DIR}"
MODEL_DEST="$CONTENTS/Resources/GraniteModels/granite-speech-4.1-2b-q4"
DEFAULT_LLAMA_CLI="$HOME/.cache/fractal-build/llama.cpp/build-fractal/bin/llama-cli"
LLAMA_CLI="${FRACTAL_LLAMA_CLI:-$DEFAULT_LLAMA_CLI}"
LLAMA_DEST="$CONTENTS/Resources/Granite/bin/llama-cli"
DEFAULT_LLAMA_SERVER="$HOME/.cache/fractal-build/llama.cpp/build-fractal/bin/llama-server"
LLAMA_SERVER="${FRACTAL_LLAMA_SERVER:-$DEFAULT_LLAMA_SERVER}"
LLAMA_SERVER_DEST="$CONTENTS/Resources/Granite/bin/llama-server"
DEFAULT_KOKORO_DIR="$HOME/.fractal/models/kokoro-82m-bf16"
KOKORO_DIR="${FRACTAL_KOKORO_MODEL_DIR:-$DEFAULT_KOKORO_DIR}"
KOKORO_DEST="$CONTENTS/Resources/KokoroModels/Kokoro-82M-bf16"
XCODE_PRODUCTS="$PACKAGE/.xcode-build/Build/Products/Release"

REQUIRED_MODEL_FILES=(
  granite-speech-4.1-2b-Q4_K_M.gguf
  mmproj-model-f16.gguf
)

cd "$ROOT"
cargo build --release

cd "$PACKAGE"
xcodebuild \
  -scheme FractalVoice \
  -configuration Release \
  -destination "platform=macOS,arch=arm64" \
  -derivedDataPath .xcode-build \
  build \
  CODE_SIGNING_ALLOWED=NO \
  -quiet

if [[ ! -f "$XCODE_PRODUCTS/mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib" ]]; then
  echo "Missing MLX Metal shader library." >&2
  echo "Install it with: xcodebuild -downloadComponent MetalToolchain" >&2
  exit 1
fi

rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$(dirname "$LLAMA_DEST")"
cp "$XCODE_PRODUCTS/FractalVoice" "$CONTENTS/MacOS/FractalVoice"
cp "$ROOT/target/release/fractal" "$CONTENTS/Resources/fractal"
cp "$PACKAGE/Info.plist" "$CONTENTS/Info.plist"
cp "$PACKAGE/THIRD_PARTY_NOTICES.txt" "$CONTENTS/Resources/THIRD_PARTY_NOTICES.txt"
if [[ ! -x "$LLAMA_CLI" ]]; then
  echo "Missing Granite inference engine: $LLAMA_CLI" >&2
  echo "Run scripts/prepare-granite-speech.sh first." >&2
  exit 1
fi
if [[ ! -x "$LLAMA_SERVER" ]]; then
  echo "Missing persistent Granite inference server: $LLAMA_SERVER" >&2
  echo "Run scripts/prepare-granite-speech.sh first." >&2
  exit 1
fi
cp "$LLAMA_CLI" "$LLAMA_DEST"
cp "$LLAMA_SERVER" "$LLAMA_SERVER_DEST"
chmod 755 \
  "$CONTENTS/MacOS/FractalVoice" \
  "$CONTENTS/Resources/fractal" \
  "$LLAMA_DEST" \
  "$LLAMA_SERVER_DEST"

for model_file in "${REQUIRED_MODEL_FILES[@]}"; do
  if [[ ! -f "$MODEL_DIR/$model_file" ]]; then
    echo "Missing Granite model file: $MODEL_DIR/$model_file" >&2
    echo "Run scripts/prepare-granite-speech.sh first." >&2
    exit 1
  fi
done
(cd "$MODEL_DIR" && shasum -a 256 -c "$PACKAGE/GRANITE_MODEL_SHA256SUMS")
mkdir -p "$MODEL_DEST"
for model_file in "${REQUIRED_MODEL_FILES[@]}"; do
  cp "$MODEL_DIR/$model_file" "$MODEL_DEST/$model_file"
done
cp "$PACKAGE/GRANITE_MODEL_SHA256SUMS" \
  "$CONTENTS/Resources/GraniteModels/GRANITE_MODEL_SHA256SUMS"

if [[ ! -f "$KOKORO_DIR/kokoro-v1_0.safetensors" \
   || ! -f "$KOKORO_DIR/af_heart.safetensors" ]]; then
  echo "Missing Kokoro 82M assets. Run scripts/prepare-kokoro.sh first." >&2
  exit 1
fi
(cd "$KOKORO_DIR" && shasum -a 256 -c "$PACKAGE/KOKORO_MODEL_SHA256SUMS")
mkdir -p "$KOKORO_DEST"
cp "$KOKORO_DIR/kokoro-v1_0.safetensors" "$KOKORO_DEST/"
cp "$KOKORO_DIR/af_heart.safetensors" "$KOKORO_DEST/"
cp "$PACKAGE/KOKORO_MODEL_SHA256SUMS" \
  "$CONTENTS/Resources/KokoroModels/KOKORO_MODEL_SHA256SUMS"

# Keep Kokoro's generated resources and MLX Metal shaders inside the
# application so it remains a complete offline install.
for resource_bundle in "$XCODE_PRODUCTS"/*.bundle; do
  [[ -d "$resource_bundle" ]] \
    && cp -R "$resource_bundle" "$CONTENTS/Resources/"
done

SIGNING_IDENTITY="${FRACTAL_CODESIGN_IDENTITY:--}"
if [[ "$SIGNING_IDENTITY" == "-" ]]; then
  codesign --force --deep --sign - "$APP"
else
  codesign \
    --force \
    --deep \
    --options runtime \
    --timestamp \
    --sign "$SIGNING_IDENTITY" \
    "$APP"
fi

cd "$DIST"
rm -f "FractalVoice-macOS.zip"
ditto -c -k --sequesterRsrc --keepParent "Fractal Voice.app" "FractalVoice-macOS.zip"

echo "$APP"
echo "$DIST/FractalVoice-macOS.zip"
