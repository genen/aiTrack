#!/usr/bin/env bash
# Runs `cargo test` with the given arguments, then prunes stale build artifacts (see
# scripts/prune-target.sh), so repeated test runs do not fill the disk. The exit status is cargo's.
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here/.."
# tests never start real programs (browser, apps, banners); see launch::spawning_disabled
export AITRACK_NO_SPAWN=1
cargo test "$@"
status=$?
"$here/prune-target.sh" --minutes "${PRUNE_MINUTES:-30}" || true
exit $status
