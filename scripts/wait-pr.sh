#!/usr/bin/env bash
# Waits for a pull request's checks to finish and says why one failed.
#
#   scripts/wait-pr.sh <pr-number> [min-checks]
#   scripts/wait-pr.sh --merge <pr-number> [min-checks]
#
# Polls every 15 s until the PR has at least <min-checks> checks and none is
# pending. Right after a push no checks have registered yet; that counts as
# still waiting, not as a failure. <min-checks> defaults to the number of jobs
# in the latest completed CI run on main (6 if that can't be read); pass it
# when a change adds or removes a CI job.
#
# Then prints one `name: result` line per check, sorted by name, and for each
# failed GitHub Actions job the last 30 lines of its failed-step log.
#
# Exit status: 0 if every check passed, was skipped or was neutral; 1 if any
# failed or was cancelled; 2 after 30 minutes without a result, or on a usage
# error. With --merge, a PR whose checks all passed is merged with
# `gh pr merge --merge --delete-branch`, which deletes the remote branch but
# leaves the local checkout alone; anything else is left alone. A merge that
# GitHub refuses (a conflict, say) also exits 1.
set -euo pipefail

usage() {
  echo "usage: $0 [--merge] <pr-number> [min-checks]" >&2
  exit 2
}

merge=0
if [ "${1:-}" = "--merge" ]; then
  merge=1
  shift
fi
[ $# -ge 1 ] && [ $# -le 2 ] || usage
pr=$1
case $pr in '' | *[!0-9]*) usage ;; esac

interval=15
timeout=1800

if [ $# -eq 2 ]; then
  min=$2
else
  min=$(gh run list --workflow ci.yml --branch main --status completed --limit 1 \
    --json databaseId -q '.[0].databaseId' 2>/dev/null || true)
  if [ -n "$min" ]; then
    min=$(gh run view "$min" --json jobs -q '.jobs | length' 2>/dev/null || true)
  fi
fi
case $min in '' | *[!0-9]* | 0) min=6 ;; esac

# Fails early on a PR number that doesn't exist, rather than polling for 30 min.
gh pr view "$pr" --json state -q .state >/dev/null || exit 2

fields=name,state,bucket,link
errfile=$(mktemp)
trap 'rm -f "$errfile"' EXIT
start=$(date +%s)
while :; do
  # `gh pr checks` exits 8 while checks are pending and 1 when none are
  # reported yet, so its status says nothing here; the JSON does.
  err=0
  counts=$(gh pr checks "$pr" --json "$fields" \
    -q '"\(length) \(map(select(.bucket == "pending")) | length)"' 2>"$errfile") || err=$?
  case $counts in
    [0-9]*' '[0-9]*) set -- $counts ;;
    *)
      msg=$(<"$errfile")
      case $msg in
        *no\ checks\ reported*) ;;
        *) echo "wait-pr: gh pr checks failed (exit $err): $msg" >&2 ;;
      esac
      set -- 0 0 ;;
  esac
  total=$1 pending=$2
  if [ "$total" -ge "$min" ] && [ "$pending" -eq 0 ]; then
    break
  fi
  elapsed=$(( $(date +%s) - start ))
  if [ "$elapsed" -ge "$timeout" ]; then
    echo "wait-pr: gave up on PR #$pr after $((timeout / 60)) min:" \
      "$total of $min checks registered, $pending pending" >&2
    exit 2
  fi
  echo "wait-pr: PR #$pr: $total of $min checks registered, $pending pending (${elapsed}s)" >&2
  sleep "$interval"
done

gh pr checks "$pr" --json "$fields" \
  -q 'sort_by(.name) | .[] | "\(.name): \(.state | ascii_downcase)"' 2>/dev/null || true

# One "run-id job-id name" line per failed or cancelled Actions job.
failed=$(gh pr checks "$pr" --json "$fields" -q '
  .[] | select(.bucket == "fail" or .bucket == "cancel")
  | (.link | capture("/runs/(?<run>[0-9]+)/job/(?<job>[0-9]+)")? // {run: "-", job: "-"})
    as $id | "\($id.run) \($id.job) \(.name)"' 2>/dev/null || true)

if [ -n "$failed" ]; then
  tab=$(printf '\t') esc=$(printf '\033')
  while read -r run job name; do
    echo
    if [ "$run" = "-" ]; then
      echo "--- $name: no Actions log (external check)"
      continue
    fi
    echo "--- $name: last 30 lines of gh run view $run --job $job --log-failed"
    # Drops the "job<TAB>step<TAB>timestamp " prefix and colour codes.
    gh run view "$run" --job "$job" --log-failed 2>&1 | tail -n 30 |
      sed -E -e "s/^[^$tab]*$tab[^$tab]*$tab[0-9T:.-]+Z //" -e "s/$esc\[[0-9;]*m//g" || true
  done <<EOF
$failed
EOF
  [ "$merge" -eq 1 ] && echo "wait-pr: not merging PR #$pr: a check failed" >&2
  exit 1
fi

if [ "$merge" -eq 1 ]; then
  # --repo keeps gh off the local checkout: without it gh also switches to and
  # pulls main, which fails in a worktree when main is checked out elsewhere,
  # and then skips deleting the remote branch.
  repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
  if ! gh pr merge "$pr" --repo "$repo" --merge --delete-branch; then
    echo "wait-pr: checks passed but merging PR #$pr failed" >&2
    exit 1
  fi
fi
exit 0
