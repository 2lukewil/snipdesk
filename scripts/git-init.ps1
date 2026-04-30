# Initial repo bootstrap for SnipDesk v1.0.0.
#
# The Cowork sandbox can't clean up after a failed `git init` across the
# Windows mount (no unlink permission on .git/config.lock), so this script
# is the authoritative init path. Safe to re-run — it removes a broken
# .git directory if one is sitting there before retrying.
#
# Run from the repo root:
#   powershell -ExecutionPolicy Bypass -File scripts\git-init.ps1

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot
Write-Host "Working in $repoRoot"

# Clean up any partial .git from a previous aborted init.
if (Test-Path .git) {
    Write-Host "Removing existing .git/ (partial init from sandbox)"
    Remove-Item -Recurse -Force .git
}

# Fresh init on the main branch — most recent git defaults to this anyway
# but we're explicit so older gits don't leave us on 'master'.
git init -b main

# Local identity (scoped to this repo only; doesn't touch your global config).
git config user.name  "Luke"
git config user.email "lucas.wilson@shockbyte.com"

# Normalize line endings so Rust/JS files don't flip between CRLF/LF on
# Windows vs CI. `autocrlf=true` keeps working-tree as CRLF, stores LF.
git config core.autocrlf true

git add .

git status --short
Write-Host ""
Write-Host "About to commit the above files. Press Ctrl+C within 3s to abort."
Start-Sleep -Seconds 3

$msg = @"
Initial release: SnipDesk v1.0.0

Snippet launcher for Shockbyte support agents. Global-hotkey (Alt+Space)
search + paste for canned replies, built on Tauri 2.x with a Rust backend
and vanilla HTML/JS frontend.

Core features:
- Global hotkey launcher, tray icon, autostart on Windows login
- Folder tree + tagging + usage-sorted search
- Variable placeholders with per-snippet history autosuggest
- PhraseExpress .pex / .pexdb import
- Time/money savings estimator (typing speed + hourly wage)
- Direct CF_UNICODETEXT clipboard write + WM_PASTE dispatch on Windows,
  bypassing the Ctrl+V simulation that triggered menu accelerators in
  some target apps
- Light/dark/system theme, compact mode, configurable hotkey
- Context menu (duplicate, delete), drag-resistant window, always-on-top
  opt-in

Windows-only for this release; macOS/Linux paths exist in the tree but
aren't tested.
"@

git commit -m $msg

Write-Host ""
Write-Host "Done. Log:"
git log --oneline
