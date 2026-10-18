#!/usr/bin/env bash
# Fails if the Linux .deb does not carry the binary, the desktop file, the icon and the release extras
# (README.md, README.pdf, LICENSE, docs/img/ with pictures) under /usr/share/doc/aitrack, or if the binary was
# built with the test hooks. Lists the contents first. Used by make-linux-deb.sh and the release workflow.
#
#   scripts/linux-contents-check.sh deb FILE.deb   dpkg-deb -c listing + binary and PDF checks
#   scripts/linux-contents-check.sh names           check a path list (as printed by `dpkg-deb -c`, or plain
#                                                   absolute paths) read from stdin   (offline test)
# AITRACK_MIN_PDF_BYTES (default 20000) is the smallest README.pdf accepted.
set -euo pipefail

min_pdf="${AITRACK_MIN_PDF_BYTES:-20000}"
doc=usr/share/doc/aitrack

check_names() {
  local names missing=0 f
  # keep only the last field of `dpkg-deb -c` lines; strip "./" and any " -> target"
  names="$(sed -E 's# -> .*$##; s#^.* \./#./#; s#^\./##; s#^/##')"
  for f in usr/bin/aitrack usr/share/applications/aitrack.desktop \
           usr/share/icons/hicolor/256x256/apps/aitrack.png usr/share/icons/hicolor/48x48/apps/aitrack.png \
           "$doc/README.md" "$doc/README.pdf" "$doc/LICENSE" "$doc/THIRD-PARTY-NOTICES.md" \
           "$doc/copyright" "$doc/changelog.gz"; do
    if ! printf '%s\n' "$names" | grep -Fxq "$f"; then echo "MISSING: /$f" >&2; missing=1; fi
  done
  if ! printf '%s\n' "$names" | grep -Eq "^$doc/docs/img/.+[^/]\$"; then echo "MISSING: /$doc/docs/img/ (no pictures)" >&2; missing=1; fi
  return $missing
}

check_pdf() { # $1 = size in bytes, $2 = first 5 bytes
  [ "$1" -ge "$min_pdf" ] || { echo "README.pdf is only $1 bytes (need $min_pdf)." >&2; return 1; }
  [ "$2" = "%PDF-" ] || { echo "README.pdf does not start with %PDF-." >&2; return 1; }
}

main() {
  local kind="${1:-}" file="${2:-}"
  case "$kind" in
    names) check_names ;;
    deb)
      [ -f "$file" ] || { echo "$file not found." >&2; return 1; }
      echo "Contents of $(basename "$file"):"
      dpkg-deb -c "$file" | sed 's/^/  /'
      dpkg-deb -c "$file" | check_names
      local tmp rc=0
      tmp="$(mktemp -d)"
      dpkg-deb -x "$file" "$tmp"
      check_pdf "$(wc -c < "$tmp/$doc/README.pdf" | tr -d ' ')" "$(head -c 5 "$tmp/$doc/README.pdf")" || rc=1
      if grep -q 'test hooks' "$tmp/usr/bin/aitrack"; then echo "aitrack was built with the test-hooks feature." >&2; rc=1; fi
      rm -rf "$tmp"
      return $rc
      ;;
    *) echo "usage: $0 deb FILE | names < list" >&2; return 2 ;;
  esac
}

main "$@"
