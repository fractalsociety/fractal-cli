#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOURCE="$ROOT/macos/FractalVoice/Resources/FractalVoice.svg"
OUTPUT="$ROOT/macos/FractalVoice/Resources/FractalVoice.icns"
WORK="$(mktemp -d)"
ICONSET="$WORK/FractalVoice.iconset"

trap 'rm -rf "$WORK"' EXIT
mkdir -p "$ICONSET"

MASTER="$WORK/FractalVoice.png"
sips -s format png "$SOURCE" --out "$MASTER" >/dev/null

resize() {
  local size="$1"
  local name="$2"
  sips -z "$size" "$size" "$MASTER" --out "$ICONSET/$name" >/dev/null
}

resize 16 icon_16x16.png
resize 32 icon_16x16@2x.png
resize 32 icon_32x32.png
resize 64 icon_32x32@2x.png
resize 128 icon_128x128.png
resize 256 icon_128x128@2x.png
resize 256 icon_256x256.png
resize 512 icon_256x256@2x.png
resize 512 icon_512x512.png
cp "$MASTER" "$ICONSET/icon_512x512@2x.png"

iconutil --convert icns --output "$OUTPUT" "$ICONSET"
echo "$OUTPUT"
