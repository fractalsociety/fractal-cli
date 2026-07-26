#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/dist/Fractal Voice.app"
ARCHIVE="$ROOT/dist/FractalVoice-macOS.zip"
PROFILE="${FRACTAL_NOTARY_PROFILE:-fractal-notarytool}"
IDENTITY="${FRACTAL_CODESIGN_IDENTITY:-}"

if [[ -z "$IDENTITY" ]]; then
  IDENTITY="$(
    security find-identity -v -p codesigning \
      | sed -n 's/.*"\(Developer ID Application:.*\)"/\1/p' \
      | head -1
  )"
fi

if [[ -z "$IDENTITY" ]]; then
  echo "A Developer ID Application certificate is required." >&2
  echo "Install it in Keychain, then rerun this script." >&2
  exit 1
fi

if ! xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1; then
  echo "Notary credentials are not stored under Keychain profile '$PROFILE'." >&2
  echo "Run: xcrun notarytool store-credentials \"$PROFILE\"" >&2
  exit 1
fi

FRACTAL_CODESIGN_IDENTITY="$IDENTITY" "$ROOT/scripts/build-macos-app.sh"

codesign --verify --deep --strict --verbose=2 "$APP"
SIGNATURE_DETAILS="$(codesign --display --verbose=4 "$APP" 2>&1)"
if [[ "$SIGNATURE_DETAILS" != *"Authority=Developer ID Application:"* ]]; then
  echo "The app is not signed with Developer ID Application." >&2
  exit 1
fi
if [[ "$SIGNATURE_DETAILS" != *"Runtime Version="* ]]; then
  echo "The app is missing hardened runtime signing." >&2
  exit 1
fi

NOTARY_RESULT="$(
  xcrun notarytool submit "$ARCHIVE" \
  --keychain-profile "$PROFILE" \
  --wait \
  --output-format json
)"
echo "$NOTARY_RESULT"
if [[ "$NOTARY_RESULT" != *'"status":"Accepted"'* \
   && "$NOTARY_RESULT" != *'"status": "Accepted"'* ]]; then
  echo "Apple did not accept the notarization submission." >&2
  exit 1
fi

xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=4 "$APP"

rm -f "$ARCHIVE"
(
  cd "$ROOT/dist"
  ditto -c -k --sequesterRsrc --keepParent \
    "Fractal Voice.app" \
    "FractalVoice-macOS.zip"
)

shasum -a 256 "$ARCHIVE"
echo "$ARCHIVE"
