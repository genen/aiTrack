#!/usr/bin/env bash
# Builds dist/aiTrack.app from target/release/aitrack (run `cargo build --release` first) and signs it.
# macOS, Apple Silicon. Without signing secrets in the environment the app is ad-hoc signed only: Gatekeeper
# asks the user to approve it once (right-click > Open, or System Settings > Privacy & Security > Open Anyway).
# With the secrets (see scripts/sign-notarize-mac.sh and docs/MAC-SIGNING.md) it is signed with a Developer ID
# Application certificate and, if notary credentials are set, notarized and stapled.
#
#   scripts/make-app.sh                 build the app
#   scripts/make-app.sh --print-plist [VERSION]   print the Info.plist (default: the Cargo.toml version; any OS, used by tests)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Prints the package version from Cargo.toml (the first `version = "..."` line), e.g. 0.3.1-beta.
cargo_version() {
  local v
  v="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$root/Cargo.toml" | head -n 1)"
  [ -n "$v" ] || { echo "No version found in Cargo.toml." >&2; return 1; }
  printf '%s\n' "$v"
}

# Prints Info.plist for the given Cargo version. CFBundleShortVersionString is the full version;
# CFBundleVersion is numeric only (the -beta suffix is dropped, like the MSI version).
info_plist() {
  local version="$1" build
  build="${version%%-*}"
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>com.sirgene.aitrack</string>
  <key>CFBundleName</key>
  <string>aiTrack</string>
  <key>CFBundleDisplayName</key>
  <string>aiTrack</string>
  <key>CFBundleExecutable</key>
  <string>aitrack</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleShortVersionString</key>
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${build}</string>
  <key>CFBundleIconFile</key>
  <string>aitrack</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST
}

main() {
  local version app bin mode
  if [ "${1:-}" = "--print-plist" ]; then
    info_plist "${2:-$(cargo_version)}"
    return 0
  fi
  version="$(cargo_version)"
  bin="$root/target/release/aitrack"
  [ -x "$bin" ] || { echo "$bin not found. Run: cargo build --release" >&2; return 1; }
  [ -s "$root/assets/icon/aitrack.icns" ] || { echo "assets/icon/aitrack.icns is missing." >&2; return 1; }

  app="$root/dist/aiTrack.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$bin" "$app/Contents/MacOS/aitrack"
  chmod 755 "$app/Contents/MacOS/aitrack"
  cp "$root/assets/icon/aitrack.icns" "$app/Contents/Resources/aitrack.icns"
  info_plist "$version" > "$app/Contents/Info.plist"
  printf 'APPL????' > "$app/Contents/PkgInfo"

  plutil -lint "$app/Contents/Info.plist"
  mode="$("$root/scripts/sign-notarize-mac.sh" --print-mode)"
  if [ "$mode" = adhoc ]; then
    # ad-hoc signature ("-"): no developer identity, no secrets
    codesign --force --deep -s - "$app"
    codesign --verify --deep --strict "$app"
  else
    # Developer ID signature (+ notarization and stapling when notary credentials are set)
    "$root/scripts/sign-notarize-mac.sh" app "$app"
  fi
  echo "Built $app (version $version, mode: $mode)"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
