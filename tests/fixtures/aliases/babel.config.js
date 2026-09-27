module.exports = function (api) {
  api.cache(true);
  return {
    plugins: [
      ["module-resolver", { root: ["./src/roots"], alias: { "~": "./src", "^@feature/(.+)$": "./src/features/\\1" } }],
    ],
  };
};
