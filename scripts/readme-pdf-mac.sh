#!/usr/bin/env bash
# Makes dist/README.pdf from README.md (same commit, so it cannot go stale). macOS (runs on any Unix with Chrome).
# Mirrors scripts/readme-pdf.ps1: Markdown -> HTML, screenshots in docs/img inlined as data URIs, a print
# stylesheet, then headless Chrome prints it to PDF. Build-time tools only: nothing of them goes into the dmg/zip.
# Why this route: Chrome headless is preinstalled on GitHub macos-14 runners, and the only missing piece is a
# Markdown converter. Python 3 ships with the runner; the `markdown` package (pinned) is the one pip install,
# put in a throwaway venv when it is not importable, so the runner needs no brew/npm and nothing is global.
# Fails if the PDF is missing, under 20 KB, or does not start with %PDF-.
#
#   scripts/readme-pdf-mac.sh [OUT_DIR]      default dist/;  CHROME=/path/to/chrome overrides the browser
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-$root/dist}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
pdf="$out/README.pdf"
rm -f "$pdf"

tmp="$(mktemp -d)"
# Chrome's helper processes can outlive the main one and keep writing to the profile, so stop them
# first; a leftover temp folder must not fail the build once the PDF is written.
trap 'pkill -f "$tmp/profile" 2>/dev/null; sleep 1; rm -rf "$tmp" 2>/dev/null || true' EXIT

py=python3
if ! "$py" -c 'import markdown' 2>/dev/null; then
  echo "Installing markdown==3.7 into a throwaway venv."
  "$py" -m venv "$tmp/venv"
  "$tmp/venv/bin/pip" install --quiet --disable-pip-version-check markdown==3.7
  py="$tmp/venv/bin/python"
fi

chrome="${CHROME:-}"
if [ -z "$chrome" ]; then
  for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
           "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge" \
           "$(command -v google-chrome || true)" "$(command -v chromium || true)"; do
    if [ -n "$c" ] && [ -x "$c" ]; then chrome="$c"; break; fi
  done
fi
[ -n "$chrome" ] || { echo "Neither Google Chrome nor Microsoft Edge was found to print the PDF." >&2; exit 1; }

"$py" - "$root" "$tmp/README.html" <<'PY'
import base64, os, re, sys
import markdown

root, dest = sys.argv[1], sys.argv[2]
with open(os.path.join(root, "README.md"), encoding="utf-8") as f:
    html = markdown.markdown(f.read(), extensions=["tables", "fenced_code", "sane_lists"])

def inline(m):
    path = os.path.join(root, *m.group(1).split("/"))
    if not os.path.isfile(path):
        sys.exit("The README shows %s, which does not exist." % m.group(1))
    with open(path, "rb") as f:
        return 'src="data:image/png;base64,%s"' % base64.b64encode(f.read()).decode()

html = re.sub(r'src="(docs/img/[^"]+)"', inline, html)
if 'src="docs/' in html:
    sys.exit("A local image was not inlined.")

css = """
@page { size: A4; margin: 16mm 15mm; }
body { font-family: -apple-system, "Helvetica Neue", Arial, sans-serif; font-size: 10.5pt; line-height: 1.45; color: #1a1a1a; }
h1 { font-size: 22pt; border-bottom: 2px solid #ccc; padding-bottom: 4pt; }
h2 { font-size: 15pt; border-bottom: 1px solid #ddd; padding-bottom: 3pt; margin-top: 18pt; page-break-after: avoid; }
h3, h4 { page-break-after: avoid; }
pre { background: #f4f4f4; border: 1px solid #ddd; padding: 6pt 8pt; white-space: pre-wrap; word-wrap: break-word; page-break-inside: avoid; }
code { font-family: Menlo, "Courier New", monospace; font-size: 9.5pt; background: #f4f4f4; }
pre code { background: none; }
table { border-collapse: collapse; margin: 6pt 0; page-break-inside: avoid; }
th, td { border: 1px solid #bbb; padding: 3pt 6pt; text-align: left; vertical-align: top; }
th { background: #eee; }
blockquote { border-left: 3px solid #bbb; margin-left: 0; padding-left: 10pt; color: #444; }
img { max-width: 100%; height: auto; page-break-inside: avoid; }
a { color: #0b5ca8; text-decoration: none; }
"""
page = ('<!doctype html><html><head><meta charset="utf-8"><title>aiTrack README</title><style>%s</style>'
        '</head><body>%s</body></html>' % (css, html))
with open(dest, "w", encoding="utf-8") as f:
    f.write(page)
PY

echo "Printing with $chrome"
"$chrome" --headless=new --disable-gpu --no-sandbox --no-pdf-header-footer \
  --user-data-dir="$tmp/profile" --print-to-pdf="$pdf" "file://$tmp/README.html" >/dev/null 2>&1 &
pid=$!
for _ in $(seq 1 120); do
  kill -0 "$pid" 2>/dev/null || break
  sleep 1
done
if kill -0 "$pid" 2>/dev/null; then echo "The browser did not exit in 120 s; it was stopped."; kill "$pid" || true; fi
wait "$pid" 2>/dev/null || true

[ -f "$pdf" ] || { echo "README.pdf was not written." >&2; exit 1; }
size="$(wc -c < "$pdf" | tr -d ' ')"
[ "$size" -ge 20000 ] || { echo "README.pdf is only $size bytes." >&2; exit 1; }
[ "$(head -c 5 "$pdf")" = "%PDF-" ] || { echo "README.pdf does not start with %PDF-." >&2; exit 1; }
echo "Built $pdf ($size bytes)"
