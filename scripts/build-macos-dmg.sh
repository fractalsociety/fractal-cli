#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="${FRACTAL_DIST_DIR:-$ROOT/dist}"
APP="${FRACTAL_APP_PATH:-$DIST/Fractal Voice.app}"
OUTPUT="${FRACTAL_DMG_PATH:-$DIST/FractalVoice-macOS.dmg}"
PACKAGE="$ROOT/macos/FractalVoice"
STAGING="$(mktemp -d)"

trap 'rm -rf "$STAGING"' EXIT
if [[ ! -d "$APP" ]]; then
  echo "Missing application bundle: $APP" >&2
  echo "Run scripts/build-macos-app.sh first." >&2
  exit 1
fi
if ! command -v npx >/dev/null 2>&1; then
  echo "Missing npx. Install Node.js before building the DMG." >&2
  exit 1
fi

ditto "$APP" "$STAGING/Fractal Voice.app"
cp "$PACKAGE/DMG.json" "$STAGING/DMG.json"
cp "$PACKAGE/Resources/FractalVoice.icns" "$STAGING/FractalVoice.icns"
sips -s format png "$PACKAGE/Resources/DMGBackground.svg" \
  --out "$STAGING/DMGBackground.png" >/dev/null

rm -f "$OUTPUT"
npx --yes appdmg@0.6.6 "$STAGING/DMG.json" "$OUTPUT"
if [[ -n "${FRACTAL_CODESIGN_IDENTITY:-}" \
   && "${FRACTAL_CODESIGN_IDENTITY}" != "-" ]]; then
  codesign \
    --force \
    --timestamp \
    --sign "$FRACTAL_CODESIGN_IDENTITY" \
    "$OUTPUT"
fi
echo "$OUTPUT"
