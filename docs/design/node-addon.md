# Feature: ESLint rules backed by a native Node.js add-on

Status: **draft, revision 3** (after Cold Reads 1 and 2; their findings are at the end). No code until the next Cold Read and sign-off. Owner and sign-off: the maintainer.

## Problem

detangle's rules run today only when someone runs `detangle check`, `detangle watch` or the explorer. So a developer sees a forbidden import or a new cycle at CI time, or not at all, and not while writing the import.

Editors already surface ESLint results inline as you type. Putting detangle's violations there needs a way to answer "what's wrong with this file?" synchronously, in milliseconds, inside a process that lives for hours. Running the CLI once per file can't do that on large repos (see Context).

This is a maintainer-driven bet, not a response to user requests. That's why it ships as **experimental**.

## Background

**detangle** is a Rust CLI, also shipped on npm with a small Node.js API, that scans a JavaScript/TypeScript project, builds its import graph and checks architecture rules against it. The rules come from `detangle.toml`, or a JavaScript rules config it imports.

### Glossary

| Term | Meaning |
| ---- | ------- |
| module | A source file in the graph, identified by its path relative to the project root (for example `src/a/b.ts`). npm packages and Node built-ins are modules too. |
| rule | An entry in the config, with a name, a severity (`error`, `warn`, `info`, `off`) and an optional `comment`. Kinds: `[[forbidden]]` (a matching import is a violation); `allowed` (every import must match some `allowed` entry, or it's reported as `not-in-allowed`); `[[required]]` (matching modules must import something); plus module-only conditions inside `forbidden`: `orphan` (no imports in or out), `reachable` (reachable from given entry points, or not), and dependents count (`module = { … }` with more or fewer than N importers). `off` rules are never evaluated. |
| violation | One rule match. Its `scope` is `module`, `folder` (the graph of directories) or `group` (the graph of configured groups or Nx projects). It has a `from` node, an optional `to` node (none for rules about a node alone: orphan, reachable, dependents count, `required`), a `cycle` (for circular rules) and, at group scope only, `imports`: the module-level imports behind it. |
| circular violation | Reported **per import edge**: every edge that lies on a cycle and matches the rule yields its own violation, with `from` = that edge's importing file. So each file in a cycle gets its own violation for its import of the next file. |
| root, `dir` | `detangle check <dir>` scans `dir`. The **root** is `find_root(dir)`: the nearest ancestor of `dir` with `detangle.toml`, else the nearest with `package.json`, else `dir`. Module ids are relative to the root. |
| `Session` (`src/scan.rs`) | The scanned files: each file's extracted import strings (`raw`), their resolved targets, the parse-error count, and a `stamp` (mtime + size) used to skip unchanged files. `Session::update(paths)` re-reads changed files. If a path was added, removed or renamed (a "structural" change), it re-walks `dir` and re-resolves every file. Its `work.graph_changed` flag says whether any file's imports changed. |
| `Project` (`src/main.rs`) | A loaded config plus a `Session`. `Project::analyze()` builds the `Graph` and evaluates the rules into an `Analysis` (graph + violations). |
| `extract` (`src/scan.rs`) | Parses one file's source (with oxc) and returns its import strings. They're the **cooked** string values, the same as ESTree's `Literal.value`. Vue/Svelte/Astro files go through `src/sfc.rs` first. |
| parse cache | The optional on-disk cache (`--cache`) of each file's extracted imports, keyed by stamp or content hash. |
| exotic require | `options.exotic_require` in the config: extra function names treated like `require` (for example `requireLazy`). |
| watcher (`src/watch.rs`) | The `notify`-based recursive file watcher used by `detangle watch` and the explorer (the interactive terminal UI). It waits for 150 ms of quiet before releasing a batch, to coalesce editors' multi-write saves. It flags a change as a **config change** when the file name is `detangle.toml`, `package.json`, `tsconfig*.json`, `webpack.config.*`, `vite.config.*`, `babel.config.*`, `.babelrc*`, `.env*` or `project.json`. |
| `--mode` | The existing CLI flag passed to Vite/webpack config evaluation, so aliases defined per mode resolve correctly. |
| `one_shot` (`src/main.rs`) | Deliberately leaks a one-shot command's `Project` and `Analysis`, because the OS reclaims them faster at exit than freeing them does. That was measured at ~20 ms for VS Code's graph when it was introduced. |
| platform packages | The 8 npm packages `detangle-{darwin-arm64, darwin-x64, linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, linux-arm64-musl, windows-x64, windows-arm64}`, each holding the binary for one target. They're listed in `npm/platforms.json`. `npm/binary.js`'s `platform()` picks the one for this machine. |
| `--concurrency N` | ESLint's option (ESLint 9.34+) to lint in N worker threads. |

## Context: what the Node API costs today

Measured 2026-09-27 on the maintainer's Mac. Numbers are medians of 7 runs from `target/release/detangle` at `22899e5`, driven from Node 24.

| Project | `detangle check -f json` (spawn from Node) | `check()` API | `graph -f json --externals` | `JSON.parse` of that | `analyze()` API | JSON size |
| ------- | ------ | ----- | ----- | ---- | ----- | ------- |
| VS Code `src/` (9.6k files) | 197 ms | 199 ms | 240 ms | 60 ms | 302 ms | 31.4 MB |
| excalidraw | 24 ms | 23 ms | 25 ms | 3 ms | 27 ms | 1.7 MB |

Other measurements on VS Code:
- Spawning `detangle --version` from Node: 3.7 ms (median of 21).
- Phase times from `DETANGLE_TIMINGS=1` for `graph -f json`: scan 170 ms, graph 15 ms, rules 5 ms, total 219 ms.
- `detangle watch` rebuild after an edit, 8 runs each:
  - one that changes imports: 57–61 ms;
  - one that doesn't: 2–5 ms.
- A one-file project holding VS Code's largest file (1.4 MB): scan ~3 ms, which includes resolver setup. For a median-size file (5.9 KB) the same measurement is also ~3 ms, so it's all setup. **The per-file parse of a median file isn't measured yet**; the benchmark (Success criteria) measures it.
- Walking `src/` (`rg --files`, same `ignore` crate): ~30 ms. `stat` of all 9.6k files from Python: ~25 ms.

What these numbers mean:
- **Starting a process costs ~4 ms.** The only other overhead in the Node API is JSON, and only `analyze()` on huge repos pays it (~100 ms).
- **The 170 ms scan repeated on every call is the real cost.** Only a Session that stays alive avoids it.
- **A general Node API returning the graph is a different feature.** Its cost would be dominated by building JS objects, which is out of scope and not measured here. This design never returns the graph.

**Why ESLint is the consumer.** ESLint rules are **synchronous** (`create(context)` and its visitors can't await), so an async API can't serve them. In an editor, the ESLint server lives for hours and lints the open file after each edit. That makes a warm Session the right tool, where `spawnSync` would cost ~200 ms per file.

**Positioning (editor first).** The ESLint rules are for **editor feedback**. `detangle check` stays the authoritative gate for CI and pre-commit. The docs say this plainly, because of `eslint --cache` (see D10).

## Scope

- **In:**
  - A napi-rs add-on, `detangle.node`, exposing `rootOf`, `open` and a synchronous `Project` (contract in D3). The Project keeps a `Session` alive.
  - Two ESLint rules, `detangle/errors` and `detangle/warnings`, in the existing `detangle` npm package at `detangle/eslint`, plus `configs.recommended`. They support ESLint 9 and 10 flat config.
  - Freshness in long-lived processes:
    - an overlay of the linted file's buffer (D6);
    - a watcher that starts only in editor-like processes (D7).
  - Shipping `detangle.node` inside the 8 existing platform packages.
  - `scripts/cmp-output.sh` and `scripts/bench-eslint.mjs`, committed so the checks can be reproduced.
  - Release **0.2.0**, with the ESLint integration marked **experimental**.
- **Out:**
  - Changing the existing `analyze`/`check`/`report`/`graph`/`migrate` Node API, or any CLI output.
  - Unsaved changes in files other than the one being linted.
  - Files that don't exist on disk yet: no reports.
  - Re-linting other open files when the graph changes. They update the next time the editor lints them.
  - Folder- and group-scope violations **without an import** (dependents count, orphan or reachable at group scope). They have no file to show in; `detangle check` lists them.
  - Tracking files that a JavaScript rules config imports.
  - A fallback when the add-on can't load (D11).
  - Autofixes and suggestions. Rules defined in the ESLint config.
  - Legacy `.eslintrc`, and ESLint < 9.
  - Any new npm package.
  - Bundler plugins, an LSP, and a `detangle serve` daemon.
  - Sharing one Session across ESLint worker threads.

## Success criteria

"Lint" means **one ESLint pass over one file with both rules enabled**; thanks to D5, that's one add-on call. Targets are for VS Code `src/` on the maintainer's Mac, measured by `scripts/bench-eslint.mjs <corpus>`, which drives ESLint's `Linter` API with and without the rules and reports the difference.

| Case | Target | Basis |
| ---- | ------ | ----- |
| Lint, file unchanged, median-size file | ≤ 0.5 ms added | estimate: 1.4 MB parses in ≤ 3 ms, so 6 KB should take ~0.02 ms, plus the lookup and node matching |
| Lint, file unchanged, 1.4 MB file | ≤ 5 ms added | parse ~3 ms (one parse, D5) |
| Lint after an import edit | ≤ 60 ms added | `watch` measures 57–61 ms, including freeing the old graph synchronously; D8 moves that off ESLint's thread |
| First lint in a process (cold scan) | ≤ 250 ms | CLI total is 219 ms |
| First re-lint: watcher start plus catch-up (D7) | ≤ 100 ms | walk ~30 ms, stamps ~25 ms |
| Lint after a config change | ≤ 250 ms | a full re-open, like a cold scan |

A missed target **blocks the 0.2.0 release**, unless the maintainer accepts that specific miss in the release commit message with the measured number. The measured numbers go in the implementation commit message either way.

Correctness criterion: the drift test (Risks) passes, comparing **locations**, not only messages.

## Files & touch points

- `Cargo.toml`:
  - Add a `[lib]` target (`src/lib.rs`), and turn the root into a workspace with the member `napi/`.
  - `include` still covers `src/**`. `cargo package` drops the `[workspace]` table (verified with a probe crate), so `cargo install detangle` builds as before. CI runs `cargo package` and builds the resulting `.crate` alone.
  - Keep `panic = "unwind"` (the default) in every profile (D8).
- `src/lib.rs` (**new**): declares the modules `main.rs` declares today and re-exports `Project` and `Analysis`. The crate docs say "internal API, no semver guarantees".
- `src/project.rs` (**new**):
  - `Project` and `Analysis`, moved from `main.rs` without changing their logic.
  - `Project::violations_for(path) -> FileReport`, which implements D9's placement table. It relies on a per-`Analysis` index from module to violations, built lazily on first use.
  - `Project::config_stamps()`: the stamps of the config files read at open (D8).
- `src/main.rs`: uses the lib. `one_shot` stays here.
- `src/scan.rs`:
  - `Session::overlay(path, source) -> bool` (D6).
  - `Session::contains(path)`.
  - `Session::rescan()`: a forced structural update, meaning a re-walk plus a stamp comparison (D7).
- `src/watch.rs`: `Watcher::drain() -> Result<Changes, Disconnected>`, which returns every pending change with no quiet period. It returns `Disconnected` if the watcher's thread has died.
- `napi/Cargo.toml`, `napi/src/lib.rs` (**new**): the `detangle-napi` crate (`publish = false`, `crate-type = ["cdylib"]`, `napi`/`napi-derive`). It contains:
  - The exports from D3, each wrapped in `std::panic::catch_unwind`, which turns a panic into the `Failed` state (D8). `#[napi(catch_unwind)]` is a backstop.
  - The state machine (D8), the lint tracker (D7), the dropper thread and the env cleanup hook (D8).
- `npm/native.js` (**new**):
  - Loads `detangle.node` from the platform package via `binary.js`'s `platform()`.
  - Resolves each Project's key (D8).
  - Shares one result per `context.sourceCode` object (D5).
  - Emits process warnings (D12).
- `npm/eslint.js`, `npm/eslint.mjs`, `npm/eslint.d.ts` (**new**): the plugin `{ meta, rules: { errors, warnings }, configs: { recommended } }`. The rules match specifiers to AST nodes (D9).
- `npm/package.json`:
  - `exports["./eslint"]` and the new files in `files`.
  - `peerDependencies.eslint: ">=9"`, optional via `peerDependenciesMeta`.
  - `devDependencies`: `eslint@^10`, `eslint9: "npm:eslint@^9"` (an npm alias, so both versions install side by side), `typescript-eslint` for TS fixtures.
  - Version 0.2.0.
- `npm/eslint.test.mjs` (**new**):
  - RuleTester cases for node kinds.
  - The drift test (Risks).
  - Watcher tests: `eslint .` and `eslint --fix .` start zero watchers; two lints ≥ 1 s apart start one.
  - The worker teardown test.
  - A Windows-only case test (a path differing only in case).
  - A panic test through a hidden `__panicForTest()` export, compiled only with the `test-hooks` cargo feature, asserting one re-open per change and no re-open storm.
- `tests/fixtures/eslint/` (**new**): module, circular, folder, group, orphan and `required` rules, an exotic require, and duplicate specifiers.
- `tests/fixtures/eslint-monorepo/` (**new**): workspace packages, `detangle.toml` at the repo root, and a cross-package cycle.
- `scripts/cmp-output.sh` (**new**): `cmp-output.sh <base-bin> <new-bin> <project>…` runs `check`, `check -f json`, `graph -f json` and `graph -f dot` with both binaries. It strips timing fields and asserts byte-identical output. It generalizes the maintainer's local script.
- `scripts/bench-eslint.mjs` (**new**): the Success-criteria benchmark. It takes a corpus path and isn't run in CI.
- `scripts/npm-packages.mjs`: copies `detangle.node` into each platform package.
- `.github/workflows/release.yml`:
  - Builds `detangle-napi` per target.
  - On musl, `RUSTFLAGS=-C target-feature=-crt-static` is set **only on the add-on build step**, so the CLI binary stays static. The step checks this with `file bin/detangle`, which must say "statically linked".
  - Each runner smoke-tests the add-on with `node -e "require('./detangle.node')"` before upload.
- `.github/workflows/ci.yml`:
  - Builds the add-on and runs `npm/eslint.test.mjs` on Linux, macOS and Windows against ESLint 10, and on Linux also against `eslint9`.
  - Runs `cargo package` plus a standalone build.
- `docs/reference.md`, `README.md`, `site/index.html`: an ESLint section with the experimental label, the editor-first positioning, the `--cache` caveat, and the `root` option for monorepos without a root `detangle.toml`.

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
  - `rootOf(dir: string) → string`: `find_root(dir)`. It always returns a path; with no marker file it's `dir` itself.
  - `open(dir: string, options: { config?: string; mode?: string }) → Project`. This is `detangle check <dir> [--config …] [--mode …]` kept alive. Paths are absolute; `native.js` resolves them first (D8).
  - `Project.violationsFor(file: string, text: string) → { violations, problems, exoticRequire }`:
    - `violations: { rule, severity: "error" | "warn" | "info", message, specifiers: string[] }[]`.
      - `specifiers` are the import strings **in this file** where the violation should be shown. An empty list means line 1.
      - `message` is built in Rust from the same pieces `detangle check -f text` prints: rule name, `from → to`, the cycle, the comment.
    - `problems: string[]`: persistent state problems (D12).
    - `exoticRequire: string[]`: the config's exotic-require names, for node matching (D9).
  - `Project.close()`: stops the watcher and frees the Session. Used by tests; normally the cleanup hook does it.
- **Rejected:**
  - Returning `to`, `cycle` and `imports` and placing in JavaScript: it spreads the placement rules over two languages.
  - Returning the whole `Analysis`: a different feature.
- **Cost we're accepting:** the add-on isn't a general Node API.

### D4: Ship as `detangle/eslint`, with the `.node` file inside the existing platform packages

- **Chosen:** a subpath export. The add-on sits at `detangle-<platform>/detangle.node`, next to `bin/detangle`.
- **Rejected:**
  - `eslint-plugin-detangle`, or separate per-platform add-on packages: each new npm package needs its own trusted-publisher setup on npmjs.com, which takes a security-key confirmation per save.
  - One package bundling all 8 add-ons: every install would download all 8.
- **Cost we're accepting:** each platform package grows by the add-on's size, unmeasured until the first build (see Risks). Flat config only.

### D5: One add-on call per lint, shared by both rules

- **Chosen:**
  - `native.js` keeps a `WeakMap` from `context.sourceCode` to the call's result. ESLint creates one `SourceCode` per pass over a file and gives it to every rule, so the first rule to run calls the add-on and the second reuses the result.
  - A new pass, whether an edit or a `--fix` pass, is a new `SourceCode` and gets a fresh call.
- **Rejected:**
  - A `(file, text)` memo (revision 1): it skipped the watcher drain, so it went stale after other files changed.
  - One call per rule (revision 2): it made every lint look like a re-lint to D7, and doubled the parse cost.
- **Cost we're accepting:** relies on ESLint sharing one `SourceCode` across rules within a pass. A test asserts it, on ESLint 9 and 10.

### D6: The linted file's buffer is authoritative for that file (overlay)

- **Chosen:** `violationsFor(file, text)` does these steps in order:
  1. **Track** the lint (D7). This may start the watcher and run the catch-up.
  2. **Drain** the watcher, if it's running, and apply the changes: `Session::update`, or a re-open for a config change (D8).
  3. **Membership:** if `file` isn't a Session member (`Session::contains`), return no violations. **The Session decides membership, never the overlay.** Excluded, generated, out-of-`dir` and not-yet-saved files land here.
     - A newly created file joins through the watcher or the catch-up.
     - Because step 1 comes before step 3, a file created before the watcher started reports from its first re-lint on.
  4. **Overlay:** `Session::overlay(file, text)`.
     - Runs `extract` on `text` and compares the result with the file's stored `raw` and parse-error count.
     - If they're equal, returns `false`, with no change.
     - Otherwise it replaces `raw`, re-resolves **only this file**, sets `stamp = None` so the next disk update re-reads it, and sets `graph_changed`.
     - Other files don't need re-resolving. Resolution depends on the **set** of files and on the config files (`package.json` `exports`, tsconfig `paths`). An overlay changes neither, and config files are handled by re-opening (D8).
  5. **Analyse** again only if step 2 or 4 set `graph_changed`.
- **Overlay lifetime:**
  - An overlay lasts until a disk event for that path, the next lint of that file, or a re-open.
  - A disk event for a file whose buffer still has unsaved edits (a `git checkout` or a formatter) replaces the overlay with the disk content. The next lint of the buffer puts it back.
  - A buffer discarded without saving leaves its overlay in place **indefinitely**, until one of those happens or ESLint restarts.
  - Overlays are never written to the parse cache.
- **Rejected:**
  - Disk only: new unsaved imports would only show after saving.
  - An mtime check of every file on every call: ~25 ms on VS Code.
  - Hashing the buffer against the disk: needs a stored hash per file.
  - Overlaying files that aren't on disk: the resolver reads the real filesystem, so no other file could resolve to them.
- **Cost we're accepting:** the discarded-buffer case, and a brief disk-content window after an external write. Unsaved edits in files that aren't being linted are invisible.

### D7: The watcher starts on the first re-lint at least 1 s after the file's previous lint

- **Chosen:**
  - The Project keeps, for each Session member it has served, the time of its last lint. This is bounded by the Session's size.
  - A lint counts as a **re-lint** when the file was served before **and ≥ 1 s** has passed since that lint. `--fix` passes (milliseconds apart) and the second rule of a pass (D5) never count.
  - On the first re-lint, the Project:
    1. starts a watcher on the root;
    2. runs one **catch-up**:
       - `Session::rescan()` picks up source edits, additions and deletions since the open;
       - a comparison of `Project::config_stamps()` (the config files read at open) re-opens the Project if any of them changed (D8).
  - From then on, step 2 of D6 drains the watcher **with no quiet period**.
    - The quiet period exists to coalesce editors' multi-write saves before a TUI redraw. Here, the worst case is a half-written file parsed once. Its next write event re-marks it dirty, and the next lint corrects it.
    - A branch switch lands as one structural update, bounded by a full scan (≤ 250 ms).
  - The watcher is owned by the Project **wrapper**, outside the Session, so it survives a re-open or a panic (D8).
- **Rejected:**
  - Counting every second call: fires in `eslint .` and `--fix` (Cold Read 2).
  - Counting only text changes: `--fix` passes change the text.
  - A `{ watch: true }` option: one shared ESLint config serves CI and editors.
  - Sniffing `process.argv` or the environment: fragile.
- **Cost we're accepting:**
  - Until the first re-lint, changes to other files are invisible. In an editor, that window is the first edit.
  - Config files read at open but not in `config_stamps()` aren't caught by the catch-up. `config_stamps()` covers the files `Project::open` reads directly (the `config` file, `detangle.toml`, the root `package.json` and `tsconfig*.json`, and any Vite/webpack config), not nested ones.
  - A long-lived tool that lints a file twice ≥ 1 s apart gets a watcher. That's harmless.

### D8: Lifecycle. Project keys, a state machine, panic recovery and cleanup

- **Project key and root (`native.js`):**
  - The directory to scan (`dir`) is, in order:
    1. the rule option `root`, resolved against ESLint's `context.cwd`;
    2. otherwise, the directory of the `config` option (resolved against `context.cwd`);
    3. otherwise, `rootOf(dirname(file))`, cached per directory.
  - With a `detangle.toml` at the repo root, all three give the same root as `detangle check` run at the repo root. Without one, a monorepo should set `root`, and the docs say so.
  - Projects are cached in a module-level `Map` keyed by `(canonical dir, canonical config path, mode)`, so different spellings of the same file share one Project.
  - There's no eviction. One process sees a handful of distinct keys, and each ESLint worker thread has its own module instance.
  - The `rootOf` cache is cleared when a Project re-opens because of a config change, so a `detangle.toml` added later is picked up then.
- **States** (the Rust wrapper; the watcher and lint tracker live in the wrapper and survive every transition):

  | State | Holds | Returned on each call |
  | ----- | ----- | --------------------- |
  | `Ready` | Session and Analysis | violations |
  | `Stale` | last good Session and Analysis, plus a config error | violations from the last good analysis, plus the error in `problems` |
  | `Broken` | a config error, no Session | no violations; the error in `problems` |
  | `Failed` | a panic message, no Session | no violations; the panic in `problems` |

  | From | Event | To |
  | ---- | ----- | -- |
  | (none) | `open` succeeds / fails | `Ready` / `Broken` |
  | `Ready`, `Stale` | config change (watcher, or catch-up) → re-open succeeds / fails | `Ready` / `Stale` (keeps the last good Session) |
  | `Broken` | config change → re-open succeeds / fails | `Ready` / `Broken` |
  | `Ready`, `Stale` | a panic in any call | `Failed` (the Session is dropped) |
  | `Failed` | the next watcher batch of any kind → re-open succeeds / fails / panics | `Ready` / `Broken` / `Failed` |

  - A "config change" is a watcher event the watcher flags as a config change, or an event for the `config` file itself (it may have any name).
  - With no watcher running (one-shot runs), `Broken` and `Failed` stay as they are for the rest of the process. At most one re-open happens per watcher batch, so a panic that recurs every time costs one re-open per change, never one per keystroke.
- **Panics:**
  - Every export wraps its body in `std::panic::catch_unwind`. A panic moves to `Failed`, and the call returns normally with the panic in `problems`. Throwing is avoided (D12).
  - `#[napi(catch_unwind)]` is a backstop. All profiles keep `panic = "unwind"`, and `napi/Cargo.toml` says why.
- **Background threads:**
  - The `notify` watcher thread: if it dies, `drain()` returns `Disconnected`. The Project drops the watcher, emits a warning (D12), and continues with overlay-only freshness. It doesn't restart the watcher.
  - The **dropper thread** is one per Project. It receives replaced Analyses and frees them, off ESLint's thread. If sending to it fails, the Analysis is freed inline.
- **Cleanup:** an env cleanup hook (`napi_add_env_cleanup_hook`, run when a worker thread or the process ends) stops each Project's watcher, closes the dropper channel, joins both threads, and drops every Project of that env.
- **Rejected:**
  - Re-opening on every call after a panic: a cold scan per keystroke.
  - A time-based backoff: it can retry into the same panic over and over.
  - Never re-opening in-process: one bad save disables the rules for the editor session.
  - `find_root(file)` alone: a monorepo without a root `detangle.toml` would get one Project per package and lose violations across packages.
  - Guessing the outermost `package.json`: it would disagree with `detangle check`.
  - A process-global Session shared across worker threads.
- **Cost we're accepting:**
  - Monorepos without a root `detangle.toml` must set `root`.
  - With `--concurrency N`, the scan runs N times (N × ~170 ms of CPU on VS Code), without watchers.

### D9: Two rules split by severity, and a placement table

- **Rules:**
  - `detangle/errors` reports detangle `error` violations.
  - `detangle/warnings` reports `warn` and `info`.
  - `recommended` sets them to ESLint `"error"` and `"warn"`.
  - Both take `{ root?: string, config?: string, mode?: string }`. Both rules should use the same values; different values mean different keys, so separate Projects.
- **Placement.** For a violation `v` and the linted file `L`. "Imports of `b` in `L`" means every entry of `L`'s edge to `b`: a file can import the same target more than once, for example a type import and a value import.

  | Violation kind | Shown in `L` when | `specifiers` in `L` |
  | -------------- | ----------------- | ------------------- |
  | Module scope with `to` (forbidden, `not-in-allowed`, circular) | `v.from == L` | the imports of `v.to` in `L` |
  | Module scope without `to` (orphan, reachable, dependents count, `required`) | `v.from == L` | none, so line 1 |
  | Folder scope, `F → T` | `L` is inside `F`'s subtree, and `L` imports some `b` with `folderTarget(b) == T`, where `b` is not a local file inside `F` | the imports of each such `b` in `L` |
  | Group scope with `imports` | some entry of `v.imports` starts in `L` | those entries' specifiers; other entries show in their own files |
  | Folder or group scope without `to`/`imports` | never | not shown; see Out |

  - `folderTarget(b)` repeats the folder graph's construction in `src/graph.rs`:
    - a local file → its directory (`.` at the root);
    - an npm module → `node_modules/<package name>`, including `@scope/name`;
    - a built-in or unresolved module → its id.
  - A circular rule therefore shows in **every** file on the cycle whose edge matches the rule, each on its import of the next file, just as `detangle check` lists them.
- **Node matching (JavaScript):**
  - A report goes on every node in `L` whose **string-literal** source equals a specifier. The strings compare equal because `extract` stores cooked values, like ESTree's `Literal.value`, including any `?query` suffix. The node kinds are:
    - `ImportDeclaration`;
    - `ExportNamedDeclaration`/`ExportAllDeclaration` with a source;
    - `ImportExpression`;
    - `require(...)` and calls to the `exoticRequire` names;
    - `TSImportEqualsDeclaration`;
    - `TSImportType`.
  - A template literal without expressions counts as a literal. Other non-literal sources never match; detangle doesn't resolve them either.
  - A specifier that matches no node (for example `/// <reference>`, SFC template imports, Angular `templateUrl`) is shown at line 1, with the specifier in the message.
- **Rejected:**
  - One rule for everything: it loses the severity split.
  - One ESLint rule per detangle rule: rule names must be known before the config is read.
- **Cost we're accepting:** a detangle `info` shows as an ESLint warning under `recommended`.

### D10: `eslint --cache` is documented, not handled

- **Chosen:** the docs state that the rules depend on other files, so `eslint --cache` can miss violations caused by changes in other files. CI and pre-commit should run `detangle check`.
- **Rejected:** detecting `--cache`. ESLint doesn't expose it to rules, so it would mean sniffing argv or the cache file.
- **Cost we're accepting:** cached ESLint runs can under-report.

### D11: No fallback when the add-on can't load

- **Chosen:** every lint reports one `problems` message: `detangle add-on unavailable: <reason>; run \`detangle check\``.
- **Rejected:** a one-time `spawnSync` of the binary. Its triggers barely exist:
  - The add-on and binary ship in, and fail with, the same platform package.
  - Node-API is ABI-stable across Node versions.
  - Placing reports would need `graph -f json --externals` (31 MB on VS Code) and a second, JavaScript implementation of D9.
- **Cost we're accepting:** runtimes without Node-API get only the message.

### D12: Problems and warnings

- **Chosen:**
  - **`problems`** are persistent state problems: add-on unavailable (D11), a config error (`Stale`, `Broken`), or a panic (`Failed`). They're returned on every call while the state lasts, and shown at line 1 of every linted file. Within one pass, only the **first rule to run** reports them: the shared result (D5) records that they've been reported. The first rule is determined by the order in the user's ESLint config.
  - **Warnings** are one-time notices that don't block results: the watcher failed to start, or the watcher stopped. `native.js` emits them with `process.emitWarning`, once per Project. Editors show them in the ESLint output channel.
  - A config that is invalid on the first `open` means `Broken`. A config that becomes invalid later means `Stale`.
- **Rejected:**
  - Throwing: ESLint turns it into a crash message for the whole file.
  - Showing watcher failures as lint messages: they would appear in every file, although results still work.
  - Reporting problems from both rules: duplicates.
- **Cost we're accepting:** a broken config shows at line 1 of every file linted until it's fixed. That's intended.

## Edge cases & failure modes

| Scenario | Behavior | Why |
| -------- | -------- | --- |
| `eslint .` / `eslint --fix .` | One scan per thread; no watcher; one overlay parse per pass | D5, D7 |
| `eslint --concurrency N` | N scans, no watchers | D7, D8 |
| `eslint --cache` | May miss cross-file violations | D10 |
| Monorepo, `detangle.toml` at the repo root | One Project for the repo, the same root as `detangle check` | D8 |
| Monorepo without a root `detangle.toml`, no `root` option | One Project per package; violations across packages are missing | D8 cost; the docs say to set `root` |
| Config change while the watcher is running | Re-open (≤ 250 ms). Success: `Ready`. Failure: `Stale`, with the last good results plus the error | D8 |
| Config change before the watcher has started | Caught at the first re-lint by comparing `config_stamps()`. Nested package.json/tsconfig files aren't compared | D7 cost |
| Config invalid on first open | `Broken`: a line-1 problem on every file. Recovers on a config change once the watcher is running; in one-shot runs, stays for the run | D8, D12 |
| JavaScript rules config, or Vite/webpack evaluation | The add-on spawns `node` synchronously, the first `node` on the ESLint process's `PATH`. Editors launched from a GUI may have a different `PATH` from the shell; if `node` isn't found, open fails and the Project is `Broken` with that error | Unchanged code path; costs time only at open |
| Edit to a file a JavaScript rules config imports | Not detected | Out |
| Linted file not in the Session (excluded, generated, outside `dir`, not saved yet) | No reports | D6 step 3 |
| File created before the watcher started | Reported from its first re-lint (the catch-up runs before the membership check) | D6 order |
| Unsaved import edit | Checked live | D6 |
| External write to a file with unsaved edits | Disk content until the next lint of the buffer | D6 |
| Buffer discarded without saving | Overlay stays indefinitely: until a disk event for the path, the next lint of that file, a re-open, or an ESLint restart | D6 cost |
| Processor virtual filenames (`README.md/0.js`) | Not Session members, so no reports | D6 step 3 |
| Buffer with syntax errors | ESLint fails to parse and never runs the rules | ESLint behavior |
| Vue/Svelte through their ESLint parsers | `text` is the full SFC source, which `extract` handles via `sfc.rs` | Same input the CLI reads |
| Specifier with no matching AST node | Line 1, specifier named in the message | D9 |
| Same specifier imported twice | Both nodes are reported | D9 |
| `require(variable)` | Never matched | D9 |
| Folder/group violation without an import | Not shown | Out |
| Watcher fails to start, or its thread dies | One `process.emitWarning`; from then on, only the linted file stays fresh | D8, D12 |
| Half-written file drained | Parsed as-is once; corrected by its next event | D7 |
| Branch switch (thousands of events) | One structural update, ≤ 250 ms on VS Code | D7 |
| Add-on missing or fails to load | A `problems` line on every file | D11 |
| Rust panic | `Failed`; a `problems` line; one re-open on the next watcher batch | D8 |
| Stack overflow or abort in Rust | The ESLint process dies | Can't be caught; D1 |
| ESLint worker thread exits | The cleanup hook stops the watcher and dropper threads and frees the Projects | D8 |
| Symlinked paths from ESLint | `dunce::canonicalize` before lookup | The Session stores canonical paths |
| Case-mismatched paths | macOS: canonicalize returns the on-disk case (checked with native `realpath` on APFS). Windows: covered by the CI case test | Same |

## Risks

- **Placement drifts from the CLI.**
  - Caught by: `npm/eslint.test.mjs` runs a real `ESLint` over `tests/fixtures/eslint`, `tests/fixtures/eslint-monorepo` and `tests/fixtures/conditions`. It compares the `(file, line, message)` set against an oracle the test computes itself:
    - violations come from `detangle check -f json`;
    - specifiers come from `graph -f json --externals` (each module's `dependencies[].specifier`);
    - D9's table is applied in JavaScript.
  - This follows the project's standing rule: correctness is checked against the real tool's output, not only unit tests.
  - Not caught:
    - bugs shared by the oracle and the implementation, which are written separately in JavaScript and Rust;
    - duplicate imports of one target through **different** specifiers, because graph JSON shows one specifier per edge. The fixture's duplicate-specifier case is covered by RuleTester instead.
- **The overlay diverges from a fresh scan.**
  - Caught by `scan.rs` unit tests, each asserting `graph_changed` and that the Session equals a fresh scan:
    - overlay equal to the disk version;
    - overlay adds an import;
    - overlay, then a disk save with the same text;
    - overlay, then the disk reverted;
    - overlay, then a drain, then `rescan()`, then another overlay.
- **Rust panics become ESLint crashes.** Caught by `catch_unwind`, the panic test (`test-hooks`), and a local run of the rules over the VS Code and excalidraw clones before each release. The clones aren't in CI. Not caught: aborts, OOM, stack overflow.
- **musl or Windows ARM add-on builds fail.** The release matrix builds them; CI only covers x64 Linux/Windows and arm64 macOS. Mitigation: the smoke `require` on each release runner, and the check that the musl CLI binary is still static, both before anything is uploaded.
- **Thread teardown bugs** (a watcher outliving its env). Caught by: a test that opens a Project in a `worker_threads` Worker, starts the watcher (two lints ≥ 1 s apart), terminates the Worker, and asserts the process exits cleanly.
- **Stale results in editors.** macOS FSEvents can coalesce or drop events. Nothing catches this automatically; it self-heals on the next save of the affected file, a re-open, or an ESLint restart.
- **Package size.** Measured on the first build. If a platform package goes over ~15 MB, D4 is reopened. The alternative is one extra `-node` package per platform, with its trusted-publisher setups.
- **The lib split changes the binary's output.** Caught by `scripts/cmp-output.sh` against the `22899e5` binary on VS Code and excalidraw.
- **ESLint API changes.** CI tests ESLint 10 (current: 10.11) on all three OSes and ESLint 9 on Linux. The shared-`SourceCode` assumption (D5) has its own test.
- **Latency misses its targets.** `scripts/bench-eslint.mjs` before release. A miss blocks the release unless it's explicitly accepted (Success criteria).

## Sign-off

- [x] Cold Read 1 — explain-back, implementation plan and critique (revision 1 → findings addressed in revision 2)
- [x] Cold Read 2 — the same three reads (revision 2 → findings addressed in revision 3)
- [ ] Cold Read 3 — on revision 3
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
