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
import { withBrand } from "./brand.mjs";

loadEnv();

const extraArgs = process.argv.slice(2);

const code = await withBrand(() => {
  const r = spawnSync("npx", ["tauri", "build", ...extraArgs], {
    stdio: "inherit",
    env: process.env,
    shell: true,
  });
  return r.status ?? 1;
});
process.exit(code);
