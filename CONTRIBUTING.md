# Contributing to detangle

Thanks for helping. The most useful contributions, in order:

1. **A repository where detangle gets something wrong.** An import it can't resolve that your bundler can, a cycle it misses or invents, a migrated config that reports different violations than the original tool. Open a [bug report](https://github.com/debug-diary-1/detangle/issues/new?template=bug.yml) with a public repository and commit, or a few files that reproduce it. Resolution edge cases (aliases, monorepo layouts, `exports` conditions) are where help matters most.
2. **Your repository's numbers.** How long `detangle check` takes on your codebase, on your machine, and what it found. Use the ["my repo's numbers"](https://github.com/debug-diary-1/detangle/issues/new?template=numbers.yml) template.
3. **Code.** Issues labelled `good first issue` are small and self-contained. For anything larger, open an issue first so we can agree on the approach.

## Building and testing

You need Rust 1.94 or newer and Node.js 18 or newer (for the npm package and the ESLint rules).

```sh
cargo build                     # the CLI: target/debug/detangle
cargo test                      # unit tests and CLI tests (tests/cli.rs), including the Node.js API
cargo clippy --workspace --all-targets -- -D warnings
```

The ESLint rules run in a native add-on (`napi/`). Their tests drive real ESLint against the add-on and the CLI you built:

```sh
cargo build -p detangle-napi --features test-hooks
(cd npm && npm ci)
DETANGLE_ADDON=target/debug/libdetangle_napi.dylib DETANGLE_BIN=target/debug/detangle node --test npm/eslint.test.mjs
```

(On Linux the add-on is `libdetangle_napi.so`; on Windows, `detangle_napi.dll` and `detangle.exe`.)

## Making a change

- **Start with a failing test.** Most behaviour is tested through the real binary in `tests/cli.rs`, against a small project in `tests/fixtures/` or one written to a temporary directory. A bug report's reproduction usually becomes the test.
- **Keep output stable.** For a refactor or a performance change, `scripts/cmp-output.sh <old-binary> <new-binary> <project>...` compares `check` and `graph` output byte for byte.
- **Don't run `cargo fmt`** on files you aren't otherwise changing: the code isn't rustfmt-formatted, and a whole-file reformat buries the real change. Match the surrounding style.
- **One change per pull request**, with a description of what changed and how you checked it. CI runs on Linux, macOS and Windows; Windows often catches path handling.
- **CI runs `scripts/guardrails.sh`**, which checks commit authorship, tool names and what lives in `docs/`; run it locally with `scripts/guardrails.sh origin/main..HEAD`.
- Benchmarks against other tools live in [detangle-bench](https://github.com/debug-diary-1/detangle-bench), not in this repository.

## License

detangle is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in detangle, as defined in the Apache-2.0 license, is dual licensed as above, without any additional terms or conditions.
