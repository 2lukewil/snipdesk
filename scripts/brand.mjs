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
//     icon.png                 # optional; single master PNG (1024x1024 ideal)
//     installer-assets/        # optional; only needed for branded NSIS chrome
//       <whatever>.bmp / .ico / .rtf
//
// brand.json schema:
//   {
//     "name":              "<display name>",        // required
//     "identifier":        "<lite bundle id>",      // required
//     "teams_identifier":  "<teams bundle id>",     // required
//     "slug":              "<filename-safe slug>",  // optional
//     "icon_source":       "<filename>",            // optional master PNG; "icon.png" is conventional
//     "updater_url":       "<lite updater feed>",   // optional
//     "teams_updater_url": "<teams updater feed>",  // optional
//     "deep_link_scheme":  "<custom URL scheme>",   // optional
//     "server_url":        "<default server URL>",  // optional
//     "sso_only":          true | false,            // optional
//     "installer": {                                // optional NSIS cosmetics
//       "header_image":   "<filename>",             //   24-bit BMP, 150x57, in installer-assets/
//       "sidebar_image":  "<filename>",             //   24-bit BMP, 164x314, in installer-assets/
//       "installer_icon": "<filename>",             //   .ico for the installer chrome
//       "license_file":   "<filename>"              //   .rtf shown on the License page
//     }
//   }
//
// server_url + sso_only seed defaults in Settings::default(); end
// users can still flip both from the Team Library settings panel.
//
// installer.* values are bare filenames inside the bundle's
// installer-assets/ folder; this script copies them into
// src-tauri/installer-assets/ for the build and removes them again
// on restore. That target dir is gitignored so a customer build
// never dirties the tracked tree.
//
// Per-asset fallback: each installer.* field is independent. If a
// customer's brand.json declares a field whose file isn't actually
// in their bundle, the override is skipped (with a warning) and
// the value already in tauri.conf.json wins for that field. So a
// project that wires its own defaults under
// src-tauri/installer-defaults/<file> via tauri.conf.json's
// bundle.windows.nsis section gets those defaults whenever a
// whitelabel leaves out (or fails to provide) the matching asset.
//
// icon_source is a single PNG (ideally 1024x1024 with transparency)
// that `tauri icon` expands into the full platform-specific app
// icon set inside src-tauri/icons/. Before the expansion this
// script snapshots the tracked icon files in memory; restore puts
// the originals back so the worktree ends each build identical to
// how it started.

import {
  readFileSync,
  writeFileSync,
  existsSync,
  mkdirSync,
  copyFileSync,
  unlinkSync,
  rmdirSync,
  readdirSync,
  statSync,
} from "node:fs";
import { execSync, spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const INSTALLER_ASSETS_DIR = join(repoRoot, "src-tauri", "installer-assets");
const APP_ICONS_DIR = join(repoRoot, "src-tauri", "icons");

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

// Parse `--whitelabel=<ref>` / `--wl=<ref>` (and the
// space-separated forms) out of the argv tail that npm forwards
// after `--`. Returns the remaining args + the resolved absolute
// path, so the wrapper can:
//
//   1. set $BRAND_CONFIG to the resolved path
//   2. forward the leftover args to tauri (filter out the flag)
//
// `<ref>` can be either:
//   - a bare slug like "acme" -> resolves to brands/acme/brand.json
//   - a path containing a slash or ending in .json -> used as-is
//
// Missing bundle = hard exit with a list of available slugs so a
// typo doesn't silently fall back to a vanilla build.
export function parseBrandFlag(args) {
  const remaining = [];
  let resolved = null;
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    let value = null;
    if (a.startsWith("--whitelabel=") || a.startsWith("--wl=") || a.startsWith("--brand=")) {
      value = a.split("=").slice(1).join("=");
    } else if (a === "--whitelabel" || a === "--wl" || a === "--brand") {
      value = args[i + 1];
      i++;
    } else {
      remaining.push(a);
      continue;
    }
    if (!value) {
      console.error("[brand] --whitelabel needs a value (slug or path to brand.json)");
      process.exit(1);
    }
    resolved = resolveBrandRef(value);
  }
  return { brandConfigPath: resolved, remainingArgs: remaining };
}

function resolveBrandRef(ref) {
  const looksLikePath =
    ref.includes("/") || ref.includes("\\") || ref.toLowerCase().endsWith(".json");
  if (looksLikePath) {
    if (!existsSync(ref)) {
      console.error(`[brand] --whitelabel path doesn't exist: ${ref}`);
      process.exit(1);
    }
    return resolve(ref);
  }
  const bundlePath = join(repoRoot, "brands", ref, "brand.json");
  if (existsSync(bundlePath)) return bundlePath;
  console.error(`[brand] no bundle at brands/${ref}/brand.json`);
  const brandsDir = join(repoRoot, "brands");
  if (existsSync(brandsDir)) {
    let entries = [];
    try {
      entries = readdirSync(brandsDir).filter((name) => {
        if (name === "_template") return false;
        try {
          return statSync(join(brandsDir, name)).isDirectory();
        } catch {
          return false;
        }
      });
    } catch {}
    if (entries.length > 0) {
      console.error(`  available bundles under brands/: ${entries.join(", ")}`);
    } else {
      console.error("  no bundles found under brands/ (only the _template stub)");
    }
  }
  process.exit(1);
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

function ensureClean(cfg) {
  // Union of every file that substitution OR JSON patching OR
  // icon regen may touch. `git diff --quiet -- <paths>` exits 1
  // when any has uncommitted changes. We refuse to substitute
  // over uncommitted work because a mid-build crash would
  // conflate the user's edits with the substitution and make
  // restore unsafe.
  const watched = Array.from(
    new Set([...TARGETS, ...JSON_PATCHES.map((p) => p.target)]),
  );
  if (cfg.icon_source) watched.push("src-tauri/icons");
  try {
    execSync(`git diff --quiet -- ${watched.join(" ")}`, {
      cwd: repoRoot,
      stdio: "ignore",
    });
  } catch {
    console.error(
      "[brand] target paths have uncommitted changes; commit or stash before building with $BRAND_CONFIG set:",
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
// Installer-asset patches are handled by applyWhitelabelInstaller
// (further down) because each path must be coupled to the matching
// file copy + existence check. JSON_PATCHES stays for any future
// purely-textual JSON overrides.
const JSON_PATCHES = [];

// Maps brand.json's installer.* field names to the JSON path in
// tauri.conf.json that gets patched. applyWhitelabelInstaller
// walks this list and, for each entry whose source file is
// present in the bundle, stages the file and rewrites the JSON
// path. Missing files skip the override, so the stock default
// (whatever tauri.conf.json already points at) wins per field.
//
// Header / sidebar / icon live under bundle.windows.nsis; the
// license file is a top-level bundle.licenseFile (Tauri's NSIS
// bundler picks it up from there automatically and it doubles as
// the license for any other installer formats we might add).
const INSTALLER_FIELD_MAP = [
  { field: "header_image",   jsonPath: ["bundle", "windows", "nsis", "headerImage"] },
  { field: "sidebar_image",  jsonPath: ["bundle", "windows", "nsis", "sidebarImage"] },
  { field: "installer_icon", jsonPath: ["bundle", "windows", "nsis", "installerIcon"] },
  { field: "license_file",   jsonPath: ["bundle", "licenseFile"] },
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

// Per-asset staging + patching, paired so a missing file in the
// bundle gracefully falls back to whatever default is already in
// tauri.conf.json (or Tauri's built-in NSIS chrome when no default
// is wired). For each installer.* field the customer declares:
//   - file present in bundle/installer-assets/ -> copy it into
//     src-tauri/installer-assets/, patch the matching nsis key to
//     point at the staged copy
//   - file missing -> warn, skip both copy and patch; the stock
//     value in tauri.conf.json wins for that field
//
// Returns a state object for restore (lists what was copied + the
// tauri.conf.json backup) or null when the customer declared no
// installer block at all.
function applyWhitelabelInstaller(cfg, brandConfigPath, backups) {
  if (!cfg.installer || !brandConfigPath) return null;
  const bundleDir = dirname(resolve(brandConfigPath));
  const sourceDir = join(bundleDir, "installer-assets");
  if (!existsSync(sourceDir)) {
    console.warn(
      `[brand] installer block set but ${sourceDir} doesn't exist; ` +
        "every override will fall back to the project default.",
    );
    return null;
  }
  const tauriConfRel = "src-tauri/tauri.conf.json";
  const tauriConfAbs = join(repoRoot, tauriConfRel);
  const targetExisted = existsSync(INSTALLER_ASSETS_DIR);
  const copied = [];
  let parsedConf = null;

  for (const { field, jsonPath } of INSTALLER_FIELD_MAP) {
    const filename = cfg.installer[field];
    if (typeof filename !== "string" || !filename) continue;
    const src = join(sourceDir, filename);
    if (!existsSync(src)) {
      console.warn(
        `[brand] installer.${field} declared "${filename}" but it's not in ` +
          `the bundle; keeping the project default for ${jsonPath.join(".")}.`,
      );
      continue;
    }
    // Lazy: only create the dir / parse the config the first time
    // we actually have a file to stage.
    if (!targetExisted && copied.length === 0) {
      mkdirSync(INSTALLER_ASSETS_DIR, { recursive: true });
    }
    if (!parsedConf) {
      if (!backups.has(tauriConfAbs)) {
        backups.set(tauriConfAbs, readFileSync(tauriConfAbs, "utf8"));
      }
      parsedConf = JSON.parse(readFileSync(tauriConfAbs, "utf8"));
    }
    const dst = join(INSTALLER_ASSETS_DIR, filename);
    copyFileSync(src, dst);
    copied.push(dst);
    setJsonPath(parsedConf, jsonPath, `installer-assets/${filename}`);
    console.log(`[brand] override: ${jsonPath.join(".")} -> installer-assets/${filename}`);
  }

  if (parsedConf) {
    writeFileSync(tauriConfAbs, JSON.stringify(parsedConf, null, 2) + "\n");
  }
  return { copied, createdDir: !targetExisted && copied.length > 0 };
}

// Recursive file walker. `tauri icon` writes into subdirectories
// (android/, ios/) under src-tauri/icons/, so a flat readdir
// misses most of what the snapshot needs to cover.
function walkFiles(root) {
  const out = [];
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop();
    let entries;
    try {
      entries = readdirSync(dir);
    } catch {
      continue;
    }
    for (const name of entries) {
      const p = join(dir, name);
      try {
        const s = statSync(p);
        if (s.isDirectory()) stack.push(p);
        else if (s.isFile()) out.push(p);
      } catch {
        // Symlink race / permissions; skip silently.
      }
    }
  }
  return out;
}

// Take a byte-perfect snapshot of every file currently under
// src-tauri/icons/ (recursive). Used as the restore target after
// `tauri icon` overwrites the directory with the customer's
// generated set.
function snapshotIconsDir() {
  if (!existsSync(APP_ICONS_DIR)) return null;
  const snapshot = new Map();
  for (const p of walkFiles(APP_ICONS_DIR)) {
    try {
      snapshot.set(p, readFileSync(p));
    } catch {}
  }
  return snapshot;
}

function restoreIcons(snapshot) {
  if (!snapshot) return;
  if (!existsSync(APP_ICONS_DIR)) return;
  // Remove any file currently present that wasn't in the snapshot
  // (`tauri icon` may have created new platform shapes).
  for (const p of walkFiles(APP_ICONS_DIR)) {
    if (!snapshot.has(p)) {
      try {
        unlinkSync(p);
      } catch {}
    }
  }
  // Write each snapshotted file back, recreating parent dirs if
  // tauri icon happened to delete them (it shouldn't, but defensive).
  for (const [p, bytes] of snapshot) {
    try {
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, bytes);
    } catch (e) {
      console.warn(`[brand] failed to restore icon ${p}: ${e.message}`);
    }
  }
  // Clean up any empty subdirectory tauri icon may have left behind
  // (e.g. an android/ or ios/ folder that wasn't in the original
  // tree at all). Bottom-up so deeper dirs go first.
  removeEmptyChildDirs(APP_ICONS_DIR);
}

function removeEmptyChildDirs(root) {
  const dirs = [];
  const stack = [root];
  while (stack.length) {
    const d = stack.pop();
    dirs.push(d);
    let entries;
    try {
      entries = readdirSync(d);
    } catch {
      continue;
    }
    for (const name of entries) {
      const p = join(d, name);
      try {
        if (statSync(p).isDirectory()) stack.push(p);
      } catch {}
    }
  }
  dirs.sort((a, b) => b.length - a.length);
  for (const d of dirs) {
    if (d === root) continue;
    try {
      if (readdirSync(d).length === 0) rmdirSync(d);
    } catch {}
  }
}

// Run `tauri icon <master>` to populate src-tauri/icons/ with the
// customer's full platform-specific icon set. Snapshots the
// directory first so restore can revert; throws if tauri icon
// fails so the caller can short-circuit the rest of the build.
function regenerateAppIcons(cfg, brandConfigPath) {
  if (!cfg.icon_source || !brandConfigPath) return null;
  const bundleDir = dirname(resolve(brandConfigPath));
  const masterPath = join(bundleDir, cfg.icon_source);
  if (!existsSync(masterPath)) {
    console.warn(
      `[brand] icon_source not found at ${masterPath}; skipping app-icon regen`,
    );
    return null;
  }
  console.log(`[brand] regenerating app icons from ${masterPath}`);
  const snapshot = snapshotIconsDir();
  const r = spawnSync("npx", ["tauri", "icon", masterPath], {
    stdio: "inherit",
    shell: true,
    cwd: repoRoot,
  });
  if (r.status !== 0) {
    // Restore immediately so a partial-overwrite state never
    // leaks into the rest of the build path.
    restoreIcons(snapshot);
    throw new Error(`[brand] tauri icon failed (exit ${r.status})`);
  }
  return snapshot;
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
  ensureClean(cfg);
  console.log(`[brand] applying overrides for "${cfg.name}"`);
  const backups = applySubs(cfg);
  applyJsonPatches(cfg, backups);
  const assets = applyWhitelabelInstaller(cfg, process.env.BRAND_CONFIG, backups);
  const iconSnapshot = regenerateAppIcons(cfg, process.env.BRAND_CONFIG);
  const restore = () => {
    restoreFiles(backups);
    cleanupInstallerAssets(assets);
    restoreIcons(iconSnapshot);
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
