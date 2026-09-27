# tangle

Fast dependency analysis and architecture rules for JavaScript/TypeScript, React, Vue, Svelte and Angular, with a live-reloading terminal explorer. Written in Rust on top of the [oxc](https://oxc.rs) parser and resolver.

Analyses VS Code's `src/` (9.6k files, 113k dependencies) in **0.3 s**.

## Install

```sh
cargo install --path .
```

## Usage

```sh
tangle                        # interactive explorer; rebuilds live as you edit
tangle watch                  # re-run the rules on every change
tangle check                  # run the rules; exit 1 on errors (CI)
tangle check -f github        # GitHub Actions annotations on the PR
tangle check --strict         # also fail on warnings
tangle report --open          # self-contained HTML report
tangle stats                  # overview + hotspots
tangle why src/app.ts lodash  # shortest import chain from A to B
tangle affected --since origin/main --filter '\.test\.ts$'   # tests to run
tangle graph -f mermaid --collapse 2 > deps.mmd               # architecture diagram
tangle graph -f dot --focus 'features/cart' | dot -Tsvg > cart.svg
tangle init                   # write a starter tangle.toml
```

The project root is the nearest ancestor containing `tangle.toml`, or else `package.json`. Pointing tangle at a subdirectory scans only that subdirectory, and paths are still reported relative to the root.

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

`tangle watch` and the explorer keep the parsed project in memory. What gets redone depends on the change:

| Change | Work redone | VS Code `src/` |
|---|---|---|
| Edit a file | re-parse and re-resolve only that file, then rebuild the graph and rerun the rules | ~70 ms |
| Add, remove or rename a file or folder | walk the tree again and re-resolve every import (the new file can change what `./foo` points to); only new or changed files are re-parsed | ~170 ms |
| Change `tsconfig`, `package.json` or `tangle.toml`, or press `r` | full rebuild | ~300 ms |

tangle checks the filesystem itself to decide whether files were added or removed, because watchers (notably macOS FSEvents) often report an ordinary save as a new file. Atomic saves from editors like vim and JetBrains therefore count as plain edits.

## What it understands

- **React**: JSX/TSX, including JSX in plain `.js` files as Create React App, Vite and Babel setups allow. `React.lazy(() => import(...))` shows up as a dynamic import.
- **Vue and Svelte** single-file components. It reads the `<script>` and `<script setup>` blocks, uses the `lang="ts"` or `lang="tsx"` setting, and treats `<script src>` as an import. Template content and `<svelte:head>` browser scripts are ignored.
- **Angular**: `templateUrl`, `styleUrl` and `styleUrls` in decorators become dependencies of type `resource`, so a missing template or stylesheet is reported. Lazy `loadComponent` and `loadChildren` routes are picked up as dynamic imports.
- If a tsconfig can't be loaded (for example, it `extends` a package that isn't installed), tangle falls back to resolving without it, so a single broken tsconfig doesn't make every import unresolvable.
- Every import form: `import`, `import type`, `export … from`, `import()`, `require()`, `import x = require()`, `import("x").T`
- **Aliases** from Vite (`resolve.alias` in object or array form, including RegExp `find` and `/src`-style root-relative replacements), from webpack (`resolve.alias`, including `name$` and `false`, plus `resolve.modules` and `resolve.extensions`), from Babel's `babel-plugin-module-resolver` (`alias`, including `^regex` keys with `\1`, and `root`), or declared directly in `tangle.toml` (see below)
- tsconfig `paths` (per-file discovery or an explicit tsconfig), `package.json` `exports` and `imports`, and `.js`→`.ts` extension aliasing
- Type-only and dynamic imports. By default type-only imports don't count toward cycles, because they're erased at runtime.
- npm dependency classification by walking every enclosing `package.json`, so monorepo roots work: `npm`, `npm-dev`, `npm-peer`, `npm-optional`, `npm-undeclared`. `@types/*` packages count as declared for type-only packages.
- `.gitignore` files are respected.

## Rules

`tangle.toml` (run `tangle init` for a commented starter). Paths are regular expressions matched against root-relative paths. Lookarounds work, and a list of patterns means "any of these".

```toml
allowed_severity = "error"    # for [[allowed]] below; top-level keys go before any table

[options]
exclude = ["**/node_modules/**", "**/dist/**"]   # globs of files to skip
include_only = '^src/'        # regex: keep only these modules (sources and targets)
exclude_path = '^src/generated/'
cycles_ignore_type_only = true

[[forbidden]]
name = "no-circular"
severity = "warn"             # error | warn | info | off
to = { circular = true }

# cycles that pass through a module in shared/ (considers every cycle, not one arbitrary one)
[[forbidden]]
name = "no-cycles-via-shared"
to = { circular = true, via = '^src/shared/' }

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

- **`--mode`:** `tangle check --mode staging` (and every other command) overrides `mode` for one run.
- **`NODE_ENV`:** defaults to `production` for a production mode or a `build`, otherwise `development`, as in Vite. A value in `vars` or your shell wins.
- **webpack:** as with webpack-cli, `env` also gets `WEBPACK_SERVE`, or `WEBPACK_BUILD` and `WEBPACK_BUNDLE`.
- **Babel:** `api.env()` follows `BABEL_ENV`, then `NODE_ENV`. They only run when named here, never by auto-detection. Aliases rewrite the import before resolution, so tsconfig `paths`, package `exports` and the other resolution rules still apply to the result. Editing any of these config files triggers a full rebuild in watch mode.

### Groups, tags and Nx projects

Groups name parts of the codebase: features, layers, packages. Each distinct match of a group's `path` regex (at the start of a module path) is one group instance, and a module belongs to the first group that matches it. Modules inherit their group's tags. `scope = "group"` rules run on the group graph, where dependencies inside one group instance don't count and cycles are computed between groups.

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

With `options.nx_projects = true`, every Nx project (`project.json`, or a `package.json` with an `nx` section) is a group, rediscovered on each run with its `tags`. Each project also gets the tag `projectType:<type>`, and nested projects take precedence over their parents. Tag conditions (`tags`, `tags_not`, and `tags_all` on `from`) accept Nx patterns: exact tags, `*` globs and `/regex/`. On the module scope, `to.cross_group = true` matches dependencies between different groups, and the `relative` dependency type matches imports written as a relative or absolute path.

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
| `to` | `path`, `path_not`, `circular`, `via`, `via_only`, `max_cycle_length`, `tags`, `tags_not`, `cross_group`, `dependency_types`, `dependency_types_not`, `could_not_resolve`, `type_only`, `dynamic`, `reachable`, `more_unstable`, `more_than_one_dependency_type`, `license`, `license_not` |
| `module` | `path`, `path_not`, `number_of_dependents_less_than`, `number_of_dependents_more_than` (with `from` restricting which dependents count) |

Dependency types: `local`, `npm`, `npm-dev`, `npm-peer`, `npm-optional`, `npm-undeclared`, `core`, `unresolvable`, `type-only`, `dynamic`, `require`, `reexport`, `resource`, `import`, `aliased` (a tsconfig-paths, `#imports` or workspace import of a local file), `deprecated` (the installed package is marked deprecated). A package declared in several `package.json` sections has all of the matching types, for example `npm` and `npm-dev`. npm packages can also be matched as `node_modules/<name>/`.

## HTML report

`tangle report` writes one self-contained HTML file with no external requests, so it can be attached to CI runs or shared as a file. It has:

- **Violations**, grouped by rule, with a filter, severity toggles and the cycle behind every circular dependency.
- **Modules**, a sortable table (fan-in, fan-out, instability, cycle, violations). Selecting a module shows its imports, its importers, violations in both directions, and the shortest loop through it.
- **Cycles**, each strongly connected component with its shortest loop.
- **Folder graph**, an interactive map of folders. It picks a readable depth automatically, colours folder cycles red, and lets you zoom, pan and drag. Double-click a folder to open it, and use the breadcrumbs to go back up.

It follows the system light or dark theme. VS Code's 113k dependencies produce a 3.3 MB report.

### Adopting rules in a legacy codebase

```sh
tangle check --write-baseline .tangle-baseline.json   # record today's violations
tangle check --baseline .tangle-baseline.json         # fail only on new ones
```

## Migrating to tangle

```sh
tangle migrate --dry-run   # preview the generated tangle.toml
tangle migrate             # write tangle.toml (+ .tangle-baseline.json)
tangle check               # same checks as before, much faster
```

`tangle migrate` looks at the project root, converts every dependency-rule setup it finds into one `tangle.toml`, and tells you what to change:

| Source | What's converted |
|---|---|
| **JavaScript rules configs** (`.js`, `.cjs`, `.mjs` or `.json` files declaring `forbidden`, `allowed` or `required` rules). These are recognised by content, not file name. | Every rule, `extends` presets, options, and the known-violations file they point at |
| **ESLint** (`eslint.config.*`, `.eslintrc.{js,cjs,json,yaml,yml}`, `package.json` `eslintConfig`) | `import/no-cycle` (with `maxDepth`), `import/no-restricted-paths` (zones, `except`, `basePath`, messages) and `import/no-extraneous-dependencies` (dev/optional/peer globs), also under `import-x`. `files`, `ignores` and `overrides` scoping carry over (an `"off"` for test files becomes a `path_not`), and so do the `import/resolver` webpack and TypeScript settings. Legacy `extends` chains are followed: relative files, shareable `eslint-config-*` packages and `plugin:…` configs. As in ESLint, a later severity-only setting keeps the rule's earlier options. |
| **Nx** (`@nx/enforce-module-boundaries` in an ESLint config) | Project discovery (`nx_projects`), every depConstraint (`sourceTag` or `allSourceTags`, `onlyDependOnLibsWithTags`, `notDependOnLibsWithTags`, `bannedExternalImports`, `allowedExternalImports`), and Nx's built-in checks: project cycles, importing applications, relative imports across projects, and "a project without tags matching a constraint can't depend on libraries" |
| **eslint-plugin-boundaries** (`boundaries/dependencies` or `boundaries/element-types`) | `boundaries/elements` become `[[groups]]` (folder, file and full modes, `basePattern`). Policies, in both the v6+ `{ to: { element: { type } } }` format and the legacy format, including `types.anyOf` and `!type`, are replayed with the plugin's last-match-wins semantics. Capture conditions are skipped with a warning. |
| **madge** (`.madgerc`, `package.json` `madge`, or `madge --circular` in a script) | A circular-dependency rule, `excludeRegExp`, `tsConfig`, `webpackConfig`, `skipTypeImports` |
| **Known-violation files** (JSON arrays of `{ from, to, rule: { name } }`) | `.tangle-baseline.json`, applied automatically through `options.baseline` |

Rules found in several places (for example a madge cycle check and a cycle rule) are merged, keeping the stricter severity. Anything that can't be converted exactly is listed at the top of `tangle.toml` for review instead of being silently loosened. `package.json` scripts that ran the old tools get a suggested `tangle check` replacement.

**Switching CI without surprises:** tangle finds cycles that other tools miss, so the first run may report more than before. `tangle check --write-baseline` records today's findings. From then on, `tangle check` fails only on new violations.

You can also run a JavaScript rules config directly without converting it: `tangle check -c rules.config.js`.

### Verified against the tools themselves

- **ESLint 9 + eslint-plugin-import 2.32**, on a project using all three rules with zones, `except`, file-scoped overrides and dev-dependency globs: tangle's migrated config reports **exactly the same 7 violations**, rule for rule, file for file, import for import.
- **ESLint 9 legacy configs with `extends`**: a zone from a relative config, `no-cycle` with `maxDepth: 1` from a shareable config (kept when the root sets `"warn"`), and a directory the shared config turned off but the root re-enabled. Tangle reports **exactly the same 7 findings**. `maxDepth` 1, 2 and 3 on a project with cycles of 2, 3 and 4 modules match the plugin exactly.
- **Nx 21** (`@nx/enforce-module-boundaries`), on a workspace with scope and type tags, an application, an untagged lib, a banned external, a relative cross-project import and project cycles: tangle flags **exactly the same 10 imports**. Adding a new tagged project afterwards is enforced without migrating again, because projects are rediscovered on every run.
- **eslint-plugin-boundaries 7.2**, using both the legacy `rules` and v6+ `policies` formats, with a later `disallow` overriding an `allow` and a negated `!app` selector: **exactly the same 4 violations**.
- **madge 8 on excalidraw** (873 modules): madge's own dependency graph puts 168 files on cycles. `madge --circular` lists 128 of them; tangle reports all 168, with no extras, in 0.03 s against madge's 2.5 s.
- **A JavaScript rules config with known violations:** all 14 known violations were carried into the baseline. The only findings left were genuine cycle dependencies the original setup never reported, each shown with its cycle.

## Cycle detection

On VS Code, checked against an independently computed ground truth, tangle finds all 1,945 dependencies that sit on a cycle, with no false positives. Each one is reported with a concrete cycle as evidence. `via` and `viaOnly` consider every simple cycle through a dependency, not just one arbitrary cycle.
