#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="${FRACTAL_APP_STORE_DIST_DIR:-$ROOT/dist-app-store}"
APP="$DIST/Fractal Voice.app"
PKG="$DIST/FractalVoice-AppStore.pkg"
APP_IDENTITY="${FRACTAL_APP_STORE_APP_IDENTITY:-}"
INSTALLER_IDENTITY="${FRACTAL_APP_STORE_INSTALLER_IDENTITY:-}"
PROFILE="${FRACTAL_APP_STORE_PROVISIONING_PROFILE:-}"
MAIN_ENTITLEMENTS="$ROOT/macos/FractalVoice/AppStore/FractalVoice.entitlements"
CHILD_ENTITLEMENTS="$ROOT/macos/FractalVoice/AppStore/FractalVoiceChild.entitlements"

if [[ -z "$APP_IDENTITY" || -z "$INSTALLER_IDENTITY" || -z "$PROFILE" ]]; then
  cat >&2 <<'EOF'
Mac App Store signing inputs are required:
  FRACTAL_APP_STORE_APP_IDENTITY
  FRACTAL_APP_STORE_INSTALLER_IDENTITY
  FRACTAL_APP_STORE_PROVISIONING_PROFILE

Use a Mac App Distribution application identity, a Mac Installer Distribution
identity, and the provisioning profile for com.fractalsociety.voice.
EOF
  exit 1
fi

if [[ ! -f "$PROFILE" ]]; then
  echo "Provisioning profile not found: $PROFILE" >&2
  exit 1
fi
if ! security find-identity -v -p codesigning | grep -Fq "\"$APP_IDENTITY\""; then
  echo "Application signing identity is not installed: $APP_IDENTITY" >&2
  exit 1
fi
if ! security find-certificate -a -c "$INSTALLER_IDENTITY" >/dev/null 2>&1; then
  echo "Installer signing identity is not installed: $INSTALLER_IDENTITY" >&2
  exit 1
fi

FRACTAL_DIST_DIR="$DIST" \
FRACTAL_SWIFT_CONDITIONS="APP_STORE" \
FRACTAL_CODESIGN_IDENTITY="$APP_IDENTITY" \
FRACTAL_CODESIGN_MAIN_ENTITLEMENTS="$MAIN_ENTITLEMENTS" \
FRACTAL_CODESIGN_CHILD_ENTITLEMENTS="$CHILD_ENTITLEMENTS" \
FRACTAL_EMBEDDED_PROVISIONING_PROFILE="$PROFILE" \
  "$ROOT/scripts/build-macos-app.sh"

codesign --verify --deep --strict --verbose=2 "$APP"
APP_ENTITLEMENTS="$(codesign -d --entitlements - "$APP" 2>&1)"
if [[ "$APP_ENTITLEMENTS" != *"com.apple.security.app-sandbox"* ]]; then
  echo "Built app is missing App Sandbox." >&2
  exit 1
fi

rm -f "$PKG"
productbuild \
  --component "$APP" /Applications \
  --sign "$INSTALLER_IDENTITY" \
  "$PKG"
pkgutil --check-signature "$PKG"
echo "$PKG"
