#!/usr/bin/env node
//
// Two-step Teams build:
//   1. vite build --mode teams         (Teams-flavored frontend bundle)
//   2. tauri build --features teams    (Rust binary + MSI)
//
// Step 2 overrides Tauri's beforeBuildCommand so it doesn't re-run vite in
// free mode and clobber dist/, and repoints the updater at the Teams manifest.
// Those overrides live in src-tauri/tauri.teams.conf.json and are passed to
// `tauri build` by PATH - NOT as an inline JSON string. An inline
// `--config '{...}'` gets its double-quotes stripped by the Windows shell
// (shell: true), so tauri receives `{build:{...}}` and dies with
// "key must be a string". A file path has nothing for the shell to mangle.

import { spawnSync } from "node:child_process";
import process from "node:process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { readdirSync, renameSync, existsSync, readFileSync } from "node:fs";

import { loadEnv } from "./load-env.mjs";
import { withBrand, parseBrandFlag } from "./brand.mjs";

// Resolve the brand-derived names this build will produce. Reads
// $BRAND_CONFIG up front (before withBrand restores) so the
// post-build rename can find the file by its full productName-
// derived prefix and emit a slug-derived stable filename. Vanilla
// builds with no BRAND_CONFIG fall through to the historical
// "SnipDesk" / "SnipDesk-Teams-setup.exe" pair, so the existing
// updater URL keeps resolving.
function resolveBrandNames() {
  const path = process.env.BRAND_CONFIG;
  if (!path || !existsSync(path)) {
    return { sourcePrefix: "SnipDesk", installerPrefix: "SnipDesk" };
  }
  const cfg = JSON.parse(readFileSync(path, "utf8"));
  const name = cfg.name || "SnipDesk";
  const installerPrefix =
    (typeof cfg.slug === "string" && cfg.slug) ||
    name.replace(/[^A-Za-z0-9]/g, "");
  return { sourcePrefix: name, installerPrefix };
}

// Pull signing key + passphrase from .env if present so local builds can
// sign updater artifacts without per-shell env-var ceremony. No-op in CI.
loadEnv();

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const teamsConfigPath = join(repoRoot, "src-tauri", "tauri.teams.conf.json");

// Lift --whitelabel=<slug|path> out of the forwarded args. When
// present, BRAND_CONFIG gets pointed at the resolved bundle and
// the leftover args travel on to tauri. See tauri-build.mjs for
// usage examples.
const { brandConfigPath, remainingArgs } = parseBrandFlag(process.argv.slice(2));
if (brandConfigPath) {
  process.env.BRAND_CONFIG = brandConfigPath;
  console.log(`[build-teams] [brand] using bundle: ${brandConfigPath}`);
}
// Extra args after `--` (e.g. `npm run tauri:build:teams -- --bundles nsis`)
// are forwarded to `tauri build` so CI can scope the build to NSIS.
const extraArgs = remainingArgs;

const childEnv = { ...process.env, SNIPDESK_TEAMS_BUILD: "1" };

// Throwing inside the callback instead of process.exit'ing lets
// withBrand's finally block run and restore the worktree before
// the error propagates. A bare process.exit() from here bypasses
// the finally entirely - that's how a failed build used to leave
// the tree in a substituted state until the next git restore.
try {
  await withBrand(async () => {
    console.log("[build-teams] vite build --mode teams");
    const vite = spawnSync(
      "npx",
      ["vite", "build", "--mode", "teams"],
      { stdio: "inherit", env: childEnv, shell: true },
    );
    if (vite.status !== 0) {
      throw new Error(`[build-teams] vite build failed (exit ${vite.status})`);
    }

    console.log(
      `[build-teams] tauri build --features teams --config ${teamsConfigPath} ${extraArgs.join(" ")}`,
    );
    const tauri = spawnSync(
      "npx",
      ["tauri", "build", "--features", "teams", "--config", teamsConfigPath, ...extraArgs],
      { stdio: "inherit", env: childEnv, shell: true },
    );
    if (tauri.status !== 0) {
      throw new Error(`[build-teams] tauri build failed (exit ${tauri.status})`);
    }
  });
} catch (err) {
  console.error(err.message || err);
  process.exit(1);
}

// Normalize the Teams NSIS installer to a stable, version-less, ASCII name.
// Tauri derives the filename from productName ("SnipDesk" -> "SnipDesk_<ver>_
// x64-setup.exe"); a fixed name keeps the updater's
// releases/latest/download/<name> URL constant across versions and avoids the
// space/%20 mess the "SnipDesk Lite" name would otherwise carry. The .sig
// signs the installer bytes, not the filename, so renaming is safe.
// For brand-overridden builds the source prefix is the full
// productName (no trailing space, since the Teams config sets
// productName without the " Lite" suffix) and the target prefix
// is the slug-derived ASCII form.
const nsisDir = join(repoRoot, "target", "release", "bundle", "nsis");
const { sourcePrefix, installerPrefix } = resolveBrandNames();
const sourceRegex = new RegExp(
  `^${sourcePrefix.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}_.*_x64-setup\\.exe$`,
);
const built = existsSync(nsisDir)
  ? readdirSync(nsisDir).find((f) => sourceRegex.test(f))
  : undefined;
if (!built) {
  console.error(`[build-teams] could not find Teams NSIS installer in ${nsisDir}`);
  process.exit(1);
}
const targetExe = `${installerPrefix}-Teams-setup.exe`;
for (const [from, to] of [
  [built, targetExe],
  [`${built}.sig`, `${targetExe}.sig`],
]) {
  const fromPath = join(nsisDir, from);
  if (existsSync(fromPath)) {
    renameSync(fromPath, join(nsisDir, to));
    console.log(`[build-teams] renamed ${from} -> ${to}`);
  }
}

console.log("[build-teams] done");
