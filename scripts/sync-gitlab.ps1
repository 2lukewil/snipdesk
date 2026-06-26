# Sync the GitLab deploy mirror from this repo's checked-out HEAD.
#
# Flow: the GitHub repo is the source of truth with full history;
# the GitLab repo carries a linear chain of sync commits (one per
# sync) so the deploy side never sees a force-push and the ops team
# can commit their own files (.gitlab-ci.yml, Helm charts) directly
# into it without our syncs touching them.
#
# Each run:
#   0. realigns the mirror onto the latest origin/main, so the sync
#      lands on top of whatever ops merged/committed there (.gitlab-ci.yml,
#      Helm charts) and the dev -> main merge request never conflicts,
#   1. wipes the mirror worktree (except .git and PRESERVE paths),
#   2. extracts this repo's tracked files (git archive HEAD - never
#      untracked or ignored files),
#   3. deletes the EXCLUDE list (upstream-only files the deploy repo
#      deliberately doesn't carry),
#   4. commits the result in the mirror with the upstream sha.
#
# Pushing is intentionally left to the operator:
#   git -C <mirror> push origin main

param(
    # The GitLab mirror clone. The folder name is local convention;
    # pass -Mirror to use another path.
    [string]$Mirror = "E:\snipdesk-gitlab",
    # This repo's root (the sync source).
    [string]$Source = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = "Stop"

# Upstream-only paths the deploy repo does not carry.
$Exclude = @(
    ".github",                      # GitHub Actions; GitLab CI is owned in the mirror
    "scripts\sync-gitlab.ps1",      # this script syncs TO the mirror, not into it
    "scripts\clear-icon-cache.ps1", # local dev convenience, unreferenced
    "scripts\reinstall.ps1"         # local dev convenience, unreferenced
)

# Mirror-owned paths a sync must never delete or overwrite.
$Preserve = @(
    ".git",
    ".gitlab-ci.yml",
    "ci",
    "chart",
    "helm"
)

if (-not (Test-Path (Join-Path $Mirror ".git"))) {
    throw "$Mirror is not a git repository - clone or init it first"
}

# 0. Realign the mirror onto the latest deploy main. Ops commit
#    .gitlab-ci.yml / Helm charts straight to origin/main, and each
#    dev -> main merge advances it; without resetting to it here the
#    mirror drifts and the next sync's dev branch conflicts. Everything
#    tracked is regenerated below, so discarding local-only sync commits
#    and any stray working changes (checkout -f -B) is safe.
git -C $Mirror fetch origin
git -C $Mirror checkout -f -B main origin/main

$upstreamSha = (git -C $Source rev-parse --short HEAD).Trim()
$upstreamDesc = (git -C $Source describe --tags --always HEAD).Trim()

# 1. Clear the mirror worktree, keeping mirror-owned paths.
Get-ChildItem -Force $Mirror | Where-Object { $Preserve -notcontains $_.Name } | ForEach-Object {
    Remove-Item -Recurse -Force $_.FullName
}

# 2. Extract tracked files from the source HEAD.
$archive = Join-Path $env:TEMP "snipdesk-sync.tar"
git -C $Source archive --format=tar -o $archive HEAD
tar -xf $archive -C $Mirror
Remove-Item $archive

# 3. Drop the upstream-only paths.
foreach ($path in $Exclude) {
    $full = Join-Path $Mirror $path
    if (Test-Path $full) {
        Remove-Item -Recurse -Force $full
    }
}

# 4. Commit in the mirror. --allow-empty-message avoided; no-op
#    syncs (nothing changed) are skipped instead of committing.
git -C $Mirror add -A
$pending = (git -C $Mirror status --porcelain)
if (-not $pending) {
    Write-Output "mirror already up to date with $upstreamDesc ($upstreamSha)"
    exit 0
}
git -C $Mirror commit -m "Sync from snipdesk $upstreamDesc ($upstreamSha)"
Write-Output "mirror synced to $upstreamDesc ($upstreamSha) - review with: git -C $Mirror show --stat"
Write-Output "push with: git -C $Mirror push origin main"
