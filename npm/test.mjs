// Run with DETANGLE_BIN pointing at a detangle binary: node --test npm/test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import detangle, { analyze, check, report, graph, migrate } from "./index.mjs";

const fixtures = fileURLToPath(new URL("../tests/fixtures/", import.meta.url));
const conditions = fixtures + "conditions";
const rules = conditions + "/rules.config.cjs";

test("ESM and CommonJS entry points expose the same API", () => {
  const cjs = createRequire(import.meta.url)("./index.js");
  assert.deepEqual(Object.keys(cjs).sort(), ["analyze", "check", "default", "graph", "migrate", "report"]);
  assert.equal(cjs.default, cjs);
  assert.equal(detangle.analyze, cjs.analyze);
});

test("analyze returns modules, cycles and violations", async () => {
  const a = await analyze(conditions, { config: rules });
  assert.equal(a.summary.cycles, 3); // tsPreCompilationDeps: the type-only c1 ↔ c2 loop counts
  const deep = a.modules.find((m) => m.id === "src/a/b/deep.ts");
  assert.ok(deep.dependencies.some((d) => d.module === "lodash" && d.types.includes("npm-bundled")));
  assert.ok(a.violations.some((v) => v.rule === "up"));
});

test("analyze applies graph filters", async () => {
  const a = await analyze(conditions, { config: "/dev/null", focus: /src\/c4/ });
  assert.deepEqual(a.modules.map((m) => m.id).sort(), ["src/c3.ts", "src/c4.ts", "src/c5.ts"]);
});

test("check counts violations and reports the CLI exit code", async () => {
  const r = await check(conditions, { config: rules });
  assert.equal(r.errors, 19);
  assert.equal(r.exitCode, 1);
  assert.equal(r.violations[0].severity, "error");
});

test("report renders the CLI formats", async () => {
  const { output, exitCode } = await report(conditions, { config: rules, format: "markdown" });
  assert.match(output, /\| `up` \| error \| 2 \|/);
  assert.equal(exitCode, 1);
});

test("graph returns text formats and JSON", async () => {
  const csv = await graph(conditions, { config: "/dev/null", format: "csv", focus: "c6" });
  assert.equal(csv.split("\n")[0], '"","src/c6.ts","src/c7.ts",""');
  const mmd = await graph(conditions, { config: "/dev/null", format: "mermaid", collapse: /^src\/[^/]+\//, highlight: "deep" });
  assert.match(mmd, /\["src\/a\/"\]:::highlight/);
  const json = await graph(conditions, { config: "/dev/null", format: "json", from: /deep/, maxDepth: 1 });
  assert.ok(json.modules.length > 0);
});

test("migrate previews the generated config", async () => {
  const { config } = await migrate(fixtures + "cycle-depth");
  assert.match(config, /max_cycle_length = 3/);
});

test("errors from the binary are thrown", async () => {
  await assert.rejects(analyze(conditions, { config: conditions + "/missing.toml" }), /detangle analyze failed/);
  await assert.rejects(analyze(conditions, { bin: "/nonexistent/detangle" }), /couldn't run/);
});
