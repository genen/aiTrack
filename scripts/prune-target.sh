#!/usr/bin/env bash
# Deletes STALE build artifacts from cargo's target directory (never sources).
#
# Every code change gives each test binary and each example a new hash-named file
# (target/debug/deps/<name>-<hash>), and cargo never removes the old ones, so a development
# machine slowly fills up (tens of GB for a project with many integration tests). This script
# removes the superseded copies and nothing else.
#
# What it removes (only below a cargo target dir, only regular files, symlinks are never followed):
#   * superseded builds: in <target>/{debug,release}/{deps,examples} and
#     <target>/<triple>/{debug,release}/{deps,examples}, of the hash-suffixed artifacts
#     (name-<16 hex>.ext, larger than --min-size-mb, default 2) that share a name and extension,
#     every one except the NEWEST, once it has not been modified for --minutes (default 30). The
#     newest build of each crate / test / example is never deleted, however old it is, so live
#     dependencies are not rebuilt on every run;
#   * incremental compilation directories (<profile>/incremental/*) older than 45 minutes;
#   * with --all-but-newest: the same superseded builds without the --minutes wait (only the
#     5 minute floor below applies);
#   * with --emergency (disk full): ALSO every file larger than --min-size-mb in those folders that
#     was not modified for --minutes, the newest builds and dependencies included. The next build
#     recompiles them.
# Nothing modified in the last 5 minutes is ever touched (a build may be running).
#
# Usage: scripts/prune-target.sh [--target DIR] [--minutes N] [--min-size-mb N]
#                                [--all-but-newest] [--emergency] [--dry-run] [-h]

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="$here/../target"
minutes=30
min_mb=2
all_but_newest=0
emergency=0
dry=0
SAFE_MIN=5          # never touch anything newer than this many minutes
INCREMENTAL_MIN=45  # incremental directories older than this are removed

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="$2"; shift 2 ;;
    --minutes) minutes="$2"; shift 2 ;;
    --min-size-mb) min_mb="$2"; shift 2 ;;
    --all-but-newest) all_but_newest=1; shift ;;
    --emergency) emergency=1; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "prune-target: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$minutes$min_mb" in *[!0-9]*|"") echo "prune-target: --minutes and --min-size-mb must be whole numbers" >&2; exit 2 ;; esac

if [ ! -d "$target" ] || [ -L "$target" ]; then
  echo "prune-target: $target is not a directory (nothing to do)" >&2
  exit 0
fi
target="$(cd "$target" && pwd -P)"
# cargo marks its target directories with CACHEDIR.TAG; refuse anything else
if ! grep -q "Signature: 8a477f597d28d172789f06886806bc55" "$target/CACHEDIR.TAG" 2>/dev/null; then
  echo "prune-target: $target has no cargo CACHEDIR.TAG, refusing to delete anything" >&2
  exit 1
fi

# profile directories: <target>/{debug,release} and <target>/<triple>/{debug,release}
profile_dirs=()
for d in "$target"/debug "$target"/release "$target"/*/debug "$target"/*/release; do
  [ -d "$d" ] && [ ! -L "$d" ] && profile_dirs+=("$d")
done

removed_bytes=0
removed_files=0
# a file listed by two rules (emergency + superseded) counts once. (A newline-separated list, not
# `declare -A`: macOS ships bash 3.2, which has no associative arrays.)
nl=$'\n'
seen_list="$nl"

# Portability (GNU on Linux, BSD on macOS): files directly in $1 matching the extra find tests, as
# "mtime<TAB>bytes<TAB>path" lines.
if find / -maxdepth 0 -printf '' >/dev/null 2>&1; then
  list_files() { local d="$1"; shift; find "$d" -maxdepth 1 -type f "$@" -printf '%T@\t%s\t%p\n' 2>/dev/null; }
else
  list_files() { local d="$1"; shift; find "$d" -maxdepth 1 -type f "$@" -exec stat -f '%m%t%z%t%N' {} + 2>/dev/null; }
fi
# bytes used by a directory tree (du -sb is GNU only)
dir_bytes() {
  local b
  if b=$(du -sb -- "$1" 2>/dev/null); then echo "${b%%[[:space:]]*}"
  else b=$(du -sk -- "$1" 2>/dev/null | cut -f1); echo $(( ${b:-0} * 1024 )); fi
}
# --one-file-system is GNU only
rm_dir() { if rm --help 2>&1 | grep -q -- --one-file-system; then rm -rf --one-file-system -- "$1"; else rm -rf -- "$1"; fi; }

remove_file() { # path size reason
  local p="$1" size="$2" why="$3"
  case "$seen_list" in *"$nl$p$nl"*) return 0 ;; esac
  seen_list="$seen_list$p$nl"
  if [ "$dry" -eq 1 ]; then
    printf 'would remove %s (%s MB, %s)\n' "${p#"$target"/}" "$((size / 1048576))" "$why"
  else
    rm -f -- "$p" || return 0
    printf 'removed %s (%s MB, %s)\n' "${p#"$target"/}" "$((size / 1048576))" "$why"
  fi
  removed_bytes=$((removed_bytes + size))
  removed_files=$((removed_files + 1))
}

for pd in ${profile_dirs[@]+"${profile_dirs[@]}"}; do
  for sub in deps examples; do
    dir="$pd/$sub"
    [ -d "$dir" ] && [ ! -L "$dir" ] || continue
    age=$(( minutes > SAFE_MIN ? minutes : SAFE_MIN ))
    if [ "$emergency" -eq 1 ]; then
      # disk-full mode: every big file untouched for $age minutes, whatever it is
      while IFS=$'\t' read -r _mtime size path; do
        [ -n "$path" ] && remove_file "$path" "$size" "emergency: not modified for $age min"
      done < <(list_files "$dir" -size "+${min_mb}M" -mmin "+$age")
    fi

    # superseded builds: group name-<16 hex>.ext by name+ext, newest first; everything but the
    # first of a group is a candidate, if it is old enough. (Files that are not hash-suffixed are
    # not builds of ours to judge and stay.)
    wait=$age
    [ "$all_but_newest" -eq 1 ] && wait=$SAFE_MIN
    while IFS=$'\t' read -r group size path; do
      remove_file "$path" "$size" "older build of $group"
    done < <(
      list_files "$dir" -size "+${min_mb}M" \
      | awk -F'\t' '{
          n = split($3, parts, "/"); base = parts[n];
          # name-<16 hex>[.ext]: the hash is the part after the last dash, up to the first dot
          p = 0; for (i = length(base); i > 0; i--) if (substr(base, i, 1) == "-") { p = i; break }
          if (p == 0) next;
          rest = substr(base, p + 1); dot = index(rest, ".");
          h = dot ? substr(rest, 1, dot - 1) : rest; ext = dot ? substr(rest, dot) : "";
          if (length(h) == 16 && h ~ /^[0-9a-f]+$/)
            print substr(base, 1, p - 1) ext "\t" $1 "\t" $2 "\t" $3;
        }' \
      | sort -t$'\t' -k1,1 -k2,2nr \
      | awk -F'\t' -v now="$(date +%s)" -v wait="$wait" '{
          if ($1 != last) { last = $1; next }          # the newest of its group stays
          if (now - $2 > wait * 60) print $1 "\t" $3 "\t" $4;
        }'
    )
  done
  inc="$pd/incremental"
  if [ -d "$inc" ] && [ ! -L "$inc" ]; then
    while IFS= read -r d; do
      size=$(dir_bytes "$d")
      if [ "$dry" -eq 1 ]; then
        printf 'would remove %s (%s MB, incremental data older than %s min)\n' "${d#"$target"/}" "$((size / 1048576))" "$INCREMENTAL_MIN"
      else
        rm_dir "$d" || continue
        printf 'removed %s (%s MB, incremental data older than %s min)\n' "${d#"$target"/}" "$((size / 1048576))" "$INCREMENTAL_MIN"
      fi
      removed_bytes=$((removed_bytes + size))
      removed_files=$((removed_files + 1))
    done < <(find "$inc" -mindepth 1 -maxdepth 1 -type d -mmin "+$INCREMENTAL_MIN" 2>/dev/null)
  fi
done

verb="Reclaimed"; [ "$dry" -eq 1 ] && verb="Would reclaim"
printf '%s %s MB in %s item(s) from %s\n' "$verb" "$((removed_bytes / 1048576))" "$removed_files" "$target"
