#!/usr/bin/env bash
# Signs (Developer ID Application, hardened runtime), notarizes and staples the macOS app and dmg. macOS only,
# except --print-mode and --parse-identity, which run anywhere (used by tests).
#
#   scripts/sign-notarize-mac.sh --print-mode        print the mode chosen from the environment (no secret is read out)
#   scripts/sign-notarize-mac.sh --parse-identity    read `security find-identity` text on stdin, print the first Developer ID hash
#   scripts/sign-notarize-mac.sh app dist/aiTrack.app    sign, notarize and staple the app
#   scripts/sign-notarize-mac.sh dmg dist/x.dmg          sign, notarize and staple the dmg
#
# Secrets come ONLY from environment variables (GitHub Actions secrets). Nothing here prints them:
#   signing:     APPLE_CERT_P12_BASE64, APPLE_CERT_PASSWORD            (Developer ID Application .p12, base64)
#   notarizing:  APPLE_API_KEY_P8_BASE64, APPLE_API_KEY_ID, APPLE_API_ISSUER_ID   (App Store Connect API key, preferred)
#            or  APPLE_ID, APPLE_APP_PASSWORD, APPLE_TEAM_ID                      (Apple ID + app-specific password)
#   optional:    APPLE_SIGNING_IDENTITY  (name or hash, to pick one when the keychain holds several)
#
# Modes: adhoc (no certificate: the caller keeps its ad-hoc signature), signed (certificate but no usable
# notary credentials: signed, NOT notarized), notarized-api-key, notarized-apple-id.
#
# Entitlements: none. The hardened runtime needs an entitlement only for JIT, unsigned memory, loading
# unsigned libraries, DYLD variables, or sending Apple events from the app process itself. aiTrack reads the
# 'Claude Code-credentials' Keychain item by running /usr/bin/security (a child process, its own signature),
# runs osascript/open as children, posts notifications (UNUserNotificationCenter needs no entitlement, only a
# signed bundle) and makes outbound HTTPS (client network access needs an entitlement only in the App Sandbox,
# which is not used). Unverified on a Mac: if a notarized build misbehaves, add an entitlements file then.
set -euo pipefail

have() { [ -n "${!1:-}" ]; }

# Prints the signing/notarizing mode for the current environment.
choose_mode() {
  if ! have APPLE_CERT_P12_BASE64 || ! have APPLE_CERT_PASSWORD; then
    echo adhoc
  elif have APPLE_API_KEY_P8_BASE64 && have APPLE_API_KEY_ID && have APPLE_API_ISSUER_ID; then
    echo notarized-api-key
  elif have APPLE_ID && have APPLE_APP_PASSWORD && have APPLE_TEAM_ID; then
    echo notarized-apple-id
  else
    echo signed
  fi
}

# Reads `security find-identity -v -p codesigning` text on stdin; prints the SHA-1 of the first
# "Developer ID Application" identity. Fails if there is none.
parse_identity() {
  local hash
  hash="$(sed -n 's/^[[:space:]]*[0-9][0-9]*)[[:space:]]*\([0-9A-Fa-f]\{40\}\)[[:space:]]*".*Developer ID Application.*$/\1/p' | head -n 1)"
  [ -n "$hash" ] || return 1
  printf '%s\n' "$hash"
}

work=""        # temp dir for decoded secrets (removed on exit)
keychain=""    # temp keychain path
old_keychains=""
identity=""

cleanup() {
  { set +x; } 2>/dev/null
  if [ -n "$keychain" ]; then
    if [ -n "$old_keychains" ]; then
      # shellcheck disable=SC2086
      security list-keychains -d user -s $old_keychains >/dev/null 2>&1 || true
    fi
    security delete-keychain "$keychain" >/dev/null 2>&1 || true
  fi
  [ -z "$work" ] || rm -rf "$work"
}

# Creates a temporary keychain with a random password, imports the .p12, finds the identity.
setup_keychain() {
  { set +x; } 2>/dev/null
  local kpw
  work="$(mktemp -d)"
  trap cleanup EXIT
  kpw="$(uuidgen)$(uuidgen)"
  keychain="$work/aitrack-signing.keychain-db"
  old_keychains="$(security list-keychains -d user | tr -d '"' | tr '\n' ' ')"
  security create-keychain -p "$kpw" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$kpw" "$keychain"
  printf '%s' "$APPLE_CERT_P12_BASE64" | base64 --decode > "$work/cert.p12"
  security import "$work/cert.p12" -k "$keychain" -P "$APPLE_CERT_PASSWORD" -f pkcs12 -T /usr/bin/codesign -T /usr/bin/security >/dev/null
  security set-key-partition-list -S apple-tool:,apple: -s -k "$kpw" "$keychain" >/dev/null
  rm -f "$work/cert.p12"
  # shellcheck disable=SC2086
  security list-keychains -d user -s "$keychain" $old_keychains >/dev/null
  if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
    identity="$APPLE_SIGNING_IDENTITY"
  else
    identity="$(security find-identity -v -p codesigning "$keychain" | parse_identity)" \
      || { echo "No 'Developer ID Application' identity found in the imported certificate." >&2; return 1; }
  fi
  echo "Signing identity found (Developer ID Application)."
}

# Runs `xcrun notarytool submit --wait` on a file with the credentials for the mode.
notarize() {
  { set +x; } 2>/dev/null
  local file="$1" mode out id
  mode="$(choose_mode)"
  out="$work/notary.json"
  case "$mode" in
    notarized-api-key)
      printf '%s' "$APPLE_API_KEY_P8_BASE64" | base64 --decode > "$work/key.p8"
      xcrun notarytool submit "$file" --wait --output-format json \
        --key "$work/key.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" > "$out" || true
      ;;
    notarized-apple-id)
      xcrun notarytool submit "$file" --wait --output-format json \
        --apple-id "$APPLE_ID" --password "$APPLE_APP_PASSWORD" --team-id "$APPLE_TEAM_ID" > "$out" || true
      ;;
    *) echo "notarize called in mode $mode" >&2; return 1 ;;
  esac
  id="$(sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F-]*\)".*/\1/p' "$out" | head -n 1)"
  if grep -q '"status"[[:space:]]*:[[:space:]]*"Accepted"' "$out"; then
    echo "Notarization accepted (submission $id)."
    return 0
  fi
  echo "Notarization failed (submission ${id:-unknown}). Apple's log (holds no secrets):" >&2
  if [ -n "$id" ]; then
    if [ "$mode" = notarized-api-key ]; then
      xcrun notarytool log "$id" --key "$work/key.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" >&2 || true
    else
      xcrun notarytool log "$id" --apple-id "$APPLE_ID" --password "$APPLE_APP_PASSWORD" --team-id "$APPLE_TEAM_ID" >&2 || true
    fi
  fi
  return 1
}

sign_app() {
  local app="$1" mode zipf
  [ -d "$app" ] || { echo "$app not found." >&2; return 1; }
  setup_keychain
  mode="$(choose_mode)"
  codesign --force --deep --options runtime --timestamp --keychain "$keychain" -s "$identity" "$app"
  codesign --verify --deep --strict --verbose=2 "$app"
  if [ "$mode" = signed ]; then
    echo "Signed with Developer ID, NOT notarized (no notary credentials)."
    return 0
  fi
  zipf="$work/app-for-notary.zip"
  ditto -c -k --keepParent "$app" "$zipf"
  notarize "$zipf"
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"
  spctl -a -t exec -vv "$app"
  echo "App signed, notarized and stapled."
}

sign_dmg() {
  local dmg="$1" mode
  [ -f "$dmg" ] || { echo "$dmg not found." >&2; return 1; }
  setup_keychain
  mode="$(choose_mode)"
  codesign --force --timestamp --keychain "$keychain" -s "$identity" "$dmg"
  codesign --verify --strict --verbose=2 "$dmg"
  if [ "$mode" = signed ]; then
    echo "dmg signed with Developer ID, NOT notarized (no notary credentials)."
    return 0
  fi
  notarize "$dmg"
  xcrun stapler staple "$dmg"
  xcrun stapler validate "$dmg"
  spctl -a -t open --context context:primary-signature -vv "$dmg"
  echo "dmg signed, notarized and stapled."
}

main() {
  case "${1:-}" in
    --print-mode) choose_mode ;;
    --parse-identity) parse_identity ;;
    app) sign_app "${2:?usage: $0 app <path.app>}" ;;
    dmg) sign_dmg "${2:?usage: $0 dmg <path.dmg>}" ;;
    *) echo "usage: $0 --print-mode | --parse-identity | app <path> | dmg <path>" >&2; return 2 ;;
  esac
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
