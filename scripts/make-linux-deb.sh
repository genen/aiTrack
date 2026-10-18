#!/usr/bin/env bash
# Builds dist/aitrack_<debver>_amd64.deb from target/release/aitrack and the files in packaging/.
# Run scripts/readme-pdf.sh first (dist/README.pdf is part of the package; it is never committed).
# Debian version: Cargo.toml's 0.3.15-beta becomes 0.3.15~beta (sorts before 0.3.15).
#   scripts/make-linux-deb.sh [BINARY]       default target/release/aitrack
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
bin="${1:-target/release/aitrack}"
[ -x "$bin" ] || { echo "$bin not found; run cargo build --release." >&2; exit 1; }
[ -f dist/README.pdf ] || { echo "dist/README.pdf not found; run scripts/readme-pdf.sh." >&2; exit 1; }

version=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
debver=$(printf '%s' "$version" | sed 's/-/~/')
deb="dist/aitrack_${debver}_amd64.deb"
date_rfc="$(git log -1 --format=%aD 2>/dev/null || date -R)"

pkg="$(mktemp -d)"
trap 'rm -rf "$pkg"' EXIT
chmod 755 "$pkg"
doc="$pkg/usr/share/doc/aitrack"
install -d -m 755 "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/share/applications" "$doc/docs"

install -m 755 "$bin" "$pkg/usr/bin/aitrack"
strip --strip-unneeded "$pkg/usr/bin/aitrack" 2>/dev/null || true
install -m 644 packaging/aitrack.desktop "$pkg/usr/share/applications/aitrack.desktop"
for s in 16 32 48 64 128 256 512; do
  install -D -m 644 "packaging/icons/aitrack-$s.png" "$pkg/usr/share/icons/hicolor/${s}x${s}/apps/aitrack.png"
done
install -m 644 README.md LICENSE THIRD-PARTY-NOTICES.md "$doc/"
install -m 644 dist/README.pdf "$doc/README.pdf"
cp -r docs/img "$doc/docs/img"
find "$doc/docs" -type d -exec chmod 755 {} +
find "$doc/docs" -type f -exec chmod 644 {} +

cat > "$doc/copyright" <<C
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: aiTrack
Upstream-Contact: Sir Gene <aitrack@sirgene.com>
Source: https://github.com/genen/aiTrack

Files: *
Copyright: Sir Gene
License: GPL-3.0-or-later

License: GPL-3.0-or-later
 This program is free software: you can redistribute it and/or modify it
 under the terms of the GNU General Public License as published by the
 Free Software Foundation, either version 3 of the License, or (at your
 option) any later version.
 .
 The full text is in /usr/share/doc/aitrack/LICENSE, and on Debian systems
 in /usr/share/common-licenses/GPL-3.
 .
 Dependency licences are listed in /usr/share/doc/aitrack/THIRD-PARTY-NOTICES.md.
C
chmod 644 "$doc/copyright"

{
  printf 'aitrack (%s) unstable; urgency=medium\n\n' "$debver"
  printf '  * Release %s. See CHANGELOG.md in this folder for the full history.\n\n' "$version"
  printf ' -- Sir Gene <aitrack@sirgene.com>  %s\n' "$date_rfc"
} | gzip -9n > "$doc/changelog.gz"
chmod 644 "$doc/changelog.gz"
gzip -9n -c CHANGELOG.md > "$doc/CHANGELOG.md.gz"; chmod 644 "$doc/CHANGELOG.md.gz"

size=$(du -sk --exclude=DEBIAN "$pkg" | cut -f1)
cat > "$pkg/DEBIAN/control" <<C
Package: aitrack
Version: $debver
Architecture: amd64
Section: utils
Priority: optional
Maintainer: Sir Gene <aitrack@sirgene.com>
Homepage: https://github.com/genen/aiTrack
Installed-Size: $size
Depends: libc6 (>= 2.35), libxkbcommon0, libxkbcommon-x11-0, libx11-6, libxcursor1, libxi6, libxrandr2, libwayland-client0, libwayland-egl1, libx11-xcb1, libxrender1, libgl1, libegl1
Recommends: libnotify-bin
Description: Small always-on-top meter for Claude and Grok plan usage
 aiTrack shows how much of your Claude and Grok plan limits you have used,
 in a compact window with LED-style bars. It reads the local files of the
 installed tools; it never sends a prompt.
C
chmod 644 "$pkg/DEBIAN/control"

mkdir -p dist
dpkg-deb --build --root-owner-group "$pkg" "$deb" >/dev/null
echo "Built $deb"
