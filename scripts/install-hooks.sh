#!/usr/bin/env bash
# Opt in with: bash scripts/install-hooks.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
root=$(pwd -P)
configured=$(git config --get core.hooksPath || true)
if [ -n "$configured" ] && [ "$configured" != .githooks ] && [ "$configured" != "$root/.githooks" ]; then
  echo "Refusing to replace existing core.hooksPath: $configured" >&2
  exit 1
fi
# Even when re-running installation, do not hide unrelated active default hooks.
hooks=$(git rev-parse --git-common-dir)/hooks
if [ -d "$hooks" ]; then
  for hook in "$hooks"/*; do
    case "$hook" in *.sample) continue ;; esac
    if [ -f "$hook" ] && [ -x "$hook" ]; then
      echo "Refusing to hide active hook: $hook" >&2
      exit 1
    fi
  done
fi
for hook in .githooks/pre-commit .githooks/pre-push; do
  [ -f "$hook" ] || { echo "Missing repository hook: $hook" >&2; exit 1; }
done
chmod +x .githooks/pre-commit .githooks/pre-push
git config --local core.hooksPath .githooks
echo 'guardrails hooks installed (.githooks)'
