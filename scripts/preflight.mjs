// Pre-build sanity checks. Designed to be cheap (sub-second
// when everything's clean) and called from every build / dev
// wrapper before the slow part starts.
//
// Today: just `cargo fmt`. CI runs `cargo fmt --all --check` and
// the build fails if anything's misformatted - this preflight
// catches it locally first, auto-fixes, and continues. The fix
// shows up in `git status` so you remember to `git add` before
// the next commit.

import { spawnSync } from "node:child_process";

export function runPreflight() {
  // --check exits non-zero when anything would be reformatted but
  // doesn't write to disk. Cheap probe before the actual fixup.
  const probe = spawnSync("cargo", ["fmt", "--all", "--check"], {
    stdio: "ignore",
    shell: true,
  });
  if (probe.status === 0) return;
  if (probe.status === null) {
    // Couldn't even spawn cargo (PATH issue, missing toolchain).
    // Surface the noise but don't block the build - the developer
    // will hit the same error a few seconds later when the real
    // tauri build runs cargo for real.
    console.warn("[preflight] couldn't run `cargo fmt` (cargo not on PATH?). Skipping.");
    return;
  }
  console.warn("[preflight] Rust formatting drifted. Auto-fixing with `cargo fmt --all`...");
  const fix = spawnSync("cargo", ["fmt", "--all"], {
    stdio: "inherit",
    shell: true,
  });
  if (fix.status !== 0) {
    console.error(
      "[preflight] cargo fmt failed to auto-fix; run it manually and commit before retrying.",
    );
    process.exit(fix.status ?? 1);
  }
  console.warn(
    "[preflight] cargo fmt finished. Review with `git diff` and `git add` before your next commit so CI sees the fix.",
  );
}
