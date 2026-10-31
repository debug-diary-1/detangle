# Releasing

1. Set the same version in `Cargo.toml`, `npm/package.json` (with its platform `optionalDependencies`) and `.pre-commit-hooks.yaml` (`detangle@<version>`), and commit. A test fails if the pre-commit hook's version doesn't match.
2. Tag it and push the tag: `git tag -a v0.2.0 -m "detangle 0.2.0" && git push origin v0.2.0`.

The `Release` workflow then builds binaries for all eight targets, attaches them (with `SHA256SUMS`) to a GitHub release, and publishes the npm packages, the crate and the Homebrew formula.

## After publishing

The workflow's last job, `after-publish`, does what used to be done by hand:

1. It waits for npm to serve the new tarballs (`detangle-linux-x64-musl-<version>.tgz` and `detangle-<version>.tgz`), checking every 30 seconds for up to 40 minutes. npm has taken 2 to 25 minutes.
2. It runs `install-check.yml` with the version, which installs the published package on every platform, and fails if that run fails.
3. It opens the lockfile PR, "npm: lock the published <version> platform packages", from the branch `lock-<version>`: `npm install --package-lock-only --ignore-scripts --prefer-online` in `npm/`, which fills `npm/package-lock.json`'s platform entries (the release commit leaves them as `{ "optional": true }`). Until it's merged, `npm ci` fails on every open PR.

What's still manual:

- Start CI on the lockfile PR. A PR opened with the workflow's `GITHUB_TOKEN` doesn't trigger workflows, so push an empty commit to its branch (`git commit --allow-empty -m "Run CI"`), then merge it when CI is green.
- If the job fails, rerun it or finish its remaining steps by hand: `gh workflow run install-check.yml -f version=<version>`, or the lockfile commands above. If it couldn't open the PR, its summary links to one ready to open.

Opening the PR needs the repository setting "Allow GitHub Actions to create and approve pull requests" (Settings, Actions, General, Workflow permissions).

npm and crates.io use trusted publishing, so no registry tokens are stored in the repository. Each package on npmjs.com (`detangle` and the eight `detangle-*` platform packages) and the `detangle` crate on crates.io trust GitHub Actions, owner `debug-diary-1`, repository `detangle`, workflow `release.yml`. The only secret is `HOMEBREW_TAP_TOKEN`, a fine-grained GitHub token with Contents read/write on `debug-diary-1/homebrew-tap`.

A new npm package or crate can't be created by trusted publishing: publish its first version by hand, then add the trusted publisher.
