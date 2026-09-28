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

// Node matching, one form at a time. The add-on reads the files on disk, so
// src/app.ts has its no-legacy violation on "./legacy/old" whatever the
// linted text is; these check which nodes of that text carry it.
const eslintFixture = path.join(fixtures, "eslint");
const legacy = "no-legacy: src/app.ts → src/legacy/old.ts — Use the new API";

async function lintText(code) {
  const eslint = new ESLint10({
    cwd: eslintFixture,
    overrideConfigFile: true,
    overrideConfig: [
      { files: ["**/*.ts"], languageOptions: { parser: tseslint.parser } },
      { plugins: { detangle }, rules: { "detangle/errors": "error" } },
    ],
  });
  const [r] = await eslint.lintText(code, { filePath: path.join(eslintFixture, "src/app.ts") });
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

// Forms detangle doesn't resolve either, so no node carries the violation:
// it goes to line 1, naming the import string.
const unmatched = [
  ["require(variable)", `const id = "./legacy/old";\nrequire(id);`],
  ["template literal with an expression", "const x = 'old';\nrequire(`./legacy/${x}`);"],
  ["an unknown callee", `load("./legacy/old");`],
  ["require with two arguments", `require("./legacy/old", 1);`],
  ["templateUrl outside a decorator", `const c = { templateUrl: "./legacy/old" };`],
];
for (const [name, code] of unmatched) {
  test(`node matching: never ${name}`, async () => {
    assert.deepEqual(await lintText(code), [`1:1 ${legacy} [import "./legacy/old"]`]);
  });
}

test("folder and group violations without an import are shown nowhere", async () => {
  const cli = detangleJson("check", "-f", "json", eslintFixture).map((v) => v.rule);
  assert.ok(cli.includes("orders-folder-unused") && cli.includes("orders-group-unused"));
  const got = await lint(eslintFixture, ["**/*.ts"], recommended({}));
  assert.ok(!got.some((m) => /orders-(folder|group)-unused/.test(m.message)));
});
