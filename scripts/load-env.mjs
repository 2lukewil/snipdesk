// Light .env loader for local builds. Reads .env from the repo root if
// present, sets process.env, then optionally resolves TAURI_SIGNING_PRIVATE_KEY
// from a file path. CI already has the env vars set by GitHub Actions; this
// loader never overwrites pre-existing values, so CI is unaffected.
//
// Why not just `node --env-file=.env`? Multi-line values (the private key
// content) need quoting that Node's built-in parser handles inconsistently
// across versions, and we want the option to specify a file path instead.
// Forty lines of bespoke parsing buys us a much cleaner DX.

import { existsSync, readFileSync } from "node:fs";
import { dirname, isAbsolute, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..");
const envPath = resolve(repoRoot, ".env");

// KEY=VALUE per line. Values may be wrapped in double quotes to preserve
// surrounding whitespace and embedded newlines. `#` starts a comment when
// it's the first non-whitespace character on a line, but not inside a
// quoted value. Unrecognized lines are skipped silently.
function parseEnv(text) {
  const out = {};
  const lines = text.split(/\r?\n/);
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      i++;
      continue;
    }
    const eq = line.indexOf("=");
    if (eq < 0) {
      i++;
      continue;
    }
    const key = line.slice(0, eq).trim();
    let value = line.slice(eq + 1);
    // Quoted multi-line: consume lines until the closing quote.
    if (value.startsWith('"') && !value.endsWith('"')) {
      const parts = [value.slice(1)];
      i++;
      while (i < lines.length) {
        const l = lines[i];
        if (l.endsWith('"')) {
          parts.push(l.slice(0, -1));
          break;
        }
        parts.push(l);
        i++;
      }
      value = parts.join("\n");
    } else if (value.startsWith('"') && value.endsWith('"')) {
      value = value.slice(1, -1);
    } else {
      // Strip inline trailing comments only for unquoted values.
      const hash = value.indexOf(" #");
      if (hash >= 0) value = value.slice(0, hash);
      value = value.trim();
    }
    out[key] = value;
    i++;
  }
  return out;
}

export function loadEnv() {
  if (!existsSync(envPath)) return { loaded: false };
  let parsed;
  try {
    parsed = parseEnv(readFileSync(envPath, "utf8"));
  } catch (err) {
    console.error(`[env] couldn't read ${envPath}: ${err.message}`);
    return { loaded: false };
  }
  // CI values (set before this script runs) take precedence over .env.
  let applied = 0;
  for (const [k, v] of Object.entries(parsed)) {
    if (process.env[k] === undefined) {
      process.env[k] = v;
      applied++;
    }
  }
  // If the user gave a path instead of an inline key, resolve it now so
  // Tauri's signer (which expects the literal content in the env var)
  // sees what it needs.
  if (
    !process.env.TAURI_SIGNING_PRIVATE_KEY &&
    process.env.TAURI_SIGNING_PRIVATE_KEY_PATH
  ) {
    let p = process.env.TAURI_SIGNING_PRIVATE_KEY_PATH;
    if (!isAbsolute(p)) p = resolve(repoRoot, p);
    try {
      process.env.TAURI_SIGNING_PRIVATE_KEY = readFileSync(p, "utf8");
    } catch (err) {
      console.error(
        `[env] TAURI_SIGNING_PRIVATE_KEY_PATH points at ${p} but couldn't read it: ${err.message}`,
      );
    }
  }
  return { loaded: true, applied };
}
