const path = require("path");
module.exports = (env, argv) => ({
  mode: argv.mode,
  resolve: {
    alias: {
      "@components": path.resolve(__dirname, "src/components"),
      utils$: path.resolve(__dirname, "src/utils/index.js"),
      "legacy-lib": false,
    },
    modules: ["node_modules", path.resolve(__dirname, "src/shared")],
    extensions: [".js", ".jsx"],
  },
});
