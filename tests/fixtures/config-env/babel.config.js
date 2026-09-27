module.exports = (api) => {
  api.cache(true);
  return { plugins: [["module-resolver", { alias: { "@babel-env": `./src/benv/${api.env()}` } }]] };
};
