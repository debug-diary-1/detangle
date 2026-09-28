import "./shell";
import "./pages/home";
import "./amd";
import "./ref";
import "./node";
import "./widget.component";
import "./cyc/a";

declare const module: { require(id: string): unknown };
declare const require: (id: string) => unknown;

// A dotted exotic require, and a template literal without expressions.
export const a = module.require("./legacy/old");
export const b = require(`./legacy/old`);
export const c = import("./legacy/old");
