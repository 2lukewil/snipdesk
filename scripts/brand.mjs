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
// Brand bundle layout (referenced by $BRAND_CONFIG pointing at the
// brand.json inside it):
//
//   <bundle>/
//     brand.json
//     installer-assets/        # optional; only needed for branded NSIS chrome
//       <whatever>.bmp / .ico / .rtf
//
// brand.json schema:
//   {
//     "name":              "<display name>",        // required
//     "identifier":        "<lite bundle id>",      // required
//     "teams_identifier":  "<teams bundle id>",     // required
//     "slug":              "<filename-safe slug>",  // optional
//     "updater_url":       "<lite updater feed>",   // optional
//     "teams_updater_url": "<teams updater feed>",  // optional
//     "deep_link_scheme":  "<custom URL scheme>",   // optional
//     "server_url":        "<default server URL>",  // optional
//     "sso_only":          true | false,            // optional
//     "installer": {                                // optional NSIS cosmetics
//       "header_image":   "<filename>",             //   150x57 bmp in installer-assets/
//       "sidebar_image":  "<filename>",             //   164x314 bmp in installer-assets/
//       "installer_icon": "<filename>",             //   .ico for the installer chrome
//       "license_file":   "<filename>"              //   .rtf shown on the License page
//     }
//   }
//
// server_url + sso_only seed defaults in Settings::default(); end
// users can still flip both from the Team Library settings panel.
// installer.* values are bare filenames inside the bundle's
// installer-assets/ folder; this script copies them into
// src-tauri/installer-assets/ for the build and removes them again
// on restore. The src-tauri target dir is gitignored so a customer
// build never dirties the tracked tree.

import {
  readFileSync,
  writeFileSync,
  existsSync,
  mkdirSync,
  copyFileSync,
  unlinkSync,
  rmdirSync,
  readdirSync,
} from "node:fs";
import { execSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const INSTALLER_ASSETS_DIR = join(repoRoot, "src-tauri", "installer-assets");

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
  // Union of every file that substitution OR JSON patching may
  // touch. `git diff --quiet -- <paths>` exits 1 when any has
  // uncommitted changes. We refuse to substitute over uncommitted
  // work because a mid-build crash would conflate the user's edits
  // with the substitution and make restore unsafe.
  const watched = Array.from(
    new Set([...TARGETS, ...JSON_PATCHES.map((p) => p.target)]),
  );
  try {
    execSync(`git diff --quiet -- ${watched.join(" ")}`, {
      cwd: repoRoot,
      stdio: "ignore",
    });
  } catch {
    console.error(
      "[brand] target files have uncommitted changes; commit or stash before building with $BRAND_CONFIG set:",
    );
    for (const t of watched) console.error(`  ${t}`);
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

// JSON-shaped overrides, applied after the text substitutions.
// Each entry sets one key inside a tracked JSON file, creating
// missing intermediate objects as needed. Rules with a value of
// null / undefined are skipped, so a customer config without an
// `installer` block leaves the tracked tauri.conf.json untouched.
// Map a bare filename from brand.json's installer block to the
// in-tree path Tauri will read at build time. Returns undefined
// when the field is absent so the patch loop skips it cleanly.
function nsisAssetPath(filename) {
  if (typeof filename !== "string" || !filename) return undefined;
  return `installer-assets/${filename}`;
}

const JSON_PATCHES = [
  {
    target: "src-tauri/tauri.conf.json",
    path: ["bundle", "windows", "nsis", "headerImage"],
    value: (c) => c.installer && nsisAssetPath(c.installer.header_image),
  },
  {
    target: "src-tauri/tauri.conf.json",
    path: ["bundle", "windows", "nsis", "sidebarImage"],
    value: (c) => c.installer && nsisAssetPath(c.installer.sidebar_image),
  },
  {
    target: "src-tauri/tauri.conf.json",
    path: ["bundle", "windows", "nsis", "installerIcon"],
    value: (c) => c.installer && nsisAssetPath(c.installer.installer_icon),
  },
  {
    target: "src-tauri/tauri.conf.json",
    path: ["bundle", "windows", "nsis", "license"],
    value: (c) => c.installer && nsisAssetPath(c.installer.license_file),
  },
];

function setJsonPath(obj, path, value) {
  let cur = obj;
  for (let i = 0; i < path.length - 1; i++) {
    if (typeof cur[path[i]] !== "object" || cur[path[i]] === null) {
      cur[path[i]] = {};
    }
    cur = cur[path[i]];
  }
  cur[path[path.length - 1]] = value;
}

function applyJsonPatches(cfg, backups) {
  // Group patches by target so each file is parsed + written once.
  const byTarget = new Map();
  for (const patch of JSON_PATCHES) {
    const value = patch.value(cfg);
    if (typeof value !== "string" || value.length === 0) continue;
    if (!byTarget.has(patch.target)) byTarget.set(patch.target, []);
    byTarget.get(patch.target).push({ path: patch.path, value });
  }
  for (const [rel, patches] of byTarget) {
    const abs = join(repoRoot, rel);
    if (!backups.has(abs)) {
      // File wasn't in TARGETS; capture its pristine bytes now so
      // restore reverts cleanly even though we never text-subbed.
      backups.set(abs, readFileSync(abs, "utf8"));
    }
    const current = readFileSync(abs, "utf8");
    const parsed = JSON.parse(current);
    for (const { path, value } of patches) setJsonPath(parsed, path, value);
    writeFileSync(abs, JSON.stringify(parsed, null, 2) + "\n");
  }
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

// Copy installer assets out of the brand bundle into the tracked
// src-tauri/installer-assets/ directory so Tauri's NSIS bundler
// can read them at the relative paths the JSON patches set.
// Returns a cleanup descriptor (or null) the restore step uses to
// remove exactly what we created - so a build never leaves stray
// customer files behind in the worktree.
function stageInstallerAssets(cfg, brandConfigPath) {
  if (!cfg.installer || !brandConfigPath) return null;
  const bundleDir = dirname(resolve(brandConfigPath));
  const sourceDir = join(bundleDir, "installer-assets");
  if (!existsSync(sourceDir)) {
    console.warn(
      `[brand] installer block set but ${sourceDir} doesn't exist; ` +
        "skipping asset copy. Tauri's NSIS build will fail when it " +
        "tries to read the patched paths.",
    );
    return null;
  }
  const targetExisted = existsSync(INSTALLER_ASSETS_DIR);
  if (!targetExisted) mkdirSync(INSTALLER_ASSETS_DIR, { recursive: true });
  const copied = [];
  const fields = ["header_image", "sidebar_image", "installer_icon", "license_file"];
  for (const field of fields) {
    const filename = cfg.installer[field];
    if (typeof filename !== "string" || !filename) continue;
    const src = join(sourceDir, filename);
    if (!existsSync(src)) {
      console.warn(`[brand] installer asset missing in bundle: ${src}`);
      continue;
    }
    const dst = join(INSTALLER_ASSETS_DIR, filename);
    copyFileSync(src, dst);
    copied.push(dst);
    console.log(`[brand] staged installer asset: ${filename}`);
  }
  return { copied, createdDir: !targetExisted };
}

function cleanupInstallerAssets(state) {
  if (!state) return;
  for (const path of state.copied) {
    try {
      unlinkSync(path);
    } catch (e) {
      console.warn(`[brand] failed to remove staged asset ${path}: ${e.message}`);
    }
  }
  // Only remove the directory if we created it AND it's now empty.
  // Leaves a manually-populated dir alone for the unusual case
  // where someone is iterating outside the bundle convention.
  if (state.createdDir && existsSync(INSTALLER_ASSETS_DIR)) {
    try {
      const left = readdirSync(INSTALLER_ASSETS_DIR);
      if (left.length === 0) rmdirSync(INSTALLER_ASSETS_DIR);
    } catch (e) {
      console.warn(`[brand] failed to clean installer-assets dir: ${e.message}`);
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
  applyJsonPatches(cfg, backups);
  const assets = stageInstallerAssets(cfg, process.env.BRAND_CONFIG);
  const restore = () => {
    restoreFiles(backups);
    cleanupInstallerAssets(assets);
  };
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
