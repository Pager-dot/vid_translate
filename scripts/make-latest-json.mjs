#!/usr/bin/env node
// Build the updater manifest from the artifacts the release job collected.
//
//   node scripts/make-latest-json.mjs <dist-dir> <tag>
//
// Why this is hand-rolled rather than left to tauri-action: this repo builds with
// `tauri build` directly, hand-rolls the DMG, and repacks the AppImage *after* the bundler
// runs — so the workflow re-signs those artifacts itself and only it knows the final file
// names.
//
// The hard-fail on an incomplete platform matters more than it looks: Tauri validates the
// WHOLE manifest before it compares versions, so a single missing signature doesn't degrade
// that one platform — it breaks updates for every platform at once. Better to fail the
// release than to publish a manifest that silently bricks the update channel.

import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const [distDir, tag] = process.argv.slice(2);
if (!distDir || !tag) {
  console.error("usage: make-latest-json.mjs <dist-dir> <tag>");
  process.exit(1);
}

const version = tag.replace(/^v/, "");
const repo = process.env.GITHUB_REPOSITORY || "Pager-dot/vid_translate";

// download-artifact flattens with merge-multiple, but a nested layout is still possible.
function walk(dir) {
  return readdirSync(dir).flatMap((entry) => {
    const full = join(dir, entry);
    return statSync(full).isDirectory() ? walk(full) : [full];
  });
}
const files = walk(distDir);
const basename = (f) => f.split("/").pop();

// Which artifact each platform updates FROM. Only these three formats are updater-capable:
// the .deb is owned by dpkg, the .msi is a second Windows installer we can't also list, and
// the .dmg is a first-time download wrapping the same .app as the tarball.
const PLATFORMS = {
  "darwin-aarch64": (f) => f.endsWith(".app.tar.gz"),
  "linux-x86_64": (f) => f.endsWith(".AppImage"),
  "windows-x86_64": (f) => f.endsWith("-setup.exe"),
};

const platforms = {};
const problems = [];

for (const [key, match] of Object.entries(PLATFORMS)) {
  const found = files.filter((f) => match(basename(f)));
  if (found.length === 0) {
    problems.push(`${key}: no updater artifact found`);
    continue;
  }
  if (found.length > 1) {
    problems.push(`${key}: ${found.length} candidates, cannot choose: ${found.map(basename).join(", ")}`);
    continue;
  }
  const artifact = found[0];
  const sigPath = `${artifact}.sig`;
  let signature;
  try {
    // The manifest wants the CONTENTS of the .sig file, not a path or a URL.
    signature = readFileSync(sigPath, "utf8").trim();
  } catch {
    problems.push(`${key}: ${basename(artifact)} has no signature next to it (${basename(sigPath)})`);
    continue;
  }
  if (!signature) {
    problems.push(`${key}: ${basename(sigPath)} is empty`);
    continue;
  }
  platforms[key] = {
    url: `https://github.com/${repo}/releases/download/${tag}/${encodeURIComponent(basename(artifact))}`,
    signature,
  };
}

if (problems.length) {
  console.error("::error::cannot build latest.json — the update channel would break for ALL platforms:");
  for (const p of problems) console.error(`  - ${p}`);
  process.exit(1);
}

const manifest = {
  version,
  notes: `See https://github.com/${repo}/releases/tag/${tag}`,
  pub_date: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
  platforms,
};

const out = join(distDir, "latest.json");
writeFileSync(out, JSON.stringify(manifest, null, 2) + "\n");
console.log(`Wrote ${out} for ${version}:`);
for (const [k, v] of Object.entries(platforms)) console.log(`  ${k} -> ${decodeURIComponent(v.url.split("/").pop())}`);
