<#
.SYNOPSIS
  Force Windows to rebuild its icon cache after a SnipDesk reinstall.

.DESCRIPTION
  Windows caches icons at SEVERAL levels, and reinstalling an app only
  invalidates some of them. If the taskbar or Start Menu still shows the
  OLD SnipDesk icon after a fresh install, one of these caches is stale:

    1. %LOCALAPPDATA%\IconCache.db               (legacy shell cache)
    2. Explorer\iconcache_*.db                    (per-size shards, Win10+)
    3. Explorer\thumbcache_*.db                   (thumbnail cache)
    4. Taskbar-pinned .lnk shortcut                (caches the icon at pin time)
    5. Start Menu .lnk shortcut                    (installed by the MSI)
    6. MUICache registry                           (tracks EXE display info)
    7. TrayNotify IconStreams                      (system tray icon blob)
    8. %WINDIR%\Installer\{ProductGUID}\*.ico      (MSI-extracted icons)

  This script handles #1–3, #6, #7, #8 automatically. It also LISTS any #4/#5
  pinned shortcuts pointing at SnipDesk so you can unpin & re-pin them —
  those can't be refreshed any other way without a reboot.

.EXAMPLE
  PS C:\repos\snipdesk> .\scripts\clear-icon-cache.ps1

.EXAMPLE
  # Aggressive mode - also removes pinned shortcuts (you'll need to re-pin)
  PS C:\repos\snipdesk> .\scripts\clear-icon-cache.ps1 -RemovePins
#>
[CmdletBinding()]
param(
  [switch]$RemovePins   # Also delete taskbar-pinned SnipDesk shortcut (user must re-pin)
)

$ErrorActionPreference = "Continue"

Write-Host "Clearing Windows icon caches for SnipDesk..." -ForegroundColor Cyan
Write-Host ""

# --- 0. Detect current ProductGUID via registry Uninstall keys -----------
# MSIs store icons in %WINDIR%\Installer\{GUID}\ -- these persist across
# uninstalls (Windows Installer's resilience feature) and are a common
# source of stale icons on reinstall. Purging them forces a clean extract.
function Get-SnipDeskProductGuids {
  $uninstallKeys = @(
    "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*",
    "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*",
    "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*"
  )
  $guids = @()
  foreach ($kp in $uninstallKeys) {
    Get-ItemProperty -Path $kp -ErrorAction SilentlyContinue |
      Where-Object { $_.DisplayName -like "SnipDesk*" } |
      ForEach-Object {
        if ($_.PSChildName -match '^{[0-9A-Fa-f-]+}$') { $guids += $_.PSChildName }
      }
  }
  return $guids | Select-Object -Unique
}

# --- 1. Stop Explorer so it releases the cache DBs -----------------------
$explorerWasRunning = [bool](Get-Process explorer -ErrorAction SilentlyContinue)
if ($explorerWasRunning) {
  Write-Host "[1/7] Stopping Explorer..." -ForegroundColor DarkGray
  Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 600
}

# --- 2. Delete cache DBs -------------------------------------------------
Write-Host "[2/7] Removing cache databases..." -ForegroundColor DarkGray
$caches = @(
  "$env:LOCALAPPDATA\IconCache.db",
  "$env:LOCALAPPDATA\Microsoft\Windows\Explorer\iconcache_*.db",
  "$env:LOCALAPPDATA\Microsoft\Windows\Explorer\thumbcache_*.db"
)
$removed = 0
foreach ($pattern in $caches) {
  Get-Item -Path $pattern -ErrorAction SilentlyContinue | ForEach-Object {
    try {
      Remove-Item -Path $_.FullName -Force -ErrorAction Stop
      Write-Host "    removed $($_.FullName)" -ForegroundColor DarkGreen
      $removed++
    } catch {
      Write-Host "    could not remove $($_.FullName): $_" -ForegroundColor DarkYellow
    }
  }
}

# --- 3. Purge MUICache entries for any path containing "snipdesk" --------
Write-Host "[3/7] Cleaning MUICache entries for snipdesk.exe..." -ForegroundColor DarkGray
try {
  $muiKey = "HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\Shell\MuiCache"
  if (Test-Path $muiKey) {
    $props = Get-ItemProperty -Path $muiKey
    $props.PSObject.Properties |
      Where-Object { $_.Name -like "*snipdesk*" } |
      ForEach-Object {
        Remove-ItemProperty -Path $muiKey -Name $_.Name -ErrorAction SilentlyContinue
        Write-Host "    removed $($_.Name)" -ForegroundColor DarkGreen
      }
  }
} catch {
  Write-Host "    MUICache clean skipped: $_" -ForegroundColor DarkYellow
}

# --- 4. Locate pinned shortcuts pointing at SnipDesk ---------------------
Write-Host "[4/7] Scanning for SnipDesk shortcuts..." -ForegroundColor DarkGray
$shortcutDirs = @(
  "$env:APPDATA\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar",
  "$env:APPDATA\Microsoft\Windows\Start Menu\Programs",
  "$env:PROGRAMDATA\Microsoft\Windows\Start Menu\Programs",
  "$env:USERPROFILE\Desktop"
)
$wsh = New-Object -ComObject WScript.Shell
$snipdeskLnks = @()
foreach ($dir in $shortcutDirs) {
  if (-not (Test-Path $dir)) { continue }
  Get-ChildItem -Path $dir -Filter "*.lnk" -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
    try {
      $shortcut = $wsh.CreateShortcut($_.FullName)
      if ($shortcut.TargetPath -match "snipdesk") {
        $snipdeskLnks += $_.FullName
      }
    } catch {}
  }
}

if ($snipdeskLnks.Count -gt 0) {
  Write-Host "    found these pinned SnipDesk shortcuts:" -ForegroundColor Yellow
  foreach ($lnk in $snipdeskLnks) { Write-Host "      $lnk" -ForegroundColor Yellow }
  if ($RemovePins) {
    foreach ($lnk in $snipdeskLnks) {
      try {
        Remove-Item -Path $lnk -Force
        Write-Host "    removed $lnk" -ForegroundColor DarkGreen
      } catch {
        Write-Host "    could not remove $lnk: $_" -ForegroundColor DarkYellow
      }
    }
  } else {
    Write-Host ""
    Write-Host "    ^ These shortcuts cache the icon at pin time and will not" -ForegroundColor Yellow
    Write-Host "      refresh automatically. Either re-run this script with" -ForegroundColor Yellow
    Write-Host "      -RemovePins, or unpin SnipDesk from your taskbar /" -ForegroundColor Yellow
    Write-Host "      Start Menu and re-pin it." -ForegroundColor Yellow
  }
} else {
  Write-Host "    no SnipDesk shortcuts found." -ForegroundColor DarkGray
}

# --- 5. Clear TrayNotify IconStreams (tray icon cache) -------------------
# The notification area caches every tray icon it has EVER seen in two
# registry blobs: IconStreams and PastIconsStream. If SnipDesk's tray icon
# ever rendered wrong, those blobs can keep serving the stale bitmap
# forever. Explorer rebuilds them on next launch.
Write-Host "[5/7] Clearing TrayNotify icon cache..." -ForegroundColor DarkGray
try {
  $trayKey = "HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\TrayNotify"
  if (Test-Path $trayKey) {
    foreach ($val in "IconStreams", "PastIconsStream") {
      if ((Get-Item $trayKey).GetValue($val, $null) -ne $null) {
        Remove-ItemProperty -Path $trayKey -Name $val -ErrorAction SilentlyContinue
        Write-Host "    cleared $val" -ForegroundColor DarkGreen
      }
    }
  }
} catch {
  Write-Host "    TrayNotify clean skipped: $_" -ForegroundColor DarkYellow
}

# --- 6. Purge Windows Installer cached icons -----------------------------
# When Windows installs an MSI, it extracts the product icon into
# %WINDIR%\Installer\{ProductGUID}\ and keeps it there permanently. On
# repair/reinstall Windows Installer can serve the OLD cached icon from
# this folder instead of pulling a fresh one from the new MSI. Wiping the
# folder forces re-extraction on next install.
Write-Host "[6/7] Checking Windows Installer icon cache..." -ForegroundColor DarkGray
$guids = Get-SnipDeskProductGuids
if ($guids.Count -gt 0) {
  foreach ($guid in $guids) {
    $installerDir = Join-Path $env:WINDIR "Installer\$guid"
    if (Test-Path $installerDir) {
      Get-ChildItem -Path $installerDir -Filter "*.ico" -ErrorAction SilentlyContinue |
        ForEach-Object {
          try {
            Remove-Item -Path $_.FullName -Force -ErrorAction Stop
            Write-Host "    removed $($_.FullName)" -ForegroundColor DarkGreen
          } catch {
            Write-Host "    could not remove $($_.FullName): $_" -ForegroundColor DarkYellow
          }
        }
      Get-ChildItem -Path $installerDir -Filter "*.exe" -ErrorAction SilentlyContinue |
        ForEach-Object {
          # ARPPRODUCTICON is sometimes stored as an extracted .exe stub too
          if ($_.Name -match "ARPPRODUCTICON|snipdesk") {
            try {
              Remove-Item -Path $_.FullName -Force -ErrorAction Stop
              Write-Host "    removed $($_.FullName)" -ForegroundColor DarkGreen
            } catch {
              Write-Host "    could not remove $($_.FullName): $_" -ForegroundColor DarkYellow
            }
          }
        }
    }
  }
} else {
  Write-Host "    no installed SnipDesk ProductGUID found (already uninstalled or fresh install)." -ForegroundColor DarkGray
}

# --- 7. Rebuild shell + restart Explorer ---------------------------------
Write-Host "[7/7] Refreshing shell..." -ForegroundColor DarkGray
$ie4uinit = Join-Path $env:SystemRoot "System32\ie4uinit.exe"
if (Test-Path $ie4uinit) {
  & $ie4uinit "-show" | Out-Null
  & $ie4uinit "-ClearIconCache" | Out-Null
}

if ($explorerWasRunning -and -not (Get-Process explorer -ErrorAction SilentlyContinue)) {
  Start-Process explorer.exe
}

Write-Host ""
Write-Host "Cache clear complete ($removed cache file(s) removed)." -ForegroundColor Cyan
if ($snipdeskLnks.Count -gt 0 -and -not $RemovePins) {
  Write-Host "Next: unpin + re-pin SnipDesk OR re-run with -RemovePins." -ForegroundColor Yellow
}
Write-Host "If the icon is still wrong, sign out and back in — that rebuilds the" -ForegroundColor DarkGray
Write-Host "per-user Start Menu/taskbar database from scratch." -ForegroundColor DarkGray
