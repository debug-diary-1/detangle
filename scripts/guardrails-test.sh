#!/usr/bin/env bash
# Single-quoted bash -c programs expand their arguments in the child shell.
# shellcheck disable=SC2016
# Fast, offline regression tests. All commits and pushes use disposable repos.
set -euo pipefail
source_root=$(cd "$(dirname "$0")/.." && pwd)
sandbox=$(mktemp -d)
trap 'rm -rf "$sandbox"' EXIT
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NO_REPLACE_OBJECTS || true
passed=0
expect_ok() {
  if ! "$@" > "$sandbox/output" 2>&1; then
    cat "$sandbox/output" >&2
    printf 'FAIL: expected success: %s\n' "$*" >&2
    exit 1
  fi
  passed=$((passed + 1))
}
expect_fail() {
  if "$@" > "$sandbox/output" 2>&1; then
    cat "$sandbox/output" >&2
    printf 'FAIL: expected rejection: %s\n' "$*" >&2
    exit 1
  fi
  passed=$((passed + 1))
}
assert_message() {
  if ! grep -Fq "$1" "$sandbox/output"; then
    cat "$sandbox/output" >&2
    printf 'FAIL: missing diagnostic: %s\n' "$1" >&2
    exit 1
  fi
}
commit_unchecked() { git -c core.hooksPath=/dev/null commit -qm "$1"; }
new_repo() {
  mkdir "$sandbox/$1"
  cd "$sandbox/$1"
  git init -q -b main
  git config user.name Tester
  git config user.email tester@example.com
  mkdir scripts .githooks docs
  cp "$source_root/scripts/guardrails.sh" "$source_root/scripts/install-hooks.sh" scripts/
  cp "$source_root/.githooks/pre-commit" "$source_root/.githooks/pre-push" .githooks/
  printf 'reference\n' > docs/reference.md
  git add .
  commit_unchecked baseline
}

new_repo staged
expect_ok bash scripts/guardrails.sh
printf 'private\n' > docs/private.md
git add docs/private.md
# The file is staged even when the worktree no longer contains it.
rm docs/private.md
expect_fail bash .githooks/pre-commit
assert_message '[docs]'
git reset -q -- docs/private.md
# An unstaged private doc must not prevent a clean commit.
printf 'private\n' > docs/private.md
expect_ok bash .githooks/pre-commit
rm docs/private.md
name='cru''iser'
printf '%s\n' "$name" > names.txt
git add names.txt
printf 'clean worktree\n' > names.txt
expect_fail bash .githooks/pre-commit
assert_message '[naming]'
git reset -q -- names.txt
printf 'clean staged content\n' > names.txt
git add names.txt
printf '%s\n' "$name" > names.txt
expect_ok bash .githooks/pre-commit
# Conflicted stages must fail closed.
blob=$(printf 'x\n' | git hash-object -w --stdin)
printf '100644 %s 1\tconflict\n100644 %s 2\tconflict\n' "$blob" "$blob" | git update-index --index-info
expect_fail bash .githooks/pre-commit
git reset -q HEAD -- conflict names.txt
rm names.txt
# Filenames containing newlines must not split or hide prohibited paths.
printf 'private\n' > $'docs/private\nnotes.md'
git add docs/
expect_fail bash .githooks/pre-commit
assert_message '[docs]'
git reset -q -- docs/
rm $'docs/private\nnotes.md'
expect_fail bash scripts/guardrails.sh --unknown
expect_fail bash scripts/guardrails.sh 'HEAD..missing'
expect_fail bash scripts/guardrails.sh '..HEAD'
expect_fail bash scripts/guardrails.sh 'HEAD...HEAD'
expect_fail bash scripts/guardrails.sh HEAD..HEAD extra

# Installation is repeatable, with refusal before changing unrelated hooks.
expect_ok bash scripts/install-hooks.sh
expect_ok bash scripts/install-hooks.sh
[ "$(git config --get core.hooksPath)" = .githooks ]
git config core.hooksPath other-hooks
expect_fail bash scripts/install-hooks.sh
[ "$(git config --get core.hooksPath)" = other-hooks ]
git config --unset core.hooksPath
printf '#!/bin/sh\nexit 0\n' > .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit
expect_fail bash scripts/install-hooks.sh
[ -z "$(git config --get core.hooksPath || true)" ]
rm .git/hooks/pre-commit
expect_ok bash scripts/install-hooks.sh
printf 'private\n' > docs/private.md
git add docs/private.md
expect_fail git commit -qm 'blocked staged doc'
assert_message '[docs]'
git reset -q -- docs/private.md
rm docs/private.md

new_repo push
remote="$sandbox/remote.git"
git init -q --bare "$remote"
git remote add origin "$remote"
expect_ok bash scripts/install-hooks.sh
# First push to an empty bare repo, then normal update.
expect_ok git push -q origin main
base=$(git rev-parse HEAD)
printf 'good\n' > good.txt
git add good.txt
git commit -qm good
expect_ok git push -q origin main
# The current index is unrelated to the historical commit being checked.
printf 'private\n' > docs/private.md
git add docs/private.md
commit_unchecked 'private intermediate tree'
private_commit=$(git rev-parse HEAD)
git rm -q docs/private.md
commit_unchecked 'delete private doc'
expect_fail git push -q origin main
assert_message '[docs]'
expect_fail bash scripts/guardrails.sh "$base..HEAD"
assert_message '[docs]'
# Local replacement refs must not mask the original objects Git publishes.
private_parent=$(git rev-parse "$private_commit^")
replacement=$(printf 'local clean replacement\n' | git commit-tree "$private_parent^{tree}" -p "$private_parent")
git replace "$private_commit" "$replacement"
expect_fail git push -q origin main
assert_message '[docs]'
expect_fail bash scripts/guardrails.sh "$base..HEAD"
assert_message '[docs]'
git replace -d "$private_commit" > /dev/null
# New branches must inspect their intermediate trees too.
expect_fail git push -q origin HEAD:refs/heads/private-branch
assert_message '[docs]'
# Multiple updates must all be checked.
git branch good-branch "$base"
expect_fail git push -q origin good-branch HEAD:refs/heads/second-private
assert_message '[docs]'
# A clean outgoing history succeeds even with a bad staged index.
git reset -q --hard origin/main
printf 'private staged\n' > docs/private.md
git add docs/private.md
expect_ok git push -q origin HEAD:refs/heads/new-clean-branch
git reset -q -- docs/private.md
rm docs/private.md
# Deletions send no new trees, including a branch with published violations.
expect_ok git push -q origin :refs/heads/new-clean-branch
zero=$(git hash-object --stdin < /dev/null)
printf -v zero '%*s' "${#zero}" ''
zero=${zero// /0}
expect_fail bash -c 'printf "malformed" | bash .githooks/pre-push origin "$1"' _ "$remote"
expect_fail bash -c 'printf "malformed\n" | bash .githooks/pre-push origin "$1"' _ "$remote"
expect_fail bash -c 'printf "HEAD invalid refs/heads/main invalid\n" | bash .githooks/pre-push origin "$1"' _ "$remote"
expect_fail bash -c 'printf "HEAD %s refs/heads/main %s extra\n" "$(git rev-parse HEAD)" "$2" | bash .githooks/pre-push origin "$1"' _ "$remote" "$zero"
expect_fail bash -c 'printf "HEAD %s invalid %s\n" "$(git rev-parse HEAD)" "$2" | bash .githooks/pre-push origin "$1"' _ "$remote" "$zero"
expect_fail bash -c 'printf "missing %s refs/heads/main %s\n" "$(git rev-parse HEAD)" "$2" | bash .githooks/pre-push origin "$1"' _ "$remote" "$zero"
expect_fail bash -c 'printf "HEAD %s refs/heads/main %s\n" "$(git rev-parse HEAD)" "$2" | bash .githooks/pre-push origin "$1"' _ "$sandbox/nonexistent.git" "$zero"

# Published bad history is excluded using advertised refs on every branch.
# Stale tracking refs must neither hide new commits nor make old ones fail.
printf 'published old doc\n' > docs/old-private.md
git add docs/old-private.md
commit_unchecked 'already published violation'
git -c core.hooksPath=/dev/null push -q origin HEAD:refs/heads/legacy
legacy=$(git rev-parse HEAD)
git rm -q docs/old-private.md
commit_unchecked 'remove published doc'
git update-ref refs/remotes/origin/legacy "$base"
expect_ok git push -q origin HEAD:refs/heads/from-legacy
# A remote-tracking ref pointing at an unpublished bad tree cannot exempt it.
printf 'unpublished doc\n' > docs/private.md
git add docs/private.md
commit_unchecked 'unpublished violation'
git update-ref refs/remotes/origin/fake HEAD
expect_fail git push -q origin HEAD:refs/heads/stale-tracking
assert_message '[docs]'
git reset -q --hard "$legacy"
expect_ok git push -q origin :refs/heads/legacy

# Valid force updates and annotated commit tags work; deleting a blob tag
# sends no new trees and does not require a commit-shaped remote object.
expect_ok git push -q --force origin "$base":refs/heads/from-legacy
git tag -a clean-tag "$base" -m clean
expect_ok git push -q origin clean-tag
expect_ok git push -q origin :refs/tags/clean-tag
blob=$(printf 'old blob\n' | git hash-object -w --stdin)
git tag old-blob "$blob"
git -c core.hooksPath=/dev/null push -q origin old-blob
expect_ok git push -q origin :refs/tags/old-blob
# A bogus remote base or mismatched source must fail closed.
expect_fail bash -c 'printf "HEAD %s refs/heads/from-legacy %s\n" "$(git rev-parse HEAD)" "$(git rev-parse HEAD)" | bash .githooks/pre-push origin "$1"' _ "$remote"
expect_fail bash -c 'printf "HEAD %s refs/heads/new-branch %s\n" "$2" "$3" | bash .githooks/pre-push origin "$1"' _ "$remote" "$base" "$zero"

new_repo first-private
first_remote="$sandbox/first-private.git"
git init -q --bare "$first_remote"
git remote add origin "$first_remote"
expect_ok bash scripts/install-hooks.sh
printf 'private\n' > docs/private.md
git add docs/private.md
commit_unchecked 'first private doc'
git rm -q docs/private.md
commit_unchecked 'remove before first push'
expect_fail git push -q origin main
assert_message '[docs]'

# Identity policy: contributor, maintainer, bot, and GitHub merge exemptions.
new_repo identity
identity_base=$(git rev-parse HEAD)
printf 'contributor\n' > good.txt
git add good.txt
git commit -qm contributor
expect_ok bash scripts/guardrails.sh "$identity_base..HEAD"
git config user.name Pallav
git config user.email wrong@example.com
printf 'wrong identity\n' >> good.txt
git add good.txt
commit_unchecked 'wrong maintainer identity'
expect_fail bash scripts/guardrails.sh "$identity_base..HEAD"
assert_message '[identity]'
git -c core.hooksPath=/dev/null -c user.email=15070765+debug-diary-1@users.noreply.github.com commit --amend --reset-author -qm 'correct identity'
expect_ok bash scripts/guardrails.sh "$identity_base..HEAD"
git config user.name Tester
git config user.email tester@example.com
git -c core.hooksPath=/dev/null commit --allow-empty -qm $'coauthor\n\nCo-authored-by: Claude <bot@example.com>'
expect_fail bash scripts/guardrails.sh "$identity_base..HEAD"
assert_message '[identity]'
git -c core.hooksPath=/dev/null commit --amend --allow-empty --author='automation[bot] <bot@example.com>' --no-edit -q
expect_ok bash scripts/guardrails.sh "$identity_base..HEAD"
# Bot exemption applies only to identity, never to tree checks.
printf 'private\n' > docs/private.md
git add docs/private.md
git -c core.hooksPath=/dev/null commit --author='automation[bot] <bot@example.com>' -qm 'bot private doc'
expect_fail bash scripts/guardrails.sh "$identity_base..HEAD"
assert_message '[docs]'
git reset -q --hard HEAD~1
parent=$(git rev-parse HEAD)
other=$(printf 'other\n' | git commit-tree "HEAD^{tree}" -p "$identity_base")
merge=$(printf 'merge\n\nCo-authored-by: Claude <bot@example.com>\n' | GIT_AUTHOR_NAME=Pallav GIT_AUTHOR_EMAIL=wrong@example.com GIT_COMMITTER_EMAIL=noreply@github.com git commit-tree "HEAD^{tree}" -p "$parent" -p "$other")
expect_ok bash scripts/guardrails.sh "$identity_base..$merge"
printf 'guardrails regression tests: %s checks passed\n' "$passed"
