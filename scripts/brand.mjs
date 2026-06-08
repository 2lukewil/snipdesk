// Per-build constant substitution. Optional: when $BRAND_CONFIG
// points at a JSON file, the wrapper substitutes a small fixed set
// of identifiers in tracked source for the duration of one build,
// then restores the originals. When $BRAND_CONFIG is unset, this
// is a complete no-op and the build runs identically to before.
//
// Tracked source always carries the project's own constants. A
// mid-build crash leaves the working tree dirty; the wrapper traps
// SIGINT/SIGTERM and runs restore, and a final restore lives inside
// a `finally` block so the only way to lose the original is a
// SIGKILL. A pre-substitution `git diff --quiet` check ensures we
// never overwrite uncommitted work in the target files.
//
// Config shape (JSON object at the path in $BRAND_CONFIG):
//   {
//     "name":              "<display name>",        // required
//     "identifier":        "<lite bundle id>",      // required
//     "teams_identifier":  "<teams bundle id>",     // required
//     "updater_url":       "<lite updater feed>",   // optional
//     "teams_updater_url": "<teams updater feed>",  // optional
//     "deep_link_scheme":  "<custom URL scheme>",   // optional
//     "server_url":        "<default server URL>",  // optional
//     "sso_only":          true | false             // optional
//   }
//
// server_url + sso_only seed defaults in Settings::default(); end
// users can still flip both from the Team Library settings panel.

import { readFileSync, writeFileSync } from "node:fs";
import { execSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// Files touched by substitution. Keep this list in sync with any
// new source that hard-codes one of the constants below.
const TARGETS = [
  "src-tauri/tauri.conf.json",
  "src-tauri/tauri.teams.conf.json",
  "src-tauri/src/lib.rs",
  "src/index.html",
  "src/main.js",
  "crates/snipdesk-core/src/settings.rs",
];

// Stock constants that have a customer-overridable counterpart.
// `from` is searched verbatim (case-sensitive); `to` resolves at
// runtime from the loaded config. A missing config field skips that
// rule entirely so partial overrides are safe.
const RULES = [
  { from: "SnipDesk",          to: (c) => c.name },
  { from: "com.snipdesk.lite", to: (c) => c.identifier },
  { from: "com.snipdesk.teams", to: (c) => c.teams_identifier },
  {
    from: "https://github.com/2lukewil/snipdesk/releases/latest/download/snipdesk-update.json",
    to: (c) => c.updater_url,
  },
  {
    from: "https://github.com/2lukewil/snipdesk/releases/latest/download/snipdesk-teams-update.json",
    to: (c) => c.teams_updater_url,
  },
  // Deep-link scheme only lives in the Teams Tauri config, as a
  // bare string in a JSON array. Searching the literal quoted form
  // dodges accidental hits on "snipdesk" inside URLs / identifiers.
  { from: "\"snipdesk\"", to: (c) => c.deep_link_scheme && `"${c.deep_link_scheme}"` },
  // Settings::default seeds. The `from` patterns match the exact
  // lines in settings.rs's Default impl; the replacement keeps the
  // surrounding context so future readers still see a stock-style
  // initializer (just with a non-empty / true literal).
  {
    from: "server_url: String::new(),",
    to: (c) => c.server_url && `server_url: String::from(${rustStringLiteral(c.server_url)}),`,
  },
  {
    from: "prefer_sso_signin: false,",
    to: (c) => (c.sso_only ? "prefer_sso_signin: true," : null),
  },
];

// Escape a string for embedding as a Rust "..." literal. JSON
// already protects against most pitfalls but URLs can carry the
// occasional backslash; quotes shouldn't appear in URLs but the
// guard is cheap and covers any future caller.
function rustStringLiteral(s) {
  return `"${s.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

function loadConfig() {
  const path = process.env.BRAND_CONFIG;
  if (!path) return null;
  let raw;
  try {
    raw = readFileSync(path, "utf8");
  } catch (e) {
    console.error(`[brand] couldn't read $BRAND_CONFIG (${path}): ${e.message}`);
    process.exit(1);
  }
  let cfg;
  try {
    cfg = JSON.parse(raw);
  } catch (e) {
    console.error(`[brand] $BRAND_CONFIG isn't valid JSON: ${e.message}`);
    process.exit(1);
  }
  for (const key of ["name", "identifier", "teams_identifier"]) {
    if (typeof cfg[key] !== "string" || !cfg[key]) {
      console.error(`[brand] config missing required string field: ${key}`);
      process.exit(1);
    }
  }
  return cfg;
}

function ensureClean() {
  // `git diff --quiet -- <paths>` exits 1 when any of the listed
  // paths has uncommitted changes. We refuse to substitute over
  // uncommitted work because a mid-build crash would conflate the
  // user's edits with the substitution and make restore unsafe.
  try {
    execSync(`git diff --quiet -- ${TARGETS.join(" ")}`, {
      cwd: repoRoot,
      stdio: "ignore",
    });
  } catch {
    console.error(
      "[brand] target files have uncommitted changes; commit or stash before building with $BRAND_CONFIG set:",
    );
    for (const t of TARGETS) console.error(`  ${t}`);
    process.exit(1);
  }
}

function applySubs(cfg) {
  const backups = new Map();
  const effective = RULES
    .map((r) => ({ from: r.from, to: r.to(cfg) }))
    .filter((r) => typeof r.to === "string" && r.to.length > 0);
  for (const rel of TARGETS) {
    const abs = join(repoRoot, rel);
    const original = readFileSync(abs, "utf8");
    backups.set(abs, original);
    let next = original;
    for (const { from, to } of effective) {
      next = next.split(from).join(to);
    }
    if (next !== original) writeFileSync(abs, next);
  }
  return backups;
}

function restoreFiles(backups) {
  for (const [abs, original] of backups) {
    try {
      writeFileSync(abs, original);
    } catch (e) {
      console.error(`[brand] failed to restore ${abs}: ${e.message}`);
    }
  }
}

// Wraps a build invocation. Pass a callback that runs the actual
// build (tauri / vite / etc.); this fn handles config load,
// substitution, signal traps, and the unconditional restore.
export async function withBrand(callback) {
  const cfg = loadConfig();
  if (!cfg) return await callback();
  ensureClean();
  console.log(`[brand] applying overrides for "${cfg.name}"`);
  const backups = applySubs(cfg);
  const restore = () => restoreFiles(backups);
  const onSignal = (sig, code) => {
    restore();
    process.exit(code);
  };
  process.once("SIGINT", () => onSignal("SIGINT", 130));
  process.once("SIGTERM", () => onSignal("SIGTERM", 143));
  try {
    return await callback();
  } finally {
    restore();
    console.log("[brand] restored original source files");
  }
}
