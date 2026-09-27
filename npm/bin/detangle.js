#!/usr/bin/env node
"use strict";
// Runs the native detangle binary for this platform.

const { spawnSync } = require("node:child_process");
const { binaryPath, platform } = require("../binary.js");

const bin = binaryPath();
if (!bin) {
  const p = platform();
  console.error(
    p
      ? `detangle: the ${p.package} package is missing. Reinstall without --no-optional / --omit=optional.`
      : `detangle: no prebuilt binary for ${process.platform}-${process.arch}. Install with \`cargo install --locked detangle\` and set DETANGLE_BIN, or use a supported platform.`,
  );
  process.exit(2);
}
const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (r.error) {
  console.error(`detangle: couldn't run ${bin}: ${r.error.message}`);
  process.exit(2);
}
if (r.signal) process.kill(process.pid, r.signal);
process.exit(r.status ?? 1);
