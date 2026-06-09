#!/usr/bin/env node
//
// Teams-flavored dev loop: tauri dev with the teams cargo feature, the
// teams Tauri conf, and SNIPDESK_TEAMS_BUILD=1 in the environment so
// the vite child (spawned by tauri's beforeDevCommand) picks the teams
// branches via vite.config.js's env-var safety net.
//
// PowerShell on Windows can't set per-command env vars without cross-env
// or `$env:X = "1"; ...`. This script sets it in the spawned process's
// env so it propagates to tauri AND down to its vite child.

import { spawn } from "node:child_process";
import { join, resolve } from "node:path";

import { loadEnv } from "./load-env.mjs";
import { withBrand, parseBrandFlag } from "./brand.mjs";
import { runPreflight } from "./preflight.mjs";

loadEnv();
runPreflight();

// `import.meta.dirname` is the Node 20.11+ canonical replacement for the
// older `dirname(fileURLToPath(import.meta.url))` polyfill.
const repoRoot = resolve(import.meta.dirname, "..");
const teamsConfigPath = join(repoRoot, "src-tauri", "tauri.teams.conf.json");

const { brandConfigPath, remainingArgs } = parseBrandFlag(process.argv.slice(2));
if (brandConfigPath) {
  process.env.BRAND_CONFIG = brandConfigPath;
  console.log(`[dev-teams] [brand] using bundle: ${brandConfigPath}`);
}
const extraArgs = remainingArgs;

const childEnv = { ...process.env, SNIPDESK_TEAMS_BUILD: "1" };

await withBrand(
  () =>
    new Promise((resolveFn) => {
      console.log(
        `[dev-teams] tauri dev --features teams --config ${teamsConfigPath} ${extraArgs.join(" ")}`,
      );
      const child = spawn(
        "npx",
        ["tauri", "dev", "--features", "teams", "--config", teamsConfigPath, ...extraArgs],
        { stdio: "inherit", env: childEnv, shell: true },
      );
      child.on("exit", (code) => resolveFn(code ?? 0));
    }),
);
