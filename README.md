# detangle

Fast dependency analysis and architecture rules for JavaScript and TypeScript: find import cycles, enforce boundaries between parts of your code, and explore the dependency graph. Works with React, Vue, Svelte and Angular. Written in Rust on top of the [oxc](https://oxc.rs) parser and resolver.

**[Website](https://debug-diary-1.github.io/detangle/)** · **[Full reference](https://github.com/debug-diary-1/detangle/blob/main/docs/reference.md)** · **[Releases](https://github.com/debug-diary-1/detangle/releases)**

| VS Code `src/` (10k modules, 113k imports) | Time | Memory |
|---|---|---|
| detangle | 0.2 s | 160 MB |
| the JavaScript rules tool | 40 s | 3.7 GB |

## Install

```sh
npm install --save-dev detangle          # or run once: npx detangle check
brew install debug-diary-1/tap/detangle
cargo install detangle
```

Prebuilt binaries for macOS, Linux and Windows (x64 and arm64) are on the [releases page](https://github.com/debug-diary-1/detangle/releases).

## Quick start

```sh
detangle                # interactive explorer, updates as you edit
detangle check          # run the rules; exits 1 on errors
detangle init           # write a starter detangle.toml
detangle report --open  # HTML report
```

With no config, `detangle check` reports import cycles, unresolvable imports, undeclared npm packages and orphaned files.

## Rules

Rules live in `detangle.toml`. Paths are regular expressions matched against paths relative to the project root.

```toml
[[forbidden]]
name = "no-circular"
severity = "warn"
to = { circular = true }

# features may not import each other
[[forbidden]]
name = "no-cross-feature"
severity = "error"
from = { path = '^src/features/([^/]+)/' }
to = { path = '^src/features/', path_not = '^src/features/$1/' }
```

## Already using another tool?

```sh
detangle migrate --dry-run   # preview
detangle migrate             # write detangle.toml
```

`migrate` converts JavaScript rules configs, ESLint import rules (`import/no-cycle`, `no-restricted-paths`, …), Nx module boundaries, eslint-plugin-boundaries and madge. On an existing codebase, `detangle check --write-baseline` records today's violations so CI fails only on new ones.

## In CI

```yaml
- run: npx detangle check -f github   # violations appear as annotations on the pull request
```

## Learn more

- [Full reference](https://github.com/debug-diary-1/detangle/blob/main/docs/reference.md): every command, rule option, the explorer, groups and Nx projects, the Node.js API and the HTML report.
- [Releasing](https://github.com/debug-diary-1/detangle/blob/main/docs/releasing.md)

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/debug-diary-1/detangle/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/debug-diary-1/detangle/blob/main/LICENSE-MIT), at your option.
