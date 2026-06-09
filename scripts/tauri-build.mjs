#!/usr/bin/env node
//
// Local-friendly wrapper around `tauri build` for the Lite flavor. Loads
// .env so the signing key + passphrase reach Tauri without the user having
// to set env vars in every shell session. Extra args after the npm script
// are forwarded to tauri (e.g. `npm run tauri:build -- --bundles nsis`).
//
// CI doesn't go through this wrapper - release.yml calls `npx
// @tauri-apps/cli build` directly with the env vars from GitHub Secrets.

import { spawnSync } from "node:child_process";
import process from "node:process";

import { loadEnv } from "./load-env.mjs";
import { withBrand, parseBrandFlag } from "./brand.mjs";
import { runPreflight } from "./preflight.mjs";

loadEnv();
runPreflight();

// Lift --whitelabel=<slug|path> out of the forwarded args. When
// present, BRAND_CONFIG gets pointed at the resolved bundle and
// the leftover args travel on to tauri. Either of these works:
//   npm run tauri:build -- --whitelabel=acme
//   npm run tauri:build -- --whitelabel=acme --bundles nsis
//   BRAND_CONFIG=brands/acme/brand.json npm run tauri:build
const { brandConfigPath, remainingArgs } = parseBrandFlag(process.argv.slice(2));
if (brandConfigPath) {
  process.env.BRAND_CONFIG = brandConfigPath;
  console.log(`[brand] using bundle: ${brandConfigPath}`);
}
const extraArgs = remainingArgs;

const code = await withBrand(() => {
  const r = spawnSync("npx", ["tauri", "build", ...extraArgs], {
    stdio: "inherit",
    env: process.env,
    shell: true,
  });
  return r.status ?? 1;
});
process.exit(code);
