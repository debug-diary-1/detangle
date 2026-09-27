#!/bin/bash
# Prepares the project the explorer demo runs on: excalidraw at a fixed tag,
# with its dependencies installed (so npm imports resolve as in a real
# checkout). Then record with: vhs docs/demo/explorer.tape
set -euo pipefail
dir=/tmp/detangle-demo
if [ ! -d "$dir" ]; then
  git clone --quiet --depth 1 --branch v0.18.1 https://github.com/excalidraw/excalidraw "$dir"
fi
cd "$dir"
# Only files are read, never run: skip install scripts and excalidraw's Node engine range.
npx -y yarn@1.22.22 install --frozen-lockfile --ignore-scripts --ignore-engines --non-interactive --silent
