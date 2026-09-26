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

| | conditions |
|---|---|
| `from` | `path`, `path_not`, `orphan` |
| `to` | `path`, `path_not`, `circular`, `via`, `via_only`, `dependency_types`, `dependency_types_not`, `could_not_resolve`, `type_only`, `dynamic`, `reachable`, `more_unstable`, `more_than_one_dependency_type`, `license`, `license_not` |
| `module` | `path`, `path_not`, `number_of_dependents_less_than`, `number_of_dependents_more_than` (with `from` restricting which dependents count) |

Dependency types: `local`, `npm`, `npm-dev`, `npm-peer`, `npm-optional`, `npm-undeclared`, `core`, `unresolvable`, `type-only`, `dynamic`, `require`, `reexport`, `resource`, `import`, `aliased` (a tsconfig-paths, `#imports` or workspace import of a local file), `deprecated` (the installed package is marked deprecated). A package declared in several `package.json` sections has all of the matching types, for example `npm` and `npm-dev`. npm packages can also be matched as `node_modules/<name>/`.

### Adopting rules in a legacy codebase

```sh
tangle check --write-baseline .tangle-baseline.json   # record today's violations
tangle check --baseline .tangle-baseline.json         # fail only on new ones
```

## Migrating an existing JavaScript rules config

If your rules live in a JavaScript config (a `.js`, `.cjs`, `.mjs` or `.json` file exporting `forbidden`, `allowed` and `required` rules plus `options`), tangle can run it directly or convert it. JavaScript configs are evaluated with Node, so `extends` chains that point at preset packages work too. Plain JSON configs don't need Node.

```sh
tangle check -c rules.config.js                   # run it as is
tangle init --from rules.config.js                # convert it to tangle.toml
```

Rule names, severities, regexes (including `$1` groups and lookarounds), `allowed`, `allowedSeverity`, `required`, module rules, `via`/`viaOnly`, licenses, dependency types and the `exclude`/`includeOnly`/`tsConfig`/`tsPreCompilationDeps` options all carry over. A rule that uses something tangle can't honour exactly (for example `scope: "folder"` or the `npm-bundled` type) is skipped with a warning rather than silently loosened. A skipped `allowed` rule gets a louder warning, because dropping it adds violations.

## Cycle detection

On VS Code, checked against an independently computed ground truth, tangle finds all 1,945 dependencies that sit on a cycle, with no false positives. Each one is reported with a concrete cycle as evidence. `via` and `viaOnly` consider every simple cycle through a dependency, not just one arbitrary cycle.
