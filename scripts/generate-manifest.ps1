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
  [string]$RepoUrl = "https://github.com/2lukewil/snipdesk",
  # Prefix of the renamed installer files in target/release/bundle/nsis/.
  # Defaults to "SnipDesk" so the vanilla flow finds SnipDesk-Lite-setup.exe
  # / SnipDesk-Teams-setup.exe. Whitelabel builds pass their own
  # PascalCase brand prefix (e.g. "Acme") to match the rename done in
  # release.yml / build-teams.mjs.
  [string]$InstallerPrefix = "SnipDesk",
  # Prefix of the emitted manifest JSON files at the repo root. Defaults
  # to "snipdesk" so the vanilla flow writes snipdesk-update.json /
  # snipdesk-teams-update.json. Whitelabel builds pass their own
  # kebab-case prefix (e.g. "snipdesk-acme") so each customer gets a
  # distinct manifest URL in the same GitHub release.
  [string]$ManifestPrefix = "snipdesk",
  # Skip the Lite manifest entirely. Set for whitelabel builds where
  # we deliberately only ship the Teams installer (whitelabel = Teams
  # only; building Lite for every customer wastes minutes for a flavour
  # nobody ships). Vanilla call sites leave this off so both manifests
  # still get produced as before.
  [switch]$TeamsOnly
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

# Tauri's NSIS bundles land here. Both installers are normalized to stable,
# version-less names post-build: the Lite installer by release.yml, the Teams
# installer by build-teams.mjs. (Locally, run the offline rename yourself or
# just build Teams; this script expects the canonical names below.)
$nsisDir = "target\release\bundle\nsis"

$teamsExe = Join-Path $nsisDir "$InstallerPrefix-Teams-setup.exe"
if (-not (Test-Path $teamsExe)) {
  throw "Couldn't find $teamsExe. Did you run npm run tauri:build:teams?"
}
$teamsSig = "$teamsExe.sig"
if (-not (Test-Path $teamsSig)) {
  throw "Missing $teamsSig - build with TAURI_SIGNING_PRIVATE_KEY set so the .sig is emitted."
}
$teamsSigContent = (Get-Content $teamsSig -Raw).Trim()

if (-not $TeamsOnly) {
  $offlineExe = Join-Path $nsisDir "$InstallerPrefix-Lite-setup.exe"
  if (-not (Test-Path $offlineExe)) {
    throw "Couldn't find $offlineExe. Did the offline build + rename run?"
  }
  $offlineSig = "$offlineExe.sig"
  if (-not (Test-Path $offlineSig)) {
    throw "Missing $offlineSig - build with TAURI_SIGNING_PRIVATE_KEY set so the .sig is emitted."
  }
  $offlineSigContent = (Get-Content $offlineSig -Raw).Trim()
}

$pubDate = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")

# Both URLs use GitHub Releases' "/latest/download/" pattern: GitHub auto-
# redirects to whatever release is marked latest, so we never bake a
# version-specific URL into the running binary's auto-updater config.
$teamsManifest = [ordered]@{
  version  = $Version
  notes    = $Notes
  pub_date = $pubDate
  platforms = @{
    "windows-x86_64" = @{
      signature = $teamsSigContent
      url       = "$RepoUrl/releases/download/v$Version/$InstallerPrefix-Teams-setup.exe"
    }
  }
}
$teamsOut = Join-Path $repoRoot "$ManifestPrefix-teams-update.json"
$teamsManifest | ConvertTo-Json -Depth 6 | Set-Content -Path $teamsOut -Encoding UTF8

if (-not $TeamsOnly) {
  $offlineManifest = [ordered]@{
    version  = $Version
    notes    = $Notes
    pub_date = $pubDate
    platforms = @{
      "windows-x86_64" = @{
        signature = $offlineSigContent
        url       = "$RepoUrl/releases/download/v$Version/$InstallerPrefix-Lite-setup.exe"
      }
    }
  }
  $offlineOut = Join-Path $repoRoot "$ManifestPrefix-update.json"
  $offlineManifest | ConvertTo-Json -Depth 6 | Set-Content -Path $offlineOut -Encoding UTF8
}

Write-Host ""
Write-Host "Generated:"
if (-not $TeamsOnly) { Write-Host "  $offlineOut" }
Write-Host "  $teamsOut"
