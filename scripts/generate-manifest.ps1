<#
.SYNOPSIS
  Generates the two latest.json update manifests after a signed release build.

.DESCRIPTION
  Reads the .sig files produced by `tauri signer sign --bundle ...` for the
  offline and Teams NSIS installers, splices their contents into a JSON
  template, and emits two manifest files at the repo root:

      snipdesk-update.json
      snipdesk-teams-update.json

  Attach all six files (two installers, two .sig files, two manifest .jsons)
  to a GitHub release tagged v<version>. Connected SnipDesk clients pick up
  the new release on next launch.

.PARAMETER Version
  The release version string, matching workspace Cargo.toml's [workspace.package].version.
  e.g. "1.1.0" - without the leading "v".

.PARAMETER Notes
  Release notes shown to users in the update toast. Keep it short.

.PARAMETER RepoUrl
  Defaults to 2lukewil/snipdesk; override only if the repo path differs.

.EXAMPLE
  PS> .\scripts\generate-manifest.ps1 -Version 1.1.0 -Notes "Fix paste mojibake on em dashes"
#>

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [string]$Version,
  [Parameter(Mandatory = $true)] [string]$Notes,
  [string]$RepoUrl = "https://github.com/2lukewil/snipdesk"
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

# Tauri's NSIS bundles land here. Both installers are normalized to stable,
# version-less names post-build: the Lite installer by release.yml, the Teams
# installer by build-teams.mjs. (Locally, run the offline rename yourself or
# just build Teams; this script expects the canonical names below.)
$nsisDir = "target\release\bundle\nsis"

$offlineExe = Join-Path $nsisDir "SnipDesk-Lite-setup.exe"
$teamsExe   = Join-Path $nsisDir "SnipDesk-Teams-setup.exe"

if (-not (Test-Path $offlineExe)) {
  throw "Couldn't find $offlineExe. Did the offline build + rename run?"
}
if (-not (Test-Path $teamsExe)) {
  throw "Couldn't find $teamsExe. Did you run npm run tauri:build:teams?"
}

$offlineSig = "$offlineExe.sig"
$teamsSig   = "$teamsExe.sig"

if (-not (Test-Path $offlineSig)) {
  throw "Missing $offlineSig - build with TAURI_SIGNING_PRIVATE_KEY set so the .sig is emitted."
}
if (-not (Test-Path $teamsSig)) {
  throw "Missing $teamsSig - build with TAURI_SIGNING_PRIVATE_KEY set so the .sig is emitted."
}

# .sig files are single-line base64. Strip whitespace defensively.
$offlineSigContent = (Get-Content $offlineSig -Raw).Trim()
$teamsSigContent   = (Get-Content $teamsSig -Raw).Trim()

$pubDate = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")

# Both URLs use GitHub Releases' "/latest/download/" pattern: GitHub auto-
# redirects to whatever release is marked latest, so we never bake a
# version-specific URL into the running binary's auto-updater config.
$offlineManifest = [ordered]@{
  version  = $Version
  notes    = $Notes
  pub_date = $pubDate
  platforms = @{
    "windows-x86_64" = @{
      signature = $offlineSigContent
      url       = "$RepoUrl/releases/download/v$Version/SnipDesk-Lite-setup.exe"
    }
  }
}

$teamsManifest = [ordered]@{
  version  = $Version
  notes    = $Notes
  pub_date = $pubDate
  platforms = @{
    "windows-x86_64" = @{
      signature = $teamsSigContent
      url       = "$RepoUrl/releases/download/v$Version/SnipDesk-Teams-setup.exe"
    }
  }
}

$offlineOut = Join-Path $repoRoot "snipdesk-update.json"
$teamsOut   = Join-Path $repoRoot "snipdesk-teams-update.json"

$offlineManifest | ConvertTo-Json -Depth 6 | Set-Content -Path $offlineOut -Encoding UTF8
$teamsManifest   | ConvertTo-Json -Depth 6 | Set-Content -Path $teamsOut   -Encoding UTF8

Write-Host ""
Write-Host "Generated:"
Write-Host "  $offlineOut"
Write-Host "  $teamsOut"
Write-Host ""
Write-Host "Next: create a GitHub release tagged v$Version and attach these six files:"
Write-Host "  $offlineExe"
Write-Host "  $offlineSig"
Write-Host "  $offlineOut"
Write-Host "  $teamsExe"
Write-Host "  $teamsSig"
Write-Host "  $teamsOut"
Write-Host ""
Write-Host "Or use gh:"
Write-Host "  gh release create v$Version -t `"SnipDesk $Version`" -n `"$Notes`" ``"
Write-Host "    `"$offlineExe`" `"$offlineSig`" `"$offlineOut`" ``"
Write-Host "    `"$teamsExe`" `"$teamsSig`" `"$teamsOut`""
