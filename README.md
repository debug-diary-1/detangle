# tangle

Fast dependency analysis and architecture rules for JavaScript/TypeScript, Vue, Svelte and Angular, with a live-reloading terminal explorer. It does the same job as the JavaScript rules tool, written in Rust on top of the [oxc](https://oxc.rs) parser and resolver.

| VS Code `src/` (9.6k files, 113k deps) | time |
|---|---|
| the JavaScript rules tool 18.4 | 40.3 s |
| **tangle** | **0.29 s** |

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

## What it understands

- **Vue and Svelte** single-file components. It reads the `<script>` and `<script setup>` blocks, uses the `lang="ts"` or `lang="tsx"` setting, and treats `<script src>` as an import. Template content and `<svelte:head>` browser scripts are ignored.
- **Angular**: `templateUrl`, `styleUrl` and `styleUrls` in decorators become dependencies of type `resource`, so a missing template or stylesheet is reported. Lazy `loadComponent` and `loadChildren` routes are picked up as dynamic imports.
- If a tsconfig can't be loaded (for example, it `extends` a package that isn't installed), tangle falls back to resolving without it, so a single broken tsconfig doesn't make every import unresolvable.
- Every import form: `import`, `import type`, `export … from`, `import()`, `require()`, `import x = require()`, `import("x").T`
- tsconfig `paths` (per-file discovery or an explicit tsconfig), `package.json` `exports` and `imports`, and `.js`→`.ts` extension aliasing
- Type-only and dynamic imports. By default type-only imports don't count toward cycles, because they're erased at runtime.
- npm dependency classification by walking every enclosing `package.json`, so monorepo roots work: `npm`, `npm-dev`, `npm-peer`, `npm-optional`, `npm-undeclared`. `@types/*` packages count as declared for type-only packages.
- `.gitignore` files are respected.

## Rules

`tangle.toml` (run `tangle init` for a commented starter):

```toml
[options]
exclude = ["**/node_modules/**", "**/dist/**"]
cycles_ignore_type_only = true

[[forbidden]]
name = "no-circular"
severity = "warn"            # error | warn | info | off
to = { circular = true }

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
```

`from`: `path`, `path_not`, `orphan`
`to`: `path`, `path_not`, `circular`, `dependency_types`, `dependency_types_not`, `could_not_resolve`, `type_only`, `dynamic`, `reachable`, `more_unstable`

Dependency types: `local`, `npm`, `npm-dev`, `npm-peer`, `npm-optional`, `npm-undeclared`, `core`, `unresolvable`, `type-only`, `dynamic`, `require`, `reexport`, `resource`.

### Adopting rules in a legacy codebase

```sh
tangle check --write-baseline .tangle-baseline.json   # record today's violations
tangle check --baseline .tangle-baseline.json         # fail only on new ones
```
