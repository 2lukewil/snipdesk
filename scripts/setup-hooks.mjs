// Activate the tracked .githooks/ directory for this clone.
//
// `git config core.hooksPath .githooks` tells git to look in
// .githooks/ instead of the default .git/hooks/, so any hook
// committed to the repo is picked up by every developer who
// runs `npm install` without anyone having to symlink files
// by hand.
//
// Tolerant by design: if git isn't on PATH (CI docker stages,
// fresh container image during the build, etc.) we warn and
// exit 0 so `npm install` doesn't fail. The hook will be
// activated whenever the developer next runs `npm install`
// from a git-available environment.

import { spawnSync } from "node:child_process";

const r = spawnSync("git", ["config", "core.hooksPath", ".githooks"], {
  stdio: "ignore",
});

if (r.status === 0) {
  console.log("[setup-hooks] activated .githooks/ for this clone");
} else if (r.status === null) {
  // git not on PATH - common in CI minimal containers.
  console.warn("[setup-hooks] git not available; skipping hook activation");
} else {
  console.warn(
    `[setup-hooks] git config exited ${r.status}; hooks may not fire. ` +
      "Run `git config core.hooksPath .githooks` manually to activate.",
  );
}
