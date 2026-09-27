# detangle

Fast dependency analysis and architecture rules for JavaScript/TypeScript, React, Vue, Svelte and Angular, with a live-reloading terminal explorer. Written in Rust on top of the [oxc](https://oxc.rs) parser and resolver.

Analyses VS Code's `src/` (9.6k files, 113k dependencies) in **0.3 s**.

## Install

```sh
cargo install --path .
```

## Usage

```sh
detangle                        # interactive explorer; rebuilds live as you edit
detangle watch                  # re-run the rules on every change
detangle check                  # run the rules; exit 1 on errors (CI)
detangle check -f github        # GitHub Actions annotations on the PR
detangle check -f markdown      # summary + details for a PR comment or job summary
detangle check -f teamcity      # TeamCity inspections (also: -f azure for Azure DevOps)
detangle check --strict         # also fail on warnings
detangle report --open          # self-contained HTML report
detangle stats                  # overview + hotspots
detangle why src/app.ts lodash  # shortest import chain from A to B
detangle affected --since origin/main --filter '\.test\.ts$'   # tests to run
detangle graph -f mermaid --collapse 2 > deps.mmd               # architecture diagram
detangle graph -f dot --focus 'features/cart' | dot -Tsvg > cart.svg
detangle graph --focus 'cart' --focus-depth 2 --highlight 'api/'   # two steps out, api modules marked
detangle graph --reaches 'src/db/' -f mermaid                       # everything that depends on db
detangle graph --collapse '^packages/[^/]+/' -f mermaid             # one node per package
detangle graph --from 'src/main\.ts$' --max-depth 2                 # what the entry imports, 2 steps deep
detangle graph -f d2 > deps.d2; detangle graph -f csv > matrix.csv    # D2 diagram, adjacency matrix
detangle init                   # write a starter detangle.toml
```

The project root is the nearest ancestor containing `detangle.toml`, or else `package.json`. Pointing detangle at a subdirectory scans only that subdirectory, and paths are still reported relative to the root.

### Explorer keys

| key | action |
|---|---|
| `1`–`4` / `Tab` | Modules · Violations · Cycles · Hotspots |
| `↑↓` `j k`, `PgUp/PgDn`, `g G` | move |
| `←→` `h l` | switch pane: modules / imports / imported by |
| `Enter` | follow the selected dependency |
| `Backspace` `b` | go back |
| `/` | filter (space-separated terms) |
| `s` | sort by path, fan-in, fan-out or instability |
| `e` | show npm packages, builtins and unresolved imports |
| `c` / `v` | jump to this module's cycle or violations |
| `r` | rebuild now (it rebuilds automatically on changes; `--no-watch` turns that off) |

## Incremental rebuilds

`detangle watch` and the explorer keep the parsed project in memory. What gets redone depends on the change:

| Change | Work redone | VS Code `src/` |
|---|---|---|
| Edit a file without changing its imports | re-parse and re-resolve only that file; the analysis is kept, since the graph can't have changed | ~5 ms |
| Edit a file's imports | re-parse and re-resolve only that file, then rebuild the graph and rerun the rules | ~50 ms |
| Add, remove or rename a file or folder | walk the tree again and re-resolve every import (the new file can change what `./foo` points to); only new or changed files are re-parsed | ~170 ms |
| Change `tsconfig`, `package.json` or `detangle.toml`, or press `r` | full rebuild | ~300 ms |

Set `DETANGLE_TIMINGS=1` to print how long each phase (config, scan, graph, rules) took to stderr.

detangle checks the filesystem itself to decide whether files were added or removed, because watchers (notably macOS FSEvents) often report an ordinary save as a new file. Atomic saves from editors like vim and JetBrains therefore count as plain edits.

### Parse cache

`--cache` (or `options.cache = true`) keeps each file's parsed imports in `node_modules/.cache/detangle` and re-parses only files that changed. On VS Code's `src/` a warm `check` takes about 110 ms instead of 205 ms. Resolution always runs fresh, so installing packages or adding files is never missed. By default, files count as changed when their modification time or size changes. With `--cache-strategy content` (`options.cache_strategy = "content"`), a file whose timestamp changed but whose contents didn't is still reused. That helps fresh CI checkouts once the cache is restored: the first run hashes and rewrites the cache, and later runs are fast again.

## What it understands

- **React**: JSX/TSX, including JSX in plain `.js` files as Create React App, Vite and Babel setups allow. `React.lazy(() => import(...))` shows up as a dynamic import.
- **Vue and Svelte** single-file components. It reads the `<script>` and `<script setup>` blocks, uses the `lang="ts"` or `lang="tsx"` setting, and treats `<script src>` as an import. Template content and `<svelte:head>` browser scripts are ignored.
- **Angular**: `templateUrl`, `styleUrl` and `styleUrls` in decorators become dependencies of type `resource`, so a missing template or stylesheet is reported. Lazy `loadComponent` and `loadChildren` routes are picked up as dynamic imports.
- If a tsconfig can't be loaded (for example, it `extends` a package that isn't installed), detangle falls back to resolving without it, so a single broken tsconfig doesn't make every import unresolvable.
- Every import form: `import`, `import type`, `export … from`, `import()`, `require()`, `import x = require()`, `import("x").T`, AMD `define([...])` / `require([...])`, and `/// <reference path|types>` and `/// <amd-dependency>` directives
- Opt-in, as in other tools: JSDoc type imports (`@import … from "x"`, `{import("x").T}`) with `options.jsdoc_imports = true`, `process.getBuiltinModule("fs")` with `options.builtin_module_calls = true`, and require-like functions with `options.exotic_require = ["module.require"]`
- **Aliases** from Vite (`resolve.alias` in object or array form, including RegExp `find` and `/src`-style root-relative replacements), from webpack (`resolve.alias`, including `name$` and `false`, plus `resolve.modules` and `resolve.extensions`), from Babel's `babel-plugin-module-resolver` (`alias`, including `^regex` keys with `\1`, and `root`), or declared directly in `detangle.toml` (see below)
- tsconfig `paths` (per-file discovery or an explicit tsconfig), `package.json` `exports` and `imports`, and `.js`→`.ts` extension aliasing
- Type-only and dynamic imports. By default type-only imports don't count toward cycles, because they're erased at runtime.
- npm dependency classification by walking every enclosing `package.json`, so monorepo roots work: `npm`, `npm-dev`, `npm-peer`, `npm-optional`, `npm-undeclared`. `@types/*` packages count as declared for type-only packages.
- `.gitignore` files are respected.
- **Yarn Plug'n'Play**: when the root has a `.pnp.cjs`, packages resolve through it (from Yarn's zip cache), with PnP's strictness: undeclared transitive packages are unresolvable.
- Resolution can be tuned in `[options.resolve]`, where each key replaces detangle's default:

  ```toml
  [options.resolve]
  condition_names = ["browser", "import"]   # package.json exports conditions
  main_fields = ["browser", "module", "main"]
  main_files = ["index", "main"]
  exports_fields = ["exports"]
  alias_fields = ["browser"]                # browser-field remapping
  extensions = [".ts", ".js"]
  preserve_symlinks = true
  builtins_add = ["electron", "vscode"]     # or builtins = [...] to replace Node's list
  yarn_pnp = false                          # default: on when .pnp.cjs exists
  ```

## Rules

`detangle.toml` (run `detangle init` for a commented starter). Paths are regular expressions matched against root-relative paths. Lookarounds work, and a list of patterns means "any of these".

```toml
allowed_severity = "error"    # for [[allowed]] below; top-level keys go before any table

[options]
exclude = ["**/node_modules/**", "**/dist/**"]   # globs of files to skip
include_only = '^src/'        # regex: keep only these modules (sources and targets)
exclude_path = '^src/generated/'
do_not_follow = '^src/vendor/'   # keep these modules, but not their own imports
exclude_dynamic = true           # leave import() dependencies out
cycles_ignore_type_only = true

[[forbidden]]
name = "no-circular"
severity = "warn"             # error | warn | info | off
to = { circular = true }

# cycles that pass through a module in shared/ (considers every cycle, not one arbitrary one)
[[forbidden]]
name = "no-cycles-via-shared"
to = { circular = true, via = '^src/shared/' }

# cycles made of runtime imports only (via / via_only also take dependency_types(_not))
[[forbidden]]
name = "no-runtime-cycles"
to = { circular = true, via_only = { dependency_types_not = ["type-only"] } }

# only tight loops: the shortest cycle through the dependency has at most 3 modules
[[forbidden]]
name = "no-short-cycles"
to = { circular = true, max_cycle_length = 3 }

# $1 refers to capture groups of from.path
[[forbidden]]
name = "no-cross-feature"
severity = "error"
from = { path = '^src/features/([^/]+)/' }
to = { path = '^src/features/', path_not = '^src/features/$1/' }

# dead-file detection: everything must be reachable from the entry point
[[forbidden]]
name = "no-dead-files"
from = { path = '^src/index\.ts$' }
to = { path = '^src/', reachable = false }

[[forbidden]]
name = "no-copyleft"
severity = "error"
to = { license = 'GPL|AGPL' }

# rules about modules themselves: "shared" code must have 2+ dependents
[[forbidden]]
name = "utils-must-be-shared"
module = { path = '^src/utils/', number_of_dependents_less_than = 2 }

# allow-list: anything not matching an allowed rule is reported as not-in-allowed
# (with `allowed_severity`, set at the top of the file)
[[allowed]]
from = { path = '^src/ui/' }
to = { path = ['^src/ui/', '^src/domain/'] }

# modules matching `module` must depend on something matching `to`
[[required]]
name = "controllers-extend-base"
severity = "error"
module = { path = '\.controller\.ts$' }
to = { path = 'base-controller' }
```

### Aliases

```toml
[options]
vite_config = "vite.config.ts"         # or .js / .mjs / .mts; defineConfig and functions work
webpack_config = "webpack.config.js"   # or .ts; functions and arrays of configs work
babel_config = "babel.config.js"       # or .babelrc / package.json
aliases = { "@" = "./src", "@lib" = "./packages/lib/src" }
```

Vite, webpack and Babel configs are evaluated with Node, so their dependencies must be installed. By default they're evaluated as a development server would evaluate them. To choose what they see:

```toml
[options.config_env]
mode = "production"                  # Vite `mode`, webpack `argv.mode` (default "development")
command = "build"                    # Vite `command`: "serve" (default) or "build"
webpack_env = { production = true }  # webpack `env`, as with `webpack --env production`
vars = { API_TARGET = "staging" }    # environment variables while evaluating configs
env_files = [".env", ".env.{mode}"]  # default: .env, .env.local, .env.{mode}, .env.{mode}.local; false = off
env_dir = "config"                   # where .env files live (default: project root)
```

`.env` files are loaded into the environment the configs are evaluated in, so a webpack config that reads `process.env.X` from a `.env` file works. The rules:

- **Precedence:** later files override earlier ones; `vars` beat your shell, which beats `.env` files.
- **Syntax:** as `dotenv` + `dotenv-expand`: `export`, `#` comments, single, double or backtick quotes, multi-line quoted values, `\n` escapes in double quotes, and `${VAR}`, `${VAR:-default}` and `$VAR` expansion (`\$` for a literal `$`).
- **Expansion order:** expansion runs after merging, with the same precedence, so `.env.production` can change a value that `.env` builds on.
- **Watch mode:** editing a `.env*` file triggers a rebuild.

- **`--mode`:** `detangle check --mode staging` (and every other command) overrides `mode` for one run.
- **`NODE_ENV`:** defaults to `production` for a production mode or a `build`, otherwise `development`, as in Vite. A value in `vars` or your shell wins.
- **webpack:** as with webpack-cli, `env` also gets `WEBPACK_SERVE`, or `WEBPACK_BUILD` and `WEBPACK_BUNDLE`.
- **Babel:** `api.env()` follows `BABEL_ENV`, then `NODE_ENV`. They only run when named here, never by auto-detection. Aliases rewrite the import before resolution, so tsconfig `paths`, package `exports` and the other resolution rules still apply to the result. Editing any of these config files triggers a full rebuild in watch mode.

### Groups, tags and Nx projects

Groups name parts of the codebase: features, layers, packages. Each distinct match of a group's `path` regex (at the start of a module path) is one group instance, and a module belongs to the first group that matches it. With `options.group_match = "deepest"`, it belongs to the one matching the longest path instead, so a component folder nested in a module folder is its own group. Modules inherit their group's tags. `scope = "group"` rules run on the group graph, where dependencies inside one group instance don't count and cycles are computed between groups.

```toml
[[groups]]
name = "feature"                       # the group's type, also a tag
path = '^src/features/[^/]+'           # one group per feature folder
tags = ["layer:domain"]

[[groups]]
name = "ui"
path = '^src/ui/[^/]+'

# features may only use ui and each other
[[forbidden]]
name = "feature-deps"
scope = "group"
from = { tags = ["feature"] }
to = { tags_not = ["feature", "ui"] }
```

With `options.nx_projects = true`, every Nx project is a group, rediscovered on each run with its `tags`. Projects are found the way Nx finds them without plugins: every `project.json`, and every `package.json` in the package manager's workspaces (`workspaces`, `pnpm-workspace.yaml`, `lerna.json`). Those also get Nx's `npm:public`/`npm:private` and keyword tags. Each project also gets `projectType:application` or `projectType:library` (inferred as Nx does when unset) and `target:<name>` for each of its targets. Nested projects take precedence over their parents. Tag conditions (`tags`, `tags_not`, `reaches_tags`, and `tags_all` on `from`) accept Nx patterns: exact tags, `*` globs and `/regex/`. On the module scope, `to.cross_group = true` / `false` matches dependencies between different groups / within one group. The `relative` dependency type matches imports written as a relative or absolute path.

A group dependency stands for the imports behind it. At group scope, `specifier`, `specifier_not`, `dependency_types` and `dependency_types_not` are checked against those imports: the rule matches when at least one import satisfies them. The same goes for a module that imports one target several times (`import` and `export … from`, or `lodash` and `lodash/fp`). Violations list those imports, so `detangle check` shows the files to fix, and GitHub annotations land on them.

```toml
# no feature may pull in the server layer, even indirectly
[[forbidden]]
name = "features-stay-client-side"
scope = "group"
from = { tags = ["type:feature"] }
to = { reaches_tags = ["type:server"] }

# a lib loaded with import() somewhere must not also be imported statically
[[forbidden]]
name = "keep-lazy-libs-lazy"
scope = "group"
to = { lazy_loaded = true, dependency_types_not = ["dynamic", "type-only"] }
```

### Folder scope

Add `scope = "folder"` to evaluate a rule on the folder graph instead of the module graph. Every directory stands for its whole subtree, so `src/features/cart` includes `src/features/cart/ui/…`. Folder names have no trailing slash, and root-level files belong to no folder.

```toml
# features may not depend on each other (as whole subtrees)
[[forbidden]]
name = "no-cross-feature"
scope = "folder"
from = { path = '^src/features/([^/]+)$' }
to = { path = '^src/features/', path_not = '^src/features/$1(/|$)' }

# no cycles between folders, even when there are none between modules
[[forbidden]]
name = "no-folder-cycles"
scope = "folder"
to = { circular = true }

# stable dependencies principle, per folder
[[forbidden]]
name = "depend-on-stable-folders"
scope = "folder"
from = { path = '^src/' }
to = { path = '^src/', more_unstable = true }
```

At folder scope, a module edge `a → b` makes every folder containing `a` but not `b` depend on `b`'s folder. Instability uses module-level coupling across the folder boundary, and cycles are found exactly. `module` rules count dependent folders. `orphan` and `reachable` apply only to modules.

| | conditions |
|---|---|
| `from` | `path`, `path_not`, `orphan`, `tags`, `tags_not`, `tags_all` |
| `to` | `path`, `path_not`, `specifier`, `specifier_not`, `circular`, `via`, `via_only`, `max_cycle_length`, `tags`, `tags_not`, `reaches_tags`, `cross_group`, `lazy_loaded`, `ancestor`, `exotically_required`, `exotic_require`, `exotic_require_not`, `dependency_types`, `dependency_types_not`, `could_not_resolve`, `type_only`, `dynamic`, `reachable`, `more_unstable`, `more_than_one_dependency_type`, `license`, `license_not` |
| `module` | `path`, `path_not`, `number_of_dependents_less_than`, `number_of_dependents_more_than` (with `from` restricting which dependents count) |

Dependency types: `local`, `npm`, `npm-dev`, `npm-peer`, `npm-optional`, `npm-undeclared`, `npm-bundled`, `core`, `unresolvable`, `type-only`, `dynamic`, `require`, `reexport`, `resource`, `amd`, `triple-slash`, `jsdoc`, `exotic-require`, `process-get-builtin-module`, `import` (a plain `import`/`export`), `aliased` (a tsconfig-paths, `#imports` or workspace import of a local file), `deprecated` (the installed package is marked deprecated). A package declared in several `package.json` sections has all of the matching types, for example `npm` and `npm-dev`. npm packages can also be matched as `node_modules/<name>/`.

## Node.js API

`npm/` is a small package (`detangle-deps`, not published yet) that runs the `detangle` binary and returns plain JavaScript values, with TypeScript types for both ESM and CommonJS. It uses `options.bin`, then `$DETANGLE_BIN`, then `detangle` on `PATH`.

```js
import { analyze, check, report, graph, migrate } from "detangle-deps";

const { modules, cycles, violations } = await analyze("src", { cache: true });
const { errors, exitCode } = await check(".", { strict: true });
const { output } = await report(".", { format: "markdown" });        // text, markdown, github, teamcity, azure
const svgSource = await graph(".", { format: "dot", focus: /cart/, focusDepth: 2 });
const { config } = await migrate(".");                               // the detangle.toml migrate would write
```

## HTML report

`detangle report` writes one self-contained HTML file with no external requests, so it can be attached to CI runs or shared as a file. It has:

- **Violations**, grouped by rule, with a filter, severity toggles and the cycle behind every circular dependency.
- **Modules**, a sortable table (fan-in, fan-out, instability, cycle, violations). Selecting a module shows its imports, its importers, violations in both directions, and the shortest loop through it.
- **Cycles**, each strongly connected component with its shortest loop.
- **Folder graph**, an interactive map of folders. It picks a readable depth automatically, colours folder cycles red, and lets you zoom, pan and drag. Double-click a folder to open it, and use the breadcrumbs to go back up.

It follows the system light or dark theme. VS Code's 113k dependencies produce a 3.3 MB report.

### Adopting rules in a legacy codebase

```sh
detangle check --write-baseline .detangle-baseline.json   # record today's violations
detangle check --baseline .detangle-baseline.json         # fail only on new ones
detangle check --write-baseline --baseline-mode shrink-only   # drop fixed entries, never add new ones
```

With `options.baseline_stale = "warn"` (or `"info"`, `"error"`), entries that no longer occur are reported as `stale-baseline-entry`, so the baseline doesn't silently keep permission for violations that were fixed.

## Migrating to detangle

```sh
detangle migrate --dry-run   # preview the generated detangle.toml
detangle migrate             # write detangle.toml (+ .detangle-baseline.json)
detangle check               # same checks as before, much faster
```

`detangle migrate` looks at the project root, converts every dependency-rule setup it finds into one `detangle.toml`, and tells you what to change:

| Source | What's converted |
|---|---|
| **JavaScript rules configs** (`.js`, `.cjs`, `.mjs` or `.json` files declaring `forbidden`, `allowed` or `required` rules). These are recognised by content, not file name. | Every rule, `extends` presets, options, and the known-violations file they point at |
| **ESLint** (`eslint.config.*`, `.eslintrc.{js,cjs,json,yaml,yml}`, `package.json` `eslintConfig`) | `import/no-cycle` (with `maxDepth`), `import/no-restricted-paths` (zones, `except`, `basePath`, messages) and `import/no-extraneous-dependencies` (dev/optional/peer globs), also under `import-x`. `files`, `ignores` and `overrides` scoping carry over (an `"off"` for test files becomes a `path_not`), and so do the `import/resolver` webpack and TypeScript settings. Legacy `extends` chains are followed: relative files, shareable `eslint-config-*` packages and `plugin:…` configs. As in ESLint, a later severity-only setting keeps the rule's earlier options. |
| **Nx** (`@nx/enforce-module-boundaries` in an ESLint config) | Project discovery (`nx_projects`) and every depConstraint: `sourceTag` or `allSourceTags`, `onlyDependOnLibsWithTags` (including `[]`), `notDependOnLibsWithTags` (transitively, as Nx checks it), and `bannedExternalImports` / `allowedExternalImports` (matched against the import as Nx does). Options: `allow`, `enforceBuildableLibDependency` with `buildTargets`, `banTransitiveDependencies`, `allowCircularSelfDependency` and `checkDynamicDependenciesExceptions`. Nx's built-in checks: project cycles, imports of apps and e2e projects, relative imports across projects or outside every project, self-imports through the project's own alias, static imports of lazy-loaded libraries, and "a project without tags matching a constraint can't depend on libraries". Like Nx, `require()` calls aren't checked. |
| **eslint-plugin-boundaries** (`boundaries/dependencies`, `element-types`, `entry-point`, `external`, `no-unknown`) | `boundaries/elements` become `[[groups]]`: folder, file and full modes, `partialMatch`, `basePattern`, and captures. As in the plugin, a file belongs to its innermost element. `boundaries/ignore`, `boundaries/include` and `boundaries/dependency-nodes` carry over. Policies are replayed with the plugin's semantics: the last match wins, `disallow` beats `allow` within a policy, and a missing `default` means disallow. This works in the v6+ `{ to: { element: { type, types, captured } } }` format and the legacy `["type", { captured }]` format. Captured-value conditions (literal, glob, or comparing with the source through `{{ from.element.captured.x }}` / `${from.x}` templates) become `$1`-style rules. `entry-point` (allowed file paths inside elements), `external` (module names and paths) and `no-unknown` convert too. Not converted, with a warning: `no-private`, `no-unknown-files`, file descriptors, and selectors on `parent`, `path` or imported names. |
| **madge** (`.madgerc`, `package.json` `madge`, or `madge --circular` in a script) | A circular-dependency rule, `excludeRegExp`, `tsConfig`, `webpackConfig`, `skipTypeImports` |
| **Known-violation files** (JSON arrays of `{ from, to, rule: { name } }`) | `.detangle-baseline.json`, applied automatically through `options.baseline` |

Rules found in several places (for example a madge cycle check and a cycle rule) are merged, keeping the stricter severity. Anything that can't be converted exactly is listed at the top of `detangle.toml` for review instead of being silently loosened. `package.json` scripts that ran the old tools get a suggested `detangle check` replacement.

**Switching CI without surprises:** detangle finds cycles that other tools miss, so the first run may report more than before. `detangle check --write-baseline` records today's findings. From then on, `detangle check` fails only on new violations.

You can also run a JavaScript rules config directly without converting it: `detangle check -c rules.config.js`.

### Verified against the tools themselves

- **ESLint 9 + eslint-plugin-import 2.32**, on a project using all three rules with zones, `except`, file-scoped overrides and dev-dependency globs: detangle's migrated config reports **exactly the same 7 violations**, rule for rule, file for file, import for import.
- **ESLint 9 legacy configs with `extends`**: a zone from a relative config, `no-cycle` with `maxDepth: 1` from a shareable config (kept when the root sets `"warn"`), and a directory the shared config turned off but the root re-enabled. Detangle reports **exactly the same 7 findings**. `maxDepth` 1, 2 and 3 on a project with cycles of 2, 3 and 4 modules match the plugin exactly.
- **Nx 21** (`@nx/enforce-module-boundaries`), on a workspace with scope and type tags, an application, an untagged lib, a banned external, a relative cross-project import and project cycles: detangle flags **exactly the same 10 imports**. Adding a new tagged project afterwards is enforced without migrating again, because projects are rediscovered on every run.
- **Nx 21 options**, on a second workspace with `allow`, buildable libraries (executor and command targets), `banTransitiveDependencies` (a package declared only by another project, one not installed), a transitive `notDependOnLibsWithTags`, an empty `onlyDependOnLibsWithTags`, a `workspaces` package without an `nx` section, a project typed only by its `tsconfig.app.json`, an e2e project, a self-import through the project's alias, a relative import outside every project, a lazy-loaded library also imported statically, and `require()` and `import()` calls: **exactly the same 13 imports**. (Nx 21's `checkNestedExternalImports` compares the imported project's name with the nested package's name, so it never reports anything; detangle converts it to nothing.)
- **eslint-plugin-boundaries 7.2**, using both the legacy `rules` and v6+ `policies` formats, with a later `disallow` overriding an `allow` and a negated `!app` selector: **exactly the same 4 violations**.
- **eslint-plugin-boundaries 7.2 with captures**: modules and nested components compared through captured values, a literal captured value in a later `disallow`, a policy set without `default`, an ignored test file, `entry-point`, `external` (including a banned subpath) and `no-unknown`: **exactly the same 10 imports**. The legacy format with `basePattern`/`baseCapture`, a literal source condition, `boundaries/include` and `dependency-nodes: ["import"]` also matches exactly.
- **madge 8 on excalidraw** (873 modules): madge's own dependency graph puts 168 files on cycles. `madge --circular` lists 128 of them; detangle reports all 168, with no extras, in 0.03 s against madge's 2.5 s.
- **A JavaScript rules config with known violations:** all 14 known violations were carried into the baseline. The only findings left were genuine cycle dependencies the original setup never reported, each shown with its cycle.

## Cycle detection

On VS Code, checked against an independently computed ground truth, detangle finds all 1,945 dependencies that sit on a cycle, with no false positives. Each one is reported with a concrete cycle as evidence. `via` and `viaOnly` consider every simple cycle through a dependency, not just one arbitrary cycle.
