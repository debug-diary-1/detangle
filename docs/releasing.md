# Releasing

1. Set the same version in `Cargo.toml` and `npm/package.json`, and commit.
2. Tag it and push the tag: `git tag -a v0.2.0 -m "detangle 0.2.0" && git push origin v0.2.0`.

The `Release` workflow then builds binaries for all eight targets, attaches them (with `SHA256SUMS`) to a GitHub release, and publishes the npm packages, the crate and the Homebrew formula.

npm and crates.io use trusted publishing, so no registry tokens are stored in the repository. Each package on npmjs.com (`detangle` and the eight `detangle-*` platform packages) and the `detangle` crate on crates.io trust GitHub Actions, owner `debug-diary-1`, repository `detangle`, workflow `release.yml`. The only secret is `HOMEBREW_TAP_TOKEN`, a fine-grained GitHub token with Contents read/write on `debug-diary-1/homebrew-tap`.

A new npm package or crate can't be created by trusted publishing: publish its first version by hand, then add the trusted publisher.
