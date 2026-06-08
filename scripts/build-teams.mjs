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
import { readdirSync, renameSync, existsSync } from "node:fs";

import { loadEnv } from "./load-env.mjs";
import { withBrand } from "./brand.mjs";

// Pull signing key + passphrase from .env if present so local builds can
// sign updater artifacts without per-shell env-var ceremony. No-op in CI.
loadEnv();

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const teamsConfigPath = join(repoRoot, "src-tauri", "tauri.teams.conf.json");
// Extra args after `--` (e.g. `npm run tauri:build:teams -- --bundles nsis`)
// are forwarded to `tauri build` so CI can scope the build to NSIS.
const extraArgs = process.argv.slice(2);

const childEnv = { ...process.env, SNIPDESK_TEAMS_BUILD: "1" };

await withBrand(async () => {
  console.log("[build-teams] vite build --mode teams");
  const vite = spawnSync(
    "npx",
    ["vite", "build", "--mode", "teams"],
    { stdio: "inherit", env: childEnv, shell: true },
  );
  if (vite.status !== 0) {
    console.error("[build-teams] vite build failed");
    process.exit(vite.status ?? 1);
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
    console.error("[build-teams] tauri build failed");
    process.exit(tauri.status ?? 1);
  }
});

// Normalize the Teams NSIS installer to a stable, version-less, ASCII name.
// Tauri derives the filename from productName ("SnipDesk" -> "SnipDesk_<ver>_
// x64-setup.exe"); a fixed name keeps the updater's
// releases/latest/download/<name> URL constant across versions and avoids the
// space/%20 mess the "SnipDesk Lite" name would otherwise carry. The .sig
// signs the installer bytes, not the filename, so renaming is safe.
// `^SnipDesk_` (underscore) matches only the Teams build, never the Lite one
// ("SnipDesk Lite_...", a space) even when both land in the same dir in CI.
const nsisDir = join(repoRoot, "target", "release", "bundle", "nsis");
const built = existsSync(nsisDir)
  ? readdirSync(nsisDir).find((f) => /^SnipDesk_.*_x64-setup\.exe$/.test(f))
  : undefined;
if (!built) {
  console.error(`[build-teams] could not find Teams NSIS installer in ${nsisDir}`);
  process.exit(1);
}
for (const [from, to] of [
  [built, "SnipDesk-Teams-setup.exe"],
  [`${built}.sig`, "SnipDesk-Teams-setup.exe.sig"],
]) {
  const fromPath = join(nsisDir, from);
  if (existsSync(fromPath)) {
    renameSync(fromPath, join(nsisDir, to));
    console.log(`[build-teams] renamed ${from} -> ${to}`);
  }
}

console.log("[build-teams] done");
