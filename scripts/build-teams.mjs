#!/usr/bin/env node
//
// Two-step Teams build:
//   1. vite build --mode teams         (Teams-flavored frontend bundle)
//   2. tauri build --features teams    (Rust binary + MSI)
//
// Step 2 overrides Tauri's beforeBuildCommand so it doesn't re-run vite in
// free mode and clobber dist/. Done in a node script rather than an npm
// chain because cross-shell env vars and the inline JSON config override
// don't survive package.json quoting on Windows.

import { spawnSync } from "node:child_process";
import process from "node:process";

const childEnv = { ...process.env, SNIPDESK_TEAMS_BUILD: "1" };

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

const tauriConfigOverride = JSON.stringify({
  build: { beforeBuildCommand: "" },
});

console.log("[build-teams] tauri build --features teams");
const tauri = spawnSync(
  "npx",
  ["tauri", "build", "--features", "teams", "--config", tauriConfigOverride],
  { stdio: "inherit", env: childEnv, shell: true },
);
if (tauri.status !== 0) {
  console.error("[build-teams] tauri build failed");
  process.exit(tauri.status ?? 1);
}

console.log("[build-teams] done");
