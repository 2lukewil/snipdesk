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
  e.g. "1.1.0" — without the leading "v".

.PARAMETER Notes
  Release notes shown to users in the update toast. Keep it short.

.PARAMETER RepoUrl
  Defaults to lukew/snipdesk; override only if the repo path differs.

.EXAMPLE
  PS> .\scripts\generate-manifest.ps1 -Version 1.1.0 -Notes "Fix paste mojibake on em dashes"
#>

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [string]$Version,
  [Parameter(Mandatory = $true)] [string]$Notes,
  [string]$RepoUrl = "https://github.com/lukew/snipdesk"
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

# Tauri's NSIS bundles land here. Filenames are derived from productName +
# version, except the Teams build's post-rename step normalizes the Teams
# filenames. Offline retains Tauri's full template.
$nsisDir = "src-tauri\target\release\bundle\nsis"

$offlineExe = Get-ChildItem -Path $nsisDir -Filter "SnipDesk_*-setup.exe" |
  Sort-Object LastWriteTime -Descending | Select-Object -First 1
$teamsExe   = Join-Path $nsisDir "snipdesk-teams-setup.exe"

if (-not $offlineExe -or -not (Test-Path $offlineExe.FullName)) {
  throw "Couldn't find offline NSIS installer in $nsisDir. Did the build complete?"
}
if (-not (Test-Path $teamsExe)) {
  throw "Couldn't find $teamsExe. Did you run npm run tauri:build:teams?"
}

$offlineSig = "$($offlineExe.FullName).sig"
$teamsSig   = "$teamsExe.sig"

if (-not (Test-Path $offlineSig)) {
  throw "Missing $offlineSig — sign with: npx @tauri-apps/cli signer sign --bundle `"$($offlineExe.FullName)`""
}
if (-not (Test-Path $teamsSig)) {
  throw "Missing $teamsSig — sign with: npx @tauri-apps/cli signer sign --bundle `"$teamsExe`""
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
      url       = "$RepoUrl/releases/download/v$Version/$($offlineExe.Name)"
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
      url       = "$RepoUrl/releases/download/v$Version/snipdesk-teams-setup.exe"
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
Write-Host "  $($offlineExe.FullName)"
Write-Host "  $offlineSig"
Write-Host "  $offlineOut"
Write-Host "  $teamsExe"
Write-Host "  $teamsSig"
Write-Host "  $teamsOut"
Write-Host ""
Write-Host "Or use gh:"
Write-Host "  gh release create v$Version -t `"SnipDesk $Version`" -n `"$Notes`" ``"
Write-Host "    `"$($offlineExe.FullName)`" `"$offlineSig`" `"$offlineOut`" ``"
Write-Host "    `"$teamsExe`" `"$teamsSig`" `"$teamsOut`""
