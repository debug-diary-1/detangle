// Types for detangle/eslint.
import type { ESLint, Linter } from "eslint";

/** Options for both rules: `detangle check <dir> --config <config> --mode <mode>`. */
export interface RuleOptions {
  /** Directory to analyse, relative to ESLint's working directory (default: that directory). */
  dir?: string;
  /** Config file, relative to ESLint's working directory (default: <root>/detangle.toml). */
  config?: string;
  /** Mode for evaluating Vite / webpack configs. */
  mode?: string;
}

declare const plugin: ESLint.Plugin & {
  meta: { name: "detangle"; version: string };
  rules: NonNullable<ESLint.Plugin["rules"]>;
  configs: {
    /** Registers the plugin; `detangle/errors` as errors, `detangle/warnings` as warnings. */
    recommended: Linter.Config;
  };
};

export = plugin;
