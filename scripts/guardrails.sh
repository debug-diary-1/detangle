#!/usr/bin/env bash
# Check the staged tree, every commit in a range, or every outgoing push commit.
# Usage: scripts/guardrails.sh [--staged | <base>..<head> | --pre-push <remote> <url>]
set -euo pipefail
# Replacement refs are local views; pushes publish the original objects.
export GIT_NO_REPLACE_OBJECTS=1
cd "$(git rev-parse --show-toplevel)"
identity='15070765+debug-diary-1@users.noreply.github.com'
failed=0
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() {
  printf 'FAIL [%s] %s\n      fix: %s\n' "$1" "$2" "$3" >&2
  failed=1
}
invalid() { printf 'guardrails: %s\n' "$*" >&2; exit 2; }

check_tree() {
  local tree=$1 file status=0
  # Assemble the pattern so this checker does not match itself.
  local names="cru""iser\\|dep""cruise"
  git grep -i -l -z "$names" "$tree" -- > "$tmp/names" || status=$?
  [ "$status" -le 1 ] || invalid "cannot inspect tree $tree"
  while IFS= read -r -d '' file; do
    file=${file#*:}
    fail naming "$file in $tree names a competing tool" \
      "remove the tool's name from $file; describe it generically, or link detangle-bench"
  done < "$tmp/names"
  git ls-tree -r --name-only -z "$tree" -- docs > "$tmp/docs"
  while IFS= read -r -d '' file; do
    case "$file" in
      docs/reference.md | docs/releasing.md | docs/demo/*) ;;
      *) fail docs "$file in $tree is not an allowed path under docs/" \
        "keep design material out of the repository and remove it from every unpublished commit (allowed: docs/reference.md, docs/releasing.md, docs/demo/)" ;;
    esac
  done < "$tmp/docs"
}

check_identity() {
  local c=$1 an ae ce parents short body
  an=$(git log -1 --format=%an "$c")
  ae=$(git log -1 --format=%ae "$c")
  ce=$(git log -1 --format=%ce "$c")
  parents=$(git log -1 --format=%p "$c" | wc -w)
  short=$(git log -1 --format='%h %s' "$c")
  case "$an $ae" in *'[bot]'*) return ;; esac
  if [ "$parents" -gt 1 ] && [ "$ce" = noreply@github.com ]; then return; fi
  if [ "$(printf '%s' "$an" | tr '[:upper:]' '[:lower:]')" = pallav ] && [ "$ae" != "$identity" ]; then
    fail identity "commit $short is authored as $an <$ae>" \
      "amend or rebase it with: git -c user.name=Pallav -c user.email=$identity commit --amend --reset-author --no-edit"
  fi
  body=$(git log -1 --format=%B "$c")
  if grep -qiE '^[[:space:]]*co-authored-by:.*(claude|anthropic)' <<< "$body"; then
    fail identity "commit $short credits Claude as a co-author" \
      "reword it without the trailer: git commit --amend (or git rebase -i, reword)"
  fi
}

check_commits() {
  local c count=0
  while IFS= read -r c; do
    [ -n "$c" ] || continue
    check_tree "$c"
    check_identity "$c"
    count=$((count + 1))
  done < "$tmp/commits"
  printf 'checked %s commit(s)\n' "$count"
}

check_push() {
  local remote=$1 url=$2 local_ref local_oid remote_ref remote_oid extra oid ref commit
  local width zero
  width=$(git hash-object --stdin < /dev/null)
  width=${#width}
  printf -v zero '%*s' "$width" ''
  zero=${zero// /0}
  : > "$tmp/tips"
  : > "$tmp/old"
  : > "$tmp/updates"
  while IFS=' ' read -r local_ref local_oid remote_ref remote_oid extra || [ -n "$local_ref$local_oid$remote_ref$remote_oid$extra" ]; do
    [ -n "$local_ref" ] && [ -n "$local_oid" ] && [ -n "$remote_ref" ] && [ -n "$remote_oid" ] && [ -z "$extra" ] || invalid 'malformed pre-push input'
    [[ "$local_oid" =~ ^[0-9a-f]+$ && ${#local_oid} -eq "$width" && "$remote_oid" =~ ^[0-9a-f]+$ && ${#remote_oid} -eq "$width" ]] || invalid 'invalid object ID in pre-push input'
    git check-ref-format "$remote_ref" || invalid "invalid destination ref $remote_ref"
    printf '%s\t%s\n' "$remote_oid" "$remote_ref" >> "$tmp/updates"
    if [ "$local_oid" != "$zero" ]; then
      commit=$(git rev-parse --verify "$local_oid^{commit}") || invalid "push target $local_oid is not a locally available commit"
      ref=$(git rev-parse --verify --end-of-options "$local_ref^{commit}") || invalid "invalid source ref $local_ref"
      [ "$ref" = "$commit" ] || invalid "source ref $local_ref does not match its push object ID"
      printf '%s\n' "$commit" >> "$tmp/tips"
    elif [ "$local_ref" != '(delete)' ] || [ "$remote_oid" = "$zero" ]; then
      invalid 'invalid deletion in pre-push input'
    fi
    if [ "$local_oid" != "$zero" ] && [ "$remote_oid" != "$zero" ]; then
      # An update must have its remote base locally, even after a force push.
      commit=$(git rev-parse --verify "$remote_oid^{commit}") || invalid 'remote base is unavailable locally; fetch the remote before pushing'
    fi
  done
  # No new objects leave the repository for a deletion-only push.
  if [ ! -s "$tmp/tips" ]; then : > "$tmp/commits"; return; fi
  # Use the server's refs, not potentially stale remote-tracking refs. Published
  # history on another branch must not block an ordinary push or new branch.
  git ls-remote -- "$url" > "$tmp/remote" || invalid "cannot read refs from $remote"
  while IFS=$'\t' read -r remote_oid remote_ref; do
    if [ "$remote_oid" != "$zero" ]; then
      grep -Fqx "$remote_oid"$'\t'"$remote_ref" "$tmp/remote" || invalid 'remote refs changed or push input is invalid; retry the push'
    elif awk -F '\t' -v ref="$remote_ref" '$2 == ref { found=1 } END { exit !found }' "$tmp/remote"; then
      invalid 'new destination already exists on the remote; retry the push'
    fi
  done < "$tmp/updates"
  while IFS=$'\t' read -r oid ref; do
    [[ "$oid" =~ ^[0-9a-f]+$ && ${#oid} -eq "$width" ]] || invalid 'invalid object ID advertised by remote'
    if commit=$(git rev-parse --verify "$oid^{commit}" 2>/dev/null); then
      printf '^%s\n' "$commit" >> "$tmp/old"
    fi
  done < "$tmp/remote"
  cat "$tmp/tips" "$tmp/old" > "$tmp/revisions"
  git rev-list --stdin < "$tmp/revisions" > "$tmp/commits" || invalid 'cannot determine outgoing commits'
}

case $# in
  0) check_tree "$(git write-tree)" ;;
  1)
    if [ "$1" = --staged ]; then
      check_tree "$(git write-tree)"
    else
      # Resolve both ends explicitly: revision options and open ranges are not
      # accepted, and a typo must never silently skip a check.
      [[ "$1" == *..* && "$1" != *...* ]] || invalid "usage: $0 [--staged | <base>..<head> | --pre-push <remote> <url>]"
      base=${1%%..*}; head=${1#*..}
      [ -n "$base" ] && [ -n "$head" ] && [[ "$head" != *..* ]] || invalid 'invalid commit range'
      base=$(git rev-parse --verify --end-of-options "$base^{commit}") || invalid 'invalid range base'
      head=$(git rev-parse --verify --end-of-options "$head^{commit}") || invalid 'invalid range head'
      git rev-list "$base..$head" > "$tmp/commits"
      check_tree "$head"
      check_commits
    fi ;;
  3)
    [ "$1" = --pre-push ] && [ -n "$2" ] && [ -n "$3" ] || invalid 'invalid pre-push arguments'
    check_push "$2" "$3"
    check_commits ;;
  *) invalid "usage: $0 [--staged | <base>..<head> | --pre-push <remote> <url>]" ;;
esac
if [ "$failed" -ne 0 ]; then echo 'guardrails: FAILED' >&2; exit 1; fi
echo 'guardrails: ok'
