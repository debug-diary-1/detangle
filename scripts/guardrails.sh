#!/usr/bin/env bash
# Mechanical checks for rules that guard against publishing something that
# can't be taken back.
#
#   scripts/guardrails.sh                  tree checks (naming, docs/)
#   scripts/guardrails.sh <base>..<head>   tree checks, plus commit identity
#                                          on every commit in the range
#
# Exits non-zero if any check fails.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

identity='15070765+debug-diary-1@users.noreply.github.com'
failed=0

fail() {
  # fail <rule> <what> <fix>
  printf 'FAIL [%s] %s\n      fix: %s\n' "$1" "$2" "$3" >&2
  failed=1
}

# 1. Naming: the repository never names a competing tool. The pattern is
# assembled from pieces so this script doesn't match itself.
names="cru""iser\\|dep""cruise"
while IFS= read -r file; do
  [ -n "$file" ] || continue
  fail naming "$file names a competing tool" \
    "remove the tool's name from $file; describe it generically, or link detangle-bench"
done < <(git grep -il "$names" || true)

# 2. No design docs in docs/: only the reference, the release notes and the
# demo assets are published there.
while IFS= read -r file; do
  case "$file" in
    docs/reference.md | docs/releasing.md | docs/demo/*) ;;
    *)
      fail docs "$file is not an allowed path under docs/" \
        "keep design material out of the repository: git rm --cached '$file' (allowed: docs/reference.md, docs/releasing.md, docs/demo/)"
      ;;
  esac
done < <(git ls-files docs)

# 3. Commit identity, on a range: the maintainer's commits (author name
# Pallav) use the project identity, and no commit credits Claude or Anthropic
# as a co-author. Other contributors' commits, and their own co-authors, are
# fine. Bot authors and merge commits GitHub creates are exempt.
if [ $# -gt 0 ]; then
  range=$1
  case "$range" in
    *..*) ;;
    *) echo "usage: $0 [<base>..<head>]" >&2; exit 2 ;;
  esac
  commits=$(git rev-list "$range")
  for c in $commits; do
    an=$(git log -1 --format=%an "$c")
    ae=$(git log -1 --format=%ae "$c")
    ce=$(git log -1 --format=%ce "$c")
    parents=$(git log -1 --format=%p "$c" | wc -w)
    short=$(git log -1 --format='%h %s' "$c")
    case "$an $ae" in *'[bot]'*) continue ;; esac
    if [ "$parents" -gt 1 ] && [ "$ce" = noreply@github.com ]; then
      continue
    fi
    if [ "$(printf '%s' "$an" | tr '[:upper:]' '[:lower:]')" = pallav ] && [ "$ae" != "$identity" ]; then
      fail identity "commit $short is authored as $an <$ae>" \
        "amend or rebase it with: git -c user.name=Pallav -c user.email=$identity commit --amend --reset-author --no-edit"
    fi
    body=$(git log -1 --format=%B "$c")
    if grep -qiE '^[[:space:]]*co-authored-by:.*(claude|anthropic)' <<<"$body"; then
      fail identity "commit $short credits Claude as a co-author" \
        "reword it without the trailer: git commit --amend (or git rebase -i, reword)"
    fi
  done
  echo "checked $(printf '%s\n' "$commits" | grep -c . || true) commit(s) in $range"
fi

if [ "$failed" -ne 0 ]; then
  echo "guardrails: FAILED" >&2
  exit 1
fi
echo "guardrails: ok"
