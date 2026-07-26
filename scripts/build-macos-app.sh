#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PACKAGE="$ROOT/macos/FractalVoice"
DIST="$ROOT/dist"
APP="$DIST/Fractal Voice.app"
CONTENTS="$APP/Contents"
DEFAULT_MODEL_DIR="$HOME/.fractal/models/moonshine-v2-medium-streaming/download.moonshine.ai/model/medium-streaming-en/quantized"
MODEL_DIR="${FRACTAL_MOONSHINE_MODEL_DIR:-$DEFAULT_MODEL_DIR}"
MODEL_DEST="$CONTENTS/Resources/MoonshineModels/medium-streaming-en"

REQUIRED_MODEL_FILES=(
  adapter.ort
  cross_kv.ort
  decoder_kv.ort
  decoder_kv_with_attention.ort
  encoder.ort
  frontend.ort
  streaming_config.json
  tokenizer.bin
)

cd "$ROOT"
cargo build --release

cd "$PACKAGE"
swift build -c release

rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "$PACKAGE/.build/release/FractalVoice" "$CONTENTS/MacOS/FractalVoice"
cp "$ROOT/target/release/fractal" "$CONTENTS/Resources/fractal"
cp "$PACKAGE/Info.plist" "$CONTENTS/Info.plist"
cp "$PACKAGE/THIRD_PARTY_NOTICES.txt" "$CONTENTS/Resources/THIRD_PARTY_NOTICES.txt"
chmod 755 "$CONTENTS/MacOS/FractalVoice" "$CONTENTS/Resources/fractal"

for model_file in "${REQUIRED_MODEL_FILES[@]}"; do
  if [[ ! -f "$MODEL_DIR/$model_file" ]]; then
    echo "Missing Moonshine model file: $MODEL_DIR/$model_file" >&2
    echo "Run 'fractal voice setup' on the build Mac or set FRACTAL_MOONSHINE_MODEL_DIR." >&2
    exit 1
  fi
done
(cd "$MODEL_DIR" && shasum -a 256 -c "$PACKAGE/MOONSHINE_MODEL_SHA256SUMS")
mkdir -p "$MODEL_DEST"
for model_file in "${REQUIRED_MODEL_FILES[@]}"; do
  cp "$MODEL_DIR/$model_file" "$MODEL_DEST/$model_file"
done
cp "$PACKAGE/MOONSHINE_MODEL_SHA256SUMS" \
  "$CONTENTS/Resources/MoonshineModels/MOONSHINE_MODEL_SHA256SUMS"

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
