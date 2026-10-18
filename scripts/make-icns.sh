#!/usr/bin/env bash
# macOS only: builds assets/icon/aitrack.icns from the PNGs with iconutil.
# (scripts/make-icns.py makes an equivalent file on any OS; the committed .icns came from it.)
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
icons="$here/../assets/icon"
out="${1:-$icons/aitrack.icns}"
command -v iconutil >/dev/null || { echo "iconutil not found (macOS only). Use scripts/make-icns.py." >&2; exit 1; }
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
set="$tmp/aitrack.iconset"
mkdir "$set"
cp "$icons/aitrack-16.png"  "$set/icon_16x16.png"
cp "$icons/aitrack-32.png"  "$set/icon_16x16@2x.png"
cp "$icons/aitrack-32.png"  "$set/icon_32x32.png"
cp "$icons/aitrack-64.png"  "$set/icon_32x32@2x.png"
cp "$icons/aitrack-128.png" "$set/icon_128x128.png"
cp "$icons/aitrack-256.png" "$set/icon_128x128@2x.png"
cp "$icons/aitrack-256.png" "$set/icon_256x256.png"
cp "$icons/aitrack-512.png" "$set/icon_256x256@2x.png"
cp "$icons/aitrack-512.png" "$set/icon_512x512.png"
cp "$icons/aitrack-1024.png" "$set/icon_512x512@2x.png"
iconutil -c icns "$set" -o "$out"
echo "wrote $out"
