declare const require: (id: string) => unknown;

// A file below the folder: the folder violation shows here too.
export const button = require("../../orders/model");
