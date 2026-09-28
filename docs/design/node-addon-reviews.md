# Review history: ESLint rules backed by a native Node.js add-on

The cold reads behind [`node-addon.md`](node-addon.md). Each round gave three isolated readers one read of the doc, with no codebase and no conversation. The three reads were an explain-back, an implementation plan, and a hostile critique. Each round's findings are followed by how the next revision resolved them.

| Round | Doc read | Main findings |
| ----- | -------- | ------------- |
| 1 | revision 1 | missing contracts |
| 2 | revision 2 | design bugs (watcher trigger, state machine, root) |
| 3 | revision 3 | corners of the lazy watcher |
| 4 | revision 4 | corners of automatic root discovery; recovery depending only on the watcher |
| 5 | revision 5 | the `Failed` member set, a re-open per keystroke, dependency installs, the cost of `admit` |

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

## Cold Read 5 Findings — 2026-09-28

Same setup. All three reads were truncated at line 592 of 787 by the reader's size cap; the design itself was read in full. So this file was split out of the design doc.

### Risks (blocking)

1. **Dependency changes never re-resolve.** Nothing notices `npm install`/`pnpm add`, a version bump, a newly installed package, or an edit to a tsconfig reached through `extends` (a base above the root, `@tsconfig/*`). Lockfiles and the `extends` chain aren't polled, and `node_modules` events are filtered out. `detangle watch` has the same gap today.
2. **`Failed` recovery can't be implemented as written.** It needs a "Session member path", but `Failed` holds no Session, and when `open` panicked there never were members. Without a watcher, a source fix never recovers it.
3. **`Stale`/`Broken` could re-open on every lint** if stamps aren't updated after a failed re-open.
4. **`admit` costs far more than stated.** Admitting a file changes the file set, so every file is re-resolved (measured afterwards: 121–140 ms on VS Code), not ~30 ms. As a structural update, it also dropped every other buffer's overlay. Rejections were forgotten at every structural change, so excluded files paid a re-walk again and again.

### Gaps

- **Layering:** three things are called "Project", and there are two `violations_for` signatures. The platform packages' `files` must include the `.node`.
- **Unspecified:**
  - the `config` option's base path;
  - the source extensions;
  - `extract`'s import forms, which node matching must cover;
  - module id formats;
  - whether the root is canonicalized, and that two canonicalizers are in use;
  - the parse cache on re-read;
  - analyses per drain;
  - the `drain()` channel;
  - `meta.schema`;
  - problems with one rule enabled;
  - who accepts a missed target.
- **Coverage:**
  - a new workspace directory isn't a member directory yet;
  - `.env*`/Babel changes without a watcher;
  - the order within a drain.
- **Environment:**
  - the `$HOME` refusal vs a stray ancestor `package.json`;
  - `node` from `PATH` vs `process.execPath`;
  - no eviction (orphaned handles, `dir` disappearing);
  - folder-violation noise.
- **Costs:**
  - the placement index in the ≤ 30 ms target;
  - scan vs watcher contention;
  - no target for a new file.
- **Unverified:**
  - ESLint `cwd` per workspace folder;
  - `--concurrency` = threads;
  - OS event latency.

### Verdict

- Explain-back: NEEDS REVISION. The intent is clear; layering, stamp handling and costs are imprecise.
- Implementation: NEEDS REVISION. 13 questions.
- Critique: RISKS FOUND. Dependency installs, `Failed` recovery, the cost of `admit`.
- Trend: nothing challenged the architecture. The maintainer decided that revision 6 is the final revision, followed by human review.

### Resolution (revision 6, final)

Checked against the code while writing it:
- `SOURCE_EXTS`, the import forms `extract` recognizes, and module id formats (`src/scan.rs`, `src/graph.rs`).
- `Session::new` is the only reader and writer of the parse cache.
- The platform packages have no `exports` field.
- `oxc_resolver` keeps `TsConfig::extends` crate-private, so detangle follows the chain itself. `json-strip-comments` is already a transitive dependency.
- A structural update measured at 121–140 ms on VS Code.

How each finding was resolved:
- **Risk 1:**
  - Polling covers the tsconfig `extends` chain (followed by detangle) and the root lockfiles.
  - A lockfile change triggers `Session::refresh_resolution()`: a fresh resolver, with every file re-resolved without walking or re-parsing, ≤ 150 ms.
  - `node_modules` changes that don't touch a lockfile are Out.
- **Risk 2:**
  - `Failed` stores the dropped Session's paths and the panicking `(file, text hash)`.
  - It retries at most once per 30 s, and only after a polled change, a relevant watcher event, or a lint of different text. That works with or without a watcher.
- **Risk 3:** stamps are recorded at every open attempt, successful or not.
- **Risk 4:**
  - `Session::eligible` checks the walk's rules for one path without walking, so excluded files cost ≤ 1 ms, and rejections are kept until a re-open.
  - `admit` is the structural update the watcher would bring anyway (≤ 150 ms target).
  - Overlays now survive incremental and structural updates. An overlaid file is re-read only when its own path changes.
- **Layering:** an Architecture section names the Rust **Project**, the napi **handle** and the JavaScript **cache**. `violations_for(path)` is the Project's placement; `violationsFor(file, text)` is the handle's call.
- **Specified:**
  - source files, `extract`'s forms (and node matching for each, including AMD arrays, `getBuiltinModule` and Angular resources with the `./` rule);
  - module ids;
  - `config` resolved against `context.cwd`;
  - one canonicalizer (the add-on's `canonical`);
  - the parse cache is only touched by `Session::new`;
  - at most one analysis per call, and one consumer per `Watcher`;
  - `meta.schema`;
  - problems with one rule, and inline disables;
  - missed targets accepted by the maintainer in the release commit;
  - the `.node` added to platform packages' `files`.
- **Coverage:**
  - config classification runs after the drain's update, so a new workspace counts once its first source file is a member;
  - `.env*` and Babel configs that the open read are polled.
- **Environment:**
  - the refusal covers `/`, `$HOME` and ancestors of it; any other root is accepted with CLI parity;
  - `process.execPath` is passed as `node`;
  - idle handles are evicted after 15 minutes;
  - `dir` disappearing means `Broken`, with recovery via its stamps;
  - folder-violation noise is stated and accepted.
- **Costs:**
  - the placement index is in the ≤ 30 ms basis;
  - scan vs watcher contention is addressed;
  - rows added for a new file, an excluded file and a dependency install.
- **Unverified claims:** ESLint's `cwd` per folder is marked to confirm during implementation (`dir` covers it). `--concurrency` is stated as `worker_threads`. OS event latency is stated as a cost.
