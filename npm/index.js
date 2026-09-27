"use strict";
// Node.js API for detangle. Runs the `detangle` binary (from `options.bin`,
// $DETANGLE_BIN, or PATH) and returns its results as JavaScript values.

const { execFile } = require("node:child_process");

function run(args, options = {}) {
  const bin = options.bin || process.env.DETANGLE_BIN || "detangle";
  return new Promise((resolve, reject) => {
    execFile(bin, args, { maxBuffer: 1 << 30, cwd: options.cwd, env: { ...process.env, NO_COLOR: "1" } }, (error, stdout, stderr) => {
      if (error && typeof error.code !== "number") {
        reject(new Error(`couldn't run ${bin}: ${error.message}`));
        return;
      }
      resolve({ stdout, stderr, exitCode: error ? error.code : 0 });
    });
  });
}

/** Arguments every command shares. */
function common(dir, options) {
  const args = [dir || "."];
  if (options.config) args.push("--config", options.config);
  if (options.mode) args.push("--mode", options.mode);
  if (options.cache === true) args.push("--cache");
  else if (typeof options.cache === "string") args.push(`--cache=${options.cache}`);
  if (options.cacheStrategy) args.push("--cache-strategy", options.cacheStrategy);
  return args;
}

/** Graph view options → `detangle graph` flags. */
function viewArgs(options) {
  const args = [];
  const flag = (name, value) => {
    if (value !== undefined && value !== null && value !== false) args.push(`--${name}`, String(value));
  };
  if (options.collapse !== undefined) args.push("--collapse", options.collapse instanceof RegExp ? options.collapse.source : String(options.collapse));
  for (const [key, name] of [["focus", "focus"], ["reaches", "reaches"], ["highlight", "highlight"], ["from", "from"]]) {
    const v = options[key];
    flag(name, v instanceof RegExp ? v.source : v);
  }
  flag("focus-depth", options.focusDepth);
  flag("max-depth", options.maxDepth);
  if (options.externals) args.push("--externals");
  if (options.types === false) args.push("--no-types");
  return args;
}

function parse(result, what) {
  try {
    return JSON.parse(result.stdout);
  } catch {
    throw new Error(`detangle ${what} failed (exit ${result.exitCode}): ${result.stderr.trim()}`);
  }
}

/**
 * Analyses a project: every module with its dependencies and metrics, the
 * cycles, and the rule violations.
 */
async function analyze(dir, options = {}) {
  const r = await run(["graph", ...common(dir, options), "-f", "json", "--externals", ...viewArgs({ ...options, externals: false })], options);
  return parse(r, "analyze");
}

/**
 * Checks the rules. Resolves to the violations and counts; `exitCode` is what
 * the CLI would exit with (1 when there are errors, or warnings with `strict`).
 */
async function check(dir, options = {}) {
  const args = ["check", ...common(dir, options), "-f", "json"];
  if (options.strict) args.push("--strict");
  if (options.baseline) args.push("--baseline", options.baseline);
  const r = await run(args, options);
  const violations = parse(r, "check");
  const count = (s) => violations.filter((v) => v.severity === s).length;
  return { violations, errors: count("error"), warnings: count("warn"), info: count("info"), exitCode: r.exitCode };
}

/**
 * Rule results as text in one of the CLI's formats: "text", "markdown",
 * "github", "teamcity" or "azure".
 */
async function report(dir, options = {}) {
  const args = ["check", ...common(dir, options), "-f", options.format || "text"];
  if (options.strict) args.push("--strict");
  if (options.baseline) args.push("--baseline", options.baseline);
  const r = await run(args, options);
  if (r.exitCode > 1) throw new Error(`detangle check failed: ${r.stderr.trim()}`);
  return { output: r.stdout, exitCode: r.exitCode };
}

/**
 * The dependency graph as "dot", "mermaid", "d2", "csv" (strings) or "json"
 * (an object), with the same filters as `detangle graph`.
 */
async function graph(dir, options = {}) {
  const format = options.format || "dot";
  const r = await run(["graph", ...common(dir, options), "-f", format, ...viewArgs(options)], options);
  if (format === "json") return parse(r, "graph");
  if (r.exitCode !== 0) throw new Error(`detangle graph failed: ${r.stderr.trim()}`);
  return r.stdout;
}

/** The detangle.toml `detangle migrate` would write, without writing it. */
async function migrate(dir, options = {}) {
  const r = await run(["migrate", dir || ".", "--dry-run"], options);
  if (r.exitCode !== 0) throw new Error(`detangle migrate failed: ${r.stderr.trim()}`);
  return { config: r.stdout, summary: r.stderr };
}

module.exports = { analyze, check, report, graph, migrate };
// `import detangle from "detangle-deps"` and `require("detangle-deps").default` both work.
module.exports.default = module.exports;
