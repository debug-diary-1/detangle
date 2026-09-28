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

/** A string literal's value, or a template literal's without expressions. */
function stringValue(node) {
  if (node?.type === "Literal" && typeof node.value === "string") return node.value;
  if (node?.type === "TemplateLiteral" && node.expressions.length === 0) return node.quasis[0].value.cooked;
  return undefined;
}

/** The module string of a TypeScript `import("…")` type (typescript-eslint's shapes). */
function importTypeSource(node) {
  if (node.source) return node.source;
  const a = node.argument;
  return a?.type === "TSLiteralType" ? a.literal : a;
}

const ANGULAR_RESOURCES = new Set(["templateUrl", "styleUrl", "styleUrls"]);

/**
 * A rule reporting detangle violations of the given severities. Each goes
 * on every node in the file whose import string is one of the violation's
 * specifiers, covering the import forms detangle's scanner recognizes.
 * Violations with no specifiers, and specifiers no node carries (comment
 * directives, SFC templates), go at line 1.
 */
function rule(severities, description) {
  return {
    meta: { type: "problem", docs: { description }, schema },
    create(context) {
      const result = violationsFor(context, context.options[0]);
      // Import string → messages to report on it.
      const messages = new Map();
      const wholeFile = [];
      for (const v of result.violations) {
        if (!severities.includes(v.severity)) continue;
        if (v.specifiers.length === 0) wholeFile.push(v.message);
        for (const s of v.specifiers) {
          if (!messages.has(s)) messages.set(s, []);
          messages.get(s).push(v.message);
        }
      }
      const matched = new Set();
      const atLine1 = (message) => context.report({ loc: { line: 1, column: 0 }, message });
      /** Reports on `node` if its import string (or one of `also(value)`) is wanted. */
      const check = (node, also) => {
        const value = stringValue(node);
        if (value === undefined) return;
        for (const s of [value, ...(also ? also(value) : [])]) {
          const list = messages.get(s);
          if (!list) continue;
          matched.add(s);
          for (const message of list) context.report({ node, message });
          return;
        }
      };
      /** AMD's dependency array: the first argument that isn't a module id. */
      const amd = (args) => {
        const deps = args.find((a) => stringValue(a) === undefined);
        if (deps?.type === "ArrayExpression") for (const e of deps.elements) check(e);
      };
      if (messages.size === 0 && wholeFile.length === 0 && result.problems.length === 0) return {};
      const exotic = new Set(result.exoticRequire);
      return {
        Program() {
          for (const message of wholeFile) atLine1(message);
          // Problems with the project (e.g. its config) are reported once
          // per file, by whichever detangle rule runs first.
          for (const message of result.problems) {
            if (result.shown.has(message)) continue;
            result.shown.add(message);
            atLine1(message);
          }
        },
        "Program:exit"() {
          for (const [s, list] of messages) {
            if (!matched.has(s)) for (const message of list) atLine1(`${message} [import "${s}"]`);
          }
        },
        ImportDeclaration: (node) => check(node.source),
        ExportNamedDeclaration: (node) => check(node.source),
        ExportAllDeclaration: (node) => check(node.source),
        ImportExpression: (node) => check(node.source),
        TSImportEqualsDeclaration(node) {
          if (node.moduleReference.type === "TSExternalModuleReference") check(node.moduleReference.expression);
        },
        TSImportType: (node) => check(importTypeSource(node)),
        CallExpression(node) {
          const callee = node.callee;
          const args = node.arguments;
          if (callee.type === "Identifier" && callee.name === "require") {
            if (args.length === 1 && stringValue(args[0]) !== undefined) check(args[0]);
            else amd(args);
          } else if (callee.type === "Identifier" && callee.name === "define") {
            amd(args);
          } else if (args.length === 1) {
            const text = context.sourceCode.getText(callee);
            if (text === "process.getBuiltinModule" || exotic.has(text)) check(args[0]);
          }
        },
        // Angular: @Component({ templateUrl, styleUrl, styleUrls }). A bare
        // value is relative to the component, so it's also matched with "./".
        Property(node) {
          const name = node.key.type === "Identifier" ? node.key.name : stringValue(node.key);
          if (!ANGULAR_RESOURCES.has(name)) return;
          if (!context.sourceCode.getAncestors(node).some((a) => a.type === "Decorator")) return;
          const withDot = (v) => [`./${v}`];
          if (node.value.type === "ArrayExpression") for (const e of node.value.elements) check(e, withDot);
          else check(node.value, withDot);
        },
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
