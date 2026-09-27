const path = require("path");
const at = (...p) => path.resolve(__dirname, "src", ...p);
module.exports = (env, argv) => ({
  resolve: {
    alias: {
      "@api": at("api", env.production ? "prod" : "dev"),
      "@mode": at("mode", argv.mode),
      "@node-env": at("node-env", process.env.NODE_ENV || "unset"),
      "@var": at("var", process.env.API_TARGET || "none"),
      "@cli": at("cli", env.WEBPACK_BUILD ? "build" : env.WEBPACK_SERVE ? "serve" : "none"),
    },
  },
});
