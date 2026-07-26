#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PACKAGE="$ROOT/macos/FractalVoice"
DIST="$ROOT/dist"
APP="$DIST/Fractal Voice.app"
CONTENTS="$APP/Contents"
DEFAULT_LLAMA_CLI="$HOME/.cache/fractal-build/llama.cpp/build-fractal/bin/llama-cli"
LLAMA_CLI="${FRACTAL_LLAMA_CLI:-$DEFAULT_LLAMA_CLI}"
LLAMA_DEST="$CONTENTS/Resources/Granite/bin/llama-cli"
DEFAULT_LLAMA_SERVER="$HOME/.cache/fractal-build/llama.cpp/build-fractal/bin/llama-server"
LLAMA_SERVER="${FRACTAL_LLAMA_SERVER:-$DEFAULT_LLAMA_SERVER}"
LLAMA_SERVER_DEST="$CONTENTS/Resources/Granite/bin/llama-server"
XCODE_PRODUCTS="$PACKAGE/.xcode-build/Build/Products/Release"

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

mkdir -p "$CONTENTS/Resources/GraniteModels" "$CONTENTS/Resources/KokoroModels"
cp "$PACKAGE/GRANITE_MODEL_SHA256SUMS" \
  "$CONTENTS/Resources/GraniteModels/GRANITE_MODEL_SHA256SUMS"
cp "$PACKAGE/KOKORO_MODEL_SHA256SUMS" \
  "$CONTENTS/Resources/KokoroModels/KOKORO_MODEL_SHA256SUMS"

# Keep the inference runtime, generated pronunciation resources, and MLX Metal
# shaders in the small application. Model weights install on first launch.
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
