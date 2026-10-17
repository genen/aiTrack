#!/usr/bin/env bash
# Fails if a Mac zip or dmg does not carry the release extras next to the app: README.md, README.pdf, LICENSE and
# docs/img/ (with at least one picture). It also fails if any of them was put INSIDE aiTrack.app (that would break
# the code signature). Lists the contents first. Used by make-dmg.sh and the release workflow.
#
#   scripts/mac-contents-check.sh zip FILE.zip     unzip -Z1 listing   (any Unix)
#   scripts/mac-contents-check.sh dmg FILE.dmg     hdiutil attach -readonly -nobrowse, ls, detach   (macOS)
#   scripts/mac-contents-check.sh names            check a list of paths read from stdin   (used by tests)
# AITRACK_MIN_PDF_BYTES (default 20000) is the smallest README.pdf accepted by the zip/dmg checks.
set -euo pipefail

min_pdf="${AITRACK_MIN_PDF_BYTES:-20000}"

# Reads paths (one per line, relative to the zip/dmg root) on stdin.
check_names() {
  local names missing=0 f
  names="$(sed 's#^\./##')"
  for f in README.md README.pdf LICENSE; do
    if ! printf '%s\n' "$names" | grep -Fxq "$f"; then echo "MISSING: $f" >&2; missing=1; fi
  done
  if ! printf '%s\n' "$names" | grep -Eq '^docs/img/.+[^/]$'; then echo "MISSING: docs/img/ (no pictures)" >&2; missing=1; fi
  if ! printf '%s\n' "$names" | grep -Eq '^aiTrack\.app/Contents/MacOS/aitrack$'; then echo "MISSING: aiTrack.app/Contents/MacOS/aitrack" >&2; missing=1; fi
  if printf '%s\n' "$names" | grep -Eq '^aiTrack\.app/(.*/)?(README\.(md|pdf)|LICENSE)$'; then
    echo "WRONG PLACE: release extras are inside aiTrack.app (breaks the signature)." >&2; missing=1
  fi
  return $missing
}

check_pdf() { # $1 = size in bytes, $2 = first 5 bytes
  [ "$1" -ge "$min_pdf" ] || { echo "README.pdf is only $1 bytes (need $min_pdf)." >&2; return 1; }
  [ "$2" = "%PDF-" ] || { echo "README.pdf does not start with %PDF-." >&2; return 1; }
}

main() {
  local kind="${1:-}" file="${2:-}"
  case "$kind" in
    names)
      check_names
      ;;
    zip)
      [ -f "$file" ] || { echo "$file not found." >&2; return 1; }
      echo "Contents of $(basename "$file"):"
      unzip -Z1 "$file" | sed 's/^/  /'
      unzip -Z1 "$file" | check_names
      check_pdf "$(unzip -p "$file" README.pdf | wc -c | tr -d ' ')" "$(unzip -p "$file" README.pdf | head -c 5)"
      ;;
    dmg)
      [ -f "$file" ] || { echo "$file not found." >&2; return 1; }
      local mnt rc=0
      mnt="$(mktemp -d)"
      hdiutil attach -readonly -nobrowse -noverify -mountpoint "$mnt" "$file" >/dev/null
      echo "Contents of $(basename "$file"):"
      ls -la "$mnt" | sed 's/^/  /'
      ls -R "$mnt/docs" 2>/dev/null | sed 's/^/  /' || true
      ( cd "$mnt" && find . \( -type f -o -type l \) -not -name '.*' | sed 's#^\./##' ) | check_names || rc=1
      if [ -f "$mnt/README.pdf" ]; then
        check_pdf "$(wc -c < "$mnt/README.pdf" | tr -d ' ')" "$(head -c 5 "$mnt/README.pdf")" || rc=1
      fi
      hdiutil detach "$mnt" -quiet || hdiutil detach "$mnt" -force -quiet || true
      rmdir "$mnt" 2>/dev/null || true
      return $rc
      ;;
    *)
      echo "usage: $0 zip FILE | dmg FILE | names < list" >&2
      return 2
      ;;
  esac
}

main "$@"
