"use strict";
// ESLint plugin: detangle's violations, shown on the imports that cause them.
//
//   import detangle from "detangle/eslint";
//   export default [{ plugins: { detangle }, rules: { "detangle/errors": "error" } }];

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
          for (const message of result.problems) context.report({ loc: { line: 1, column: 0 }, message });
        },
        ImportDeclaration: (node) => report(node.source),
        ExportNamedDeclaration: (node) => report(node.source),
        ExportAllDeclaration: (node) => report(node.source),
      };
    },
  };
}

module.exports = {
  meta: { name: "detangle", version },
  rules: {
    errors: rule(["error"], "Report detangle rule violations with severity error"),
  },
};
