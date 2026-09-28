"use strict";
// ESLint plugin: detangle's violations, shown on the imports that cause them.
//
//   import detangle from "detangle/eslint";
//   export default [detangle.configs.recommended];
//
// detangle/errors reports detangle's `error` violations, detangle/warnings
// its `warn` and `info` ones. Both take { dir, config, mode }, like
// `detangle check <dir> --config … --mode …`; give both the same options,
// so they share one project.

const { violationsFor } = require("./native.js");
const { version } = require("./package.json");

const schema = [
  {
    type: "object",
    properties: { dir: { type: "string" }, config: { type: "string" }, mode: { type: "string" } },
    additionalProperties: false,
  },
];

/** A rule reporting detangle violations of the given severities. */
function rule(severities, description) {
  return {
    meta: { type: "problem", docs: { description }, schema },
    create(context) {
      const result = violationsFor(context, context.options[0]);
      // Import string → messages to report on it.
      const messages = new Map();
      for (const v of result.violations) {
        if (!severities.includes(v.severity)) continue;
        for (const s of v.specifiers) {
          if (!messages.has(s)) messages.set(s, []);
          messages.get(s).push(v.message);
        }
      }
      const report = (source) => {
        if (source?.type !== "Literal" || typeof source.value !== "string") return;
        for (const message of messages.get(source.value) ?? []) context.report({ node: source, message });
      };
      return {
        Program() {
          // Problems with the project (e.g. its config) are reported once
          // per file, by whichever detangle rule runs first.
          if (result.problemsReported) return;
          result.problemsReported = true;
          for (const message of result.problems) context.report({ loc: { line: 1, column: 0 }, message });
        },
        ImportDeclaration: (node) => report(node.source),
        ExportNamedDeclaration: (node) => report(node.source),
        ExportAllDeclaration: (node) => report(node.source),
      };
    },
  };
}

const plugin = {
  meta: { name: "detangle", version },
  rules: {
    errors: rule(["error"], "Report detangle rule violations with severity error"),
    warnings: rule(["warn", "info"], "Report detangle rule violations with severity warn or info"),
  },
  configs: {},
};

// No `files`: it applies to whatever the rest of the config lints.
plugin.configs.recommended = {
  name: "detangle/recommended",
  plugins: { detangle: plugin },
  rules: { "detangle/errors": "error", "detangle/warnings": "warn" },
};

module.exports = plugin;
