"use strict";
// Loads the detangle add-on (detangle.node) and keeps one open project per
// (dir, config, mode), so each lint is answered from a scan kept in memory.
//
// The add-on is $DETANGLE_ADDON (e.g. target/debug/libdetangle_napi.dylib
// in a checkout), else detangle.node from this machine's platform package.

const path = require("node:path");
const { platform } = require("./binary.js");

let addon;
let unavailable;

function load() {
  if (addon || unavailable) return addon;
  try {
    let file = process.env.DETANGLE_ADDON;
    if (!file) {
      const p = platform();
      if (!p) throw new Error(`no detangle package for ${process.platform}-${process.arch}`);
      file = require.resolve(path.posix.join(p.package, "detangle.node"));
    }
    // dlopen rather than require: a freshly built add-on isn't named *.node.
    const m = { exports: {} };
    process.dlopen(m, file);
    addon = m.exports;
  } catch (e) {
    unavailable = `detangle add-on unavailable: ${e.message}; run \`detangle check\``;
  }
  return addon;
}

/** Open projects by `dir \0 config \0 mode`. */
const projects = new Map();

/**
 * What to show in the file an ESLint rule is linting: `{ violations,
 * problems, exoticRequire }`. `options` are the rule's `{ dir, config, mode }`;
 * `dir` and `config` are relative to ESLint's working directory.
 */
function violationsFor(context, options = {}) {
  if (!load()) return { violations: [], problems: [unavailable], exoticRequire: [] };
  const dir = path.resolve(context.cwd, options.dir ?? ".");
  const config = options.config === undefined ? undefined : path.resolve(context.cwd, options.config);
  const key = [dir, config ?? "", options.mode ?? ""].join("\0");
  let project = projects.get(key);
  if (!project) {
    project = addon.open(dir, { config, mode: options.mode });
    projects.set(key, project);
  }
  return project.violationsFor(context.filename, context.sourceCode.text);
}

module.exports = { violationsFor };
