#!/usr/bin/env bash
# Packs dist/aiTrack.app (from scripts/make-app.sh) into dist/aiTrack-<version>-macos-arm64.dmg and
# dist/aiTrack-<version>-macos-arm64.zip, then prints their SHA256. macOS only.
# Order when signing: app signed + notarized + stapled (make-app.sh), dmg built from it, dmg signed + notarized
# + stapled, zip made from the stapled app. Without signing secrets everything stays ad-hoc signed.
# Both carry README.md, README.pdf (dist/README.pdf from scripts/readme-pdf-mac.sh), LICENSE, THIRD-PARTY-NOTICES.md and docs/img next to
# the app (never inside aiTrack.app, which would break its signature). The build fails if any is missing.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$root/Cargo.toml" | head -n 1)"
[ -n "$version" ] || { echo "No version found in Cargo.toml." >&2; exit 1; }
app="$root/dist/aiTrack.app"
[ -d "$app" ] || { echo "$app not found. Run scripts/make-app.sh first." >&2; exit 1; }

dmg="$root/dist/aiTrack-${version}-macos-arm64.dmg"
zip="$root/dist/aiTrack-${version}-macos-arm64.zip"
rm -f "$dmg" "$zip"

# The release extras. README.pdf must be made first (scripts/readme-pdf-mac.sh); it is never committed.
extras_fail=0
for f in "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.md" "$root/README.md" "$root/dist/README.pdf"; do
  [ -s "$f" ] || { echo "$f is missing. Run scripts/readme-pdf-mac.sh first (it makes dist/README.pdf)." >&2; extras_fail=1; }
done
[ -d "$root/docs/img" ] || { echo "$root/docs/img is missing." >&2; extras_fail=1; }
[ "$extras_fail" = 0 ] || exit 1
copy_extras() { # $1 = folder that sits next to aiTrack.app
  # COPYFILE_DISABLE and xattr -c keep extended attributes (for example from the browser that made README.pdf) from
  # turning into stray "._name" AppleDouble entries in the zip. mac-contents-check.sh fails on any "._*" entry.
  COPYFILE_DISABLE=1 cp "$root/README.md" "$root/dist/README.pdf" "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.md" "$1/"
  mkdir -p "$1/docs"
  COPYFILE_DISABLE=1 cp -R "$root/docs/img" "$1/docs/img"
  xattr -cr "$1/README.md" "$1/README.pdf" "$1/LICENSE" "$1/THIRD-PARTY-NOTICES.md" "$1/docs" 2>/dev/null || true
}

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/aiTrack.app"
ln -s /Applications "$stage/Applications"
copy_extras "$stage"
hdiutil create -volname "aiTrack" -srcfolder "$stage" -ov -format UDZO "$dmg"

# With signing secrets: sign the dmg, notarize and staple it (the app inside is already stapled by make-app.sh)
mode="$("$root/scripts/sign-notarize-mac.sh" --print-mode)"
if [ "$mode" != adhoc ]; then
  "$root/scripts/sign-notarize-mac.sh" dmg "$dmg"
fi
# the dmg's contents are final (and signed): check them
"$root/scripts/mac-contents-check.sh" dmg "$dmg"

# ditto keeps the bundle's attributes and the signature (and stapled ticket) intact (plain zip does not)
# The zip is made from a staging folder (stapled app + the extras beside it); without --keepParent the folder's
# children sit at the zip root: aiTrack.app/, README.md, README.pdf, LICENSE, THIRD-PARTY-NOTICES.md, docs/img.
zstage="$(mktemp -d)"
trap 'rm -rf "$stage" "$zstage"' EXIT
ditto "$app" "$zstage/aiTrack.app"
copy_extras "$zstage"
# --norsrc: no resource forks / AppleDouble ("._*") entries in the zip
COPYFILE_DISABLE=1 ditto -c -k --norsrc "$zstage" "$zip"
"$root/scripts/mac-contents-check.sh" zip "$zip"

echo "SHA256:"
( cd "$root/dist" && shasum -a 256 "$(basename "$dmg")" "$(basename "$zip")" )
