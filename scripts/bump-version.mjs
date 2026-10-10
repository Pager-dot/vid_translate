#!/usr/bin/env node
// Set the app version in the one place that still needs a literal, and refresh the lockfiles.
//
// `src-tauri/tauri.conf.json` is deliberately NOT in this list: it reads "../package.json",
// so Tauri picks the version up from npm. That leaves package.json (the source of truth,
// and what the release workflow names the DMG after) and the [package] version in
// src-tauri/Cargo.toml, which has to be a literal because Cargo has no such indirection.
//
//   npm run version:set -- 0.0.7
//
// Deliberately does not commit or tag — it prints the commands and lets you look first.

import { readFileSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const cargoToml = join(repoRoot, "src-tauri", "Cargo.toml");
const packageJson = join(repoRoot, "package.json");

const version = process.argv[2];
if (!version) {
  console.error("usage: npm run version:set -- <version>   (e.g. 0.0.7)");
  process.exit(1);
}
// Tags are `v<version>`, and the updater compares these with semver, so refuse anything
// that would not compare the way a human expects.
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`not a semver version: ${version}`);
  process.exit(1);
}

const run = (cmd, args, opts = {}) =>
  execFileSync(cmd, args, { cwd: repoRoot, stdio: "inherit", shell: process.platform === "win32", ...opts });

// 1. package.json — the source of truth.
run("npm", ["pkg", "set", `version=${version}`]);

// 2. Cargo.toml. Rewrite the version line *inside the [package] table only*; the file is
// full of `version = "..."` keys under [dependencies] and the per-target dependency tables,
// and a blanket regex would happily pin tauri to 0.0.7.
const toml = readFileSync(cargoToml, "utf8");
const lines = toml.split("\n");
let inPackage = false;
let patched = false;
for (let i = 0; i < lines.length; i++) {
  const line = lines[i];
  const table = line.match(/^\s*\[([^\]]+)\]/);
  if (table) {
    inPackage = table[1] === "package";
    continue;
  }
  if (inPackage && /^\s*version\s*=/.test(line)) {
    lines[i] = line.replace(/=.*$/, `= "${version}"`);
    patched = true;
    break;
  }
}
if (!patched) {
  console.error("could not find a version key in the [package] table of src-tauri/Cargo.toml");
  process.exit(1);
}
writeFileSync(cargoToml, lines.join("\n"));

// 3. Lockfiles, so the release commit is self-consistent and CI's `--locked`-ish builds
// don't drift.
run("npm", ["install", "--package-lock-only"]);
run("cargo", ["update", "-p", "vid_translate", "--manifest-path", "src-tauri/Cargo.toml"]);

console.log(`
Version set to ${version}. Review, then:

  git commit -am "chore(release): ${version}"
  git tag v${version}
  git push && git push --tag v${version}
`);
