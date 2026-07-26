#!/bin/bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 SUBMISSION_ID" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SUBMISSION_ID="$1"
PROFILE="${FRACTAL_NOTARY_PROFILE:-fractal-notarytool}"
DIST="${FRACTAL_DIST_DIR:-$ROOT/artifacts/notarization/$SUBMISSION_ID}"
APP="$DIST/Fractal Voice.app"
ARCHIVE="$DIST/FractalVoice-macOS.zip"

if [[ ! -d "$APP" || ! -f "$ARCHIVE" ]]; then
  echo "Missing preserved notarization artifacts in: $DIST" >&2
  exit 1
fi

RESULT="$(
  xcrun notarytool info "$SUBMISSION_ID" \
    --keychain-profile "$PROFILE" \
    --output-format json
)"
echo "$RESULT"
if [[ "$RESULT" != *'"status":"Accepted"'* \
   && "$RESULT" != *'"status": "Accepted"'* ]]; then
  echo "Submission $SUBMISSION_ID is not accepted yet." >&2
  exit 1
fi

codesign --verify --deep --strict --verbose=2 "$APP"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=4 "$APP"

rm -f "$ARCHIVE"
(
  cd "$DIST"
  ditto -c -k --sequesterRsrc --keepParent \
    "Fractal Voice.app" \
    "FractalVoice-macOS.zip"
)

shasum -a 256 "$ARCHIVE"
echo "$ARCHIVE"
