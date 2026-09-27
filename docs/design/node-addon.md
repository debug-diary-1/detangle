# Feature: ESLint rules backed by a native Node.js add-on

Status: **draft, revision 5** (after Cold Reads 1–4; their findings are at the end). No code until the next Cold Read and sign-off. Owner and sign-off: the maintainer; the independent gate is the cold reads plus human review.

## Problem

detangle's rules run today only when someone runs `detangle check`, `detangle watch` or the explorer. So a developer sees a forbidden import or a new cycle at CI time, or not at all, and not while writing the import.

Editors already surface ESLint results inline as you type. Putting detangle's violations there needs a way to answer "what's wrong with this file?" synchronously, in milliseconds, inside a process that lives for hours. Running the CLI once per file can't do that on large repos (see Context).

This is a maintainer-driven bet, not a response to user requests. That's why it ships as **experimental**.

## Background

**detangle** is a Rust CLI, also shipped on npm, that scans a JavaScript/TypeScript project, builds its import graph and checks architecture rules against it. The rules come from `detangle.toml`, or a JavaScript rules config it imports. Its existing Node.js API (`npm/index.js`) runs the CLI binary as a child process and parses its JSON output.

### Glossary

| Term | Meaning |
| ---- | ------- |
| module | A source file in the graph, identified by its path relative to the root (for example `src/a/b.ts`). npm packages and Node built-ins are modules too. |
| rule | An entry in the config, with a name, a severity (`error`, `warn`, `info`, `off`) and an optional `comment`. Kinds: `[[forbidden]]` (a matching import is a violation); `allowed` (every import must match some `allowed` entry, or it's reported as `not-in-allowed`); `[[required]]` (matching modules must import something); plus module-only conditions inside `forbidden`: `orphan` (no imports in or out), `reachable` (reachable from given entry points, or not), and dependents count (`module = { … }` with more or fewer than N importers). `off` rules are never evaluated. |
| group | A named set of modules from `[[groups]]` in the config, or an Nx project discovered from `project.json`. Groups form their own graph. |
| violation | One rule match. Its `scope` is `module`, `folder` (the graph of directories) or `group`. It has a `from` node, an optional `to` node (none for rules about a node alone), a `cycle` (for circular rules) and, at group scope only, `imports`: the module-level imports behind it. `detangle check -f json` prints each import as `{ from, specifier, to }`, where `from`/`to` are module ids and `specifier` is the import string. |
| circular violation | Reported **per import edge**: every edge that lies on a cycle and matches the rule yields its own violation, with `from` = that edge's importing file. |
| `dir` and root | `detangle check <dir>` scans `dir`. The **root** is `find_root(dir)`: the nearest ancestor of `dir` (itself included) with `detangle.toml`, else the nearest with `package.json`, else `dir`. Module ids are relative to the root, and the watcher watches the root. |
| `Session` (`src/scan.rs`) | The scanned files: each file's extracted import strings (`raw`), their resolved targets, the parse-error count, and a `stamp` (mtime + size). `Session::update(paths)` re-reads changed files. It decides what changed **by checking the filesystem, not the event kind** (watchers report plain saves as creates): a path counts as added only if it's a source file that isn't a member yet, and as removed only if a member no longer exists. So an editor's atomic save (write a temp file, rename it over the original) is a plain modification. When a path really was added, removed or renamed (a **structural** change), it re-walks `dir`. If the set of files differs, it re-resolves every file; if not, only files whose stamp moved. Its `work.graph_changed` flag says whether any file's imports changed. |
| member directory | A directory under the root that contains a Session member, or is an ancestor of one. Gitignored output such as `.next/` or `dist/` has no members, so it isn't one. |
| `Project` (`src/main.rs`) | A loaded config plus a `Session`. `Project::analyze()` builds the `Graph` and evaluates the rules into an `Analysis` (graph + violations). |
| `extract` (`src/scan.rs`) | Parses one file's source (with oxc) and returns its import strings. They're the **cooked** string values, the same as ESTree's `Literal.value`, including any `?query` suffix. Vue/Svelte/Astro files go through `src/sfc.rs` first. |
| parse cache | The optional on-disk cache of each file's extracted imports, enabled by `cache` in the config or `--cache` on the CLI. It's written to a temp file, then renamed into place. |
| exotic require | `options.exotic_require` in the config: extra callee names treated like `require`, possibly dotted (for example `module.require`). |
| watcher (`src/watch.rs`) | The `notify`-based recursive file watcher used by `detangle watch` and the explorer (the interactive terminal UI). It **ignores events** under `node_modules`, `.git` and `target`, but the OS still watches those directories (on Linux, one inotify watch per directory). It waits for 150 ms of quiet before releasing a batch. It flags a change as a **config change** when the file name is `detangle.toml`, `package.json`, `tsconfig*.json`, `webpack.config.*`, `vite.config.*`, `babel.config.*`, `.babelrc*`, `.env*` or `project.json`. |
| `--mode` | The existing CLI flag passed to Vite/webpack config evaluation, so aliases defined per mode resolve correctly. |
| `one_shot` (`src/main.rs`) | Deliberately leaks a one-shot command's `Project` and `Analysis`, because the OS reclaims them faster at exit than freeing them does (~20 ms for VS Code's graph, measured when it was introduced). |
| platform packages | The 8 npm packages `detangle-{darwin-arm64, darwin-x64, linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, linux-arm64-musl, windows-x64, windows-arm64}`, each holding the binary for one target (today 3.5–3.8 MB compressed, 8–9.6 MB unpacked). They're listed in `npm/platforms.json`; `npm/binary.js`'s `platform()` picks the one for this machine. |
| `--concurrency N` | ESLint's option (ESLint 9.34+) to lint in N worker threads, all in one process. |
| baseline `22899e5` | The commit of release 0.1.3, the last release before this work. |
| fixtures | `tests/fixtures/*`: small projects used by the Rust and npm tests. `conditions` exercises ~20 rule conditions (it produces 12 plain and 7 circular module-scope violations). |
| verify against the real tool | The project's standing rule: correctness is checked by comparing with the real CLI's output, not only with unit tests. |

## Context: what the Node API costs today

Measured 2026-09-27 on the maintainer's Mac (Apple Silicon). Numbers are medians of 7 runs from `target/release/detangle` at `22899e5`, driven from Node 24.

| Project | `detangle check -f json` (spawn from Node) | `check()` API | `graph -f json --externals` | `JSON.parse` of that | `analyze()` API | JSON size |
| ------- | ------ | ----- | ----- | ---- | ----- | ------- |
| VS Code `src/` (9.6k files, 2.7k directories) | 197 ms | 199 ms | 240 ms | 60 ms | 302 ms | 31.4 MB |
| excalidraw | 24 ms | 23 ms | 25 ms | 3 ms | 27 ms | 1.7 MB |

Other measurements:
- Spawning `detangle --version` from Node: 3.7 ms (median of 21).
- VS Code phase times (`DETANGLE_TIMINGS=1`, `graph -f json`): scan 170 ms, graph 15 ms, rules 5 ms, total 219 ms.
- VS Code peak memory for `detangle check`: 172 MB RSS.
- `detangle watch` on VS Code, rebuild after an edit, 8 runs each:
  - one that changes imports: 57–61 ms (not broken down further);
  - one that doesn't: 2–5 ms.
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
  - A napi-rs (v3) add-on, `detangle.node`, targeting Node-API 8. It exposes `open` and a synchronous `Project` (contract in D3).
  - Two ESLint rules, `detangle/errors` and `detangle/warnings`, in the existing `detangle` npm package at `detangle/eslint`, plus `configs.recommended`.
    - They support ESLint 9 and 10 flat config.
    - The Node versions ESLint supports apply to the rules: ESLint 10 needs ^20.19, ^22.13 or ≥ 24. The package's `engines` (Node ≥ 18) stays for the CLI and the existing API.
  - **One Project per `(dir, config, mode)` per ESLint thread**, where `dir` is the `dir` option or ESLint's `cwd` (D8). No automatic per-file project discovery.
  - Freshness:
    - an overlay of the linted file's buffer (D6);
    - a watcher started at open on a background thread;
    - polling of the root config files on every lint (D7).
  - Shipping `detangle.node` inside the 8 existing platform packages.
  - `scripts/cmp-output.sh` and `scripts/bench-eslint.mjs`, committed so the checks can be reproduced.
  - Two CLI fixes that come with the watcher and Session work:
    - `detangle watch` and the explorer stop ignoring `notify`'s lost-event signal;
    - the parse cache's temp file gets a per-thread name.
  - Release **0.2.0**, with the ESLint integration marked **experimental**.
- **Out:**
  - Changing the existing `analyze`/`check`/`report`/`graph`/`migrate` Node API, or any CLI output format.
  - Discovering a project per linted file (walking up from each file to its own `detangle.toml`/`package.json`). That was tried in revisions 3–4 and produced most of Cold Read 4's lifecycle holes.
  - Unsaved changes in files other than the one being linted. That includes other buffers whose overlay a structural update has replaced with disk content (D6).
  - Files that don't exist on disk yet: no reports.
  - Re-linting other open files when the graph changes. They update the next time the editor lints them.
  - Folder- and group-scope violations **without an import** (dependents count, orphan or reachable at group scope). They have no file to show in; `detangle check` lists them.
  - Tracking files that a JavaScript rules config imports.
  - Excluding `node_modules` from the OS-level watch on Linux (measured: filtering costs more than it saves).
  - A fallback when the add-on can't load (D11).
  - Autofixes and suggestions.
  - Writing detangle rules inside the ESLint config instead of `detangle.toml` or a rules config file.
  - Legacy `.eslintrc`, ESLint < 9, and typescript-eslint < 8.
  - Any new npm package.
  - Bundler plugins, an LSP, and a `detangle serve` daemon.
  - Sharing one Session across ESLint worker threads.

## Success criteria

"Lint" means **one ESLint pass over one file with both rules enabled**, which is one add-on call (D5). Targets are for VS Code `src/` on the maintainer's Mac, measured by `scripts/bench-eslint.mjs <corpus>`. The benchmark drives ESLint's `Linter` API with and without the rules on one thread and reports the difference.

| Case | Target | Basis |
| ---- | ------ | ----- |
| Lint, file unchanged, median-size file | ≤ 0.5 ms added | estimate: parse of ~6 KB, a few config stats, the lookup and node matching. To be measured |
| Lint, file unchanged, 1.4 MB file | ≤ 5 ms added | parse ≤ 3 ms, one parse per lint |
| Lint after an **unsaved** import edit in the linted file | ≤ 30 ms added | one parse, re-resolving one file, graph 15 ms, rules 5 ms |
| Lint after a **saved** import change in another file (arrives via the watcher) | ≤ 65 ms added | `watch` measures 57–61 ms. D8's off-thread free should lower it, but that saving is unmeasured and not counted |
| First lint in a process (cold scan) | ≤ 250 ms | CLI total 219 ms. The watcher is set up in parallel and isn't on this path |
| Lint after a config change (TOML config) | ≤ 250 ms | a full re-open. JS configs and Vite/webpack evaluation add their `node` spawn, reported by `DETANGLE_TIMINGS` as the `config` phase |
| Watcher setup, Linux (background) | ≤ 100 ms for VS Code `src/` | ~46 ms in a VM; re-measured on native Linux in CI (Risks) |
| Memory per Project, steady state | ≤ 1.2 × the CLI's peak RSS on the same project | CLI: 172 MB on VS Code. During a re-open the old Session stays alive until the new one is ready, so up to ~2.4× briefly |

A missed target **blocks the 0.2.0 release**, unless the maintainer accepts that specific miss in the release commit message with the measured number. The measured numbers go in the implementation commit message either way.

Correctness criterion: the drift test (Risks) passes, comparing **locations**, not only messages.

## Files & touch points

- `Cargo.toml`:
  - Add a `[lib]` target (`src/lib.rs`), and turn the root into a workspace with the member `napi/`.
  - `include` still covers `src/**`. `cargo package` drops the `[workspace]` table (verified with a probe crate), so `cargo install detangle` builds as before. CI runs `cargo package` and builds the resulting `.crate` alone.
  - Keep `panic = "unwind"` (the default) in every profile (D8).
- `src/lib.rs` (**new**): declares the modules `main.rs` declares today and re-exports `Project`, `Analysis` and `CacheArgs`. The crate docs say "internal API, no semver guarantees".
- `src/project.rs` (**new**):
  - `Project` and `Analysis`, moved from `main.rs`.
  - One logic change: `Project::open` today reads the CLI's `--cache`/`--cache-strategy` from a global (`CACHE_ARGS`, read nowhere else). It takes them as an explicit `CacheArgs` parameter instead; the CLI passes its flags, and the add-on passes none, so only the config's `cache` setting applies.
  - `Project::violations_for(path) -> FileReport`, where `FileReport = { violations: Vec<FileViolation> }` and `FileViolation = { rule, severity, message, specifiers }` (D3). It implements D9 through a per-`Analysis` index built lazily on first use:
    - module-scope violations by `from` module;
    - group-scope violations by each `imports` entry's `from` module;
    - folder-scope violations by folder id. The lookup walks the linted file's directory and its ancestors.
  - `Project::config_stamps()`: stamps of the files `open` reads directly: the `config` file (wherever it lives), `detangle.toml`, the root `package.json` and `tsconfig*.json`, and any Vite/webpack config.
- `src/main.rs`: uses the lib, and passes `CacheArgs` from its flags. `one_shot` stays here.
- `src/scan.rs`:
  - `Session::overlay(path, source) -> bool` (D6).
  - `Session::contains(path)`.
  - `Session::admit(path) -> bool`: a structural update for one path that may have been missed (D6).
  - `Session::rescan()`: a forced structural update.
  - `Session::is_member_dir(path)`.
  - The parse cache's temp file is named `{FILE}.{pid}.{thread id}` instead of `{FILE}.{pid}`, so `--concurrency` workers in one process can't collide.
- `src/watch.rs`:
  - `Changes` gains `rescan: bool`, set when `notify` reports lost events (`need_rescan()`: macOS "must scan subdirs", inotify queue overflow). The callback checks `need_rescan()` **before** its event-kind filter. Today that filter discards these events (their kind is `Other`), so `detangle watch` silently ignores lost events.
  - `detangle watch` and the explorer gain the fix too: `rescan` triggers a full rebuild (`Changes::full`). That changes behavior, not output format, and a `watch.rs` unit test covers it.
  - `Watcher::drain() -> Result<Changes, Disconnected>` returns every pending change with no quiet period. `Disconnected` (a unit error) means the watcher's thread died.
- `napi/Cargo.toml`, `napi/src/lib.rs` (**new**): the `detangle-napi` crate (`publish = false`, `crate-type = ["cdylib"]`, `napi` 3 / `napi-derive` 3). It contains:
  - The exports from D3, each wrapped in `std::panic::catch_unwind` (D8). `#[napi(catch_unwind)]` is a backstop.
  - The state machine, background watcher setup, config polling, dropper thread and env cleanup hook (D7, D8).
  - A `test-hooks` cargo feature adding a hidden `__panicForTest()` export.
- `npm/native.js` (**new**):
  - Loads `detangle.node` from the platform package via `binary.js`'s `platform()`.
  - Keeps the Project `Map` (D8), and shares one result per `(sourceCode, key)` (D5).
  - Emits process warnings (D12).
- `npm/eslint.js`, `npm/eslint.mjs`, `npm/eslint.d.ts` (**new**):
  - The plugin `{ meta, rules: { errors, warnings }, configs: { recommended } }`.
  - The rules match specifiers to AST nodes (D9).
  - `configs.recommended` is `{ name: "detangle/recommended", plugins: { detangle }, rules: { "detangle/errors": "error", "detangle/warnings": "warn" } }`, with no `files`. It applies to whatever files the user's config already lints; TypeScript, Vue and Svelte need their usual `files`/parser entries.
- `npm/package.json`:
  - `exports["./eslint"]` and the new files in `files`.
  - `peerDependencies.eslint: ">=9"`, optional via `peerDependenciesMeta`.
  - `devDependencies`: `eslint@^10`, `eslint9: "npm:eslint@^9"` (an npm alias, so both versions install side by side), `typescript-eslint@^8`.
  - Version 0.2.0.
- `npm/eslint.test.mjs` (**new**):
  - RuleTester cases for node kinds, including dotted exotic requires.
  - The drift test (Risks).
  - The shared-`SourceCode` assumption, on ESLint 9 and 10.
  - The worker teardown test.
  - The panic test (`test-hooks`).
  - A no-watcher test: with the watcher disabled by a `test-hooks` switch, a config edit is still picked up through polling, and a `Broken` Project recovers.
  - A build-output test: writing `.next/package.json` causes no re-open.
  - A new-file test: a file created and linted before its watcher event arrives is admitted.
  - A lost-events test.
  - A Windows-only case test (a path differing only in case).
  - It logs watcher setup time per OS (read by Risks).
- `tests/fixtures/eslint/` (**new**): module, circular, folder, group, orphan and `required` rules, an exotic require (dotted), and duplicate specifiers.
- `tests/fixtures/eslint-monorepo/` (**new**): workspace packages with their own `package.json`, a root `detangle.toml`, and a cross-package cycle.
- `scripts/cmp-output.sh` (**new**): `cmp-output.sh <base-bin> <new-bin> <project>…`.
  - It runs `check`, `check -f json`, `graph -f json` and `graph -f dot` with both binaries.
  - It strips the only nondeterministic output: JSON `timings`/`*_ms` fields, and `· <n>ms` in text output. It then asserts byte-identical output.
  - It replaces the maintainer's uncommitted script with the same checks.
- `scripts/bench-eslint.mjs` (**new**): the Success-criteria benchmark. It takes a corpus path and isn't run in CI.
- `scripts/npm-packages.mjs`: copies `detangle.node` into each platform package.
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

- **Chosen:**
  - `open(dir: string, options: { config?: string; mode?: string }) → Project`. This is `detangle check <dir> [--config …] [--mode …]` kept alive. Other CLI flags have no equivalent; everything else comes from the config file, the parse cache (`cache`) included. Paths are absolute, resolved by `native.js` (D8).
  - `Project.violationsFor(file: string, text: string) → { violations, problems, exoticRequire }`:
    - `violations: { rule, severity: "error" | "warn" | "info", message, specifiers: string[] }[]`.
      - `specifiers` are the import strings **in this file** where the violation should be shown. An empty list means line 1.
      - `message` is defined below.
    - `problems: string[]`: persistent state problems (D12).
    - `exoticRequire: string[]`: the config's exotic-require names, for node matching (D9).
  - `Project.close()`: stops the watcher and frees the Session. The Project then stays `Closed`: every later call returns only the problem `detangle project closed`. Only tests call `close()`; in normal use the env cleanup hook tears Projects down (D8).
  - **Message format**, built in Rust from the violation's JSON fields, as `detangle check -f json` prints them:
    - `` `${rule}: ${from}` `` then `` ` → ${to}` `` if there's a `to`;
    - then `` ` (cycle: ${cycle.join(" → ")})` `` if the cycle has more than one entry;
    - then `` ` — ${comment}` `` if there's a comment.
    - For a specifier that matches no AST node (D9), `` ` [import "${specifier}"]` `` is appended in JavaScript.
    - Example: `no-cycles: src/a.ts → src/b.ts (cycle: src/a.ts → src/b.ts → src/a.ts) — Break the cycle`.
- **Rejected:**
  - Returning `to`, `cycle` and `imports` and placing in JavaScript: it spreads the placement rules over two languages.
  - Returning the whole `Analysis`: a different feature.
  - A `rootOf` export and a `generation` counter (revision 4): they only served per-file project discovery, which is now Out.
- **Cost we're accepting:** the add-on isn't a general Node API.

### D4: Ship as `detangle/eslint`, with the `.node` file inside the existing platform packages

- **Chosen:** a subpath export. The add-on sits at `detangle-<platform>/detangle.node`, next to `bin/detangle`.
- **Rejected:**
  - `eslint-plugin-detangle`, or separate per-platform add-on packages: each new npm package needs its own trusted-publisher setup on npmjs.com, which takes a security-key confirmation per save.
  - One package bundling all 8 add-ons: every install would download all 8.
- **Cost we're accepting:** each platform package roughly doubles, from 3.5–3.8 MB compressed (measured) to an **estimated** ~7 MB, since the add-on contains most of the binary's code. If a compressed platform package goes over 10 MB, D4 is reopened (see Risks).

### D5: One add-on call per lint, shared by both rules

- **Chosen:**
  - `native.js` keeps a `WeakMap` from `context.sourceCode` to a `Map` from Project key to result. ESLint creates one `SourceCode` per pass over a file and gives it to every rule.
  - So when both rules use the same options, the first rule calls the add-on and the second reuses the result. Rules with different options have different keys, so each gets its own result from its own Project.
  - A new pass, whether an edit or a `--fix` pass, is a new `SourceCode` and gets a fresh call.
- **Rejected:**
  - A `(file, text)` memo (revision 1): stale after other files changed.
  - One call per rule (revision 2): doubled parse cost.
  - Keying on `sourceCode` alone (revision 3): the wrong Project's result when options differ.
- **Cost we're accepting:** relies on ESLint sharing one `SourceCode` across rules within a pass. A test asserts it on ESLint 9 and 10.

### D6: The linted file's buffer is authoritative for that file (overlay)

- **Chosen:** `violationsFor(file, text)` does these steps in order.
  - `file` is ESLint's `context.filename`.
  - `text` is `context.sourceCode.text`, from which ESLint has removed any BOM. `extract` ignores a BOM too, so specifiers match either way.
  1. **Freshen** (D7): poll the config stamps; drain the watcher, if it's running; apply what they found.
  2. **Membership:** if `file` isn't a Session member (`Session::contains`):
     - If the file exists on disk, has a source extension, and hasn't been rejected since the last structural change, call `Session::admit(file)`. That's a structural update for that one path (a re-walk, ~30 ms on VS Code), so a new file saved and linted before its watcher event arrives is picked up immediately.
     - If it's still not a member (excluded, generated, outside `dir`), remember the rejection until the next structural change and return no violations. Files that don't exist on disk return no violations without any work.
     - **The Session decides membership, never the overlay.**
  3. **Overlay:** `Session::overlay(file, text)`.
     - Runs `extract` on `text` and compares the result with the file's stored `raw` and parse-error count.
     - If they're equal, returns `false`, with no change.
     - Otherwise it replaces `raw`, re-resolves **only this file**, sets `stamp = None` so the next disk read of it re-parses, and sets `graph_changed`.
     - Other files don't need re-resolving. Resolution depends on the **set** of files and on config files (`package.json` `exports`, tsconfig `paths`). An overlay changes neither, and config files are handled by re-opening.
  4. **Analyse** again only if step 1, 2 or 3 set `graph_changed`.
- **Overlay lifetime:**
  - An overlay lasts until the file is next re-read from disk (on a disk event for it, or during **any** structural update), the next lint of that file, or a re-open.
  - Because every structural update re-reads overlaid files, unsaved imports in **other** open buffers revert to disk content at the next file add or remove, until those buffers are linted again. The linted file itself is never affected, since step 3 runs after step 1.
  - A buffer discarded without saving leaves its overlay in place until one of those events or an ESLint restart.
  - Overlays are never written to the parse cache.
- **Rejected:**
  - Disk only: new unsaved imports would only show after saving.
  - An mtime check of every file on every call: ~25 ms on VS Code.
  - Overlaying files that aren't on disk: the resolver reads the real filesystem.
  - Keeping overlays across structural updates: it would need the overlay texts stored and re-applied, for a case that's already Out (unsaved edits in other files).
- **Cost we're accepting:**
  - the discarded-buffer case;
  - other buffers reverting to disk at structural updates;
  - a brief disk-content window after an external write;
  - one ~30 ms re-walk the first time an excluded file is linted after each structural change.

### D7: Freshness: config polling on every lint, plus a watcher started at open on a background thread

- **Chosen:**
  - **Config polling (always on).** At the start of each call, the Project stats `config_stamps()`: a handful of files, microseconds. If any stamp changed, it re-opens (D8).
    - This alone keeps the config fresh and lets `Broken`/`Failed` recover, whether or not a watcher is running.
    - It also covers a `config` file outside the root, which the watcher can't see.
  - **The watcher.** `open` starts a thread that creates the recursive watcher on the root **before** the scan begins. The scan runs on the calling thread at the same time; watcher setup (Linux ~46–96 ms in a VM, macOS ~1.4 ms) overlaps the ~170 ms scan.
    - The watcher belongs to the Project **wrapper**, not the Session, so it survives re-opens and panics (D8).
    - Each call drains it with **no quiet period**:
      - The quiet period exists to coalesce editors' multi-write saves before a TUI redraw. Here, the worst case is a half-written file parsed once. Its next write event re-marks it dirty, and the next lint corrects it.
      - Atomic saves are ordinary modifications (Glossary: `Session`).
      - A burst such as a branch switch may span several drains. Each drain applies at most one `Session::update` and at most one re-open, so a burst costs a few updates, each bounded by a full scan (≤ 250 ms on VS Code), not one per event.
  - **Which watcher events count as a config change:** a config-named event (Glossary: watcher) counts only if the file is in `config_stamps()`, is the `config` file, or sits in a **member directory**, so nested workspace `package.json`/`tsconfig.json` files count. Config-named files in gitignored output such as `.next/package.json` or `dist/` don't. The event filter keeps skipping `node_modules`, `.git` and `target`.
  - **Lost events** (`need_rescan()`): the next drain re-opens the Project. That's one ≤ 250 ms rebuild, and it catches nested config changes too. Lost events are rare, and a partial rescan could miss those nested config changes.
  - **Without a watcher** (setup failed, inotify exhausted, or the thread died): one warning (D12); config polling and the overlay keep working. Saved changes to other source files and new files are picked up only when those files are linted themselves: step 2 admits new files, and the overlay covers the file's own imports. They aren't seen from other files' lints.
- **Rejected:**
  - A lazy watcher, started on the first re-lint (revisions 2–3). It needed lint tracking, a 1 s heuristic and a catch-up, and three cold reads kept finding its corners.
  - Relying on the watcher alone for config and recovery (revision 4): a failed watcher made `Broken`/`Failed` permanent.
  - Filtering the watch on Linux to skip `node_modules`: measured slower (~125 ms vs ~46 ms for the same tree).
  - Starting the watcher on ESLint's thread: adds 46–96 ms to the first lint on Linux.
  - Retrying watcher creation: the usual cause (exhausted inotify watches) doesn't go away within a process.
- **Cost we're accepting:**
  - Every Project watches, including `eslint .` and each `--concurrency` worker:
    - on Linux, ~15–17 µs of background CPU per directory under the root, `node_modules` included (a very large `node_modules` of ~50k directories takes ~0.8 s and 50k inotify watches per Project);
    - on macOS, one FSEvents stream;
    - on Windows, one recursive directory handle (cost logged by CI, not yet known).
  - **Startup race:** an edit made during the first ~100 ms after open, to a file the scan has already read in a directory not yet watched, is missed until that file's next event or lint.

### D8: Lifecycle. One Project per `(dir, config, mode)`, a state machine, panic recovery and cleanup

- **Which directory to scan (`native.js`):**
  - `dir` is the rule option `dir`, resolved against ESLint's `context.cwd`, or else `context.cwd` itself. That is exactly `detangle check <dir>`, or `detangle check .` run from where ESLint runs.
  - The root is `find_root(dir)`, as in the CLI, and the watcher watches the root.
  - Editors set ESLint's `cwd` per workspace folder, so each folder of a multi-root workspace gets its own Project. A project nested below the `cwd` that has its own `detangle.toml` needs `dir` (or a separate ESLint config with `dir`); the docs say so.
  - **Refusal:** if the root would be the filesystem root or the user's home directory, `open` doesn't scan. The Project is `Broken` with the problem `refusing to scan <path>; set the "dir" option`.
- **Cache:**
  - Projects live in a module-level `Map` keyed by `(canonical dir, canonical config path or "", mode or "")`, canonicalized with Node's `fs.realpathSync.native`.
  - A path that can't be canonicalized (a missing config) is used as given; `open` then fails with that error, and the Project is `Broken`.
  - Keys are fixed for the life of the process. Nothing is evicted, and nothing needs to be: one ESLint thread sees one key per distinct option set in its config, usually one. Each worker thread has its own module instance.
- **States** (the Rust wrapper; the watcher lives in the wrapper and survives every transition except `Closed`):

  | State | Holds | Each call returns |
  | ----- | ----- | ----------------- |
  | `Ready` | Session and Analysis | violations |
  | `Stale` | the last good Session and Analysis, plus a config error | violations. Overlays and drains keep applying to the last good Session. The error is in `problems` |
  | `Broken` | a config error (or a refusal), no Session | no violations; the error in `problems` |
  | `Failed` | a panic message, no Session | no violations; the panic in `problems` |
  | `Closed` | nothing | the problem `detangle project closed` |

  | From | Event | To |
  | ---- | ----- | -- |
  | (none) | `open`: succeeds / config error or refusal / panics | `Ready` / `Broken` / `Failed` |
  | `Ready`, `Stale` | config change (polled stamp, or a qualifying watcher event, or lost events) → re-open: succeeds / fails / panics | `Ready` / `Stale` (keeps the last good Session) / `Failed` |
  | `Broken` | config change → re-open: succeeds / fails / panics | `Ready` / `Broken` / `Failed` |
  | `Ready`, `Stale` | a panic in any call | `Failed` (the Session is dropped) |
  | `Failed` | a config change, or a drained batch that touches a Session member path → re-open: succeeds / fails / panics | `Ready` / `Broken` / `Failed` |
  | any | `close()` | `Closed` |

  - A re-open happens at most once per call. From `Failed` it needs a config change or a change to a source file the old Session held, not churn in unrelated files. So a panic that recurs every time costs one re-open per relevant change, never one per lint.
  - During a re-open from `Ready`/`Stale`, the old Session stays alive until the new one is ready (Success criteria: memory).
- **Panics:**
  - Every export wraps its body in `std::panic::catch_unwind`. A panic moves to `Failed`, and the call returns normally with the panic in `problems`. Throwing is avoided (D12).
  - All profiles keep `panic = "unwind"`, and `napi/Cargo.toml` says why.
- **Background threads:**
  - The watcher's thread: if it dies, `drain()` returns `Disconnected`. The Project drops the watcher, emits a warning (D12), and continues without it (D7).
  - The **dropper thread** is one per Project. It receives replaced Analyses, and the old Session after a re-open, and frees them off ESLint's thread. If sending to it fails, the value is freed inline.
- **Cleanup:** an env cleanup hook (`napi_add_env_cleanup_hook`) runs when a worker thread or the main environment is torn down. Whether Node runs it under `process.exit()` isn't verified yet (the teardown test checks). Either way the main-env path only signals and leaks, so skipping it loses nothing. Nothing runs when the process is killed.
  - In a **worker** env, it stops the watcher and joins its thread (waiting for an unfinished setup, ≤ its setup time), closes the dropper channel, joins that thread, and frees the Projects.
  - In the **main** env (process exit), it signals the threads to stop but **doesn't join** them, and leaks the Projects and the dropper's queue, like the CLI's `one_shot`. The OS reclaims everything at exit, so a short `eslint .` run never waits on watcher setup or freeing.
- **Rejected:**
  - Per-file project discovery (`rootOf` of each linted file, an ancestor watch, root switching; revisions 3–4). It made one Project per directory in unmarked trees, scanned `$HOME` via a stray `package.json`, and leaked or duplicated Projects when marker files changed inside a root.
  - The config file's directory as `dir` (revision 3): wrong when the config lives in a subdirectory.
  - Re-opening on every call after a panic: a cold scan per keystroke.
  - A time-based backoff: it can retry into the same panic over and over.
  - Never re-opening in-process: one bad save disables the rules for the editor session.
  - A process-global Session shared across worker threads.
- **Cost we're accepting:**
  - Nested projects not at ESLint's `cwd` must set `dir`.
  - A monorepo without a root `detangle.toml`, linted from the repo root, is one Project rooted at the root `package.json`. That's the same as `detangle check .` there, so no violations are lost relative to the CLI.
  - With `--concurrency N`, the scan runs N times (N × ~170 ms of CPU and N × ~172 MB on VS Code), and each worker holds its own watcher.

### D9: Two rules split by severity, and a placement table

- **Rules:**
  - `detangle/errors` reports detangle `error` violations.
  - `detangle/warnings` reports `warn` and `info`.
  - `recommended` sets them to ESLint `"error"` and `"warn"`.
  - Both take `{ dir?: string, config?: string, mode?: string }`. Using the same values for both is recommended, since different values mean two Projects (D5).
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
  - A circular rule therefore shows in **every** file on the cycle whose edge matches the rule, each on its import of the next file, just as `detangle check` lists them.
- **Node matching (JavaScript):**
  - A report goes on every node in `L` whose **string-literal** source equals a specifier. The node kinds are:
    - `ImportDeclaration`;
    - `ExportNamedDeclaration`/`ExportAllDeclaration` with a source;
    - `ImportExpression`;
    - `CallExpression`s whose callee is `require` or whose callee text (`sourceCode.getText(callee)`, for example `module.require`) equals an `exoticRequire` name;
    - `TSImportEqualsDeclaration`;
    - `TSImportType` (the typescript-eslint 8 shape, with a string literal argument).
  - A template literal without expressions counts as a literal. Other non-literal sources never match; detangle doesn't resolve them either.
  - A specifier that matches no node (for example `/// <reference>`, SFC template imports, Angular `templateUrl`) is shown at line 1, with `[import "<specifier>"]` appended to the message.
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
  - **`problems`** are persistent state problems: add-on unavailable (D11), a config error or refusal (`Stale`, `Broken`), a panic (`Failed`), or `Closed`. They're returned on every call while the state lasts, and shown at line 1 of every linted file.
    - Within one pass, only the **first rule to run** for that `(sourceCode, key)` reports them, at that rule's severity. The shared result (D5) records that they've been reported.
    - Which rule runs first follows the order in the user's ESLint config. `configs.recommended` lists `detangle/errors` first, so under it problems are errors.
    - An add-on that can't load in CI therefore fails the lint. That's intended: a CI config that enables the rules but can't run them is misconfigured, and the message says to run `detangle check`.
  - **Warnings** are one-time notices that don't block results: the watcher failed to start, or its thread died. `native.js` emits them with `process.emitWarning`, once per Project; editors show them in the ESLint output channel. They aren't `problems`, because in CI they'd flood every file while results are still correct.
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
| inotify watches exhausted (Linux, big `node_modules` or many workers) | Watcher fails: one warning. Config polling and the overlay keep working; other files' saved changes are seen when those files are linted | D7, D12 |
| `eslint --cache` | May miss cross-file violations | D10 |
| Monorepo, linted from the repo root | One Project for the repo, the same as `detangle check .` there | D8 |
| Nested project with its own `detangle.toml`, not at ESLint's `cwd` | Scanned as part of the `cwd`'s root unless `dir` points at it | D8 cost |
| Multi-root editor workspace | One Project per workspace folder (the editor sets ESLint's `cwd` per folder) | D8 |
| ESLint run from `$HOME` or `/` with no marker below | `Broken`: "refusing to scan …; set the "dir" option" | D8 refusal |
| Config change (root files, or the `config` file anywhere) | Detected by polling at the next lint, with or without a watcher. Re-open ≤ 250 ms (TOML). Success: `Ready`. Failure: `Stale`, with the last good results plus the error | D7, D8 |
| Nested workspace `package.json`/`tsconfig.json` edit | A config change via the watcher (it's in a member directory), so one re-open | D7 |
| `.next/package.json`, `dist/package.json` written by a dev server | Ignored: not a member directory | D7 |
| Config invalid on first open | `Broken`: a line-1 problem on every file; recovers at the first lint after the config is fixed | D7 polling, D8 |
| `open` panics | `Failed`; re-opens after a config change or a change to a source file | D8 |
| JavaScript rules config, or Vite/webpack evaluation | The add-on spawns `node` synchronously, the first `node` on the ESLint process's `PATH`. Editors launched from a GUI may have a different `PATH`; if `node` isn't found, the Project is `Broken` with that error | Unchanged code path; costs time only at open |
| Edit to a file a JavaScript rules config imports | Not detected | Out |
| Linted file not in the Session (excluded, generated, outside `dir`) | One `admit` check (~30 ms) the first time after each structural change, then no reports | D6 step 2 |
| New file saved and linted before its watcher event | Admitted by step 2 and reported at once | D6 |
| Untitled or unsaved new file | No reports | D6 step 2 |
| Atomic save (temp file + rename) | An ordinary modification, not structural (2–5 ms in `watch`) | Glossary: `Session` |
| Edit during the first ~100 ms after open | Possibly missed until that file's next event or lint | D7 startup race |
| Unsaved import edit | Checked live | D6 |
| Unsaved imports in another open buffer, then any file is added or removed | That buffer reverts to disk content until it's linted again | D6 cost |
| External write to a file with unsaved edits | Disk content until the next lint of the buffer | D6 |
| Buffer discarded without saving | Overlay stays until the file is re-read from disk, linted again, re-opened, or ESLint restarts | D6 cost |
| `notify` reports lost events | Re-open at the next lint | D7 |
| Branch switch | A few updates across the next lints, each ≤ 250 ms on VS Code; a re-open if config files changed | D7 |
| Processor virtual filenames (`README.md/0.js`) | Don't exist on disk, so no reports and no `admit` | D6 step 2 |
| Buffer with syntax errors | ESLint fails to parse and never runs the rules | ESLint behavior |
| Vue/Svelte through their ESLint parsers | `text` is the full SFC source, which `extract` handles via `sfc.rs` | Same input the CLI reads |
| Specifier with no matching AST node | Line 1, with `[import "…"]` in the message | D9 |
| Same specifier imported twice | Both nodes are reported | D9 |
| `require(variable)` | Never matched | D9 |
| Folder/group violation without an import | Not shown | Out |
| Add-on missing or fails to load | A `problems` line on every file (an error under `recommended`) | D11, D12 |
| Rust panic | `Failed`; a `problems` line; one re-open per relevant change | D8 |
| Stack overflow or abort in Rust | The ESLint process dies | Can't be caught; D1 |
| ESLint worker thread exits | The cleanup hook stops and joins the threads and frees the Projects | D8 |
| Process exit (`eslint .` finishing, or `process.exit()`) | The cleanup hook, if Node runs it, signals the threads and leaks the rest, like `one_shot`; otherwise the OS reclaims everything | D8 |
| Process killed | Nothing runs; the OS reclaims everything | — |
| Two `--concurrency` workers write the parse cache | Separate temp files per thread; the last rename wins; each file is complete | Files: `scan.rs` |
| Symlinked paths from ESLint | `dunce::canonicalize` before lookup | The Session stores canonical paths |
| Case-mismatched paths | macOS: canonicalize returns the on-disk case (checked with native `realpath` on APFS). Windows: covered by the CI case test | Same |

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
- **The overlay diverges from a fresh scan.**
  - Caught by `scan.rs` unit tests, each asserting `graph_changed` and that the Session equals a fresh scan:
    - overlay equal to the disk version;
    - overlay adds an import;
    - overlay, then a disk save with the same text;
    - overlay, then the disk reverted;
    - overlay, then a structural update (the overlay is dropped), then another overlay;
    - `admit` of a new file.
- **Rust panics become ESLint crashes.** Caught by `catch_unwind`, the panic test, and a local run of the rules over the VS Code and excalidraw clones before each release. The clones aren't in CI. Not caught: aborts, OOM, stack overflow.
- **Watcher cost differs on native Linux and Windows.** The Linux numbers come from a Docker VM, and Windows isn't measured.
  - Caught by: `eslint.test.mjs` logs watcher setup time for its fixture on each CI OS, and the benchmark's Linux row is re-run on a native Linux machine before release.
  - Not caught: repos with very large `node_modules`, where setup is ~0.8 s of background CPU and may exhaust inotify watches. That degrades freshness (D7), not correctness at open.
- **musl or Windows ARM add-on builds fail.** The release matrix builds them on native runners; CI only covers x64 Linux/Windows and arm64 macOS.
  - Mitigation: the smoke `require` on each release runner (musl in an Alpine container), and the check that the musl CLI binary is still static, both before anything is uploaded.
  - Not smoke-tested: x86_64 macOS, the same as its binary today.
- **Thread teardown bugs** (a watcher outliving its env). Caught by: a test that opens a Project in a `worker_threads` Worker, terminates the Worker, and asserts the process exits cleanly. Plus the `eslint .` exit-time check in the benchmark.
- **Stale results in editors.** Lost events are handled (D7). Events `notify` drops *without* signalling it aren't caught; they self-heal on the next save of the affected file, a re-open, or an ESLint restart.
- **Package size.** If a compressed platform package goes over 10 MB (estimated ~7 MB), D4 is reopened. The alternative is one extra `-node` package per platform, with its trusted-publisher setups.
- **The lib split changes the binary's output.** Caught by `scripts/cmp-output.sh` against the `22899e5` binary on VS Code and excalidraw.
  - The `CacheArgs` change is the only logic change in the move, and it's covered by the existing `--cache` tests.
  - The two CLI fixes (lost events in `watch`, cache temp names) change no output.
- **ESLint API changes.** CI tests ESLint 10 (current: 10.11) on all three OSes and ESLint 9 on Linux, all on Node 22. The shared-`SourceCode` assumption (D5) has its own test.
- **Latency or memory misses its targets.** `scripts/bench-eslint.mjs` before release. A miss blocks the release unless it's explicitly accepted.

## Sign-off

- [x] Cold Read 1 — revision 1 → addressed in revision 2
- [x] Cold Read 2 — revision 2 → addressed in revision 3
- [x] Cold Read 3 — revision 3 → addressed in revision 4
- [x] Cold Read 4 — revision 4 → addressed in revision 5
- [ ] Cold Read 5 — on revision 5
- [ ] Human review

## Cold Read Findings — 2026-09-27

Three parallel subagents, each limited to one read of this doc (no codebase, no conversation).

### Gaps

- Context: detangle, violation, rule, severity and orphan rule are never defined. Internal terms (`stamp`, `raw`, `graph_changed`, `cmp.sh`, `one_shot`, fixtures) aren't explained. References with no referent: "rule 3", "old handoff notes", "earlier draft", "It already is". No success criteria or rollout plan.
- Contract: three `violationsFor` signatures (D3, napi section, `lib.rs`). `mode?` is undefined. `find_root` has no home in JS. The `config` option's base path is unspecified.
- The Violation shape can't support D9: no `imports` field, no `from` field, and no mapping from a cycle to an import. Matching of duplicate specifiers, non-literal `require`/`import()`, and `import()` in type positions is unspecified.
- Overlays: re-resolving dependents when an overlay adds or removes a file, whether overlays survive a config rebuild, how a new file is told apart from an excluded one, why dropping the quiet period is safe. D6 and the edge-case table disagree on how long an overlay lasts.
- D7: can `check -f json` place reports? Binary lookup order. The real triggers for the fallback.
- Config errors: invalid on the first open, which rule reports them, and exception versus violation.
- Unmeasured: "<1 ms", import-edit cost, first-call and config-reload stalls, "~7k stats", "5–8 MB", "~20 ms free", typescript-eslint parse cost.

### Risks

1. The `native.js` memo, keyed on `(file, text)`, returns stale results after other files change, and ignores `config`.
2. `eslint --cache` hides cross-file violations. Other files open in the editor aren't re-linted.
3. Which files in a cycle, folder or group violation get a report is undefined, and conflicts with the "grouped by `from`" drift test.
4. No lifecycle: watchers start for one-shot runs and for every worker; no `close()`, cleanup hook or cache eviction; threads can outlive their napi environment.
5. `catch_unwind` needs `panic = "unwind"`. Poisoned-Session detection and where recovery happens are unspecified.
6. The drift test checks messages as a set, not their locations.
7. ESLint 10 may already have shipped.

### Verdict

- Explain-back: NEEDS REVISION. The intent was understood; the contracts are inconsistent.
- Implementation: NEEDS REVISION. 17 questions for the author.
- Critique: RISKS FOUND. Memo staleness, cycle attribution, lifecycle.

### Resolution (revision 2)

- Context and terms: new Background and Glossary sections; references with no referent removed; Success criteria added; rollout is 0.2.0, marked experimental.
- Contract: one signature (D3). `mode` is the CLI's `--mode`. `rootOf` is exported by the add-on. `config` is resolved against ESLint's `context.cwd`.
- Placement: the D3 contract returns `specifiers` computed in Rust. D9 has a placement table per violation kind. Circular violations are per edge in the code, so every file on a cycle is covered.
- Overlays (D6): the Session decides membership. Only the overlaid file is re-resolved, because resolution depends on the file set, not on contents. Overlays are dropped on a config re-open. How long an overlay lasts is stated once, as "indefinitely".
- The quiet period: why dropping it is safe is in D7.
- Fallback dropped (D11). Config errors are D12.
- Measured: overlay parse (~3 ms for 1.4 MB), import edit (57–61 ms), a stat sweep (~25 ms), ESLint 10.11 current. Still unmeasured: add-on size (Risks), typescript-eslint parse (claim removed).
- Risk 1: memo removed (D5). Risk 2: D10 plus positioning; re-linting other open files is Out. Risk 3: D9 table; drift test compares against an oracle. Risk 4: D7 (lazy watcher), D8 (cache, state machine, cleanup hook, dropper thread). Risk 5: D8. Risk 6: the drift test compares `(file, line, message)`. Risk 7: CI tests ESLint 10 and 9.

## Cold Read 2 Findings — 2026-09-27

Same setup: three parallel subagents, one read of revision 2 each.

### Risks (blocking)

1. **D5 contradicts D7.** Two rules make two calls per file, so "linted a second time" fires on the first file of every `eslint .` run and in every `--concurrency` worker. `eslint --fix` re-lints each fixed file up to 10 times in one process.
2. **D8 contradicts D12.** Keeping the last good Session alongside an error needs a third state. "The config file's stamp" is undefined. A panic that happens every time causes a cold re-open on every call. Watcher- and dropper-thread panics aren't handled. Problems are reported "once per Project" in one place and "by each rule" in another.
3. **The root comes from the linted file, not the config.** Without `detangle.toml`, a monorepo gets one root per package and loses cross-package violations silently. The cache key uses the raw `config` string. Nothing checks that the add-on and `detangle check` use the same root.

### Gaps

- **Numbers:** the 1.4 MB target (two calls × 3 ms > 5 ms). "CLI total 219 ms" isn't in the Context section. The ≤ 2 ms process start isn't isolated. Unmeasured: median-file cost, walk cost, ~20 ms free time. The benchmark script isn't in the repo. What a missed target means is undefined.
- **Contract:** folder/group violations without `to`/`imports`. Exotic-require names aren't in the contract. Is `raw` equal to the AST literal? `@scope/name` and built-in folders. `rootOf` with no marker file, and the JS cache of `rootOf` never refreshed. Does graph JSON expose per-edge specifiers for the oracle?
- **Ordering:** whether the watcher start happens before or after the membership check. A disk event on a file that still has unsaved edits. package.json/tsconfig edits before the watcher starts.
- **Build/CI:** the musl `-crt-static` flag must apply only to the add-on. `cargo install` with a workspace. How ESLint 9 and 10 are installed. The Windows case test isn't listed. Where the canonical `cmp.sh` lives.
- **Terms:** `dir`, parse cache, structural path, `allowed`/`reachable`/dependents count, `one_shot`, `--concurrency`. `node` on a GUI editor's `PATH`. No problem statement.

### Verdict

- Explain-back: NEEDS REVISION. Intent is clear; D5/D7 and D8/D12 contradict each other; some numbers don't add up.
- Implementation: NEEDS REVISION. 14 questions; the first step (the lib split) is clear.
- Critique: RISKS FOUND. The watcher trigger, the state machine, and root selection.

### Resolution (revision 3)

- **Risk 1:** D5 now shares one call per `SourceCode`. D7's re-lint needs ≥ 1 s since the file's previous lint. Tests assert that `eslint .` and `eslint --fix .` start zero watchers.
- **Risk 2:** D8 has four states (`Ready`, `Stale`, `Broken`, `Failed`) and a transition table.
  - "Config change" is defined as the watcher's config-file list plus the `config` file, and `config_stamps()` is compared at the catch-up.
  - After a panic, at most one re-open per watcher batch. The watcher lives in the wrapper.
  - A dead watcher thread gives a warning plus overlay-only freshness. The dropper thread falls back to freeing inline.
  - Problems vs warnings are split in D12.
- **Risk 3:** `dir` comes from the `root` option, else the `config` directory, else `find_root(file)`. The cache is keyed by canonical paths. A monorepo fixture is added.
- **Numbers:** "lint" is defined; the targets are consistent (one parse). Spawn measured at 3.7 ms, walk at ~30 ms, CLI total 219 ms. The median-file parse is marked as an estimate for the benchmark to measure. `scripts/bench-eslint.mjs` is committed, and a missed target blocks the release unless explicitly accepted.
- **Contract:**
  - `exoticRequire` is in the result.
  - Cooked specifier strings equal ESTree `Literal.value` (checked in `extract`).
  - `folderTarget` covers `@scope/name` and built-ins, from `graph.rs`.
  - Folder/group violations without an import are Out.
  - `rootOf` always returns a path; its cache is cleared on a re-open.
  - The oracle uses `dependencies[].specifier`, with its limitation stated.
- **Ordering:** in D6, tracking and catch-up run before the membership check. External writes to a file with unsaved edits have an edge-case row.
- **Build/CI:**
  - The musl flag applies only to the add-on step, with a check that the CLI binary is still static.
  - `cargo package` was probed: `[workspace]` is dropped, and CI builds the `.crate` alone.
  - ESLint 9 comes in through an npm alias.
  - The Windows case test is listed.
  - `scripts/cmp-output.sh` is committed.
- **Terms:** Problem section added. Glossary adds root/`dir`, parse cache, exotic require, rule kinds, structural update, `one_shot`, `--concurrency`. `PATH` for `node` in GUI editors is an edge case.

## Cold Read 3 Findings — 2026-09-27

Same setup: three parallel subagents, one read of revision 3 each.

### Risks (blocking)

1. **Lint tracking only counts Session members.** New files never count as re-lints, and in `Broken`/`Failed` nothing does, so the watcher never starts and a config broken at open can't recover in an editor.
2. **The D5 shared result is keyed only on `sourceCode`.** When the two rules have different options (different Projects), the second rule reuses the wrong Project's result.
3. **JS can't see a re-open.** D8 clears the `rootOf` cache on a re-open, but the D3 result doesn't report re-opens.
4. **A root added later is never picked up.** A `detangle.toml` added above per-package roots lies outside every watched tree. And with no eviction, a superseded Project would leak.
5. **`dir` and root are ambiguous.** A `root` option without a marker file makes `find_root` walk further up. "All three give the same root" is false when the config file sits in a subdirectory. Which directory the watcher watches is unstated.
6. **Watcher filters aren't specified.**
   - The watcher already skips `node_modules`, `.git` and `target` events (`src/watch.rs`), but the doc doesn't say so, or whether those directories are still *watched*.
   - A config change is matched by file name anywhere under the root, so nested `package.json` and `.env*` edits force re-opens.
   - `notify`'s lost-event signals (macOS rescan flag, inotify overflow) are ignored.

### Gaps

- **Targets whose basis doesn't cover the work:**
  - First re-lint: re-resolution, re-analysis and the Linux watcher start aren't counted.
  - Import edit: 57–61 ms measured against a ≤ 60 ms target, and the ~20 ms free-time saving was measured for `one_shot`.
  - Config reload: the `node` spawn, and freeing the Session.
  - Branch switch: drains spread over several lints.
  - Unmeasured: the 1 s basis, Session memory (and N× under `--concurrency`), add-on size.
- **Contract:**
  - The message format, which the oracle needs.
  - The folder-subtree rule for targets that are subfolders of `F`.
  - The group `imports` entry shape.
  - `filename` vs `physicalFilename`, and BOM.
  - Whether the parse cache is used, and which `check` options `open` inherits.
  - The `FileReport`/`Changes`/`Disconnected` shapes.
  - napi-rs and Node-API versions.
  - `Project::open` vs clap types.
  - `TSImportType` shape, and dotted exotic-require names.
  - What `Stale` applies overlays and drains to.
  - A panic during `open`.
  - Cleanup-hook cost at process exit vs `one_shot`.
- **Context:** the `22899e5` baseline, the `conditions` fixture, "the standing rule", and "configured groups or Nx projects" aren't introduced.

### Verdict

- Explain-back: NEEDS REVISION. Three contract holes (new-file tracking, re-open signalling, shared-result key).
- Implementation: NEEDS REVISION. 17 questions.
- Critique: RISKS FOUND. Roots added later, the catch-up cost basis, watcher filters.
- Trend: the core (D3, overlay, placement) held. The lifecycle around the lazy watcher keeps producing corner cases. Before revision 4: measure the Linux watcher setup and consider starting the watcher at open.

### Resolution (revision 4)

- **Measured first:** recursive watcher setup is ~1.4 ms on macOS and ~46 ms (VS Code `src/`) or ~96 ms (with a typical `node_modules`) on Linux. Filtering out `node_modules` was slower (~125 ms). The maintainer chose to start the watcher at open, on a background thread.
- **D7 rewritten.** Lint tracking, the 1 s heuristic and the catch-up are gone. That resolves Risk 1 (new files and `Broken` projects now always have a watcher) and the before-watcher gaps.
- **Risk 2:** the D5 result is keyed on `(sourceCode, Project key)`.
- **Risk 3:** a `generation` counter in the D3 result.
- **Risk 4:** an ancestor-marker watch for `rootOf`-derived Projects, with superseded Projects closed.
- **Risk 5:**
  - The option is renamed `dir` and is exactly `detangle check <dir>`.
  - With only `config`, `dir` is ESLint's `cwd`.
  - The root is always `find_root(dir)`, and the watcher watches the root.
- **Risk 6:**
  - The glossary states that ignored directories are still OS-watched.
  - One re-open per drained batch.
  - `need_rescan()` triggers `Session::rescan()`. This also fixes `detangle watch`, whose kind filter drops those events today.
- **Numbers:**
  - The first-re-lint target is removed.
  - Import edit target set to ≤ 65 ms, without counting the unmeasured off-thread saving.
  - The config reload target states the `node` spawn.
  - Watcher setup and memory targets added: 172 MB RSS measured.
  - Branch switches bounded per drain.
  - Package size measured (3.5–3.8 MB compressed), with a reopen threshold of 10 MB compressed.
- **Contract:**
  - The message format is specified.
  - The folder row is restated as the construction read backwards.
  - The group `imports` shape comes from the CLI's JSON.
  - `context.filename`; BOM stripped by ESLint.
  - The parse cache comes from the config only.
  - `CacheArgs` becomes an explicit parameter: the only logic change in the move.
  - napi-rs 3, Node-API 8.
  - Dotted exotic names are matched by callee text.
  - typescript-eslint 8 `TSImportType`.
  - `Stale` applies overlays and drains; `open` panicking goes to `Failed`.
  - The cleanup hook leaks at process exit, like `one_shot`.
- **Context:** the glossary adds groups, the baseline, fixtures, and "verify against the real tool". The drift test compares a multiset including columns.

## Cold Read 4 Findings — 2026-09-28

Same setup: three parallel subagents, one read of revision 4 each.

### Risks (blocking)

1. **Cache and Project lifecycle.**
   - "Both caches are dropped when `generation` changes" reads as dropping the Project `Map`, which would orphan every Project with its watcher still running.
   - A marker file (`package.json`/`detangle.toml`) created or deleted **inside** a case-3 root re-opens the old Project instead of closing it, and a second Project opens for the same tree.
   - `rootOf` with no marker gives one Project per directory. A stray `~/package.json` makes `$HOME` the root.
   - The ancestor-event filter is unspecified, and there's no `Closed` state.
2. **With no watcher, recovery is impossible.** If the watcher failed to start or died, `Broken`/`Failed` are permanent and config edits are ignored. The "only the linted file stays fresh" row is wrong: new files, other files and config edits are lost too. A `config` file outside the root is never watched.
3. **Config-named files in build output** (for example `.next/package.json`) aren't in the watcher's ignore list, so dev servers would force cold re-opens on nearly every lint.

### Confirmed in code during aggregation

- A structural update re-reads overlaid files from disk (`stamp = None` fails the reuse check), so other buffers' overlays are dropped before any disk event for them. The linted file is unaffected, because step 3 re-applies its overlay.
- The parse cache's temp file is named by process id (`{FILE}.{pid}`), so `--concurrency` workers in one process can collide writing it.
- Refuted: "atomic saves cost a structural re-walk". `Session::update` classifies changes by checking the filesystem, not by event kind. A rename onto an existing member isn't structural. The doc should say so.

### Gaps

- **Race and teardown:** a new file linted before its create event arrives. Exit blocking on watcher setup.
- **Environment:** case 2's dependence on ESLint's `cwd`. Silent cross-package loss without a root `detangle.toml`.
- **Problems:** problem severity depends on config order. An unavailable add-on makes every CI file red.
- **Lost events:** they compare only root config stamps.
- **Undefined:** `configs.recommended`, `FileReport`/index shapes, "rules defined in the ESLint config", and which edit the import-edit target covers.
- **Memory:** two Sessions during a `Stale` re-open, and the index, aren't counted.
- **Measurement and CI:**
  - The Linux numbers come from a Docker VM, and Windows watcher cost is unmeasured.
  - Node versions in CI (ESLint 10 needs Node ^20.19 / ^22.13 / ≥24).
  - Smoke tests on cross targets: a glibc `node` can't load a musl `.node`, and x86_64 macOS runs on an arm64 runner.
  - The `detangle watch` fix has no CLI test.
- **Claims:** D11's "second JS implementation" argument is weakened by the oracle. The ~7 MB package size is an estimate.

### Verdict

- Explain-back: NEEDS REVISION. Lifecycle holes only.
- Implementation: NEEDS REVISION. 12 questions; the core was judged sound.
- Critique: RISKS FOUND. Nested markers, recovery without a watcher, and atomic saves (refuted).
- Trend: the core (D3, overlay, placement, watcher at open) held. The holes cluster in automatic root discovery (added in revisions 3–4) and in recovery that depends only on the watcher.

### Resolution (revision 5)

- **Risk 1:** per-file project discovery is dropped (maintainer's decision). `dir` is the `dir` option or ESLint's `cwd`, and there's one Project per `(dir, config, mode)` per thread, fixed for the process. That removes `rootOf`, the ancestor watch, root switching, `generation`, one-Project-per-directory and the leaks. A `$HOME`/`/` root is refused, and a `Closed` state is added.
- **Risk 2:** every lint polls `config_stamps()` (including a `config` file outside the root). Recovery from `Broken`/`Failed` and config freshness no longer depend on the watcher. Without a watcher, saved changes in other files are seen when those files are linted; the edge-case row is corrected. `Failed` re-opens only on a config change or a change to a member path.
- **Risk 3:** config-named watcher events count only in member directories, so `.next/package.json` is ignored.
- **Confirmed code issues:**
  - Structural updates dropping other buffers' overlays is documented in D6 and Out.
  - The cache temp file gets a per-thread name.
  - Atomic saves: the glossary states that changes are classified by checking the filesystem.
- **Gaps:**
  - New-file race: `Session::admit` in D6 step 2.
  - Exit: the main env doesn't join threads.
  - `cwd` per workspace folder is stated.
  - Problem severity follows rule order; `recommended` lists `errors` first.
  - CI failure when the add-on is unavailable is intended.
  - Lost events re-open.
  - Defined: `configs.recommended`, `FileReport` and the index, the Out wording, and the two import-edit targets (unsaved ≤ 30 ms, saved elsewhere ≤ 65 ms).
  - Memory during a re-open (~2.4× briefly).
  - CI logs watcher setup per OS, and Linux is re-measured natively.
  - Node 22 in CI (ESLint 10 needs ^20.19/^22.13/≥24).
  - Musl smoke test in Alpine; x86_64 macOS not smoke-tested.
  - A `watch.rs` unit test for the lost-events fix.
  - D11's argument rewritten (production copy vs test oracle).
  - Package size labeled as an estimate.
