# Releasing

1. Set the same version in `Cargo.toml` and `npm/package.json`, and commit.
2. Tag it and push the tag: `git tag v0.1.0 && git push origin v0.1.0`.

The `Release` workflow then builds binaries for all eight targets, attaches them (with `SHA256SUMS`) to a GitHub release, and publishes the npm packages, the crate and the Homebrew formula. It needs the repository secrets `NPM_TOKEN`, `CARGO_REGISTRY_TOKEN` and `HOMEBREW_TAP_TOKEN`.
