# Feature: ESLint rules backed by a native Node.js add-on

Status: **draft, revision 2** (after Cold Read 1). No code until the next Cold Read and sign-off. Owner and sign-off: the maintainer.

## Background

**detangle** is a Rust CLI, also shipped on npm with a small Node.js API, that scans a JavaScript/TypeScript project, builds its import graph and checks architecture rules against it. The rules come from `detangle.toml`, or a JavaScript rules config it imports.

### Glossary

| Term | Meaning |
| ---- | ------- |
| module | A source file in the graph, identified by its path relative to the project root (for example `src/a/b.ts`). npm packages and Node built-ins are modules too. |
| rule | A `[[forbidden]]`, `[[required]]` or `allowed` entry in the config. Each has a name, a severity (`error`, `warn`, `info`, `off`) and an optional `comment`. |
| violation | One rule match. Its `scope` is `module`, `folder` (the graph of directories) or `group` (the graph of configured groups or Nx projects). It has a `from` node, an optional `to` node (none for rules about a module alone, such as orphans or `required`), a `cycle` (for circular rules) and, at group scope only, `imports`: the module-level imports behind it. |
| circular violation | Reported **per import edge**: every edge that lies on a cycle and matches the rule yields its own violation, with `from` = that edge's importing file. So each file in a cycle gets its own violation for its import of the next file. |
| `Session` (`src/scan.rs`) | The scanned files: each file's extracted import strings (`raw`), their resolved targets, the parse-error count, and a `stamp` (mtime + size) used to skip unchanged files. `Session::update(paths)` re-reads changed files; its `work.graph_changed` flag says whether any file's imports changed. |
| `Project` (`src/main.rs`) | A loaded config plus a `Session`. `Project::analyze()` builds the `Graph` and evaluates the rules into an `Analysis` (graph + violations). |
| `extract` (`src/scan.rs`) | Parses one file's source (with oxc) and returns its import strings. Vue/Svelte/Astro files go through `src/sfc.rs` first. |
| watcher (`src/watch.rs`) | `notify`-based recursive file watcher used by `detangle watch` and the explorer. It waits for 150 ms of quiet before releasing a batch, to coalesce editors' multi-write saves. |
| `find_root` (`src/config.rs`) | The nearest ancestor with `detangle.toml`, else with `package.json`. |
| `--mode` | The existing CLI flag passed to Vite/webpack config evaluation, so aliases defined per mode resolve correctly. |
| platform packages | The 8 npm packages `detangle-{darwin-arm64, darwin-x64, linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, linux-arm64-musl, windows-x64, windows-arm64}`, each holding the binary for one target. They're listed in `npm/platforms.json`. `npm/binary.js`'s `platform()` picks the one for this machine. |
| `cmp.sh` | A local script (kept outside the repo, recreated if missing) that runs two binaries on the same projects and asserts byte-identical output. It's the project's standing check for refactors and performance changes. |

## Context: what the Node API costs today

Measured 2026-09-27 on the maintainer's Mac. Numbers are medians of 7 runs from `target/release/detangle` at `22899e5`, driven from Node 24.

| Project | `detangle check -f json` (spawn from Node) | `check()` API | `graph -f json --externals` | `JSON.parse` of that | `analyze()` API | JSON size |
| ------- | ------ | ----- | ----- | ---- | ----- | ------- |
| VS Code `src/` (9.6k files) | 197 ms | 199 ms | 240 ms | 60 ms | 302 ms | 31.4 MB |
| excalidraw | 24 ms | 23 ms | 25 ms | 3 ms | 27 ms | 1.7 MB |

Other measurements on VS Code:
- Phase times from `DETANGLE_TIMINGS=1`: scan 170 ms, graph 15 ms, rules 5 ms.
- `detangle watch` rebuild after an edit, 8 runs each:
  - one that changes imports: 57–61 ms;
  - one that doesn't: 2–5 ms.
- Parse (`scan`) of the largest file, 1.4 MB: ~3 ms. The median file is 5.9 KB.
- `stat` of all 9.6k source files from Python: ~25 ms.

What these numbers mean:
- **Starting a process costs ≤ 2 ms.** The only other overhead in the Node API is JSON, and only `analyze()` on huge repos pays it (~100 ms).
- **The 170 ms scan repeated on every call is the real cost.** Only a Session that stays alive avoids it.
- **Returning the whole graph through napi wouldn't beat `JSON.parse`.**

**Why ESLint is the consumer.** ESLint rules are **synchronous** (`create(context)` and its visitors can't await), so an async API can't serve them. In an editor, the ESLint server lives for hours and lints the open file after each edit. That makes a warm Session the right tool, where `spawnSync` would cost ~200 ms per file.

**Positioning (editor first).** The ESLint rules are for **editor feedback**. `detangle check` stays the authoritative gate for CI and pre-commit. The docs say this plainly, because of `eslint --cache` (see D10).

## Scope

- **In:**
  - A napi-rs add-on, `detangle.node`, exposing `rootOf(path)` and a synchronous `Project` handle. The handle keeps a `Session` alive and answers `violationsFor(file, text)` (contract in D3).
  - Two ESLint rules, `detangle/errors` and `detangle/warnings`, in the existing `detangle` npm package at `detangle/eslint`, plus `configs.recommended`. They support ESLint 9 and 10 flat config.
  - Freshness in long-lived processes:
    - an overlay of the linted file's buffer text (D6);
    - a watcher that starts lazily, only in editor-like processes (D7).
  - Shipping `detangle.node` inside the 8 existing platform packages.
  - Release **0.2.0**, with the ESLint integration marked **experimental** in the docs.
- **Out:**
  - Changing the existing `analyze`/`check`/`report`/`graph`/`migrate` Node API. It keeps spawning the binary.
  - Unsaved changes in files other than the one being linted.
  - Files that don't exist on disk yet (untitled or unsaved new files): no reports.
  - Re-linting other open files when the graph changes. They update the next time the editor lints them.
  - A fallback when the add-on can't load (D11).
  - Autofixes and suggestions. Rules defined in the ESLint config.
  - Legacy `.eslintrc`, and ESLint < 9.
  - A separate `eslint-plugin-detangle` package. New npm packages of any kind.
  - Bundler plugins, an LSP, and a `detangle serve` daemon.
  - Sharing one Session across ESLint worker threads.

## Success criteria

On VS Code `src/`, on the maintainer's Mac, measured with a local script that drives the rules through ESLint's `Linter` API:

| Case | Target | Basis |
| ---- | ------ | ----- |
| Lint call, file unchanged, median-size file | < 0.5 ms added by the rules | overlay parse of ~6 KB plus lookup |
| Lint call, file unchanged, 1.4 MB file | ≤ 5 ms | parse measured at ~3 ms |
| Lint call after an import edit | ≤ 60 ms | `watch` measures 57–61 ms, which includes freeing the old graph synchronously; D8 moves that off the thread |
| First call in a process (cold scan) | ≤ 250 ms | CLI total is 219 ms |
| Watcher start plus catch-up (D7) | ≤ 100 ms | walk plus stamp comparison; stat alone is ~25 ms |
| Config reload | ≤ 250 ms | same as a cold scan |

Correctness criterion: the drift test (see Risks) passes with **locations**, not only messages.

## Files & touch points

- `Cargo.toml`:
  - Add a `[lib]` target (`src/lib.rs`), and turn the root into a workspace with the member `napi/`.
  - `include` still covers `src/**`, so the published crate builds both the lib and the binary.
  - Keep `panic = "unwind"` (the default) in every profile the add-on uses (D8).
- `src/lib.rs` (**new**): declares the modules `main.rs` declares today and re-exports `Project` and `Analysis`. The crate docs say "internal API, no semver guarantees".
- `src/project.rs` (**new**):
  - `Project` and `Analysis`, moved from `main.rs` without changing their logic.
  - `Project::violations_for(path) -> FileReport`, which implements D9's placement table. It relies on a per-`Analysis` index from module to violations, built lazily on first use.
- `src/main.rs`: uses the lib. `one_shot`/`Box::leak` stay here, since only one-shot CLI commands may leak.
- `src/scan.rs`:
  - `Session::overlay(path, source) -> bool` (D6).
  - `Session::contains(path)`.
  - `Session::rescan()`: `update`'s structural path, forced (D7).
- `src/watch.rs`: `Watcher::drain()`, which returns every pending change with no quiet period.
- `napi/Cargo.toml`, `napi/src/lib.rs` (**new**): the `detangle-napi` crate (`publish = false`, `crate-type = ["cdylib"]`, `napi`/`napi-derive`). It contains:
  - The exports from D3, each `#[napi(catch_unwind)]`.
  - The `Project` wrapper's state machine (D8).
  - An env cleanup hook (D8).
- `npm/native.js` (**new**):
  - Loads `detangle.node` from the platform package via `binary.js`'s `platform()`.
  - Caches `rootOf` per directory and one `Project` per `(root, config, mode)`.
  - Holds no result memo (D5).
- `npm/eslint.js`, `npm/eslint.mjs`, `npm/eslint.d.ts` (**new**):
  - The plugin `{ meta, rules: { errors, warnings }, configs: { recommended } }`.
  - The rules match specifiers to AST nodes (D9).
- `npm/package.json`:
  - `exports["./eslint"]` and the new files in `files`.
  - `peerDependencies.eslint: ">=9"`, optional via `peerDependenciesMeta`.
  - Version 0.2.0.
- `npm/eslint.test.mjs` (**new**): a RuleTester suite for node kinds, plus the drift test (see Risks).
- `tests/fixtures/eslint/` (**new**): a fixture with module, circular, folder, group, orphan and `required` rules. Today's `conditions` fixture only produces module-scope violations: 12 plain and 7 circular.
- `scripts/npm-packages.mjs`: copies `detangle.node` into each platform package.
- `.github/workflows/release.yml`: builds `detangle-napi` per target. musl needs `RUSTFLAGS=-C target-feature=-crt-static` for a cdylib. A smoke test (`node -e "require('./detangle.node')"`) runs on each runner before upload.
- `.github/workflows/ci.yml`: builds the add-on and runs `npm/eslint.test.mjs` on Linux, macOS and Windows, against ESLint 10. One Linux job also runs it against ESLint 9.
- `docs/reference.md`, `README.md`, `site/index.html`: an ESLint section with the experimental label, the editor-first positioning and the `--cache` caveat.

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
  - `rootOf(path: string) → string | null`: `find_root` of the path's directory, or `null` for paths that aren't absolute.
  - `open(root: string, options: { config?: string; mode?: string }) → Project`.
    - `config` is resolved against ESLint's `context.cwd`, like the CLI resolves `--config` against its working directory.
    - `mode` is the CLI's `--mode`.
  - `Project.violationsFor(file: string, text: string) → { violations: { rule, severity: "error" | "warn" | "info", message, specifiers: string[] }[], problems: string[] }`
    - `specifiers` are the import strings **in this file** where the violation should be shown. An empty list means line 1.
    - `message` is built in Rust from the same pieces `detangle check -f text` prints: rule name, `from → to`, the cycle, the comment.
    - `problems` are file-independent notices (config errors, watcher failure), shown at line 1 (D12).
  - `Project.close()`: stops the watcher and frees the Session. Used by tests; normally the cleanup hook does it.
- **Rejected:**
  - Returning `to`, `cycle` and `imports` and placing in JavaScript: it spreads the placement rules over two languages.
  - Returning the whole `Analysis`: as costly as the JSON it replaces.
- **Cost we're accepting:** the add-on isn't a general Node API.

### D4: Ship as `detangle/eslint`, with the `.node` file inside the existing platform packages

- **Chosen:** a subpath export. The add-on sits at `detangle-<platform>/detangle.node`, next to `bin/detangle`.
- **Rejected:**
  - `eslint-plugin-detangle`, or napi-rs's default of separate `@scope/…` packages: each new npm package needs a trusted-publisher setup on npmjs.com, which takes a security-key confirmation per save.
  - One package bundling all 8 add-ons: every install would download all 8.
- **Cost we're accepting:** each platform package grows by the add-on's size, estimated at 5–8 MB (unmeasured; see Risks). Flat config only.

### D5: No result memo in JavaScript

- **Chosen:** each rule calls `violationsFor` itself, so with both rules enabled there are two calls per lint.
- **Rejected:** a `(file, text)` memo. It skipped the watcher drain, so it returned stale results after other files changed, and it ignored `config`. A generation counter would fix that, but costs more than it saves.
- **Cost we're accepting:** the second call repeats the overlay parse: < 0.1 ms for a median file, ~3 ms for the largest VS Code file. The watcher drain and graph work happen only in the first call; the second finds nothing to do.

### D6: The linted file's buffer is authoritative for that file (overlay)

- **Chosen:** `violationsFor(file, text)`:
  1. Drains the watcher, if it's running (D7), and applies the changes with `Session::update`.
  2. If `file` is not a Session member (`Session::contains`): no reports. **The Session decides membership, never the overlay.** Excluded, generated, out-of-`dir` and not-yet-saved files all land here. A newly created file joins through the watcher, or through the catch-up when the watcher starts.
  3. `Session::overlay(file, text)`:
     - Runs `extract` on `text` and compares the result with the file's stored `raw` and parse-error count.
     - If they're equal, returns `false`, with no change.
     - Otherwise it replaces `raw`, re-resolves **only this file**, sets `stamp = None` so the next disk update re-reads it, and sets `graph_changed`.
     - Other files never need re-resolving: resolution depends on the **set** of files on disk, not their contents, and an overlay never changes that set.
  4. Re-analyses only if step 1 or 3 set `graph_changed`.
- **Overlay lifetime:**
  - An overlay lasts until a disk event for that path, the next lint of that file, or a Project re-open (a config change).
  - A buffer discarded without saving leaves its overlay in place **indefinitely**, until one of those happens or ESLint restarts.
  - Overlays are never written to the parse cache.
- **Rejected:**
  - Disk only: new unsaved imports would only show after saving.
  - An mtime check of every file on every call: ~25 ms on VS Code.
  - Hashing the buffer against the disk: needs a stored hash per file. Comparing extracted imports answers the question that matters.
  - Overlaying files that aren't on disk: the resolver reads the real filesystem, so no other file could resolve to them.
- **Cost we're accepting:** the discarded-buffer case above. Unsaved edits in files that aren't being linted are invisible.

### D7: The watcher starts on the first re-lint, with a one-time catch-up

- **Chosen:**
  - The Project records which files it has served.
  - The first time a file is linted **a second time** in the same process, which happens in editors and not in `eslint .`, the Project:
    1. Starts a watcher on the root.
    2. Runs one catch-up: `Session::rescan()`, which is **new**. It re-walks the tree like a structural `update` and compares stamps, re-parsing only changed files. That picks up edits, additions and deletions since the scan.
  - From then on, each call drains the watcher **with no quiet period**. The quiet period exists to coalesce editors' multi-write saves before a TUI redraw. Here, the worst case is a half-written file parsed once; its next write event re-marks it dirty, and the next call corrects it. A branch switch lands as one structural update, bounded by a full scan (≤ 250 ms).
- **Rejected:**
  - Always starting it: an inotify watch set and a thread per `eslint .` run and per `--concurrency` worker.
  - A rule option `{ watch: true }`: one shared ESLint config serves CI and editors, and editor users would have to know about it.
  - Sniffing `process.argv` or the environment: fragile.
- **Cost we're accepting:**
  - Until the first re-lint, changes to other files are invisible. In an editor that window is the first edit.
  - A tool that lints each file exactly once but lives long (a custom script over `Linter`) never gets a watcher. That's correct for its use.

### D8: Lifecycle. A per-thread Project cache, a Rust-side state machine, and a cleanup hook

- **Chosen:**
  - **Cache:** `native.js` keys Projects by `(root, config, mode)` in a module-level `Map`. There's no eviction: one process sees a handful of roots and option sets, and each ESLint worker thread has its own module instance, so its own Map and Projects.
  - **State machine:** the Rust `Project` wrapper holds `Ready(inner) | Broken { error, config_stamp }`.
    - A panic caught by `#[napi(catch_unwind)]` drops `inner`, reports the panic as a `problems` entry for that call, and re-opens on the next call.
    - `Broken` covers a config that fails to load. The wrapper retries only when the config file's stamp changes, which costs one `stat` per call.
  - **Freeing memory:** a replaced `Analysis` is sent to one long-lived dropper thread per Project. Freeing VS Code's graph takes ~20 ms, so it happens off ESLint's thread.
  - **Cleanup:** an env cleanup hook (`napi_add_env_cleanup_hook`, run when a worker thread or the process ends) stops the watcher, closes the dropper channel, joins both threads, and drops every Project of that env.
  - **Panics:** the add-on's build profile must keep `panic = "unwind"`, or `catch_unwind` does nothing. `napi/Cargo.toml` carries a comment saying so.
- **Rejected:**
  - A process-global Session shared across worker threads: locking and lifetimes across napi environments aren't worth it for v1.
  - Relying on garbage collection to free Projects: finalizers have no ordering guarantee relative to thread exit.
- **Cost we're accepting:** with `--concurrency N`, the scan runs N times (N × ~170 ms of CPU on VS Code). No watchers start there (D7).

### D9: Two rules split by severity, and a placement table

- **Rules:**
  - `detangle/errors` reports detangle `error` violations.
  - `detangle/warnings` reports `warn` and `info`.
  - `recommended` sets them to ESLint `"error"` and `"warn"`.
  - Both take `{ config?: string, mode?: string }`. The same values should be used for both; different values open two Projects.
- **Placement.** For a violation `v` and the linted file `L`:

  | Violation kind | Shown in `L` when | `specifiers` in `L` |
  | -------------- | ----------------- | ------------------- |
  | Module scope with `to` (forbidden, `not-in-allowed`, circular) | `v.from == L` | every import in `L` of `v.to` (all entries of the edge, for example a type import and a value import) |
  | Module scope without `to` (orphan, `reachable`, dependents count, `required`) | `v.from == L` | none, so line 1 |
  | Folder scope, `F → T` | `L` is inside `F`'s subtree and imports some `b` whose own folder is `T` (npm: `node_modules/<name>`) and which is not in `F`'s subtree | those imports. This is the folder-graph construction rule reversed. Folder violations don't record their imports |
  | Group scope | some entry of `v.imports` starts in `L` | those entries' specifiers. Other entries show in their own files |

  A circular rule therefore shows in **every** file on the cycle whose edge matches the rule, each on its import of the next file, as `detangle check` lists them.
- **Node matching (JavaScript):**
  - A report goes on every node in `L` whose **string-literal** source equals a specifier. The node kinds are:
    - `ImportDeclaration`;
    - `ExportNamedDeclaration`/`ExportAllDeclaration` with a source;
    - `ImportExpression`;
    - `require(...)` and the configured exotic-require names;
    - `TSImportEqualsDeclaration`;
    - `TSImportType`.
  - A template literal without expressions counts as a literal. Other non-literal sources never match; detangle doesn't resolve them either.
  - A specifier that matches no node (for example `/// <reference>`, or SFC template imports) is shown at line 1, with the specifier in the message.
- **Rejected:**
  - One rule for everything: it loses the severity split.
  - One ESLint rule per detangle rule: rule names must be known before the config is read.
- **Cost we're accepting:** a detangle `info` shows as an ESLint warning under `recommended`.

### D10: `eslint --cache` is documented, not handled

- **Chosen:** the docs state that the rules depend on other files, so `eslint --cache` can miss violations caused by changes in other files. CI and pre-commit should run `detangle check`.
- **Rejected:** detecting `--cache`. ESLint doesn't expose it to rules, so it would mean sniffing argv or the cache file.
- **Cost we're accepting:** cached ESLint runs can under-report. That's covered by the positioning: ESLint for feedback, `detangle check` as the gate.

### D11: No fallback when the add-on can't load

- **Chosen:** both rules report one line-1 message per file: `detangle add-on unavailable: <reason>; run \`detangle check\``.
- **Rejected:** a one-time `spawnSync` of the binary. Its triggers barely exist:
  - The add-on and binary ship in, and fail with, the same platform package.
  - Node-API is ABI-stable across Node versions.
  - Placing reports would also need `graph -f json --externals` (31 MB on VS Code) and a second, JavaScript implementation of D9.
- **Cost we're accepting:** runtimes without Node-API (for example browser sandboxes) get only the message.

### D12: Problems (config errors, watcher failure) are reported by both rules

- **Chosen:**
  - `problems` entries are reported at line 1 by **each** enabled rule, so a broken config shows up whichever rule is on.
  - When both rules are on, each problem shows twice.
  - A config that is invalid on the first `open` gives a `Broken` Project (D8) with no violations. A config that becomes invalid later keeps the last good Session, reports the error, and retries when the config file's stamp changes.
- **Rejected:**
  - Throwing: ESLint turns it into a crash message for the whole file.
  - Reporting from only one rule: rules can't see which other rules are enabled.
- **Cost we're accepting:** duplicates while the config is broken.

## Edge cases & failure modes

| Scenario | Behavior | Why |
| -------- | -------- | --- |
| `eslint .` (one-shot) | One scan per thread; no watcher; each file costs its overlay parse | D7 |
| `eslint --concurrency N` | N scans, no watchers | D7, D8 |
| `eslint --cache` | May miss cross-file violations | D10 |
| `detangle.toml`, tsconfig, package.json, Vite/webpack/Babel config or `.env` changes (watcher running) | The next call re-opens the Project (≤ 250 ms); overlays are dropped and re-applied as files are linted again | Same config file list as `detangle watch` |
| Config changes before the watcher has started | Picked up by the catch-up at the first re-lint | D7 |
| Config invalid on first open | Line-1 problem from each enabled rule; retries when the config file's stamp changes | D8, D12 |
| Config becomes invalid mid-session | Last good Session kept, plus the problem message | D12 |
| JavaScript rules config, or Vite/webpack evaluation | The add-on spawns `node` synchronously, the first `node` on `PATH`, as the CLI does | Unchanged code path; costs time only at open |
| Linted file not in the Session (excluded, generated, outside `dir`, not saved yet) | No reports | D6 step 2 |
| Unsaved import edit | Checked live | D6 |
| Buffer discarded without saving | Overlay stays indefinitely: until a disk event for that path, the next lint of that file, a config change, or an ESLint restart | D6 cost |
| Processor virtual filenames (`README.md/0.js`) | Not Session members, so no reports | D6 step 2 |
| Buffer with syntax errors | ESLint fails to parse and never runs the rules | ESLint behavior |
| Vue/Svelte through their ESLint parsers | `text` is the full SFC source, which `extract` handles via `sfc.rs` | Same input the CLI reads |
| Specifier with no matching AST node | Line 1, specifier named in the message | D9 |
| Same specifier imported twice (type import and value import) | Both nodes are reported | D9 |
| `require(variable)` | Never matched; detangle doesn't resolve it either | D9 |
| Watcher fails to start (inotify limit, sandbox, network drive) | One `problems` entry, reported once per Project; afterwards only the linted file stays fresh | D6 still applies |
| Half-written file drained | Parsed as-is once; corrected by its next event | D7 |
| Branch switch (thousands of events) | One structural update, ≤ 250 ms on VS Code | D7 |
| Add-on missing or fails to load | Line-1 message from each rule | D11 |
| Rust panic | Caught; `problems` entry; the Project is re-opened on the next call | D8 |
| Stack overflow or abort in Rust | The ESLint process dies | Can't be caught; D1 |
| ESLint worker thread exits | The cleanup hook stops the watcher and dropper threads and frees the Projects | D8 |
| Paths from ESLint are symlinked or differently cased | `dunce::canonicalize` before lookup. It resolves symlinks. On macOS it also returns the on-disk case, which was checked with native `realpath` on APFS. Windows case is covered by a CI test | The Session stores canonical paths |

## Risks

- **Placement drifts from the CLI.**
  - Caught by: `npm/eslint.test.mjs` runs a real `ESLint` over `tests/fixtures/eslint` and `tests/fixtures/conditions`. It compares the `(file, line, message)` set with an oracle that the test computes independently from `detangle check -f json` plus `graph -f json --externals` (for specifiers), applying D9's table.
  - This follows the project's standing rule: correctness is checked against the real tool's output, not only unit tests.
  - Not caught: bugs shared by the oracle and the implementation. They're written separately, in JavaScript and Rust.
- **The overlay diverges from a fresh scan.**
  - Caught by `scan.rs` unit tests, each asserting `graph_changed` and that the Session equals a fresh scan:
    - overlay equal to the disk version;
    - overlay adds an import;
    - overlay, then a disk save with the same text;
    - overlay, then the disk reverted.
- **Rust panics become ESLint crashes.** Caught by `catch_unwind`, plus a local run of the rules over the VS Code and excalidraw clones. Those clones aren't in CI; the run is a pre-release manual step. Not caught: aborts, OOM, stack overflow.
- **musl or Windows ARM add-on builds fail.** The release matrix builds them; CI only covers x64 and macOS arm64. Mitigation: the smoke `require` on each release runner, before anything is uploaded.
- **Thread teardown bugs** (a watcher outliving its env). Caught by: a test that opens a Project in a `worker_threads` Worker, triggers the watcher (by linting twice), and terminates the Worker, asserting the process exits cleanly.
- **Stale results in editors.** macOS FSEvents can coalesce or drop events. Nothing catches this automatically; it self-heals on the next save of the affected file, a config change, or an ESLint restart.
- **Package size.** Measured on the first build. If a platform package goes over ~15 MB, D4 is reopened.
- **The lib split changes the binary's output.** Caught by `cmp.sh` byte-identical checks against the `22899e5` binary on VS Code and excalidraw, for `check`, `graph -f json` and `graph -f dot`.
- **ESLint API changes.** CI tests ESLint 10 (current, 10.11) on all three OSes and ESLint 9 on Linux.
- **Latency misses its targets.** Measured locally before release against the Success criteria table, with results recorded in the commit message.

## Sign-off

- [ ] Cold Read 1 — explain-back
- [ ] Cold Read 2 — adversarial critique
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
