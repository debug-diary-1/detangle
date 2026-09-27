export type Severity = "error" | "warn" | "info" | "off";

export interface CommonOptions {
  /** Config file (default: <root>/tangle.toml, else the built-in rules). A JavaScript rules config works too. */
  config?: string;
  /** Mode for evaluating Vite / webpack configs. */
  mode?: string;
  /** Reuse parse results between runs: true, or a cache directory. */
  cache?: boolean | string;
  cacheStrategy?: "metadata" | "content";
  /** The tangle binary (default: $TANGLE_BIN, else `tangle` on PATH). */
  bin?: string;
  /** Working directory for relative paths. */
  cwd?: string;
}

export interface ViewOptions {
  /** Folder depth, or a regex: modules collapse to what it matches. */
  collapse?: number | string | RegExp;
  focus?: string | RegExp;
  focusDepth?: number;
  reaches?: string | RegExp;
  highlight?: string | RegExp;
  /** Entry points: only what they (indirectly) import. */
  from?: string | RegExp;
  maxDepth?: number;
  externals?: boolean;
  /** false: leave type-only imports out. */
  types?: boolean;
}

export interface Dependency {
  module: string;
  specifier: string;
  types: string[];
  circular: boolean;
}

export interface Module {
  id: string;
  kind: "local" | "npm" | "core" | "unresolved";
  fanIn: number;
  fanOut: number;
  instability: number;
  cycle: number | null;
  dependencies: Dependency[];
  highlighted?: boolean;
}

export interface Violation {
  rule: string;
  severity: Severity;
  comment: string | null;
  scope: "module" | "folder" | "group";
  from: string;
  to: string | null;
  cycle: string[];
  /** Group scope: the imports behind the violation. */
  imports: { from: string; specifier: string; to: string }[];
}

export interface Analysis {
  summary: {
    modules: number;
    dependencies: number;
    cycles: number;
    errors: number;
    warnings: number;
    info: number;
    timings: { scan_ms: number; graph_ms: number };
  };
  modules: Module[];
  cycles: string[][];
  violations: Violation[];
}

export interface CheckResult {
  violations: Violation[];
  errors: number;
  warnings: number;
  info: number;
  /** What the CLI would exit with. */
  exitCode: number;
}

export function analyze(dir?: string, options?: CommonOptions & ViewOptions): Promise<Analysis>;
export function check(dir?: string, options?: CommonOptions & { strict?: boolean; baseline?: string }): Promise<CheckResult>;
export function report(
  dir?: string,
  options?: CommonOptions & { format?: "text" | "markdown" | "github" | "teamcity" | "azure"; strict?: boolean; baseline?: string },
): Promise<{ output: string; exitCode: number }>;
export function graph(dir: string | undefined, options: CommonOptions & ViewOptions & { format: "json" }): Promise<Analysis>;
export function graph(dir?: string, options?: CommonOptions & ViewOptions & { format?: "dot" | "mermaid" | "d2" | "csv" }): Promise<string>;
export function migrate(dir?: string, options?: Pick<CommonOptions, "bin" | "cwd">): Promise<{ config: string; summary: string }>;

declare const api: {
  analyze: typeof analyze;
  check: typeof check;
  report: typeof report;
  graph: typeof graph;
  migrate: typeof migrate;
};
export default api;
