#!/usr/bin/env node
// Builds the npm packages for a release.
//
//   node scripts/npm-packages.mjs <version> <binaries-dir> <out-dir>
//
// <binaries-dir> holds one directory per Rust target (as in
// npm/platforms.json), each containing the detangle binary. Writes one
// platform package per target plus the main package (npm/) to <out-dir>,
// all at <version>.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const [version, binaries, out] = process.argv.slice(2);
if (!version || !binaries || !out) {
  console.error("usage: npm-packages.mjs <version> <binaries-dir> <out-dir>");
  process.exit(2);
}
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const platforms = JSON.parse(fs.readFileSync(path.join(root, "npm/platforms.json"), "utf8"));
const main = JSON.parse(fs.readFileSync(path.join(root, "npm/package.json"), "utf8"));
const licenses = ["LICENSE-MIT", "LICENSE-APACHE"];

fs.rmSync(out, { recursive: true, force: true });
for (const p of platforms) {
  const exe = p.os === "win32" ? "detangle.exe" : "detangle";
  const from = path.join(binaries, p.target, exe);
  if (!fs.existsSync(from)) throw new Error(`missing binary for ${p.target}: ${from}`);
  const dir = path.join(out, p.package);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  fs.copyFileSync(from, path.join(dir, "bin", exe));
  fs.chmodSync(path.join(dir, "bin", exe), 0o755);
  for (const l of licenses) fs.copyFileSync(path.join(root, l), path.join(dir, l));
  const pkg = {
    name: p.package,
    version,
    description: `The detangle binary for ${p.os} ${p.cpu}${p.libc ? ` (${p.libc})` : ""}. Install \`detangle\` instead.`,
    license: main.license,
    repository: main.repository,
    os: [p.os],
    cpu: [p.cpu],
    ...(p.libc ? { libc: [p.libc] } : {}),
    files: ["bin", ...licenses],
    preferUnplugged: true,
  };
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
  fs.writeFileSync(path.join(dir, "README.md"), `# ${p.package}\n\nThe \`detangle\` binary for ${p.os} ${p.cpu}${p.libc ? ` (${p.libc})` : ""}. Install [\`detangle\`](https://www.npmjs.com/package/detangle) instead; npm picks this package automatically.\n`);
}

const dir = path.join(out, "detangle");
fs.mkdirSync(dir, { recursive: true });
for (const f of main.files) {
  fs.mkdirSync(path.dirname(path.join(dir, f)), { recursive: true });
  fs.copyFileSync(path.join(root, "npm", f), path.join(dir, f));
}
for (const l of licenses) fs.copyFileSync(path.join(root, l), path.join(dir, l));
fs.copyFileSync(path.join(root, "README.md"), path.join(dir, "README.md"));
const pkg = { ...main, version, files: [...main.files, ...licenses] };
pkg.optionalDependencies = Object.fromEntries(platforms.map((p) => [p.package, version]));
fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
console.log(`wrote ${platforms.length + 1} packages to ${out}`);
