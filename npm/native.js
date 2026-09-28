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
 * The current pass over each file: its results by key, and the problems
 * shown so far. ESLint makes one
 * SourceCode per pass and gives it to every rule, so both rules share one
 * add-on call per pass; a new pass (an edit, a --fix round) is a new
 * SourceCode and a fresh call.
 */
const passes = new WeakMap();

/** Add-on calls made, for tests. */
const stats = { calls: 0 };

/**
 * What to show in the file an ESLint rule is linting: `{ violations,
 * problems, exoticRequire }`, plus `shown`, the problems already reported
 * in this pass (by any detangle rule, for any project), so the same
 * problem isn't repeated. `options`
 * are the rule's `{ dir, config, mode }`; `dir` and `config` are relative
 * to ESLint's working directory.
 */
function violationsFor(context, options = {}) {
  const dir = path.resolve(context.cwd, options.dir ?? ".");
  const config = options.config === undefined ? undefined : path.resolve(context.cwd, options.config);
  const key = [dir, config ?? "", options.mode ?? ""].join("\0");
  let pass = passes.get(context.sourceCode);
  if (!pass) passes.set(context.sourceCode, (pass = { results: new Map(), shown: new Set() }));
  let result = pass.results.get(key);
  if (!result) {
    result = { ...call(key, dir, config, options.mode, context), shown: pass.shown };
    pass.results.set(key, result);
  }
  return result;
}

function call(key, dir, config, mode, context) {
  if (!load()) return { violations: [], problems: [unavailable], exoticRequire: [] };
  let project = projects.get(key);
  if (!project) {
    project = addon.open(dir, { config, mode });
    projects.set(key, project);
  }
  stats.calls++;
  return project.violationsFor(context.filename, context.sourceCode.text);
}

module.exports = { violationsFor, stats };
