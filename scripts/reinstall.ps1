<#
.SYNOPSIS
  End-to-end reinstall of SnipDesk that reliably refreshes the taskbar icon.

.DESCRIPTION
  If you just double-click the new MSI, Windows often does an in-place upgrade
  that keeps the OLD cached icon. This script runs the full clean path:

    1. Kill any running snipdesk.exe
    2. Pre-install icon-cache scan - detect every place Windows has stashed
       the old SnipDesk icon and report findings (before we touch anything)
    3. Uninstall the existing SnipDesk MSI (by ProductName lookup)
    4. Wipe all detected icon caches + remove stale pinned shortcuts
    5. Install the new MSI from target\release\bundle\msi\
    6. Post-install cache pass (install writes fresh shortcuts)
    7. Restart Explorer

  Run AFTER `build-windows.ps1` has produced the installer.

.EXAMPLE
  PS C:\repos\snipdesk> .\scripts\reinstall.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = "Continue"
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

Write-Host "=== SnipDesk clean reinstall ===" -ForegroundColor Cyan

# --- 1. Kill running instance -------------------------------------------
Write-Host "[1/7] Closing any running SnipDesk..." -ForegroundColor DarkGray
Get-Process -Name "snipdesk" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

# --- 2. Pre-install icon scan -------------------------------------------
# BEFORE we uninstall/install anything, enumerate every cache location we
# know about and report what's there. This catches cases where a previous
# install left orphaned icon copies that the MSI uninstaller won't touch.
Write-Host "[2/7] Scanning for cached SnipDesk icons..." -ForegroundColor DarkGray
$regPaths = @(
  "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*",
  "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*",
  "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*"
)
$existing = foreach ($p in $regPaths) {
  Get-ItemProperty -Path $p -ErrorAction SilentlyContinue |
    Where-Object { $_.DisplayName -like "SnipDesk*" }
}
$installerGuids = @()
$cachedIconPaths = @()
foreach ($item in $existing) {
  if ($item.PSChildName -match '^{[0-9A-Fa-f-]+}$') {
    $installerGuids += $item.PSChildName
    $installerDir = Join-Path $env:WINDIR "Installer\$($item.PSChildName)"
    if (Test-Path $installerDir) {
      $cachedIconPaths += Get-ChildItem -Path $installerDir -Filter "*.ico" -ErrorAction SilentlyContinue |
                           Select-Object -ExpandProperty FullName
    }
  }
}
# Walk Start Menu + taskbar + desktop for stale .lnk files. These cache
# the icon at pin time and will NOT update on MSI upgrade unless deleted.
$shortcutDirs = @(
  "$env:APPDATA\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar",
  "$env:APPDATA\Microsoft\Windows\Start Menu\Programs",
  "$env:PROGRAMDATA\Microsoft\Windows\Start Menu\Programs",
  "$env:USERPROFILE\Desktop"
)
$wsh = New-Object -ComObject WScript.Shell
$staleLnks = @()
foreach ($dir in $shortcutDirs) {
  if (-not (Test-Path $dir)) { continue }
  Get-ChildItem -Path $dir -Filter "*.lnk" -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
    try {
      $sc = $wsh.CreateShortcut($_.FullName)
      if ($sc.TargetPath -match "snipdesk") { $staleLnks += $_.FullName }
    } catch {}
  }
}
# Report
if ($installerGuids.Count -gt 0) {
  Write-Host "    found installed ProductGUID(s):" -ForegroundColor Yellow
  foreach ($g in $installerGuids) { Write-Host "      $g" -ForegroundColor Yellow }
}
if ($cachedIconPaths.Count -gt 0) {
  Write-Host "    found $($cachedIconPaths.Count) cached .ico(s) in %WINDIR%\Installer:" -ForegroundColor Yellow
  foreach ($p in $cachedIconPaths) { Write-Host "      $p" -ForegroundColor Yellow }
}
if ($staleLnks.Count -gt 0) {
  Write-Host "    found $($staleLnks.Count) stale .lnk shortcut(s):" -ForegroundColor Yellow
  foreach ($p in $staleLnks) { Write-Host "      $p" -ForegroundColor Yellow }
}
if ($installerGuids.Count -eq 0 -and $staleLnks.Count -eq 0) {
  Write-Host "    no pre-existing SnipDesk artifacts detected." -ForegroundColor DarkGray
}

# --- 3. Uninstall existing install --------------------------------------
Write-Host "[3/7] Uninstalling existing SnipDesk..." -ForegroundColor DarkGray
if ($existing) {
  foreach ($item in $existing) {
    Write-Host "    uninstalling $($item.DisplayName) $($item.DisplayVersion)..." -ForegroundColor Yellow
    if ($item.UninstallString) {
      # Most MSI uninstall strings look like: MsiExec.exe /X{GUID}
      $uninstall = $item.UninstallString
      if ($uninstall -match '{[0-9A-Fa-f-]+}') {
        $guid = $Matches[0]
        Start-Process -FilePath "msiexec.exe" -ArgumentList "/x", $guid, "/qn", "/norestart" -Wait
      } else {
        # Not an MSI - try running it directly with /S.
        cmd /c "$uninstall /S" | Out-Null
      }
    }
  }
  Start-Sleep -Seconds 2
} else {
  Write-Host "    no existing install found." -ForegroundColor DarkGray
}

# --- 4. Wipe caches pre-install -----------------------------------------
# This is the critical pre-install step: now that the MSI uninstall has
# released its handles, blow away every cached icon copy BEFORE the new
# MSI writes anything. Runs with -RemovePins so pinned shortcuts don't
# keep serving the old icon from their embedded cache.
Write-Host "[4/7] Wiping all detected icon caches..." -ForegroundColor DarkGray
& (Join-Path $PSScriptRoot "clear-icon-cache.ps1") -RemovePins | Out-Null
# Some of the %WINDIR%\Installer dirs may now be empty but still exist
# -- harmless; MSI writes them back on install.
foreach ($g in $installerGuids) {
  $d = Join-Path $env:WINDIR "Installer\$g"
  if ((Test-Path $d) -and (-not (Get-ChildItem $d -Force -ErrorAction SilentlyContinue))) {
    Remove-Item -Path $d -Force -Recurse -ErrorAction SilentlyContinue
  }
}

# --- 5. Install new MSI --------------------------------------------------
$msiDir = Join-Path $repoRoot "target\release\bundle\msi"
$msi = Get-ChildItem -Path $msiDir -Filter "*.msi" -ErrorAction SilentlyContinue |
       Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $msi) {
  Write-Host "ERROR: no MSI found in $msiDir. Run build-windows.ps1 first." -ForegroundColor Red
  exit 1
}
Write-Host "[5/7] Installing $($msi.Name)..." -ForegroundColor Yellow
Start-Process -FilePath "msiexec.exe" -ArgumentList "/i", "`"$($msi.FullName)`"", "/passive", "/norestart" -Wait
Write-Host "    install complete." -ForegroundColor Green

# --- 6. Clear caches post-install ---------------------------------------
Write-Host "[6/7] Clearing icon caches (second pass - Explorer has written new shortcuts)..." -ForegroundColor DarkGray
& (Join-Path $PSScriptRoot "clear-icon-cache.ps1") | Out-Null

# --- 7. Done ------------------------------------------------------------
Write-Host "[7/7] Done." -ForegroundColor Cyan
Write-Host ""
Write-Host "SnipDesk is installed. If the taskbar icon is STILL stale:" -ForegroundColor Yellow
Write-Host "  - Unpin SnipDesk from the taskbar and re-pin it." -ForegroundColor Yellow
Write-Host "  - Or sign out of Windows and sign back in." -ForegroundColor Yellow
Write-Host ""
Write-Host "Launch SnipDesk:  Alt+Space (default hotkey)" -ForegroundColor Cyan
