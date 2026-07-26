#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PACKAGE="$ROOT/macos/FractalVoice"
DIST="$ROOT/dist"
APP="$DIST/Fractal Voice.app"
CONTENTS="$APP/Contents"

cd "$ROOT"
cargo build --release

cd "$PACKAGE"
swift build -c release

rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "$PACKAGE/.build/release/FractalVoice" "$CONTENTS/MacOS/FractalVoice"
cp "$ROOT/target/release/fractal" "$CONTENTS/Resources/fractal"
cp "$PACKAGE/Info.plist" "$CONTENTS/Info.plist"
chmod 755 "$CONTENTS/MacOS/FractalVoice" "$CONTENTS/Resources/fractal"

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
