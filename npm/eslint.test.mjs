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
import { fileURLToPath } from "node:url";
import { ESLint } from "eslint";
import tseslint from "typescript-eslint";
import detangle from "./eslint.js";

const fixtures = fileURLToPath(new URL("../tests/fixtures/", import.meta.url));
const conditions = path.join(fixtures, "conditions");

/** Lints `files` in `cwd` with detangle's rules at the given options. */
async function lint(cwd, files, rules) {
  const eslint = new ESLint({
    cwd,
    overrideConfigFile: true,
    overrideConfig: [
      {
        files: ["**/*.ts"],
        languageOptions: { parser: tseslint.parser },
        plugins: { detangle },
        rules,
      },
    ],
  });
  const results = await eslint.lintFiles(files);
  return results.flatMap((r) =>
    r.messages.map((m) => {
      assert.equal(m.fatal, undefined, `${r.filePath}: ${m.message}`);
      return { file: path.relative(cwd, r.filePath).replaceAll("\\", "/"), line: m.line, column: m.column, rule: m.ruleId, message: m.message };
    }),
  );
}

const key = (m) => `${m.file}:${m.line}:${m.column} ${m.message}`;

/** Runs the CLI for JSON output; `check` exits 1 when it finds errors. */
function detangleJson(...args) {
  const r = spawnSync(process.env.DETANGLE_BIN, args, { encoding: "utf8" });
  assert.ok(r.status === 0 || r.status === 1, `detangle ${args.join(" ")}: ${r.stderr}`);
  return JSON.parse(r.stdout);
}

/**
 * Where detangle/errors should report, worked out from the CLI's own
 * output: `check -f json` for the violations, `graph -f json --externals`
 * for each import's specifier, and the file text for where that specifier
 * is written. Covers module-scope `error` violations with a `to`, shown on
 * static `import`/`export … from` declarations; other import forms come
 * with the full placement table.
 */
function oracle(dir, config) {
  const violations = detangleJson("check", "-f", "json", "-c", config, dir);
  const graph = detangleJson("graph", "-f", "json", "--externals", "-c", config, dir);
  const modules = new Map(graph.modules.map((m) => [m.id, m]));
  const out = [];
  for (const v of violations) {
    if (v.severity !== "error" || v.scope !== "module" || v.to == null) continue;
    let message = `${v.rule}: ${v.from} → ${v.to}`;
    if (v.cycle.length > 1) message += ` (cycle: ${v.cycle.join(" → ")})`;
    if (v.comment) message += ` — ${v.comment}`;
    const text = fs.readFileSync(path.join(dir, v.from), "utf8");
    for (const d of modules.get(v.from).dependencies.filter((d) => d.module === v.to)) {
      // Every quoted occurrence of the specifier that follows `from`.
      const quoted = new RegExp(`(\\bfrom\\s*)(["'])${d.specifier.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\2`, "g");
      for (const m of text.matchAll(quoted)) {
        const at = m.index + m[1].length;
        const before = text.slice(0, at).split("\n");
        out.push({ file: v.from, line: before.length, column: before.at(-1).length + 1, message });
      }
    }
  }
  return out;
}

test("detangle/errors reports what the CLI finds, on the imports behind it", async () => {
  const config = path.join(conditions, "rules.config.cjs");
  const got = await lint(conditions, ["src/**/*.ts"], { "detangle/errors": ["error", { config: "rules.config.cjs" }] });
  const want = oracle(conditions, config);
  assert.ok(want.length >= 10, `the oracle should find plenty to compare (found ${want.length})`);
  assert.ok(got.every((m) => m.rule === "detangle/errors"));
  assert.deepEqual(got.map(key).sort(), want.map(key).sort());
});

test("a config that fails to load is reported at line 1 of every file", async () => {
  const got = await lint(conditions, ["src/c1.ts", "src/c3.ts"], { "detangle/errors": ["error", { config: "missing.toml" }] });
  assert.equal(got.length, 2);
  for (const m of got) {
    assert.equal(`${m.line}:${m.column}`, "1:1");
    assert.match(m.message, /^detangle: reading .*missing\.toml/);
  }
});

test("a project reached through a symlink reports the same", async () => {
  const link = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "detangle-link-")), "conditions");
  fs.symlinkSync(conditions, link, "junction"); // a junction needs no admin rights on Windows
  try {
    const rules = { "detangle/errors": ["error", { config: "rules.config.cjs" }] };
    const direct = await lint(conditions, ["src/**/*.ts"], rules);
    const linked = await lint(link, ["src/**/*.ts"], rules);
    assert.ok(direct.length > 0);
    assert.deepEqual(linked.map(key).sort(), direct.map(key).sort());
  } finally {
    fs.rmSync(path.dirname(link), { recursive: true, force: true });
  }
});
