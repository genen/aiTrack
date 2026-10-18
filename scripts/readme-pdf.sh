#!/usr/bin/env bash
# Makes dist/README.pdf on Linux. scripts/readme-pdf-mac.sh is reused unchanged: it is plain bash + python3 and
# finds google-chrome or chromium on PATH (CHROME=/path overrides). Needs python3 (venv module) or the
# `markdown` package. Usage: scripts/readme-pdf.sh [OUT_DIR]
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/readme-pdf-mac.sh" "$@"
