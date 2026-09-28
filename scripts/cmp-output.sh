#!/usr/bin/env bash
# Checks that two detangle binaries print the same thing.
#
#   scripts/cmp-output.sh <base-bin> <new-bin> <project>...
#
# Runs `check`, `check -f json`, `graph -f json` and `graph -f dot` with both
# binaries on each project, and compares stdout, stderr and the exit status
# byte for byte. Only timings are masked: the "· 38ms" at the end of `check`'s
# summary line and the `timings` object in `graph -f json`. Use it to show a
# refactor or performance change leaves the output alone.
set -euo pipefail

if [ $# -lt 3 ]; then
  echo "usage: $0 <base-bin> <new-bin> <project>..." >&2
  exit 2
fi
base=$1 new=$2
shift 2

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mask() {
  sed -E \
    -e 's/ · [0-9]+ms$/ · <ms>/' \
    -e 's/"(scan_ms|graph_ms)": [0-9.e+-]+/"\1": 0/'
}

run() { # <bin> <out-prefix> <project> <args>...
  local bin=$1 out=$2 dir=$3
  shift 3
  local status=0
  "$bin" "$@" "$dir" >"$out.stdout" 2>"$out.stderr" || status=$?
  echo "$status" >"$out.status"
  mask <"$out.stdout" >"$out.stdout.masked"
  mask <"$out.stderr" >"$out.stderr.masked"
}

failed=0
for dir in "$@"; do
  for cmd in "check" "check -f json" "graph -f json" "graph -f dot"; do
    # shellcheck disable=SC2086 # $cmd is split into arguments on purpose
    run "$base" "$tmp/base" "$dir" $cmd
    # shellcheck disable=SC2086
    run "$new" "$tmp/new" "$dir" $cmd
    same=1
    for part in stdout.masked stderr.masked status; do
      if ! cmp -s "$tmp/base.$part" "$tmp/new.$part"; then
        same=0
        echo "DIFF  $dir: detangle $cmd (${part%.masked})"
        diff "$tmp/base.$part" "$tmp/new.$part" | head -20 || true
      fi
    done
    if [ $same = 1 ]; then
      echo "same  $dir: detangle $cmd ($(wc -c <"$tmp/base.stdout" | tr -d ' ') bytes, exit $(cat "$tmp/base.status"))"
    else
      failed=1
    fi
  done
done
exit $failed
