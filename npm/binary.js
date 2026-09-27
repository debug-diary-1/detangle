"use strict";
// Finds the detangle binary: $DETANGLE_BIN, else the platform package npm
// installed alongside this one (see platforms.json), else null.

const path = require("node:path");
const platforms = require("./platforms.json");

function libc() {
  if (process.platform !== "linux") return undefined;
  // glibc reports its version; musl (e.g. Alpine) doesn't.
  const header = process.report && typeof process.report.getReport === "function" ? process.report.getReport().header : {};
  return header && header.glibcVersionRuntime ? "glibc" : "musl";
}

/** The platform package for this machine, if detangle ships one. */
function platform() {
  const c = libc();
  return platforms.find((p) => p.os === process.platform && p.cpu === process.arch && (p.libc === undefined || p.libc === c));
}

function binaryPath() {
  if (process.env.DETANGLE_BIN) return process.env.DETANGLE_BIN;
  const p = platform();
  if (!p) return null;
  const exe = process.platform === "win32" ? "detangle.exe" : "detangle";
  try {
    return require.resolve(path.posix.join(p.package, "bin", exe));
  } catch {
    return null;
  }
}

module.exports = { binaryPath, platform };
