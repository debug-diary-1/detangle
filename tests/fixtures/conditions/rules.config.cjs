module.exports = {
  forbidden: [
    { name: "up", severity: "error", from: {}, to: { ancestor: true } },
    { name: "not-up", severity: "error", from: { path: "deep" }, to: { ancestor: false } },
    { name: "exotic", severity: "error", from: {}, to: { exoticallyRequired: true } },
    { name: "need", severity: "error", from: {}, to: { exoticRequire: "^need$" } },
    { name: "not-need", severity: "error", from: {}, to: { exoticRequireNot: "^need$" } },
    { name: "bundled", severity: "error", from: {}, to: { dependencyTypes: ["npm-bundled"] } },
    { name: "value-cycles", severity: "error", from: {}, to: { circular: true, viaOnly: { dependencyTypesNot: ["type-only"] } } },
    { name: "lazy-cycles", severity: "error", from: {}, to: { circular: true, via: { dependencyTypes: ["dynamic-import"] } } },
  ],
  options: { tsPreCompilationDeps: true, exoticRequireStrings: ["module.require", "need"], tsConfig: { fileName: "tsconfig.json" } },
};
