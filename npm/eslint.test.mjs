// Tests for the ESLint plugin. Run from a checkout, with the add-on and the
// binary built (cargo build -p detangle-napi && cargo build), and ESLint
// installed (npm install in npm/):
//
//   DETANGLE_ADDON=target/debug/libdetangle_napi.dylib DETANGLE_BIN=target/debug/detangle \
//     node --test npm/eslint.test.mjs
//
// (libdetangle_napi.so on Linux, detangle_napi.dll on Windows.)
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { ESLint as ESLint10 } from "eslint";
import { ESLint as ESLint9 } from "eslint9";
import tseslint from "typescript-eslint";
import detangle from "./eslint.js";

// Child processes below run from npm/: make a relative add-on path absolute.
if (process.env.DETANGLE_ADDON) process.env.DETANGLE_ADDON = path.resolve(process.env.DETANGLE_ADDON);
const native = createRequire(import.meta.url)("./native.js");
const fixtures = fileURLToPath(new URL("../tests/fixtures/", import.meta.url));
const conditions = path.join(fixtures, "conditions");
const versions = { "ESLint 10": ESLint10, "ESLint 9": ESLint9 };

/** Both rules at their `recommended` severities, with these options. */
const recommended = (options) => [
  detangle.configs.recommended,
  { rules: { "detangle/errors": ["error", options], "detangle/warnings": ["warn", options] } },
];

/**
 * Lints `files` in `cwd` with the given config entries (after one that
 * parses TypeScript). Returns every message, with the file relative to `cwd`.
 */
async function lint(cwd, files, configs, ESLint = ESLint10) {
  const eslint = new ESLint({
    cwd,
    overrideConfigFile: true,
    overrideConfig: [{ files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } }, ...configs],
  });
  const results = await eslint.lintFiles(files);
  return results.flatMap((r) =>
    r.messages.map((m) => {
      assert.equal(m.fatal, undefined, `${r.filePath}: ${m.message}`);
      const file = path.relative(cwd, r.filePath).replaceAll("\\", "/");
      return { file, line: m.line, column: m.column, rule: m.ruleId, severity: m.severity, message: m.message };
    }),
  );
}

const key = (m) => `${m.file}:${m.line}:${m.column} ${m.rule} ${m.message}`;

/** Runs the CLI for JSON output; `check` exits 1 when it finds errors. */
function detangleJson(...args) {
  const r = spawnSync(process.env.DETANGLE_BIN, args, { encoding: "utf8" });
  assert.ok(r.status === 0 || r.status === 1, `detangle ${args.join(" ")}: ${r.stderr}`);
  return JSON.parse(r.stdout);
}

const dirOf = (id) => (id.includes("/") ? id.slice(0, id.lastIndexOf("/")) : "");
const inside = (id, folder) => id.startsWith(`${folder}/`);

/**
 * Where the two rules should report, worked out independently of the Rust
 * placement code from the CLI's own output: `check -f json` for the
 * violations, `graph -f json --externals` for each import's specifier, and
 * the file text for where that specifier is written. `error` violations
 * belong to detangle/errors, `warn` and `info` to detangle/warnings. This
 * is the design's placement table (D9) in JavaScript:
 * - module scope: in `from`, on its imports of `to`, or at line 1 without a `to`;
 * - folder scope `F → T`: in every file under `F`, on each import of a
 *   module outside `F` whose folder node is `T`;
 * - group scope: in each import's file, on that import;
 * - folder or group scope without a `to` or imports: nowhere.
 * An import string written nowhere in code (a comment directive) goes at
 * line 1 with `[import "…"]` added.
 */
function oracle(dir, config) {
  const flags = config ? ["-c", config] : [];
  const violations = detangleJson("check", "-f", "json", ...flags, dir);
  const graph = detangleJson("graph", "-f", "json", "--externals", ...flags, dir);
  const modules = new Map(graph.modules.map((m) => [m.id, m]));
  const folderNode = (id) => {
    const m = modules.get(id);
    if (m.kind === "local") return dirOf(id) || ".";
    return m.kind === "npm" ? `node_modules/${id}` : id;
  };
  const specifiersTo = (from, pred) => [...new Set(modules.get(from).dependencies.filter(pred).map((d) => d.specifier))];

  // [file, rule, message, specifiers]
  const placed = [];
  for (const v of violations) {
    const rule = v.severity === "error" ? "detangle/errors" : "detangle/warnings";
    let message = `${v.rule}: ${v.from}`;
    if (v.to != null) message += ` → ${v.to}`;
    if (v.cycle.length > 1) message += ` (cycle: ${v.cycle.join(" → ")})`;
    if (v.comment) message += ` — ${v.comment}`;
    if (v.scope === "module") {
      placed.push([v.from, rule, message, v.to == null ? [] : specifiersTo(v.from, (d) => d.module === v.to)]);
    } else if (v.scope === "folder" && v.to != null) {
      for (const m of modules.values()) {
        if (m.kind !== "local" || !inside(m.id, v.from)) continue;
        const outside = (d) => !(modules.get(d.module).kind === "local" && inside(d.module, v.from));
        const specs = specifiersTo(m.id, (d) => folderNode(d.module) === v.to && outside(d));
        if (specs.length) placed.push([m.id, rule, message, specs]);
      }
    } else if (v.scope === "group") {
      for (const [file, imports] of Map.groupBy(v.imports, (i) => i.from)) {
        placed.push([file, rule, message, [...new Set(imports.map((i) => i.specifier))]]);
      }
    }
  }

  const out = [];
  for (const [file, rule, message, specifiers] of placed) {
    if (specifiers.length === 0) out.push({ file, line: 1, column: 1, rule, message });
    const text = fs.readFileSync(path.join(dir, file), "utf8");
    for (const s of specifiers) {
      const at = occurrences(text, s);
      if (at.length === 0) out.push({ file, line: 1, column: 1, rule, message: `${message} [import "${s}"]` });
      for (const [line, column] of at) out.push({ file, line, column, rule, message });
    }
  }
  return out;
}

/**
 * Where an import string is written as a string literal in code: quoted or
 * in backticks, not on a comment line. An Angular resource written without
 * "./" counts for the "./…" import string detangle records.
 */
function occurrences(text, specifier) {
  const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const forms = [escape(specifier)];
  if (specifier.startsWith("./")) forms.push(`(?<=(?:templateUrl|styleUrls?)\\s*:[^\\n]*)${escape(specifier.slice(2))}`);
  const out = [];
  for (const form of forms) {
    for (const m of text.matchAll(new RegExp(`(["'\`])${form}\\1`, "g"))) {
      const before = text.slice(0, m.index).split("\n");
      if (/^\s*(\/\/|\/\*|\*)/.test(before.at(-1))) continue;
      out.push([before.length, before.at(-1).length + 1]);
    }
  }
  return out;
}

const drift = [
  ["conditions, its rules config", conditions, "rules.config.cjs", 19],
  ["conditions, the built-in rules", conditions, undefined, 7],
  ["eslint", path.join(fixtures, "eslint"), undefined, 20],
  ["eslint-monorepo", path.join(fixtures, "eslint-monorepo"), undefined, 6],
];
for (const [name, dir, config, least] of drift) {
  test(`both rules report what the CLI finds, where the placement table says: ${name}`, async () => {
    const got = await lint(dir, ["**/*.ts"], recommended(config ? { config } : {}));
    const want = oracle(dir, config && path.join(dir, config));
    assert.ok(want.length >= least, `the oracle should find at least ${least} reports (found ${want.length})`);
    assert.deepEqual(got.map(key).sort(), want.map(key).sort());
    for (const m of got) assert.equal(m.severity, m.rule === "detangle/errors" ? 2 : 1);
  });
}

test("configs.recommended registers the plugin and enables both rules, errors first", () => {
  const r = detangle.configs.recommended;
  assert.equal(r.name, "detangle/recommended");
  assert.equal(r.plugins.detangle, detangle);
  assert.deepEqual(Object.entries(r.rules), [
    ["detangle/errors", "error"],
    ["detangle/warnings", "warn"],
  ]);
  assert.equal(r.files, undefined);
});

test("the rules accept only dir, config and mode", async () => {
  await assert.rejects(
    lint(conditions, ["src/c1.ts"], [detangle.configs.recommended, { rules: { "detangle/errors": ["error", { confg: "x" }] } }]),
    /should NOT have additional properties/,
  );
});

for (const [name, ESLint] of Object.entries(versions)) {
  test(`${name} shares one SourceCode between the rules of a pass`, async () => {
    const seen = new Map();
    const probe = (id) => ({
      create(context) {
        if (!seen.has(context.filename)) seen.set(context.filename, []);
        seen.get(context.filename).push([id, context.sourceCode]);
        return {};
      },
    });
    const plugin = { rules: { a: probe("a"), b: probe("b") } };
    await lint(conditions, ["src/c1.ts", "src/c3.ts"], [{ plugins: { probe: plugin }, rules: { "probe/a": "error", "probe/b": "error" } }], ESLint);
    assert.equal(seen.size, 2);
    for (const [[a, sa], [b, sb]] of seen.values()) {
      assert.deepEqual([a, b], ["a", "b"]);
      assert.equal(sa, sb);
    }
  });

  test(`${name}: both rules share one add-on call per file`, async () => {
    const files = ["src/c1.ts", "src/c3.ts", "src/c4.ts"];
    const before = native.stats.calls;
    await lint(conditions, files, recommended({}), ESLint);
    assert.equal(native.stats.calls - before, files.length);

    // Different options are a different project, so a call each.
    const mixed = await lint(conditions, files, [
      detangle.configs.recommended,
      { rules: { "detangle/errors": ["error", { config: "rules.config.cjs" }], "detangle/warnings": "warn" } },
    ], ESLint);
    assert.equal(native.stats.calls - before, files.length * 3);
    assert.ok(mixed.some((m) => m.rule === "detangle/errors") && mixed.some((m) => m.rule === "detangle/warnings"));
  });

  test(`${name}: problems are reported once per file, by the first detangle rule`, async () => {
    const files = ["src/c1.ts", "src/c3.ts"];
    const broken = { config: "missing.toml" };
    const once = async (configs, rule, severity) => {
      const got = await lint(conditions, files, configs, ESLint);
      assert.deepEqual(
        got.map((m) => [m.file, `${m.line}:${m.column}`, m.rule, m.severity]),
        files.map((f) => [f, "1:1", rule, severity]),
      );
      for (const m of got) assert.match(m.message, /^detangle: reading .*missing\.toml/);
    };
    await once(recommended(broken), "detangle/errors", 2);
    await once([{ plugins: { detangle }, rules: { "detangle/warnings": ["warn", broken], "detangle/errors": ["error", broken] } }], "detangle/warnings", 1);
    await once([{ plugins: { detangle }, rules: { "detangle/warnings": ["warn", broken] } }], "detangle/warnings", 1);
  });
}

test("an add-on that can't load gives one message at line 1 of each file", () => {
  // In a child process: the add-on is loaded once per process. The second
  // config gives the rules different options, so two projects, which must
  // still show the same problem only once.
  const script = `
    import { ESLint } from "eslint";
    import tseslint from "typescript-eslint";
    import detangle from "./eslint.js";
    const configs = [
      [detangle.configs.recommended],
      [detangle.configs.recommended, { rules: { "detangle/warnings": ["warn", { mode: "other" }] } }],
    ];
    const out = [];
    for (const extra of configs) {
      const eslint = new ESLint({
        cwd: ${JSON.stringify(conditions)},
        overrideConfigFile: true,
        overrideConfig: [{ files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } }, ...extra],
      });
      const results = await eslint.lintFiles(["src/c1.ts", "src/c3.ts"]);
      out.push(results.map((r) => r.messages.map((m) => [m.line, m.column, m.ruleId, m.severity, m.message])));
    }
    console.log(JSON.stringify(out));
  `;
  const r = spawnSync(process.execPath, ["--input-type=module", "-e", script], {
    cwd: path.dirname(fileURLToPath(import.meta.url)),
    env: { ...process.env, DETANGLE_ADDON: path.join(os.tmpdir(), "no-such-detangle.node") },
    encoding: "utf8",
  });
  assert.equal(r.status, 0, r.stderr);
  for (const files of JSON.parse(r.stdout)) {
    assert.equal(files.length, 2);
    for (const messages of files) {
      assert.equal(messages.length, 1, JSON.stringify(messages));
      const [line, column, rule, severity, message] = messages[0];
      assert.deepEqual([line, column, rule, severity], [1, 1, "detangle/errors", 2]);
      assert.match(message, /^detangle add-on unavailable: .*; run `detangle check`$/);
    }
  }
});

test("different problems from the two rules' projects are both shown", async () => {
  const got = await lint(conditions, ["src/c1.ts"], [
    detangle.configs.recommended,
    { rules: { "detangle/errors": ["error", { config: "missing-a.toml" }], "detangle/warnings": ["warn", { config: "missing-b.toml" }] } },
  ]);
  assert.deepEqual(
    got.map((m) => [m.rule, m.message.match(/missing-.\.toml/)?.[0]]),
    [["detangle/errors", "missing-a.toml"], ["detangle/warnings", "missing-b.toml"]],
  );
});

test("a project reached through a symlink reports the same", async () => {
  const link = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "detangle-link-")), "conditions");
  fs.symlinkSync(conditions, link, "junction"); // a junction needs no admin rights on Windows
  try {
    const configs = recommended({ config: "rules.config.cjs" });
    const direct = await lint(conditions, ["src/**/*.ts"], configs);
    const linked = await lint(link, ["src/**/*.ts"], configs);
    assert.ok(direct.length > 0);
    assert.deepEqual(linked.map(key).sort(), direct.map(key).sort());
  } finally {
    fs.rmSync(path.dirname(link), { recursive: true, force: true });
  }
});

// Node matching, one form at a time, by linting text as src/app.ts of the
// eslint fixture: the buffer is the file's contents, so each text below
// imports src/legacy/old.ts only through the form under test. The tests
// use a project of their own (a distinct `mode`), so their buffers don't
// affect other tests.
const eslintFixture = path.join(fixtures, "eslint");
const legacy = "no-legacy: src/app.ts → src/legacy/old.ts — Use the new API";

async function lintText(code, { file = "src/app.ts", mode = "node-matching", rules = { "detangle/errors": ["error", { mode }] } } = {}) {
  const eslint = new ESLint10({
    cwd: eslintFixture,
    overrideConfigFile: true,
    overrideConfig: [{ files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } }, { plugins: { detangle }, rules }],
  });
  const [r] = await eslint.lintText(code, { filePath: path.join(eslintFixture, file) });
  return r.messages.map((m) => {
    assert.equal(m.fatal, undefined, m.message);
    return `${m.line}:${m.column} ${m.message}`;
  });
}

const nodeCases = [
  ["import", `import x from "./legacy/old";`, ["1:15"]],
  ["export from", `export * from "./legacy/old";`, ["1:15"]],
  ["import()", `import("./legacy/old");`, ["1:8"]],
  ["require", `require("./legacy/old");`, ["1:9"]],
  ["require, template literal", "require(`./legacy/old`);", ["1:9"]],
  ["import-equals", `import x = require("./legacy/old");`, ["1:20"]],
  ["import type", `type T = import("./legacy/old").T;`, ["1:17"]],
  ["exotic require (dotted)", `module.require("./legacy/old");`, ["1:16"]],
  ["AMD define", `define(["require", "./legacy/old"], () => {});`, ["1:20"]],
  ["AMD define with an id", `define("app", ["./legacy/old"], () => {});`, ["1:16"]],
  ["AMD require", `require(["./legacy/old"], () => {});`, ["1:10"]],
  ["every node with the import string", `import "./legacy/old";\nimport x = require("./legacy/old");`, ["1:8", "2:20"]],
];
for (const [name, code, at] of nodeCases) {
  test(`node matching: ${name}`, async () => {
    assert.deepEqual(await lintText(code), at.map((l) => `${l} ${legacy}`));
  });
}

// Forms detangle doesn't record as imports: nothing to report.
const unmatched = [
  ["require(variable)", `const id = "./legacy/old";\nrequire(id);`],
  ["template literal with an expression", "const x = 'old';\nrequire(`./legacy/${x}`);"],
  ["an unknown callee", `load("./legacy/old");`],
  ["require with two arguments", `require("./legacy/old", 1);`],
  ["templateUrl outside a decorator", `const c = { templateUrl: "./legacy/old" };`],
];
for (const [name, code] of unmatched) {
  test(`node matching: never ${name}`, async () => {
    assert.deepEqual(await lintText(code), []);
  });
}

test("folder and group violations without an import are shown nowhere", async () => {
  const cli = detangleJson("check", "-f", "json", eslintFixture).map((v) => v.rule);
  assert.ok(cli.includes("orders-folder-unused") && cli.includes("orders-group-unused"));
  const got = await lint(eslintFixture, ["**/*.ts"], recommended({}));
  assert.ok(!got.some((m) => /orders-(folder|group)-unused/.test(m.message)));
});

test("an unsaved import edit is checked live, and undone live", async () => {
  const options = { mode: "live-editing" };
  const rules = { "detangle/errors": ["error", options], "detangle/warnings": ["warn", options] };
  const lonely = (code) => lintText(code, { file: "src/lonely.ts", rules });
  const disk = fs.readFileSync(path.join(eslintFixture, "src/lonely.ts"), "utf8");
  assert.deepEqual(await lonely(disk), ["1:1 no-orphans: src/lonely.ts"]);
  // Typed, not saved: a forbidden import, and the file is no longer an orphan.
  assert.deepEqual(await lonely(`import "./legacy/old";\n${disk}`), ["1:8 no-legacy: src/lonely.ts → src/legacy/old.ts — Use the new API"]);
  assert.deepEqual(await lonely(disk), ["1:1 no-orphans: src/lonely.ts"]);
});

test("a file created and linted before the watcher reports it is checked", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "detangle-new-file-"));
  try {
    fs.cpSync(eslintFixture, dir, { recursive: true });
    const first = await lint(dir, ["src/app.ts"], recommended({}));
    assert.ok(first.length > 0);
    // Straight after, before any watcher event can have been drained.
    fs.writeFileSync(path.join(dir, "src/fresh.ts"), 'import "./legacy/old";\n');
    const got = await lint(dir, ["src/fresh.ts"], recommended({}));
    assert.deepEqual(got.map((m) => `${m.line}:${m.column} ${m.message}`), ["1:8 no-legacy: src/fresh.ts → src/legacy/old.ts — Use the new API"]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// Recovery, with no file watcher: every change below must be seen by the
// polling at the start of each lint. These need an add-on built with the
// `test-hooks` feature (cargo build -p detangle-napi --features test-hooks).
const hooks = native.addon()?.__setWatcherEnabled ? native.addon() : undefined;
const needsHooks = { skip: hooks ? false : "needs an add-on built with --features test-hooks" };

/** A throwaway project with these files; returns its dir and a lint function. */
function scratch(files) {
  const dir = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), "detangle-recovery-")));
  for (const [rel, body] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
    fs.writeFileSync(path.join(dir, rel), body);
  }
  const lintFile = async (rel, text = fs.readFileSync(path.join(dir, rel), "utf8")) => {
    const eslint = new ESLint10({
      cwd: dir,
      overrideConfigFile: true,
      overrideConfig: [{ files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } }, detangle.configs.recommended],
    });
    const [r] = await eslint.lintText(text, { filePath: path.join(dir, rel) });
    return r.messages.map((m) => `${m.line}:${m.column} ${m.message}`);
  };
  return { dir, lintFile, write: (rel, body) => fs.writeFileSync(path.join(dir, rel), body), cleanup: () => fs.rmSync(dir, { recursive: true, force: true }) };
}

/** The add-on handle behind the last lint of `dir` (the one opened last). */
const handleOf = (dir) => [...native.projects.entries()].find(([k]) => k.startsWith(`${dir}\0`))?.[1].handle;
const handleStats = (dir) => handleOf(dir).__stats();

const RULES = `
[[forbidden]]
name = "no-legacy"
severity = "error"
from = { path = '^src/' }
to = { path = '^src/legacy/' }

[[forbidden]]
name = "unresolvable"
severity = "error"
to = { could_not_resolve = true }
`;

test("polling picks up a config edit", needsHooks, async () => {
  hooks.__setWatcherEnabled(false);
  const p = scratch({ "package.json": "{}", "detangle.toml": RULES, "src/a.ts": 'import "./legacy/old";\n', "src/legacy/old.ts": "" });
  try {
    assert.deepEqual(await p.lintFile("src/a.ts"), ["1:8 no-legacy: src/a.ts → src/legacy/old.ts"]);
    p.write("detangle.toml", RULES.replace('"error"', '"warn"').replace("no-legacy", "no-old"));
    assert.deepEqual(await p.lintFile("src/a.ts"), ["1:8 no-old: src/a.ts → src/legacy/old.ts"]);
  } finally {
    hooks.__setWatcherEnabled(true);
    p.cleanup();
  }
});

test("polling picks up installed packages through the lockfile", needsHooks, async () => {
  hooks.__setWatcherEnabled(false);
  const p = scratch({ "package.json": "{}", "package-lock.json": "{}", "detangle.toml": RULES, "src/a.ts": 'import "pkg";\n' });
  try {
    assert.deepEqual(await p.lintFile("src/a.ts"), ["1:8 unresolvable: src/a.ts → pkg"]);
    // `npm install pkg`.
    fs.mkdirSync(path.join(p.dir, "node_modules/pkg"), { recursive: true });
    p.write("node_modules/pkg/index.js", "");
    p.write("package-lock.json", '{ "lockfileVersion": 3 }');
    assert.deepEqual(await p.lintFile("src/a.ts"), []);
    const s = handleStats(p.dir);
    assert.deepEqual([s.refreshes, s.opens], [1, 1]);
  } finally {
    hooks.__setWatcherEnabled(true);
    p.cleanup();
  }
});

test("a broken config recovers once it's fixed, without retrying before", needsHooks, async () => {
  hooks.__setWatcherEnabled(false);
  const p = scratch({ "package.json": "{}", "detangle.toml": "[[forbidden]\n", "src/a.ts": 'import "./legacy/old";\n', "src/legacy/old.ts": "" });
  try {
    for (let i = 0; i < 3; i++) {
      const [m, ...rest] = await p.lintFile("src/a.ts");
      assert.match(m, /^1:1 detangle: parsing .*detangle\.toml/);
      assert.deepEqual(rest, []);
    }
    assert.deepEqual([handleStats(p.dir).state, handleStats(p.dir).openAttempts], ["broken", 1]);
    p.write("detangle.toml", RULES);
    assert.deepEqual(await p.lintFile("src/a.ts"), ["1:8 no-legacy: src/a.ts → src/legacy/old.ts"]);
    assert.equal(handleStats(p.dir).state, "ready");
  } finally {
    hooks.__setWatcherEnabled(true);
    p.cleanup();
  }
});

test("a panic is reported, and recovers after the text changes and the floor passes", needsHooks, async () => {
  const p = scratch({ "package.json": "{}", "detangle.toml": RULES, "src/a.ts": 'import "./legacy/old";\n', "src/legacy/old.ts": "" });
  try {
    const good = 'import "./legacy/old";\n';
    assert.deepEqual(await p.lintFile("src/a.ts", good), ["1:8 no-legacy: src/a.ts → src/legacy/old.ts"]);
    const handle = handleOf(p.dir);
    handle.__setRetryFloorMs(300);
    const crash = `${good}// __detangle_test_panic__\n`;
    const [m] = await p.lintFile("src/a.ts", crash);
    assert.match(m, /^1:1 detangle: internal error \(test panic\)/);
    // Fixed at once: within the floor, still failed. Nothing threw.
    assert.match((await p.lintFile("src/a.ts", good))[0], /internal error/);
    await new Promise((r) => setTimeout(r, 350));
    assert.deepEqual(await p.lintFile("src/a.ts", good), ["1:8 no-legacy: src/a.ts → src/legacy/old.ts"]);
    assert.equal(handle.__stats().openAttempts, 2);
  } finally {
    p.cleanup();
  }
});

// Project keys and lifecycle (D8).

test("the same project through another spelling or a symlink is one project", async () => {
  const before = native.projects.size;
  const opts = (dir) => [detangle.configs.recommended, { rules: { "detangle/errors": ["error", { dir, mode: "keys" }], "detangle/warnings": ["warn", { dir, mode: "keys" }] } }];
  await lint(conditions, ["src/c1.ts"], opts("."));
  await lint(conditions, ["src/c1.ts"], opts(conditions + path.sep));
  const link = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "detangle-key-")), "c");
  fs.symlinkSync(conditions, link, "junction");
  try {
    await lint(conditions, ["src/c1.ts"], opts(link));
  } finally {
    fs.rmSync(path.dirname(link), { recursive: true, force: true });
  }
  assert.equal(native.projects.size, before + 1);
});

test("a root at the home directory is refused", async () => {
  const home = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), "detangle-home-")));
  fs.mkdirSync(path.join(home, "src"));
  fs.writeFileSync(path.join(home, "src/a.ts"), 'import "./b";\n');
  const saved = { HOME: process.env.HOME, USERPROFILE: process.env.USERPROFILE };
  process.env.HOME = process.env.USERPROFILE = home;
  try {
    const got = await lint(home, ["src/a.ts"], recommended({}));
    assert.deepEqual(got.map((m) => `${m.line}:${m.column} ${m.message}`), [`1:1 detangle: refusing to scan ${home}; set the "dir" option`]);
    // The dir option is the way out.
    const ok = await lint(home, ["src/a.ts"], recommended({ dir: "src" }));
    assert.ok(!ok.some((m) => /refusing/.test(m.message)), JSON.stringify(ok));
  } finally {
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("a project unused for 15 minutes is closed, and opened again when linted", async () => {
  let now = Date.now();
  native.setClock(() => now);
  try {
    const a = recommended({ mode: "idle-a" });
    const b = recommended({ mode: "idle-b" });
    await lint(conditions, ["src/c1.ts"], a);
    const key = [...native.projects.keys()].find((k) => k.endsWith("\0idle-a"));
    const first = native.projects.get(key).handle;
    now += 14 * 60 * 1000;
    await lint(conditions, ["src/c1.ts"], b);
    assert.equal(native.projects.get(key)?.handle, first, "not idle for 15 minutes yet");
    now += 2 * 60 * 1000;
    await lint(conditions, ["src/c1.ts"], b);
    assert.equal(native.projects.get(key), undefined);
    // Closed: a handle kept elsewhere only reports that.
    assert.deepEqual(first.violationsFor(path.join(conditions, "src/c1.ts"), "").problems, ["detangle: project closed"]);
    await lint(conditions, ["src/c1.ts"], a);
    assert.notEqual(native.projects.get(key).handle, first);
  } finally {
    native.setClock(Date.now);
  }
});

/** Runs an ES module script in a fresh Node from npm/; returns its result. */
function runNode(script, env = {}) {
  return spawnSync(process.execPath, ["--input-type=module", "-e", script], {
    cwd: path.dirname(fileURLToPath(import.meta.url)),
    env: { ...process.env, ...env },
    encoding: "utf8",
    timeout: 60_000,
  });
}

test("JavaScript configs are evaluated with the Node running ESLint", () => {
  // No node on PATH at all.
  const r = runNode(
    `
    import { ESLint } from "eslint";
    import tseslint from "typescript-eslint";
    import detangle from "./eslint.js";
    const eslint = new ESLint({
      cwd: ${JSON.stringify(conditions)},
      overrideConfigFile: true,
      overrideConfig: [
        { files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } },
        detangle.configs.recommended,
        { rules: { "detangle/errors": ["error", { config: "rules.config.cjs" }] } },
      ],
    });
    const [r] = await eslint.lintFiles(["src/c3.ts"]);
    console.log(JSON.stringify(r.messages.filter((m) => m.ruleId === "detangle/errors").map((m) => m.message)));
  `,
    { PATH: "", Path: "" },
  );
  assert.equal(r.status, 0, r.stderr);
  assert.deepEqual(JSON.parse(r.stdout), ["value-cycles: src/c3.ts → src/c4.ts (cycle: src/c3.ts → src/c4.ts → src/c5.ts → src/c3.ts)"]);
});

test("a worker's projects are torn down with it, and the process exits promptly", () => {
  const r = runNode(`
    import { Worker } from "node:worker_threads";
    const started = Date.now();
    const worker = new Worker(\`
      const { parentPort } = require("node:worker_threads");
      const { ESLint } = require("eslint");
      const tseslint = require("typescript-eslint");
      const detangle = require("./eslint.js");
      (async () => {
        const eslint = new ESLint({
          cwd: ${JSON.stringify(conditions)},
          overrideConfigFile: true,
          overrideConfig: [{ files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } }, detangle.configs.recommended],
        });
        const results = await eslint.lintFiles(["src/**/*.ts"]);
        parentPort.postMessage(results.reduce((n, r) => n + r.messages.length, 0));
      })();
    \`, { eval: true, execArgv: [] });
    const reports = await new Promise((resolve, reject) => { worker.once("message", resolve); worker.once("error", reject); });
    await worker.terminate();
    console.log(JSON.stringify({ reports, ms: Date.now() - started }));
  `);
  assert.equal(r.status, 0, r.stderr);
  assert.doesNotMatch(r.stderr, /panicked|Abort|Segmentation/);
  const { reports } = JSON.parse(r.stdout);
  assert.ok(reports > 0);
});

test("on Windows, a path differing only in case is the same file", { skip: process.platform !== "win32" && "Windows only" }, async () => {
  const code = fs.readFileSync(path.join(eslintFixture, "src/app.ts"), "utf8");
  const eslint = new ESLint10({
    cwd: eslintFixture,
    overrideConfigFile: true,
    overrideConfig: [{ files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } }, recommended({ mode: "case" })].flat(),
  });
  const lintAt = async (rel) => (await eslint.lintText(code, { filePath: path.join(eslintFixture, rel) }))[0].messages.map((m) => `${m.line}:${m.column} ${m.message}`);
  const exact = await lintAt("src/app.ts");
  assert.ok(exact.length > 0);
  assert.deepEqual(await lintAt("SRC/app.ts"), exact);
});

test("watcher setup time on this OS (logged)", needsHooks, async (t) => {
  // The checkout's root, npm/node_modules included: thousands of
  // directories, like a real project. Watcher setup runs on its own thread,
  // picked up by the first lint after it's done.
  const root = fileURLToPath(new URL("..", import.meta.url));
  const file = path.join(root, "npm/eslint.js");
  const started = performance.now();
  const handle = native.addon().open(root, { mode: "watcher-timing" });
  const opened = performance.now() - started;
  while (!handle.__stats().watching) {
    handle.violationsFor(file, "");
    assert.ok(performance.now() - started < 30_000, "the watcher never started");
    await new Promise((r) => setTimeout(r, 1));
  }
  const ready = performance.now() - started;
  t.diagnostic(`${process.platform}: open (scan) ${opened.toFixed(0)} ms; watcher ready ${ready.toFixed(0)} ms after the open began`);
  handle.close();
});
