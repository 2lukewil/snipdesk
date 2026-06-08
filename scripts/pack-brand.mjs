#!/usr/bin/env node
//
// Pack a brand bundle into the base64 blob that goes into the
// BRAND_BUNDLE_WHITELABEL GitHub Secret.
//
// Usage:
//   node scripts/pack-brand.mjs <bundle-dir>
//   node scripts/pack-brand.mjs brands/acme
//
// Writes the tarball + the base64 output to a scratch dir under
// the OS temp, prints the path you paste into GitHub's "Secret
// value" field, and shows the resulting tag URLs CI will produce
// on the next tag push.
//
// Cross-platform: relies on the `tar` CLI (Windows 10 1803+,
// macOS, Linux all ship one). No new deps.

import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, resolve, sep } from "node:path";
import { tmpdir } from "node:os";

const args = process.argv.slice(2).filter((a) => !a.startsWith("-"));
if (args.length !== 1) {
  console.error("usage: node scripts/pack-brand.mjs <bundle-dir>");
  console.error("  e.g. node scripts/pack-brand.mjs brands/acme");
  process.exit(2);
}
const bundleDir = resolve(args[0]);

if (!existsSync(bundleDir) || !statSync(bundleDir).isDirectory()) {
  console.error(`[pack-brand] not a directory: ${bundleDir}`);
  process.exit(1);
}
const brandJsonPath = join(bundleDir, "brand.json");
if (!existsSync(brandJsonPath)) {
  console.error(`[pack-brand] missing brand.json in ${bundleDir}`);
  process.exit(1);
}

let cfg;
try {
  cfg = JSON.parse(readFileSync(brandJsonPath, "utf8"));
} catch (e) {
  console.error(`[pack-brand] brand.json is not valid JSON: ${e.message}`);
  process.exit(1);
}
for (const required of ["name", "identifier", "teams_identifier"]) {
  if (typeof cfg[required] !== "string" || !cfg[required]) {
    console.error(`[pack-brand] brand.json missing required field: ${required}`);
    process.exit(1);
  }
}

// Slug derivation matches scripts/brand.mjs + release-server.yml so
// the operator can predict the resulting image / installer names.
const slug = cfg.slug || cfg.name.replace(/[^A-Za-z0-9]/g, "").toLowerCase();
const installerPrefix =
  (typeof cfg.slug === "string" && cfg.slug) ||
  cfg.name.replace(/[^A-Za-z0-9]/g, "");

const outDir = join(tmpdir(), "snipdesk-brand-bundles");
mkdirSync(outDir, { recursive: true });
const tgzPath = join(outDir, `${slug}-bundle.tgz`);
const b64Path = join(outDir, `${slug}-bundle.b64`);

console.log(`[pack-brand] packing ${bundleDir}`);
console.log(`[pack-brand]   brand: ${cfg.name}`);
console.log(`[pack-brand]   slug:  ${slug}`);

// `tar -czf <tgz> -C <dir> .` packs the bundle contents (not the
// dir itself) so the extracted layout matches what brand.mjs +
// release.yml expect (brand.json at the bundle root, not nested).
// --force-local stops GNU tar (default on Git Bash for Windows)
// from misreading the colon in a Windows path like `C:\...` as a
// remote-host separator. bsdtar (Windows native tar.exe + macOS)
// accepts the flag as a documented synonym, so it's portable.
const tar = spawnSync(
  "tar",
  ["--force-local", "-czf", tgzPath, "-C", bundleDir, "."],
  { stdio: "inherit" },
);
if (tar.status !== 0) {
  console.error(`[pack-brand] tar failed (exit ${tar.status})`);
  process.exit(1);
}

const tgzBytes = readFileSync(tgzPath);
// Single-line base64 with no line breaks - the secret value is
// pasted into a single GitHub Secrets field and a literal newline
// in the middle would have to round-trip through the decoder.
const b64 = tgzBytes.toString("base64");
writeFileSync(b64Path, b64);

const sizeKB = (n) => `${(n / 1024).toFixed(1)} KB`;
const b64Size = Buffer.byteLength(b64, "utf8");

console.log(`[pack-brand] wrote ${tgzPath} (${sizeKB(tgzBytes.length)})`);
console.log(`[pack-brand] wrote ${b64Path} (${sizeKB(b64Size)})`);
console.log("");
console.log("Next steps:");
console.log("  1. Open https://github.com/<owner>/<repo>/settings/secrets/actions");
console.log("  2. New repository secret -> name: BRAND_BUNDLE_WHITELABEL");
console.log(`  3. Paste the contents of: ${b64Path}`);
console.log("");
console.log("Or copy straight to the clipboard:");
console.log(`  Windows (PowerShell): Get-Content "${b64Path}" -Raw | Set-Clipboard`);
console.log(`  macOS:                pbcopy < "${b64Path}"`);
console.log(`  Linux (Wayland):      wl-copy < "${b64Path}"`);
console.log(`  Linux (X11):          xclip -selection clipboard < "${b64Path}"`);
console.log("");
console.log("Once the secret is in, the next tag push produces:");
console.log("  Desktop (on a `v*` tag):");
console.log(`    target/release/bundle/nsis/${installerPrefix}-Lite-setup.exe`);
console.log(`    target/release/bundle/nsis/${installerPrefix}-Teams-setup.exe`);
console.log(`    snipdesk-${slug}-update.json`);
console.log(`    snipdesk-${slug}-teams-update.json`);
console.log("  Server (on a `server-v*` tag):");
console.log(`    ghcr.io/2lukewil/snipdesk/snipdesk-server-${slug}:<version>`);
console.log(`    ghcr.io/2lukewil/snipdesk/snipdesk-server-${slug}:latest`);
