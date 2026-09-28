# Feature: ESLint rules backed by a native Node.js add-on

Status: **final design, revision 6**, awaiting human review. No code until sign-off.
- Five cold reads (three isolated readers each) shaped revisions 2–6. Their findings, and how each was resolved, are in [`node-addon-reviews.md`](node-addon-reviews.md).
- The maintainer decided that revision 6 is the last. Remaining questions are settled at review or implementation, not by further revisions.
- Owner and sign-off: the maintainer.

## Problem

detangle's rules run today only when someone runs `detangle check`, `detangle watch` or the explorer. So a developer sees a forbidden import or a new cycle at CI time, or not at all, and not while writing the import.

Editors already surface ESLint results inline as you type. Putting detangle's violations there needs a way to answer "what's wrong with this file?" synchronously, in milliseconds, inside a process that lives for hours. Running the CLI once per file can't do that on large repos (see Context).

This is a maintainer-driven bet, not a response to user requests. That's why it ships as **experimental**.

## Background

**detangle** is a Rust CLI, also shipped on npm, that scans a JavaScript/TypeScript project, builds its import graph and checks architecture rules against it. The rules come from `detangle.toml`, or a JavaScript rules config it imports. Its existing Node.js API (`npm/index.js`) runs the CLI binary as a child process and parses its JSON output.

### Glossary

| Term | Meaning |
| ---- | ------- |
| module | A node of the graph. **Local** files are identified by their path relative to the root, with `/` separators (`src/a/b.ts`). **npm** modules by package name (`lodash`, `@scope/name`). **Built-ins** by bare name (`fs`, `path`), except those that only exist with the prefix (`node:sqlite`, `node:test`). **Unresolved** imports by the specifier itself. |
| source file | A file with one of the extensions `ts tsx mts cts js jsx mjs cjs vue svelte` (`SOURCE_EXTS` in `src/scan.rs`). Vue and Svelte files go through `src/sfc.rs` to find their script blocks. |
| rule | An entry in the config, with a name, a severity (`error`, `warn`, `info`, `off`) and an optional `comment`. Kinds: `[[forbidden]]` (a matching import is a violation); `allowed` (every import must match some `allowed` entry, or it's reported as `not-in-allowed`); `[[required]]` (matching modules must import something); plus module-only conditions inside `forbidden`: `orphan` (no imports in or out), `reachable` (reachable from given entry points, or not), and dependents count (`module = { … }` with more or fewer than N importers). `off` rules are never evaluated. |
| group | A named set of modules from `[[groups]]` in the config, or an Nx project discovered from a `project.json`. Groups form their own graph. |
| violation | One rule match. Its `scope` is `module`, `folder` (the graph of directories) or `group`. It has a `from` node, an optional `to` node (none for rules about a node alone), a `cycle` (for circular rules) and, at group scope only, `imports`: the module-level imports behind it. `detangle check -f json` prints each import as `{ from, specifier, to }`, where `from`/`to` are module ids and `specifier` is the import string. |
| circular violation | Reported **per import edge**: every edge that lies on a cycle and matches the rule yields its own violation, with `from` = that edge's importing file. |
| `dir` and root | `detangle check <dir>` scans `dir`. The **root** is `find_root(dir)`: the nearest ancestor of `dir` (itself included) with `detangle.toml`, else the nearest with `package.json`, else `dir`. Module ids are relative to the root, and the watcher watches the root. `Project::open` canonicalizes `dir` with `dunce::canonicalize` before any of this. |
| `extract` (`src/scan.rs`) | Parses one file's source (with oxc) and returns its import strings: the **cooked** string values, the same as ESTree's `Literal.value`, including any `?query` suffix. The forms it recognizes: `import`/`export … from`, `import()`, `require()`, `import x = require()`, TypeScript `import("…")` types, AMD `define([...])`/`require([...], f)`, the configured exotic requires, `process.getBuiltinModule()` (when `builtin_module_calls` is on), Angular `templateUrl`/`styleUrl`/`styleUrls` in decorators (a bare value gets `./` prepended), `/// <reference path|types>` and `/// <amd-dependency>` directives, and JSDoc `@import`/`import("…")` (when `jsdoc_imports` is on). |
| `Session` (`src/scan.rs`) | The scanned files: each file's extracted import strings (`raw`), their resolved targets, the parse-error count, and a `stamp` (mtime + size). `Session::update(paths)` re-reads changed files. It decides what changed **by checking the filesystem, not the event kind** (watchers report plain saves as creates): a path counts as added only if it's a source file that isn't a member yet, and as removed only if a member no longer exists. So an editor's atomic save (write a temp file, rename it over the original) is a plain modification (2–5 ms on VS Code). A real add, remove or rename is a **structural** change: `update` re-walks `dir`, builds a fresh resolver and re-resolves every file (121–140 ms on VS Code). |
| member directory | A directory under the root that contains a Session member, or is an ancestor of one. Gitignored output such as `.next/` or `dist/` has no members, so it isn't one. |
| `Project` (`src/main.rs`) | A loaded config plus a `Session`. `Project::analyze()` builds the `Graph` and evaluates the rules into an `Analysis` (graph + violations). |
| parse cache | The optional on-disk cache of each file's extracted imports, enabled by `cache` in the config or `--cache` on the CLI. Only `Session::new` (a full open) reads and writes it; incremental updates never touch it. It's written to a temp file and renamed into place. |
| exotic require | `options.exotic_require` in the config: extra callee names treated like `require`, possibly dotted (for example `module.require`). |
| watcher (`src/watch.rs`) | The `notify`-based recursive file watcher used by `detangle watch` and the explorer (the interactive terminal UI). It **ignores events** under `node_modules`, `.git` and `target`, but the OS still watches those directories (on Linux, one inotify watch per directory). It waits for 150 ms of quiet before releasing a batch. It flags a change as a **config change** when the file name is `detangle.toml`, `package.json`, `tsconfig*.json`, `webpack.config.*`, `vite.config.*`, `babel.config.*`, `.babelrc*`, `.env*` or `project.json`. |
| `--mode` | The existing CLI flag passed to Vite/webpack config evaluation, so aliases defined per mode resolve correctly. |
| `DETANGLE_TIMINGS` | An environment variable that makes the CLI print per-phase times: `config`, `scan`, `groups`, `graph`, `rules`. |
| `one_shot` (`src/main.rs`) | Deliberately leaks a one-shot command's `Project` and `Analysis`, because the OS reclaims them faster at exit than freeing them does (~20 ms for VS Code's graph, measured when it was introduced). |
| platform packages | The 8 npm packages `detangle-{darwin-arm64, darwin-x64, linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, linux-arm64-musl, windows-x64, windows-arm64}`. Each holds the binary for one target in `bin/` (today 3.5–3.8 MB compressed, 8–9.6 MB unpacked), has no `exports` field, and publishes only `bin/` and licenses. They're listed in `npm/platforms.json` and generated by `scripts/npm-packages.mjs`; `npm/binary.js`'s `platform()` picks the one for this machine. Each is published through npm trusted publishing, which is configured per package on npmjs.com and takes a security-key confirmation to set up. |
| `--concurrency N` | ESLint's option (ESLint 9.34+) to lint in N `worker_threads` within one process. |
| baseline `22899e5` | The commit of release 0.1.3, the last release before this work. |
| fixtures | `tests/fixtures/*`: small projects used by the Rust and npm tests. `conditions` exercises ~20 rule conditions (it produces 12 plain and 7 circular module-scope violations). |
| verify against the real tool | The project's standing rule: correctness is checked by comparing with the real CLI's output, not only with unit tests. |

## Context: what the Node API costs today

Measured 2026-09-27 on the maintainer's Mac (Apple Silicon). Numbers are medians of 7 runs from `target/release/detangle` at `22899e5`, driven from Node 24.

| Project | `detangle check -f json` (spawn from Node) | `check()` API | `graph -f json --externals` | `JSON.parse` of that | `analyze()` API | JSON size |
| ------- | ------ | ----- | ----- | ---- | ----- | ------- |
| VS Code `src/` (9.6k files, 2.7k directories) | 197 ms | 199 ms | 240 ms | 60 ms | 302 ms | 31.4 MB |
| excalidraw | 24 ms | 23 ms | 25 ms | 3 ms | 27 ms | 1.7 MB |

Other measurements on VS Code `src/`:
- Spawning `detangle --version` from Node: 3.7 ms (median of 21).
- Phase times (`DETANGLE_TIMINGS=1`, `graph -f json`): scan 170 ms, graph 15 ms, rules 5 ms, total 219 ms. The scan runs on all cores (the `ignore` crate's parallel walker plus rayon).
- Peak memory for `detangle check`: 172 MB RSS.
- `detangle watch` rebuilds, 8–10 runs each:

  | Edit | Rebuild |
  | ---- | ------- |
  | a saved edit that changes imports | 57–61 ms |
  | a saved edit that doesn't | 2–5 ms |
  | a file added or removed (structural) | 121–140 ms |

- A one-file project holding VS Code's largest file (1.4 MB), or a median one (5.9 KB): scan ~3 ms either way. That's resolver setup, so **the per-file parse of a median file isn't measured yet**; the benchmark measures it.
- Walking `src/` (`rg --files`): ~30 ms.
- **Recursive watcher setup** (`notify` 8, which detangle uses), 5 runs each:

  | Platform | Tree | Directories | Setup |
  | -------- | ---- | ----------- | ----- |
  | macOS (FSEvents) | VS Code `src/` | 2,713 | ~1.4 ms |
  | Linux 6.12 (inotify, **Docker VM** on the same Mac) | VS Code `src/` | 2,713 | ~46 ms |
  | Linux (VM) | the same `src/` plus a typical `node_modules` (react, next, vite, vitest, typescript, eslint, jest, storybook, webpack, babel, prettier) | 6,370 | ~96 ms |
  | Linux (VM) | one non-recursive watch per directory, skipping `node_modules`/`.git`, even with `notify`'s batch API | 2,694 | ~125 ms |

  - Linux costs ~15–17 µs per directory, and `node_modules` can't be excluded from a recursive watch. Filtering is slower than watching everything.
  - Native Linux and Windows aren't measured yet; CI logs them (Risks).

What these numbers mean:
- **Starting a process costs ~4 ms.** The only other overhead in the Node API is JSON, and only `analyze()` on huge repos pays it.
- **The 170 ms scan repeated on every call is the real cost.** Only a Session that stays alive avoids it.
- **A watcher is free on macOS and costs tens of ms on Linux.** Off ESLint's thread, that's acceptable (D7).

**Why ESLint is the consumer.** ESLint rules are **synchronous** (`create(context)` and its visitors can't await), so an async API can't serve them. In an editor, the ESLint server lives for hours and lints the open file after each edit. That makes a warm Session the right tool, where `spawnSync` would cost ~200 ms per file.

**Positioning (editor first).** The ESLint rules are for **editor feedback**. `detangle check` stays the authoritative gate for CI and pre-commit. The docs say this plainly, because of `eslint --cache` (see D10).

## Scope

- **In:**
  - A napi-rs (v3) add-on, `detangle.node`, targeting Node-API 8 (contract in D3).
  - Two ESLint rules, `detangle/errors` and `detangle/warnings`, in the existing `detangle` npm package at `detangle/eslint`, plus `configs.recommended`.
    - They support ESLint 9 and 10 flat config.
    - The Node versions ESLint supports apply to the rules: ESLint 10 needs ^20.19, ^22.13 or ≥ 24. The package's `engines` (Node ≥ 18) stays for the CLI and the existing API.
  - **One Project per `(dir, config, mode)` per ESLint thread**, where `dir` is the `dir` option or ESLint's `cwd` (D8). No automatic per-file project discovery.
  - Freshness:
    - an overlay of the linted file's buffer that survives incremental updates (D6);
    - polling of the files that decide configuration and resolution (D7);
    - a watcher started at open on a background thread (D7).
  - Shipping `detangle.node` inside the 8 existing platform packages.
  - `scripts/cmp-output.sh` and `scripts/bench-eslint.mjs`, committed so the checks can be reproduced.
  - Two CLI fixes that come with the watcher and Session work:
    - `detangle watch` and the explorer stop ignoring `notify`'s lost-event signal;
    - the parse cache's temp file gets a per-thread name.
  - Release **0.2.0**, with the ESLint integration marked **experimental**.
- **Out:**
  - Changing the existing `analyze`/`check`/`report`/`graph`/`migrate` Node API, or any CLI output format.
  - Discovering a project per linted file (walking up from each file to its own `detangle.toml`/`package.json`). That was tried in revisions 3–4 and produced most of Cold Read 4's lifecycle holes.
  - Unsaved changes in files other than the one being linted, beyond the overlays that earlier lints of those files left in place.
  - Keeping overlays across a Project **re-open** (a config or lockfile change). Each buffer's overlay returns at its next lint.
  - Files that don't exist on disk yet: no reports.
  - Re-linting other open files when the graph changes. They update the next time the editor lints them.
  - Folder- and group-scope violations **without an import** (dependents count, orphan or reachable at group scope). They have no file to show in; `detangle check` lists them.
  - Tracking files that a JavaScript rules config imports.
  - Changes inside `node_modules` that don't touch a lockfile (`npm link`, hand edits).
  - Excluding `node_modules` from the OS-level watch on Linux (measured: filtering costs more than it saves).
  - A fallback when the add-on can't load (D11).
  - Autofixes and suggestions.
  - Writing detangle rules inside the ESLint config instead of `detangle.toml` or a rules config file.
  - Legacy `.eslintrc`, ESLint < 9, and typescript-eslint < 8.
  - Any new npm package.
  - Bundler plugins, an LSP, and a `detangle serve` daemon.
  - Sharing one Session across ESLint worker threads.

## Success criteria

"Lint" means **one ESLint pass over one file with both rules enabled**, which is one add-on call (D5). Targets are for VS Code `src/` on the maintainer's Mac, measured by `scripts/bench-eslint.mjs <corpus>`. The benchmark drives ESLint's `Linter` API with and without the rules on one thread, with the watcher running, and reports the difference.

| Case | Target | Basis |
| ---- | ------ | ----- |
| Lint, file unchanged, median-size file | ≤ 0.5 ms added | estimate: parse of ~6 KB, ~10 stats (D7 polling), the lookup and node matching. To be measured |
| Lint, file unchanged, 1.4 MB file | ≤ 5 ms added | parse ≤ 3 ms, one parse per lint |
| Lint after an **unsaved** import edit in the linted file | ≤ 30 ms added | one parse, re-resolving one file, graph 15 ms, rules 5 ms. The placement index is rebuilt lazily in O(number of violations) |
| Lint after a **saved** import change in another file (arrives via the watcher) | ≤ 65 ms added | `watch` measures 57–61 ms. D8's off-thread free should lower it, but that saving is unmeasured and not counted |
| First lint of a **new** file, or after any file is added or removed | ≤ 150 ms added | one structural update: 121–140 ms in `watch` |
| First lint of an **excluded** file (generated, gitignored, outside `dir`) | ≤ 1 ms added | an eligibility check against the walk's rules, no walk (D6) |
| Lint after a dependency install (lockfile changed) | ≤ 150 ms added | a resolution refresh: a fresh resolver and every file re-resolved, no walk and no re-parse. Bounded by the structural update above |
| First lint in a process (cold scan) | ≤ 250 ms | CLI total 219 ms. The watcher is set up in parallel and isn't on this path |
| Lint after a config change (TOML config) | ≤ 250 ms | a full re-open. JS configs and Vite/webpack evaluation add their `node` spawn, shown as `DETANGLE_TIMINGS`'s `config` phase |
| Watcher setup, Linux (background) | ≤ 100 ms for VS Code `src/` | ~46 ms in a VM; re-measured on native Linux (Risks) |
| Memory per Project, steady state | ≤ 1.2 × the CLI's peak RSS on the same project | CLI: 172 MB on VS Code. During a re-open the old Session stays alive until the new one is ready, so up to ~2.4× briefly |

A missed target **blocks the 0.2.0 release**, unless the maintainer accepts that specific miss in the release commit message with the measured number. The measured numbers go in the implementation commit message either way.

Correctness criterion: the drift test (Risks) passes, comparing **locations**, not only messages.

## Architecture: three layers

The word "Project" is used for one thing per layer:

| Layer | Name in this doc | Holds | Called by |
| ----- | ---------------- | --------- | --------- |
| Rust library (`src/project.rs`) | **Project** | config, `Session`, `Analysis`; `open`, `analyze`, `violations_for(path)`, `config_stamps()` | the CLI, and the handle |
| napi add-on (`napi/src/lib.rs`) | **handle** (exported to JavaScript as the class `Project`) | a state (D8) that may own a Project, the watcher, the polled stamps, the dropper thread | `native.js` |
| JavaScript (`npm/native.js`) | **cache** | a `Map` from key to handle, the per-pass results (D5), warnings already emitted | the two ESLint rules |

On each lint, the handle runs the D6 steps. It calls `Session::overlay` with the buffer text and `Project::violations_for(path)` for placement. Then it adds `problems` and `exoticRequire` to form the D3 result.

## Files & touch points

- `Cargo.toml`:
  - Add a `[lib]` target (`src/lib.rs`), and turn the root into a workspace with the member `napi/`.
  - `include` still covers `src/**`. `cargo package` drops the `[workspace]` table (verified with a probe crate), so `cargo install detangle` builds as before. CI runs `cargo package` and builds the resulting `.crate` alone.
  - Add `json-strip-comments` (already in the dependency tree through `oxc_resolver`, so nothing new to download) to read tsconfig `extends` chains (D7).
  - Keep `panic = "unwind"` (the default) in every profile (D8).
- `src/lib.rs` (**new**): declares the modules `main.rs` declares today and re-exports `Project`, `Analysis` and `CacheArgs`. The crate docs say "internal API, no semver guarantees".
- `src/project.rs` (**new**):
  - `Project` and `Analysis`, moved from `main.rs`.
  - Two logic changes, neither visible to CLI users:
    - `Project::open` takes the CLI's `--cache`/`--cache-strategy` as an explicit `CacheArgs` parameter. Today it reads them from a global (`CACHE_ARGS`, read nowhere else). The CLI passes its flags; the add-on passes none, so only the config's `cache` setting applies.
    - `Project::open` takes an optional `node` executable path for config evaluation (D8), stored in the loaded options. The CLI passes none and keeps running `node` from `PATH`.
  - `Project::violations_for(path) -> FileReport`, where `FileReport = { violations: Vec<FileViolation> }` and `FileViolation = { rule, severity, message, specifiers }` (D3). It implements D9 through a per-`Analysis` index built lazily on first use:
    - module-scope violations by `from` module;
    - group-scope violations by each `imports` entry's `from` module;
    - folder-scope violations by folder id. The lookup walks the linted file's directory and its ancestors.
  - `Project::config_stamps() -> Stamps`: the polled set defined in D7.
- `src/main.rs`: uses the lib, and passes `CacheArgs` from its flags. `one_shot` stays here.
- `src/aliases.rs`, `src/migrate/rules_js.rs`: spawn the configured `node` path when set, else `node`.
- `src/scan.rs`:
  - `Session::overlay(path, source) -> bool` (D6). An overlaid file is marked **overlaid**. `update` and structural updates keep an overlaid file's `raw` and never re-read it from disk, unless that file's own path is among the changed paths.
  - `Session::contains(path)`.
  - `Session::eligible(path) -> bool`: whether the walk would include this path. It applies the same rules the walk applies (the config's include/exclude globs, then the `.gitignore`/`.ignore` files between the root and the path), without walking.
  - `Session::admit(path)`: the structural update for one eligible path (D6).
  - `Session::refresh_resolution()`: a fresh resolver and every file re-resolved from its stored `raw`, with no walk and no re-parse (D7).
  - `Session::rescan()`: a forced structural update.
  - `Session::is_member_dir(path)`.
  - The parse cache's temp file is named `{FILE}.{pid}.{thread id}` instead of `{FILE}.{pid}`, so `--concurrency` workers in one process can't collide.
- `src/watch.rs`:
  - `Changes` gains `rescan: bool`, set when `notify` reports lost events (`need_rescan()`: macOS "must scan subdirs", inotify queue overflow). The callback checks `need_rescan()` **before** its event-kind filter. Today that filter discards these events (their kind is `Other`), so `detangle watch` silently ignores lost events.
  - `detangle watch` and the explorer gain the fix too: `rescan` triggers a full rebuild (`Changes::full`). That changes behavior, not output format, and a `watch.rs` unit test covers it.
  - `Watcher::drain() -> Result<Changes, Disconnected>` returns every pending change with no quiet period. `Disconnected` (a unit error) means the watcher's thread died. Each `Watcher` has one consumer: the CLI uses `wait`/`poll` on its own instance, and the handle uses `drain` on its own.
- `napi/Cargo.toml`, `napi/src/lib.rs` (**new**): the `detangle-napi` crate (`publish = false`, `crate-type = ["cdylib"]`, `napi` 3 / `napi-derive` 3). It contains:
  - The exports from D3, each wrapped in `std::panic::catch_unwind` (D8). `#[napi(catch_unwind)]` is a backstop.
  - The handle: its state machine, background watcher setup, polling, dropper thread and env cleanup hook (D7, D8).
  - A `test-hooks` cargo feature adding hidden exports: `__panicForTest()`, and a switch that disables the watcher.
- `npm/native.js` (**new**):
  - Loads `detangle-<platform>/detangle.node` via `binary.js`'s `platform()`.
  - Holds the cache (D8), shares one result per `(sourceCode, key)` (D5), and emits process warnings (D12).
- `npm/eslint.js`, `npm/eslint.mjs`, `npm/eslint.d.ts` (**new**):
  - The plugin `{ meta, rules: { errors, warnings }, configs: { recommended } }`.
  - Each rule's `meta.schema` is `[{ type: "object", properties: { dir: { type: "string" }, config: { type: "string" }, mode: { type: "string" } }, additionalProperties: false }]`.
  - The rules match specifiers to AST nodes (D9).
  - `configs.recommended` is `{ name: "detangle/recommended", plugins: { detangle }, rules: { "detangle/errors": "error", "detangle/warnings": "warn" } }`, with no `files`. It applies to whatever files the user's config already lints; TypeScript, Vue and Svelte need their usual `files`/parser entries.
- `npm/package.json`:
  - `exports["./eslint"]` and the new files in `files`.
  - `peerDependencies.eslint: ">=9"`, optional via `peerDependenciesMeta`.
  - `devDependencies`: `eslint@^10`, `eslint9: "npm:eslint@^9"` (an npm alias, so both versions install side by side), `typescript-eslint@^8`.
  - Version 0.2.0.
- `npm/eslint.test.mjs` (**new**):
  - RuleTester cases for every node kind in D9, including dotted exotic requires, AMD arrays and Angular resources.
  - The drift test (Risks).
  - The shared-`SourceCode` assumption, on ESLint 9 and 10.
  - The worker teardown test.
  - Panic tests (`test-hooks`): recovery after a source fix, and at most one retry per 30 s.
  - No-watcher tests (`test-hooks`):
    - a config edit, a lockfile change and a `Broken` → `Ready` recovery are all picked up by polling;
    - a failed re-open doesn't retry on the next lint.
  - A build-output test: writing `.next/package.json` causes no re-open.
  - A new-file test: a file created and linted before its watcher event arrives is admitted.
  - An excluded-file test: linting a gitignored file costs no walk.
  - An overlay-survival test: an unsaved buffer's overlay survives another file being added.
  - An idle-eviction test.
  - A lost-events test.
  - A Windows-only case test (a path differing only in case).
  - It logs watcher setup time per OS (read by Risks).
- `tests/fixtures/eslint/` (**new**):
  - module, circular, folder, group, orphan and `required` rules;
  - an exotic require (dotted), an AMD `define`, and an Angular `templateUrl`;
  - duplicate specifiers.
- `tests/fixtures/eslint-monorepo/` (**new**): workspace packages with their own `package.json`, a root `detangle.toml`, a `package-lock.json`, and a cross-package cycle.
- `scripts/cmp-output.sh` (**new**): `cmp-output.sh <base-bin> <new-bin> <project>…`.
  - It runs `check`, `check -f json`, `graph -f json` and `graph -f dot` with both binaries.
  - It strips the only nondeterministic output: JSON `timings`/`*_ms` fields, and `· <n>ms` in text output. It then asserts byte-identical output.
  - It replaces the maintainer's uncommitted script with the same checks.
- `scripts/bench-eslint.mjs` (**new**): the Success-criteria benchmark. It takes a corpus path and isn't run in CI.
- `scripts/npm-packages.mjs`: copies `detangle.node` into each platform package and adds it to that package's `files`.
- `.github/workflows/release.yml`:
  - Builds `detangle-napi` per target, on the same native runner as the target's binary.
  - On musl, `RUSTFLAGS=-C target-feature=-crt-static` is set **only on the add-on build step**, so the CLI binary stays static. The step checks this with `file bin/detangle`, which must say "statically linked".
  - Smoke test before upload: `node -e "require('./detangle.node')"`. On musl targets it runs inside a `node:22-alpine` container, because a glibc `node` can't load a musl add-on. x86_64 macOS isn't smoke-tested, the same as its binary today (it builds on an arm64 runner).
- `.github/workflows/ci.yml`:
  - Builds the add-on and runs `npm/eslint.test.mjs` on Linux, macOS and Windows with Node 22, against ESLint 10, and on Linux also against `eslint9`.
  - Runs `cargo package` plus a standalone build.
- `docs/reference.md`, `README.md`, `site/index.html`: an ESLint section with:
  - the experimental label;
  - the editor-first positioning;
  - the `--cache` caveat;
  - a flat-config example;
  - the `dir` option for projects that don't sit at ESLint's working directory.

## Decisions

### D1: Transport is a napi-rs add-on

- **Chosen:** a native add-on, called synchronously on ESLint's thread.
- **Rejected:**
  - `detangle serve` over stdio: async only, so it would need an `Atomics.wait` worker hack.
  - `spawnSync` per file: ~200 ms per file on VS Code.
  - `spawnSync` once per process: stale in editors forever.
- **Cost we're accepting:** 8 more native artifacts. An abort or stack overflow in Rust kills the ESLint process. Loader failures become a support surface.

### D2: The same crate gains a `[lib]`, and the add-on is an unpublished workspace member

- **Chosen:** `detangle` becomes lib + bin. `napi/` depends on it by path.
- **Rejected:**
  - A separate `detangle-core` crate on crates.io: one more crate to publish.
  - A cargo feature in the main crate: it would mix cdylib concerns into the CLI crate.
- **Cost we're accepting:** crates.io shows a lib API with no stability promise.

### D3: A narrow contract. Rust decides placement; JavaScript only matches nodes

- **Chosen:** the add-on exports:
  - `canonical(path: string) → string`: `dunce::canonicalize`, or the path unchanged if it doesn't exist. `native.js` uses it for every key and file path, so JavaScript and Rust agree on one canonical form.
  - `open(dir: string, options: { config?: string; mode?: string; node?: string }) → Project` (a handle).
    - This is `detangle check <dir> [--config …] [--mode …]` kept alive. Other CLI flags have no equivalent; everything else comes from the config file, the parse cache (`cache`) included.
    - `node` is the executable used for JS config and Vite/webpack evaluation. `native.js` always passes `process.execPath`, the Node that's already running ESLint.
    - Paths are absolute and canonical, resolved by `native.js` (D8).
  - `Project.violationsFor(file: string, text: string) → { violations, problems, exoticRequire }`:
    - `violations: { rule, severity: "error" | "warn" | "info", message, specifiers: string[] }[]`.
      - `specifiers` are the import strings **in this file** where the violation should be shown. An empty list means line 1.
      - `message` is defined below.
    - `problems: string[]`: persistent state problems (D12).
    - `exoticRequire: string[]`: the config's exotic-require names, for node matching (D9).
  - `Project.close()`: stops the watcher and frees the Session. The handle then stays `Closed`: every later call returns only the problem `detangle project closed`. Called by idle eviction (D8) and by tests.
  - **Message format**, built in Rust from the violation's JSON fields, as `detangle check -f json` prints them:
    - `` `${rule}: ${from}` `` then `` ` → ${to}` `` if there's a `to`;
    - then `` ` (cycle: ${cycle.join(" → ")})` `` if the cycle has more than one entry;
    - then `` ` — ${comment}` `` if there's a comment.
    - For a specifier that matches no AST node (D9), `` ` [import "${specifier}"]` `` is appended in JavaScript.
    - Example: `no-cycles: src/a.ts → src/b.ts (cycle: src/a.ts → src/b.ts → src/a.ts) — Break the cycle`.
- **Rejected:**
  - Returning `to`, `cycle` and `imports` and placing in JavaScript: it spreads the placement rules over two languages.
  - Returning the whole `Analysis`: a different feature.
  - Canonicalizing separately in JavaScript (`realpathSync.native`) and Rust (`dunce`): the two could disagree on Windows drive-letter case and short names.
- **Cost we're accepting:** the add-on isn't a general Node API.

### D4: Ship as `detangle/eslint`, with the `.node` file inside the existing platform packages

- **Chosen:** a subpath export. The add-on sits at `detangle-<platform>/detangle.node`, next to `bin/`, and is added to each platform package's `files`. Those packages have no `exports` field, so the subpath can be required.
- **Rejected:**
  - `eslint-plugin-detangle`, or separate per-platform add-on packages: each new npm package needs its own trusted-publisher setup (Glossary: platform packages).
  - One package bundling all 8 add-ons: every install would download all 8.
- **Cost we're accepting:**
  - Each platform package roughly doubles, from 3.5–3.8 MB compressed (measured) to an **estimated** ~7 MB, since the add-on contains most of the binary's code. CLI-only users download that too.
  - If a compressed platform package goes over 10 MB, D4 is reopened (see Risks).

### D5: One add-on call per lint, shared by both rules

- **Chosen:**
  - `native.js` keeps a `WeakMap` from `context.sourceCode` to a `Map` from key to result. ESLint creates one `SourceCode` per pass over a file and gives it to every rule.
  - So when both rules use the same options, the first rule calls the add-on and the second reuses the result. Rules with different options have different keys, so each gets its own result from its own handle.
  - A new pass, whether an edit or a `--fix` pass, is a new `SourceCode` and gets a fresh call.
- **Rejected:**
  - A `(file, text)` memo (revision 1): stale after other files changed.
  - One call per rule (revision 2): doubled parse cost.
  - Keying on `sourceCode` alone (revision 3): the wrong handle's result when options differ.
- **Cost we're accepting:** relies on ESLint sharing one `SourceCode` across rules within a pass. A test asserts it on ESLint 9 and 10.

### D6: The linted file's buffer is authoritative for that file (overlay)

- **Chosen:** `violationsFor(file, text)` runs these steps in order, with one analysis at most per call.
  - `file` is `canonical(context.filename)`.
  - `text` is `context.sourceCode.text`, from which ESLint has removed any BOM. `extract` ignores a BOM too, so specifiers match either way.
  1. **Freshen** (D7). Poll the stamps. Drain the watcher, if it's running, and apply the changes: source changes through `Session::update`, then config-change classification (which sees the updated member directories). Then apply what polling and the drain call for: a re-open, a resolution refresh or a rescan, at most one of each, with a re-open replacing the other two.
  2. **Membership:** if `file` isn't a Session member (`Session::contains`):
     - A path that isn't an existing source file (untitled, processor-virtual such as `README.md/0.js`, deleted): no violations, no work.
     - A path rejected before, since the last re-open: no violations, no work.
     - Otherwise `Session::eligible(file)`. If the walk wouldn't include it (excluded, gitignored, outside `dir`), remember the rejection until the next re-open and return no violations; that costs no walk.
     - If it's eligible, `Session::admit(file)`. That applies now the structural update the watcher event would bring (121–140 ms on VS Code), so a new file saved and linted before its event arrives is reported at once.
     - **The Session decides membership, never the overlay.**
  3. **Overlay:** `Session::overlay(file, text)`.
     - Runs `extract` on `text` and compares the result with the file's stored `raw` and parse-error count.
     - If they're equal, returns `false`, with no change.
     - Otherwise it replaces `raw`, re-resolves **only this file**, marks it overlaid, and sets `graph_changed`.
     - Other files don't need re-resolving. Resolution depends on the **set** of files and on configuration files (`package.json` `exports`, tsconfig `paths`, installed packages). An overlay changes none of them, and D7 handles those files.
  4. **Analyse** again if any earlier step set `graph_changed`.
- **Overlay lifetime:**
  - An overlay lasts until a disk event for **that** path, the next lint of that file, or a re-open.
  - Incremental and structural updates keep overlays: an overlaid file is never re-read unless its own path changed. Unsaved imports in other open buffers therefore stay in effect across other files' saves, additions and deletions.
  - A disk event for a file whose buffer still has unsaved edits (a `git checkout` or a formatter) replaces the overlay with the disk content. The next lint of the buffer puts it back.
  - A buffer discarded without saving leaves its overlay in place until one of those events or an ESLint restart.
  - Overlays are never written to the parse cache: only `Session::new` writes it, and it reads from disk.
- **Rejected:**
  - Disk only: new unsaved imports would only show after saving.
  - An mtime check of every file on every call: ~25 ms on VS Code.
  - Overlaying files that aren't on disk: the resolver reads the real filesystem.
  - Re-walking to find out whether an unknown file is excluded (revision 5): ~30 ms per excluded file, repeated after every structural change.
- **Cost we're accepting:** the discarded-buffer case, and a brief disk-content window after an external write.

### D7: Freshness: polling on every lint, plus a watcher started at open on a background thread

- **Polling (always on, at the start of every call).** The handle stats the files in `config_stamps()`, recorded at the last open **attempt**, successful or not:
  - `detangle.toml`, `package.json` and `tsconfig*.json` at the root, plus the root directory's own mtime. The mtime changes when a file is created or deleted there; in that case the handle lists the root directory to see whether one of these names appeared or disappeared.
  - The `config` file, wherever it lives.
  - The Vite/webpack/Babel configs and `.env*` files that the open read.
  - The tsconfig `extends` chain: every file reached from the root tsconfigs through `extends`, which detangle follows itself with the plain resolver and `json-strip-comments`, including bases above the root and `@tsconfig/*` packages.
  - The lockfiles at the root: `package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `bun.lockb`.

  That's typically 5–15 stats, a few µs each.
  - A change to any of them **except the lockfiles** means a re-open (D8).
  - A lockfile change means `Session::refresh_resolution()`: installs change what bare imports resolve to, but not the config.
  - Polling doesn't depend on the watcher, so config freshness, dependency freshness and recovery (D8) work without one.
- **The watcher.** `open` starts a thread that creates the recursive watcher on the root **before** the scan begins. The scan runs on the calling thread at the same time; watcher setup (Linux ~46–96 ms in a VM, macOS ~1.4 ms) overlaps the ~170 ms scan. The setup is mostly kernel work in one thread, while the scan uses all cores, so they barely compete.
  - The watcher belongs to the **handle**, not the Session, so it survives re-opens and panics (D8).
  - Each call drains it with **no quiet period**:
    - The quiet period exists to coalesce editors' multi-write saves before a TUI redraw. Here, the worst case is a half-written file parsed once. Its next write event re-marks it dirty, and the next lint corrects it.
    - Atomic saves are ordinary modifications (Glossary: `Session`).
    - A burst such as a branch switch may span several drains. Each drain applies at most one `Session::update` and at most one re-open, so a burst costs a few updates, each bounded by a full scan (≤ 250 ms on VS Code), not one per event.
  - **Which watcher events count as a config change** (classified after the drain's `Session::update`, so new sources are already members): a config-named event (Glossary: watcher) counts only if the file is polled, or sits in a **member directory**. So nested workspace `package.json`/`tsconfig.json` files count, and config-named files in gitignored output such as `.next/package.json` or `dist/` don't. A new workspace directory counts from the moment its first source file is a member. The event filter keeps skipping `node_modules`, `.git` and `target`.
  - **Lost events** (`need_rescan()`): the next drain re-opens the Project. That's one ≤ 250 ms rebuild, and it catches nested config changes too. Lost events are rare, and a partial rescan could miss those nested config changes.
  - **Without a watcher** (setup failed, inotify exhausted, or the thread died): one warning (D12). Polling and the overlay keep working, and step 2 admits new files when they're linted. Saved changes to other files' imports are seen when those files are linted themselves, not from other files' lints.
- **Rejected:**
  - A lazy watcher, started on the first re-lint (revisions 2–3). It needed lint tracking, a 1 s heuristic and a catch-up, and three cold reads kept finding its corners.
  - Relying on the watcher alone for config and recovery (revision 4): a failed watcher made `Broken`/`Failed` permanent.
  - Watching `node_modules` for installs: the event filter skips it, and Linux filtering is slower anyway. Lockfiles change on every install that changes dependencies.
  - A full re-open on a lockfile change: it would re-read the config and re-parse every file for no reason.
  - Filtering the watch on Linux to skip `node_modules`: measured slower (~125 ms vs ~46 ms for the same tree).
  - Starting the watcher on ESLint's thread: adds 46–96 ms to the first lint on Linux.
  - Retrying watcher creation: the usual cause (exhausted inotify watches) doesn't go away within a process.
- **Cost we're accepting:**
  - Every Project watches, including `eslint .` and each `--concurrency` worker:
    - on Linux, ~15–17 µs of background CPU per directory under the root, `node_modules` included (a very large `node_modules` of ~50k directories takes ~0.8 s and 50k inotify watches per Project);
    - on macOS, one FSEvents stream;
    - on Windows, one recursive directory handle (cost logged by CI, not yet known).
  - **Startup race:** an edit made during the first ~100 ms after open, to a file the scan has already read in a directory not yet watched, is missed until that file's next event or lint.
  - **Event latency:** a lint that runs before the OS has delivered another file's save event sees that save at the next lint.

### D8: Lifecycle. One handle per `(dir, config, mode)`, a state machine, panic recovery and cleanup

- **Which directory to scan (`native.js`):**
  - `dir` is the rule option `dir`, resolved against ESLint's `context.cwd`, or else `context.cwd` itself. That is exactly `detangle check <dir>`, or `detangle check .` run from where ESLint runs.
  - The `config` option is also resolved against `context.cwd`, as the CLI resolves `--config` against its working directory.
  - The root is `find_root(dir)`, as in the CLI, and the watcher watches the root.
  - Editor integrations run ESLint with a working directory per workspace folder by default. That's to be confirmed for vscode-eslint during implementation; the `dir` option covers any setup where it doesn't hold. A project nested below the `cwd` that has its own `detangle.toml` needs `dir`; the docs say so.
  - **Refusal:** if the root would be the filesystem root, the user's home directory (`std::env::home_dir()`), or an ancestor of it, `open` doesn't scan. The handle is `Broken` with the problem `refusing to scan <path>; set the "dir" option`.
  - Any other root is accepted as the CLI would accept it. That includes one found through a stray `package.json` in an ancestor such as `~/projects`, which `detangle check .` would also scan.
- **Cache:**
  - Handles live in a module-level `Map` keyed by `(canonical dir, canonical config path or "", mode or "")`, canonicalized with the add-on's `canonical` (D3).
  - A path that can't be canonicalized (a missing config) is used as given; `open` then fails with that error, and the handle is `Broken`.
  - **Idle eviction:** on each call, `native.js` closes any handle whose last call was more than 15 minutes ago, and removes it from the `Map`. That covers keys a changed ESLint config no longer uses, and projects the user stopped touching. A later lint of an evicted key opens a fresh handle (one cold scan).
  - Each worker thread has its own module instance, so its own `Map`.
- **States** (the handle; the watcher lives in the handle and survives every transition except `Closed`):

  | State | Holds | Each call returns |
  | ----- | ----- | ----------------- |
  | `Ready` | a Project (Session and Analysis) | violations |
  | `Stale` | the last good Project, plus a config error | violations. Overlays and drains keep applying to the last good Session. The error is in `problems` |
  | `Broken` | a config error, a missing `dir` or a refusal; no Project | no violations; the error in `problems` |
  | `Failed` | a panic message, the paths of the Session it dropped (empty if `open` panicked), and the `(file, text hash)` of the call that panicked (none if `open` panicked); no Project | no violations; the panic in `problems` |
  | `Closed` | nothing | the problem `detangle project closed` |

  | From | Event | To |
  | ---- | ----- | -- |
  | (none) | `open`: succeeds / config error, missing `dir` or refusal / panics | `Ready` / `Broken` / `Failed` |
  | `Ready`, `Stale` | a re-open trigger (a polled stamp, a qualifying watcher event, lost events) → re-open: succeeds / config error / `dir` gone / panics | `Ready` / `Stale` (keeps the last good Project) / `Broken` (drops it) / `Failed` |
  | `Broken` | a polled stamp changed → re-open: succeeds / fails / panics | `Ready` / `Broken` / `Failed` |
  | `Ready`, `Stale` | a panic in any other call | `Failed` (the Project is dropped) |
  | `Failed` | a **retry condition** (below) → re-open: succeeds / fails / panics | `Ready` / `Broken` / `Failed` |
  | any | `close()` (idle eviction, tests) | `Closed` |

  - **Stamps are recorded at every open attempt**, successful or not. A failed re-open therefore isn't retried until a polled file changes again: never once per keystroke.
  - **Retry condition for `Failed`.** At most once per 30 s, and only if something changed since the last attempt:
    - a polled stamp;
    - a watcher event for one of the dropped Session's paths, or for any source file under `dir` if `open` itself panicked;
    - or a call for a `(file, text hash)` different from the one that panicked (for example, the user fixed the file that crashed the parser).

    The 30 s floor bounds the cost of a panic that recurs every time to one re-open per 30 s. Requiring a change means a quiet project never retries.
  - During a re-open from `Ready`/`Stale`, the old Project stays alive until the new one is ready (Success criteria: memory).
- **Panics:**
  - Every export wraps its body in `std::panic::catch_unwind`. A panic moves to `Failed`, and the call returns normally with the panic in `problems`. Throwing is avoided (D12).
  - All profiles keep `panic = "unwind"`, and `napi/Cargo.toml` says why.
- **Background threads:**
  - The watcher's thread: if it dies, `drain()` returns `Disconnected`. The handle drops the watcher, emits a warning (D12), and continues without it (D7).
  - The **dropper thread** is one per handle. It receives replaced Analyses, and the old Project after a re-open, and frees them off ESLint's thread. If sending to it fails, the value is freed inline.
- **Cleanup:** an env cleanup hook (`napi_add_env_cleanup_hook`) runs when a worker thread or the main environment is torn down.
  - In a **worker** env, it stops the watcher and joins its thread (waiting for an unfinished setup, ≤ its setup time), closes the dropper channel, joins that thread, and frees the Projects.
  - In the **main** env, it signals the threads to stop but **doesn't join** them, and leaks the Projects and the dropper's queue, like the CLI's `one_shot`. The threads only touch state they own, and the OS reclaims everything at exit, so a short `eslint .` run never waits on watcher setup or freeing.
  - Whether Node runs the hook under `process.exit()` isn't verified yet (the teardown test checks). Either way the main-env path only signals and leaks, so skipping it loses nothing. Nothing runs when the process is killed.
- **Rejected:**
  - Per-file project discovery (`rootOf` of each linted file, an ancestor watch, root switching; revisions 3–4). It made one Project per directory in unmarked trees, scanned `$HOME` via a stray `package.json`, and leaked or duplicated Projects when marker files changed inside a root.
  - The config file's directory as `dir` (revision 3): wrong when the config lives in a subdirectory.
  - Re-opening on every call after a panic: a cold scan per keystroke.
  - Retrying from `Failed` on every change without a time floor: in `eslint .`, every newly linted file would be a "different file" and trigger a cold re-open.
  - Never re-opening in-process: one bad save disables the rules for the editor session.
  - A process-global Session shared across worker threads.
  - No eviction (revision 5): a handle for an option set the ESLint config no longer uses would keep ~172 MB and a watcher until exit.
- **Cost we're accepting:**
  - Nested projects not at ESLint's `cwd` must set `dir`.
  - A monorepo without a root `detangle.toml`, linted from the repo root, is one Project rooted at the root `package.json`. That's the same as `detangle check .` there, so no violations are lost relative to the CLI.
  - With `--concurrency N`, the scan runs N times (N × ~170 ms of CPU and N × ~172 MB on VS Code), and each worker holds its own watcher.
  - Returning to a project after 15 idle minutes costs one cold scan.

### D9: Two rules split by severity, and a placement table

- **Rules:**
  - `detangle/errors` reports detangle `error` violations.
  - `detangle/warnings` reports `warn` and `info`.
  - `recommended` sets them to ESLint `"error"` and `"warn"`.
  - Both take `{ dir?, config?, mode? }` (schema in Files). Using the same values for both is recommended, since different values mean two handles (D5).
- **Placement.** For a violation `v` and the linted file `L`. "The imports of `b` in `L`" means every entry of `L`'s edge to `b`: a file can import the same target more than once, for example a type import and a value import.

  | Violation kind | Shown in `L` when | `specifiers` in `L` |
  | -------------- | ----------------- | ------------------- |
  | Module scope with `to` (forbidden, `not-in-allowed`, circular) | `v.from == L` | the imports of `v.to` in `L` |
  | Module scope without `to` (orphan, reachable, dependents count, `required`) | `v.from == L` | none, so line 1 |
  | Folder scope, `F → T` | `L`'s directory is `F` or lies anywhere below `F`, and `L` imports some `b` with `folderTarget(b) == T`, where `b` is not a local file anywhere below `F` | the imports of each such `b` in `L` |
  | Group scope with `imports` | some entry of `v.imports` has `from == L` | those entries' `specifier`s. Other entries show in their own files |
  | Folder or group scope without `to`/`imports` | never | not shown; see Out |

  - The folder row is exactly the folder graph's construction in `src/graph.rs`, read backwards.
    - That construction: a module edge `a → b` gives every folder `F` that contains `a` (its directory and each ancestor up to `.`) an edge `F → folderTarget(b)`, unless `b` is a local file inside `F`. A folder never depends on its own subfolders.
    - `folderTarget(b)` is:
      - for a local file, its directory (`.` at the root);
      - for an npm module, `node_modules/<package name>`, including `@scope/name`;
      - for a built-in or unresolved module, its id.
    - One folder violation therefore shows on every contributing import in every file under `F`. That can be many files; it's where the violation actually is. The fix (or an exception in the rule) clears all of them at once.
  - A circular rule shows in **every** file on the cycle whose edge matches the rule, each on its import of the next file, just as `detangle check` lists them.
- **Node matching (JavaScript).** A report goes on every node in `L` whose string-literal source equals a specifier, covering each form `extract` recognizes (Glossary):
  - `ImportDeclaration`;
  - `ExportNamedDeclaration`/`ExportAllDeclaration` with a source;
  - `ImportExpression`;
  - `TSImportEqualsDeclaration`;
  - `TSImportType` (the typescript-eslint 8 shape, with a string literal argument);
  - `CallExpression`s: `require(...)`, `process.getBuiltinModule(...)`, and callees whose text (`sourceCode.getText(callee)`, for example `module.require`) equals an `exoticRequire` name;
  - the string elements of the array argument of `define([...])` and `require([...], f)`;
  - Angular decorator properties `templateUrl`, `styleUrl` and `styleUrls` (elements). A literal matches a specifier equal to it, or equal to `./` plus it, since `extract` prepends `./` to bare values.

  Other rules:
  - A template literal without expressions counts as a literal. Other non-literal sources never match; detangle doesn't resolve them either.
  - Forms that live in comments (`/// <reference>`, `/// <amd-dependency>`, JSDoc imports) and SFC template imports have no AST node. Their specifiers are shown at line 1, with `[import "<specifier>"]` appended to the message.
- **Rejected:**
  - One rule for everything: it loses the severity split.
  - One ESLint rule per detangle rule: rule names must be known before the config is read.
- **Cost we're accepting:** a detangle `info` shows as an ESLint warning under `recommended`.

### D10: `eslint --cache` is documented, not handled

- **Chosen:** the docs state that the rules depend on other files, so `eslint --cache` can miss violations caused by changes in other files. CI and pre-commit should run `detangle check`.
- **Rejected:** detecting `--cache`. ESLint doesn't expose it to rules.
- **Cost we're accepting:** cached ESLint runs can under-report.

### D11: No fallback when the add-on can't load

- **Chosen:** every lint reports one `problems` message: `detangle add-on unavailable: <reason>; run \`detangle check\``.
- **Rejected:** a one-time `spawnSync` of the binary. Its triggers barely exist:
  - The add-on and binary ship in, and fail with, the same platform package.
  - Node-API is ABI-stable across Node versions.
  - It would mean a second **production** implementation of D9 in JavaScript, plus 31 MB of JSON per start on VS Code. The drift test's oracle is also a JavaScript version of D9, but it's test code, checked against the Rust one on every CI run. A production copy would be a second path users depend on.
- **Cost we're accepting:** runtimes without Node-API get only the message.

### D12: Problems and warnings

- **Chosen:**
  - **`problems`** are persistent state problems: add-on unavailable (D11), a config error, missing `dir` or refusal (`Stale`, `Broken`), a panic (`Failed`), or `Closed`. They're returned on every call while the state lasts, and shown at line 1 of every linted file.
    - Within one pass, only the **first rule to run** for that `(sourceCode, key)` reports them, at that rule's severity. The shared result (D5) records that they've been reported.
    - Which rule runs first follows the order in the user's ESLint config. `configs.recommended` lists `detangle/errors` first, so under it problems are errors. With only one detangle rule enabled, that rule reports them.
    - An inline `eslint-disable` covering line 1 hides them too, as it hides any line-1 report.
    - An add-on that can't load in CI therefore fails the lint. That's intended: a CI config that enables the rules but can't run them is misconfigured, and the message says to run `detangle check`.
  - **Warnings** are one-time notices that don't block results: the watcher failed to start, or its thread died. `native.js` emits them with `process.emitWarning`, once per handle; editors show them in the ESLint output channel. They aren't `problems`, because in CI they'd flood every file while results are still correct.
- **Rejected:**
  - Throwing: ESLint turns it into a crash message for the whole file.
  - Reporting problems from both rules: duplicates.
- **Cost we're accepting:**
  - A broken config shows at line 1 of every file linted until it's fixed. That's intended.
  - With a config that lists `detangle/warnings` first, problems show as warnings.

## Edge cases & failure modes

| Scenario | Behavior | Why |
| -------- | -------- | --- |
| `eslint .` / `eslint --fix .` | One scan and one watcher per thread; one overlay parse per pass; exit never waits on the watcher | D5, D7, D8 cleanup |
| `eslint --concurrency N` | N scans, N watchers, N × memory | D7, D8 cost |
| inotify watches exhausted (Linux, big `node_modules` or many workers) | Watcher fails: one warning. Polling, the overlay and admission keep working; other files' saved import changes are seen when those files are linted | D7, D12 |
| `eslint --cache` | May miss cross-file violations | D10 |
| Monorepo, linted from the repo root | One Project for the repo, the same as `detangle check .` there | D8 |
| Nested project with its own `detangle.toml`, not at ESLint's `cwd` | Scanned as part of the `cwd`'s root unless `dir` points at it | D8 cost |
| Multi-root editor workspace | One handle per workspace folder, if the editor sets ESLint's `cwd` per folder; otherwise `dir` | D8 |
| ESLint run from `$HOME` or `/` with no marker below | `Broken`: "refusing to scan …; set the "dir" option" | D8 refusal |
| Stray `package.json` in an ancestor such as `~/projects` | Scanned from there, as `detangle check .` would | D8 |
| `dir` deleted or renamed (branch switch) | The re-open fails with "not found": `Broken`, Project dropped. Recovers when `dir` returns (its root stamps change) | D8 |
| ESLint config changes the rules' options | The new key gets a new handle; the old one is closed after 15 idle minutes | D8 eviction |
| Config change (root files, the `config` file anywhere, the tsconfig `extends` chain, `.env*` read at open) | Detected by polling at the next lint, with or without a watcher. Re-open ≤ 250 ms (TOML). Success: `Ready`. Failure: `Stale`, with the last good results plus the error, and no retry until a polled file changes again | D7, D8 |
| `npm install` / `pnpm add` / version bump | The lockfile changes: a resolution refresh ≤ 150 ms at the next lint | D7 |
| Nested workspace `package.json`/`tsconfig.json` edit | A config change via the watcher (it's in a member directory): one re-open | D7 |
| `.next/package.json`, `dist/package.json` written by a dev server | Ignored: not polled, not in a member directory | D7 |
| Config invalid on first open | `Broken`: a line-1 problem on every file; recovers at the first lint after a polled file changes | D7, D8 |
| `open` panics | `Failed`; retries at most once per 30 s after a polled change, a source event under `dir`, or a lint of different text | D8 |
| JavaScript rules config, or Vite/webpack evaluation | The add-on spawns the running Node (`process.execPath`) synchronously | D3 `node` |
| Edit to a file a JavaScript rules config imports | Not detected | Out |
| Linted file excluded, gitignored or outside `dir` | An eligibility check the first time after each re-open (≤ 1 ms), then no reports | D6 step 2 |
| New file saved and linted before its watcher event | Admitted by step 2 (≤ 150 ms) and reported at once | D6 |
| Untitled or unsaved new file | No reports, no work | D6 step 2 |
| Atomic save (temp file + rename) | An ordinary modification, not structural (2–5 ms) | Glossary: `Session` |
| Edit during the first ~100 ms after open | Possibly missed until that file's next event or lint | D7 startup race |
| Unsaved import edit | Checked live | D6 |
| Unsaved imports in another open buffer, then files are saved, added or removed | Kept: overlays survive updates | D6 |
| External write to a file with unsaved edits | Disk content until the next lint of the buffer | D6 |
| Buffer discarded without saving | Overlay stays until a disk event for that path, a lint of that file, a re-open, or an ESLint restart | D6 cost |
| Re-open (config or lost events) | All overlays dropped; each returns at its buffer's next lint | Out |
| `notify` reports lost events | Re-open at the next lint | D7 |
| Branch switch | A few updates across the next lints, each ≤ 250 ms on VS Code; a re-open if polled files changed; a resolution refresh if a lockfile changed | D7 |
| Processor virtual filenames (`README.md/0.js`) | Not an existing file: no reports, no work | D6 step 2 |
| Buffer with syntax errors | ESLint fails to parse and never runs the rules | ESLint behavior |
| Vue/Svelte through their ESLint parsers | `text` is the full SFC source, which `extract` handles via `sfc.rs` | Same input the CLI reads |
| Specifier with no matching AST node (comment directives, SFC templates) | Line 1, with `[import "…"]` in the message | D9 |
| Same specifier imported twice | Both nodes are reported | D9 |
| `require(variable)` | Never matched | D9 |
| Folder violation over many files | Shown on every contributing import | D9 |
| Folder/group violation without an import | Not shown | Out |
| Add-on missing or fails to load | A `problems` line on every file (an error under `recommended`) | D11, D12 |
| Rust panic | `Failed`; a `problems` line; retry per D8 | D8 |
| Stack overflow or abort in Rust | The ESLint process dies | Can't be caught; D1 |
| ESLint worker thread exits | The cleanup hook stops and joins the threads and frees the Projects | D8 |
| Process exit (`eslint .` finishing, or `process.exit()`) | The cleanup hook, if Node runs it, signals the threads and leaks the rest, like `one_shot`; otherwise the OS reclaims everything | D8 |
| Process killed | Nothing runs; the OS reclaims everything | — |
| Two `--concurrency` workers write the parse cache | Separate temp files per thread; the last rename wins; each file is complete | Files: `scan.rs` |
| Symlinked or case-mismatched paths | `canonical` (`dunce`) on both sides. macOS returns the on-disk case (checked with native `realpath` on APFS). Windows is covered by the CI case test | D3 |

## Risks

- **Placement drifts from the CLI.**
  - Caught by: `npm/eslint.test.mjs` runs a real `ESLint` over `tests/fixtures/eslint`, `tests/fixtures/eslint-monorepo` and `tests/fixtures/conditions`. It compares the `(file, line, column, message)` list, as a multiset, against an oracle the test computes itself:
    - violations and their fields come from `detangle check -f json`;
    - specifiers come from `graph -f json --externals` (each module's `dependencies[].specifier`);
    - D9's table and D3's message format are applied in JavaScript.
  - This follows the project's standing rule: verify against the real tool.
  - Not caught:
    - bugs shared by the oracle and the implementation, which are written separately in JavaScript and Rust;
    - duplicate imports of one target through **different** specifiers, because graph JSON shows one specifier per edge. RuleTester covers the fixture's duplicate case instead.
- **The long-lived Session diverges from a fresh scan.**
  - Caught by `scan.rs` unit tests, each asserting `graph_changed` and that the Session equals a fresh scan of the same disk-plus-overlay state:
    - overlay equal to the disk version;
    - overlay adds an import;
    - overlay, then a disk save with the same text;
    - overlay, then the disk reverted;
    - overlay, then another file added (the overlay survives), then another overlay;
    - `admit` of a new file;
    - `refresh_resolution` after a package is installed into a fixture's `node_modules`.
  - Also caught by the handle-level tests in `eslint.test.mjs` (Files), which exercise the D6 step order.
- **Rust panics become ESLint crashes.** Caught by `catch_unwind`, the panic tests, and a local run of the rules over the VS Code and excalidraw clones before each release. The clones aren't in CI. Not caught: aborts, OOM, stack overflow.
- **Watcher cost differs on native Linux and Windows.** The Linux numbers come from a Docker VM, and Windows isn't measured.
  - Caught by: `eslint.test.mjs` logs watcher setup time for its fixture on each CI OS, and the benchmark's Linux row is re-run on a native Linux machine before release.
  - Not caught: repos with very large `node_modules`, where setup is ~0.8 s of background CPU and may exhaust inotify watches. That degrades freshness (D7), not correctness at open.
- **musl or Windows ARM add-on builds fail.** The release matrix builds them on native runners; CI only covers x64 Linux/Windows and arm64 macOS.
  - Mitigation: the smoke `require` on each release runner (musl in an Alpine container), and the check that the musl CLI binary is still static, both before anything is uploaded.
  - Not smoke-tested: x86_64 macOS, the same as its binary today.
- **Thread teardown bugs** (a watcher outliving its env). Caught by: a test that opens a handle in a `worker_threads` Worker, terminates the Worker, and asserts the process exits cleanly. Plus the `eslint .` exit-time check in the benchmark.
- **Stale results in editors.** Lost events are handled (D7), and dependency installs are caught through lockfiles. Two things aren't caught:
  - events `notify` drops *without* signalling it;
  - `node_modules` changes that don't touch a lockfile.

  They self-heal on the next save of the affected file, a re-open, or an ESLint restart.
- **Package size.** If a compressed platform package goes over 10 MB (estimated ~7 MB), D4 is reopened. The alternative is one extra `-node` package per platform, with its trusted-publisher setups.
- **The lib split changes the binary's output.** Caught by `scripts/cmp-output.sh` against the `22899e5` binary on VS Code and excalidraw.
  - The `CacheArgs` and `node` parameters are the only logic changes in the move. The existing `--cache` tests and JS-config tests cover them.
  - The two CLI fixes (lost events in `watch`, cache temp names) and overlay preservation in `update` change no output: the CLI never overlays.
- **ESLint API changes.** CI tests ESLint 10 (current: 10.11) on all three OSes and ESLint 9 on Linux, all on Node 22. The shared-`SourceCode` assumption (D5) has its own test.
- **Latency or memory misses its targets.** `scripts/bench-eslint.mjs` before release. A miss blocks the release unless the maintainer explicitly accepts it.

## Sign-off

- [x] Cold Reads 1–5 — findings and resolutions in [`node-addon-reviews.md`](node-addon-reviews.md)
- [ ] Human review (the maintainer)
