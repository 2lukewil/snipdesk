#!/usr/bin/env node
//
// White-label prebuild step. Generates the per-build brand constants
// and patches the Tauri configs so customer builds get their own
// name, identifier, baked server URL, and updater endpoint while
// the main github repo stays brand-neutral.
//
// Run automatically by:
//   - postinstall  (generates stock defaults so a fresh clone has
//     working tauri.conf.json / whitelabel.rs / whitelabel.js)
//   - the tauri-build / build-teams / dev-teams wrappers, which
//     invoke this first so any environment change is reflected in
//     the very next build.
//
// $WHITELABEL_CONFIG (env): absolute path to a TOML file living
// OUTSIDE this repo. When unset, defaults produce a stock
// "SnipDesk" build.
//
// The script ALWAYS restores tauri.conf.json + tauri.teams.conf.json
// from their .template counterparts before patching, so the patch
// is reproducible across runs and never accumulates drift.
//
// Generated files (all gitignored):
//   - src-tauri/src/whitelabel.rs
//   - src/whitelabel.js
//   - src-tauri/tauri.conf.json        (rewritten from .template)
//   - src-tauri/tauri.teams.conf.json  (rewritten from .template)

import { readFileSync, writeFileSync, existsSync, copyFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { parse as parseToml } from "smol-toml";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const TAURI_CONF = join(repoRoot, "src-tauri", "tauri.conf.json");
const TAURI_CONF_TEMPLATE = `${TAURI_CONF}.template`;
const TAURI_TEAMS_CONF = join(repoRoot, "src-tauri", "tauri.teams.conf.json");
const TAURI_TEAMS_CONF_TEMPLATE = `${TAURI_TEAMS_CONF}.template`;
const RUST_OUT = join(repoRoot, "src-tauri", "src", "whitelabel.rs");
const JS_OUT = join(repoRoot, "src", "whitelabel.js");

// Stock defaults. Match the pre-whitelabel hard-coded values so a
// missing-config build is byte-identical to a pre-whitelabel build.
const DEFAULTS = {
  brand: {
    name: "SnipDesk",
    short_name: "SnipDesk",
    identifier: "com.snipdesk.lite",
    teams_identifier: "com.snipdesk.teams",
    long_description:
      "SnipDesk is a fast, searchable snippet launcher triggered by a global hotkey. Built for support agents who send similar replies many times a day.",
  },
  server: {
    baked_url: null,
    sso_only: false,
  },
  updater: {
    endpoint:
      "https://github.com/2lukewil/snipdesk/releases/latest/download/snipdesk-update.json",
    teams_endpoint:
      "https://github.com/2lukewil/snipdesk/releases/latest/download/snipdesk-teams-update.json",
  },
};

function loadConfig() {
  const path = process.env.WHITELABEL_CONFIG;
  if (!path) {
    console.log("[whitelabel] no WHITELABEL_CONFIG set; using stock defaults");
    return DEFAULTS;
  }
  if (!existsSync(path)) {
    console.error(
      `[whitelabel] WHITELABEL_CONFIG points to a file that doesn't exist: ${path}`,
    );
    process.exit(1);
  }
  let parsed;
  try {
    parsed = parseToml(readFileSync(path, "utf8"));
  } catch (e) {
    console.error(`[whitelabel] failed to parse ${path}: ${e.message}`);
    process.exit(1);
  }
  console.log(`[whitelabel] applying config from ${path}`);
  return mergeDeep(DEFAULTS, parsed);
}

// Shallow per-section merge. Customer config wins; missing keys
// fall back to DEFAULTS. We don't recurse beyond one level because
// the schema is flat enough that "deep merge" would just confuse
// readers.
function mergeDeep(base, override) {
  const out = {};
  const sections = new Set([...Object.keys(base), ...Object.keys(override)]);
  for (const key of sections) {
    const bv = base[key];
    const ov = override[key];
    if (bv && ov && typeof bv === "object" && typeof ov === "object" && !Array.isArray(bv)) {
      out[key] = { ...bv, ...ov };
    } else if (ov !== undefined) {
      out[key] = ov;
    } else {
      out[key] = bv;
    }
  }
  return out;
}

// Restore one of the live Tauri configs from its checked-in
// template. Failing here is fatal because nothing downstream works
// without the file.
function restoreFromTemplate(live, template) {
  if (!existsSync(template)) {
    console.error(
      `[whitelabel] missing template: ${template}\n` +
        `  the script can't generate ${live} without it`,
    );
    process.exit(1);
  }
  copyFileSync(template, live);
}

function patchTauriConfig(cfg) {
  const conf = JSON.parse(readFileSync(TAURI_CONF, "utf8"));
  conf.productName = `${cfg.brand.name} Lite`;
  conf.identifier = cfg.brand.identifier;
  conf.app.windows[0].title = cfg.brand.name;
  conf.bundle.longDescription = cfg.brand.long_description;
  if (cfg.updater.endpoint) {
    conf.plugins.updater.endpoints = [cfg.updater.endpoint];
  }
  writeFileSync(TAURI_CONF, JSON.stringify(conf, null, 2) + "\n");
}

function patchTauriTeamsConfig(cfg) {
  const conf = JSON.parse(readFileSync(TAURI_TEAMS_CONF, "utf8"));
  conf.productName = cfg.brand.name;
  conf.identifier = cfg.brand.teams_identifier;
  if (cfg.updater.teams_endpoint) {
    conf.plugins.updater.endpoints = [cfg.updater.teams_endpoint];
  }
  writeFileSync(TAURI_TEAMS_CONF, JSON.stringify(conf, null, 2) + "\n");
}

// Rust string literal escape. Brand strings come from TOML so we
// only need to dodge backslash + double-quote.
function rustString(s) {
  if (s === null || s === undefined) return "None";
  return `Some("${s.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}")`;
}

function writeRustConstants(cfg) {
  const body = [
    "//! AUTO-GENERATED by scripts/whitelabel.mjs - do not edit by hand.",
    "//!",
    "//! Per-build brand constants. The script regenerates this file from",
    "//! $WHITELABEL_CONFIG (or stock defaults) on every prebuild.",
    "",
    `pub const BRAND_NAME: &str = "${cfg.brand.name.replace(/"/g, '\\"')}";`,
    `pub const SHORT_NAME: &str = "${cfg.brand.short_name.replace(/"/g, '\\"')}";`,
    `pub const BAKED_SERVER_URL: Option<&str> = ${rustString(cfg.server.baked_url)};`,
    `pub const SSO_ONLY: bool = ${cfg.server.sso_only ? "true" : "false"};`,
    `pub const UPDATER_ENDPOINT: Option<&str> = ${rustString(cfg.updater.endpoint)};`,
    "",
  ].join("\n");
  writeFileSync(RUST_OUT, body);
}

function writeJsConstants(cfg) {
  const obj = {
    name: cfg.brand.name,
    shortName: cfg.brand.short_name,
    bakedServerUrl: cfg.server.baked_url,
    ssoOnly: !!cfg.server.sso_only,
    updaterEndpoint: cfg.updater.endpoint,
  };
  const body =
    "// AUTO-GENERATED by scripts/whitelabel.mjs - do not edit by hand.\n" +
    "// Per-build brand constants. Loaded by src/main.js at boot.\n" +
    `window.__BRAND = ${JSON.stringify(obj, null, 2)};\n` +
    "export default window.__BRAND;\n";
  writeFileSync(JS_OUT, body);
}

function main() {
  const cfg = loadConfig();
  restoreFromTemplate(TAURI_CONF, TAURI_CONF_TEMPLATE);
  restoreFromTemplate(TAURI_TEAMS_CONF, TAURI_TEAMS_CONF_TEMPLATE);
  patchTauriConfig(cfg);
  patchTauriTeamsConfig(cfg);
  writeRustConstants(cfg);
  writeJsConstants(cfg);
  console.log(`[whitelabel] wrote ${RUST_OUT}`);
  console.log(`[whitelabel] wrote ${JS_OUT}`);
  console.log(`[whitelabel] patched ${TAURI_CONF}`);
  console.log(`[whitelabel] patched ${TAURI_TEAMS_CONF}`);
}

main();
