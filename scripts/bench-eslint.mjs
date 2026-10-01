#!/usr/bin/env node
// Measures what the detangle ESLint rules add to linting, case by case,
// against the targets in the design's success criteria.
//
//   DETANGLE_ADDON=target/release/libdetangle_napi.dylib DETANGLE_BIN=target/release/detangle \
//     node scripts/bench-eslint.mjs <dir>
//
// <dir> is what `detangle check <dir>` would scan (e.g. a VS Code clone's
// src/), inside a git checkout: the benchmark edits files there (a saved
// import change, a new file, the lockfile, a detangle.toml) and puts them
// back with git when it's done. Build the add-on and the CLI in release
// mode, and install npm/'s dev dependencies (ESLint, typescript-eslint)
// first. With an add-on built with `--features test-hooks`, it also reports
// when the file watcher was ready.
//
// "Added" is the time for ESLint's Linter to lint a file with both rules
// (configs.recommended) minus the time without them, on one thread: the
// parse ESLint does anyway is not counted.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const npm = path.join(here, "..", "npm");
const require = createRequire(path.join(npm, "package.json"));
const { Linter } = require("eslint");
const tseslint = require("typescript-eslint");
const detangle = require("./eslint.js");
const native = require("./native.js");

const dir = path.resolve(process.argv[2] ?? "");
if (!process.argv[2] || !fs.statSync(dir, { throwIfNoEntry: false })?.isDirectory()) {
  console.error("usage: bench-eslint.mjs <dir>");
  process.exit(2);
}
for (const v of ["DETANGLE_ADDON", "DETANGLE_BIN"]) {
  if (!process.env[v]) {
    console.error(`set ${v} (see the header of this script)`);
    process.exit(2);
  }
  process.env[v] = path.resolve(process.env[v]);
}
const git = (...args) => spawnSync("git", ["-C", dir, ...args], { encoding: "utf8" });
const gitRoot = (...args) => spawnSync("git", ["-C", repo, ...args], { encoding: "utf8" });
const repo = git("rev-parse", "--show-toplevel").stdout.trim();
if (!repo) {
  console.error(`${dir} isn't in a git checkout; the benchmark needs one to put its edits back`);
  process.exit(2);
}
// The project root, as detangle finds it: the nearest detangle.toml, else package.json.
const root = (() => {
  for (let d = dir; ; d = path.dirname(d)) {
    if (fs.existsSync(path.join(d, "detangle.toml")) || fs.existsSync(path.join(d, "package.json"))) return d;
    if (path.dirname(d) === d) return dir;
  }
})();

// ---------------------------------------------------------------- setup

const files = [];
(function walk(d) {
  for (const e of fs.readdirSync(d, { withFileTypes: true })) {
    if (e.name === "node_modules" || e.name.startsWith(".")) continue;
    const p = path.join(d, e.name);
    if (e.isDirectory()) walk(p);
    else if (/\.(ts|tsx)$/.test(e.name) && !e.name.endsWith(".d.ts")) files.push({ path: p, size: fs.statSync(p).size });
  }
})(dir);
files.sort((a, b) => a.size - b.size);
const median = files[files.length >> 1];
const largest = files.at(-1);
// A file with a relative import, for the edit cases.
const withImport = (f) => /^import .* from ['"]\.{1,2}\//m.test(fs.readFileSync(f.path, "utf8"));
const editable = files.slice(files.length >> 1).find(withImport);
const other = files.slice(files.length >> 2).find((f) => f !== editable && f !== median && withImport(f));

const parse = { files: ["**/*.ts", "**/*.tsx"], languageOptions: { parser: tseslint.parser } };
const rules = [parse, detangle.configs.recommended];
const bare = [parse];
const linter = new Linter({ cwd: dir });
const verify = (config, file, text) => {
  const t = performance.now();
  const messages = linter.verify(text, config, { filename: file });
  const ms = performance.now() - t;
  const fatal = messages.find((m) => m.fatal);
  if (fatal) throw new Error(`${file}: ${fatal.message}`);
  return { ms, messages };
};
const read = (f) => fs.readFileSync(f, "utf8");
const med = (xs) => [...xs].sort((a, b) => a - b)[xs.length >> 1];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const rel = (p) => path.relative(dir, p);

/** Median added time over `n` alternating runs of `text` (or `text(i)`). */
function added(file, text, n = 21) {
  const t = typeof text === "function" ? text : () => text;
  const w = [], b = [];
  for (let i = 0; i < n; i++) {
    b.push(verify(bare, file, t(i)).ms);
    w.push(verify(rules, file, t(i)).ms);
  }
  return med(w) - med(b);
}

/** Added time of one lint with the rules, against the median without. */
function once(file, text) {
  const b = med(Array.from({ length: 7 }, () => verify(bare, file, text).ms));
  return verify(rules, file, text).ms - b;
}

const results = [];
const row = (name, ms, target, note = "") => {
  results.push({ name, ms, target, note });
  console.error(`  ${name}: ${ms.toFixed(2)} ms`);
};

// Everything the benchmark creates or edits, put back at the end.
const created = [];
const edited = new Set();
function restore() {
  for (const p of created) fs.rmSync(p, { force: true });
  if (edited.size) gitRoot("checkout", "--", ...[...edited].map((p) => path.relative(repo, p)));
}
process.on("exit", restore);
process.on("SIGINT", () => process.exit(130));

console.error(`corpus: ${dir} (root ${root}), ${files.length} TS files`);
console.error(`median file ${rel(median.path)} (${(median.size / 1024).toFixed(1)} KB), largest ${rel(largest.path)} (${(largest.size / 1024).toFixed(0)} KB)`);

// ---------------------------------------------------------------- cases

// Warm ESLint and the parser first, so the cold case is detangle's cold start.
verify(bare, median.path, read(median.path));
const rss0 = process.memoryUsage().rss;
// This process's own first lint opens the project for the cases below.
verify(rules, median.path, read(median.path));
const rssProject = process.memoryUsage().rss - rss0;

// The cold case, in fresh processes (one sample each is noisy): ESLint and
// the parser already warm, then the first lint with the rules.
{
  const script = `
    const { Linter } = require("eslint");
    const tseslint = require("typescript-eslint");
    const detangle = require("./eslint.js");
    const fs = require("fs");
    const file = ${JSON.stringify(median.path)}, text = fs.readFileSync(file, "utf8");
    const linter = new Linter({ cwd: ${JSON.stringify(dir)} });
    const parse = { files: ["**/*.ts", "**/*.tsx"], languageOptions: { parser: tseslint.parser } };
    const time = (config) => { const t = performance.now(); linter.verify(text, config, { filename: file }); return performance.now() - t; };
    const bare = [0, 1, 2, 3, 4].map(() => time([parse])).sort((a, b) => a - b)[2];
    process.stdout.write(String(time([parse, detangle.configs.recommended]) - bare));
  `;
  const xs = [0, 1, 2, 3, 4].map(() => Number(spawnSync(process.execPath, ["-e", script], { cwd: npm, encoding: "utf8", env: process.env }).stdout));
  row("first lint in a process (cold scan)", med(xs), 250, "median of 5 processes");
}

// The watcher starts on its own thread at open.
const handle = () => [...native.projects.values()][0].handle;
if (handle().__stats) {
  const t = performance.now();
  while (!handle().__stats().watching && performance.now() - t < 30_000) {
    verify(rules, median.path, read(median.path));
    await sleep(1);
  }
  console.error(`  watcher ready ${(performance.now() - t).toFixed(0)} ms after the first lint returned`);
} else {
  await sleep(1000);
}

row("lint, file unchanged, median-size file", added(median.path, read(median.path)), 0.5, `${(median.size / 1024).toFixed(1)} KB`);
row("lint, file unchanged, largest file", added(largest.path, read(largest.path), 11), 5, `${(largest.size / 1024).toFixed(0)} KB`);

{
  // Typed, not saved: the import list changes on every lint.
  const text = read(editable.path);
  const line = text.match(/^import .* from ['"]\.{1,2}\/.*$/m)[0];
  const toggled = (i) => (i % 2 ? text : text.replace(line, `${line}\n${line.replace(/^import .* from/, "import * as __bench from")}`));
  const w = [], b = [];
  for (let i = 0; i < 21; i++) {
    b.push(verify(bare, editable.path, toggled(i)).ms);
    w.push(verify(rules, editable.path, toggled(i)).ms);
  }
  row("lint after an unsaved import edit", med(w) - med(b), 30, rel(editable.path));
}

{
  // Saved in another file; arrives through the watcher.
  const original = read(other.path);
  const line = original.match(/^import .* from ['"]\.{1,2}\/.*$/m)[0];
  const changed = original.replace(line, `${line}\n${line.replace(/^import .* from/, "import * as __bench from")}`);
  edited.add(other.path);
  const xs = [];
  const text = read(median.path);
  for (let i = 0; i < 6; i++) {
    fs.writeFileSync(other.path, i % 2 ? original : changed);
    await sleep(300);
    xs.push(once(median.path, text));
  }
  row("lint after a saved import change in another file", med(xs), 65, rel(other.path));
}

{
  // A new file, linted before its watcher event can have been drained.
  const fresh = path.join(path.dirname(median.path), "__detangle_bench_new.ts");
  const text = `import * as x from "./${path.basename(median.path, ".ts")}";\nexport const y = x;\n`;
  const xs = [], removed = [];
  for (let i = 0; i < 3; i++) {
    fs.writeFileSync(fresh, text);
    created.push(fresh);
    xs.push(once(fresh, text));
    fs.rmSync(fresh);
    await sleep(300);
    removed.push(once(median.path, read(median.path)));
  }
  row("first lint of a new file", med(xs), 150);
  row("lint after a file is removed", med(removed), 150);
}

{
  // A file the walk wouldn't include: outside <dir>.
  const outside = path.join(root, `__detangle_bench_excluded_${process.pid}.ts`);
  if (path.dirname(outside) !== dir && !outside.startsWith(dir + path.sep)) {
    fs.writeFileSync(outside, "export const z = 1;\n");
    created.push(outside);
    row("first lint of an excluded file", once(outside, read(outside)), 1, "outside the scanned directory");
  }
}

{
  // An install: the lockfile changes (packages themselves untouched here).
  const lock = ["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock"].map((n) => path.join(root, n)).find((p) => fs.existsSync(p));
  if (lock) {
    edited.add(lock);
    const text = read(median.path);
    const xs = [];
    for (let i = 0; i < 3; i++) {
      fs.appendFileSync(lock, "\n");
      xs.push(once(median.path, text));
    }
    row("lint after a dependency install (lockfile changed)", med(xs), 150, path.basename(lock));
  }
}

{
  // A config change (TOML): a full re-open.
  const toml = path.join(root, "detangle.toml");
  const existed = fs.existsSync(toml);
  let base;
  if (existed) {
    edited.add(toml);
    base = read(toml);
  } else {
    // The built-in rules, which the project uses now, written out.
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "detangle-bench-"));
    spawnSync(process.env.DETANGLE_BIN, ["init", tmp]);
    base = read(path.join(tmp, "detangle.toml"));
    fs.rmSync(tmp, { recursive: true });
    created.push(toml);
  }
  const text = read(median.path);
  const xs = [];
  for (let i = 0; i < 3; i++) {
    fs.writeFileSync(toml, `# detangle-bench ${i}\n${base}`);
    xs.push(once(median.path, text));
  }
  row("lint after a config change (TOML)", med(xs), 250);
}

// Memory: what opening the project added to this process, against the
// CLI's peak RSS on the same directory.
{
  const timeArgs = process.platform === "darwin" ? ["-l"] : ["-v"];
  const r = spawnSync("/usr/bin/time", [...timeArgs, process.env.DETANGLE_BIN, "check", dir], { encoding: "utf8" });
  const m = process.platform === "darwin" ? r.stderr.match(/(\d+)\s+maximum resident set size/) : r.stderr.match(/Maximum resident set size \(kbytes\): (\d+)/);
  const cli = m ? Number(m[1]) / (process.platform === "darwin" ? 1 : 1 / 1024) : NaN;
  results.push({ name: "memory per project (RSS added at open)", mb: rssProject / 2 ** 20, cliMb: cli / 2 ** 20, target: 1.2 });
}
restore();

// Exit time: a process that lints once (cold) and exits, with and without
// the rules; the gap between its last lint and its exit.
function exitGap(withRules) {
  const script = `
    const { Linter } = require("eslint");
    const tseslint = require("typescript-eslint");
    const detangle = require("./eslint.js");
    const linter = new Linter({ cwd: ${JSON.stringify(dir)} });
    const parse = { files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } };
    const config = ${withRules} ? [parse, detangle.configs.recommended] : [parse];
    linter.verify(require("fs").readFileSync(${JSON.stringify(median.path)}, "utf8"), config, { filename: ${JSON.stringify(median.path)} });
    process.stdout.write(String(performance.timeOrigin + performance.now()));
  `;
  const r = spawnSync(process.execPath, ["-e", script], { cwd: npm, encoding: "utf8", env: process.env });
  return Date.now() - Number(r.stdout);
}
{
  const w = med([0, 1, 2, 3, 4].map(() => exitGap(true)));
  const b = med([0, 1, 2, 3, 4].map(() => exitGap(false)));
  results.push({ name: "process exit after the last lint (added)", ms: w - b, target: 50, note: `${w.toFixed(0)} vs ${b.toFixed(0)} ms` });
}

// ---------------------------------------------------------------- report

console.log(`\n${os.type()} ${os.arch()}, Node ${process.version}, ${os.cpus().length} cores; ${rel(dir) || dir}: ${files.length} TS files\n`);
console.log("| Case | Measured | Target | |");
console.log("| ---- | -------- | ------ | - |");
for (const r of results) {
  if (r.mb !== undefined) {
    const ratio = r.mb / r.cliMb;
    console.log(`| ${r.name} | ${r.mb.toFixed(0)} MB (${ratio.toFixed(2)} × the CLI's ${r.cliMb.toFixed(0)} MB) | ≤ ${r.target} × | ${ratio <= r.target ? "ok" : "**miss**"} |`);
  } else {
    console.log(`| ${r.name}${r.note ? ` (${r.note})` : ""} | ${Math.max(0, r.ms).toFixed(2)} ms | ≤ ${r.target} ms | ${r.ms <= r.target ? "ok" : "**miss**"} |`);
  }
}
