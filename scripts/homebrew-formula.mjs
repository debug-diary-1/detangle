#!/usr/bin/env node
// Prints the Homebrew formula for a release.
//
//   node scripts/homebrew-formula.mjs <version> <dist-dir>
//
// <dist-dir> holds the release archives (detangle-<version>-<target>.tar.gz).
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const [version, dist] = process.argv.slice(2);
if (!version || !dist) {
  console.error("usage: homebrew-formula.mjs <version> <dist-dir>");
  process.exit(2);
}
const base = `https://github.com/debug-diary-1/detangle/releases/download/v${version}`;
const asset = (target) => {
  const file = `detangle-${version}-${target}.tar.gz`;
  const sha = crypto.createHash("sha256").update(fs.readFileSync(path.join(dist, file))).digest("hex");
  return `url "${base}/${file}"\n      sha256 "${sha}"`;
};

process.stdout.write(`class Detangle < Formula
  desc "Fast dependency analysis and architecture rules for JavaScript and TypeScript"
  homepage "https://github.com/debug-diary-1/detangle"
  version "${version}"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      ${asset("aarch64-apple-darwin")}
    end
    on_intel do
      ${asset("x86_64-apple-darwin")}
    end
  end

  on_linux do
    on_arm do
      ${asset("aarch64-unknown-linux-musl")}
    end
    on_intel do
      ${asset("x86_64-unknown-linux-musl")}
    end
  end

  def install
    bin.install "detangle"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/detangle --version")
  end
end
`);
