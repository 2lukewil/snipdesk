<#
.SYNOPSIS
  Installs prerequisites (if missing) and builds SnipDesk as a Windows installer.

.DESCRIPTION
  Run this from an elevated PowerShell prompt (Run as Administrator).

  Everything is logged to scripts\build.log for post-mortem diagnosis, and the
  window pauses at the end so errors stay on screen.

.EXAMPLE
  PS C:\repos\snipdesk> Set-ExecutionPolicy -Scope Process Bypass -Force
  PS C:\repos\snipdesk> .\scripts\build-windows.ps1
#>

[CmdletBinding()]
param(
  [switch]$SkipIcons,
  [switch]$SkipPrereqs  # Assume Rust, Node, MSVC, WebView2, WiX already installed.
)

# Do NOT use "Stop" globally — winget and some installers return non-zero for
# benign states ("already installed", "no updates found") and we don't want
# those to kill the whole build. We'll check return codes explicitly where it
# matters.
$ErrorActionPreference = "Continue"

# --- Logging --------------------------------------------------------------
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$logDir = Join-Path $repoRoot "scripts"
$logPath = Join-Path $logDir "build.log"
try { Start-Transcript -Path $logPath -Force | Out-Null } catch {
  Write-Host "Couldn't start transcript ($_). Continuing without log file." -ForegroundColor DarkYellow
}

function Pause-AtEnd {
  Write-Host ""
  Write-Host "Log written to: $logPath" -ForegroundColor Cyan
  Write-Host "Press any key to close this window..." -ForegroundColor Cyan
  try { $null = $Host.UI.RawUI.ReadKey("NoEcho,IncludeKeyDown") } catch { Read-Host "Enter to exit" | Out-Null }
}

trap {
  Write-Host ""
  Write-Host "FATAL: $($_.Exception.Message)" -ForegroundColor Red
  Write-Host $_.ScriptStackTrace -ForegroundColor DarkGray
  try { Stop-Transcript | Out-Null } catch {}
  Pause-AtEnd
  exit 1
}

Write-Host "Repo root: $repoRoot" -ForegroundColor Cyan
Write-Host "Log file:  $logPath" -ForegroundColor Cyan
Write-Host ""

# --- Helpers --------------------------------------------------------------
function Test-Command($name) {
  $null -ne (Get-Command $name -ErrorAction SilentlyContinue)
}

function Refresh-Path {
  $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" +
              [System.Environment]::GetEnvironmentVariable("Path", "User")
}

function Invoke-Winget($id, $extraArgs = @()) {
  $args = @("install", "--id", $id,
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--disable-interactivity") + $extraArgs
  Write-Host "    winget $($args -join ' ')" -ForegroundColor DarkGray
  & winget @args
  $ec = $LASTEXITCODE
  # Documented benign exit codes:
  #   0             success
  #   -1978335189   no applicable update found
  #   -1978335212   package already installed
  #   -1978335215   no package found matching input criteria (some bundles)
  $benign = @(0, -1978335189, -1978335212)
  if ($benign -notcontains $ec) {
    Write-Host "    winget returned $ec — not fatal, continuing. (Re-run with elevation if needed.)" -ForegroundColor DarkYellow
  }
  Refresh-Path
}

function Check-Admin {
  $currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
  $isAdmin = $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  if (-not $isAdmin) {
    Write-Host "WARNING: This script is NOT running elevated." -ForegroundColor Yellow
    Write-Host "         winget installs will likely fail. Close this window," -ForegroundColor Yellow
    Write-Host "         right-click PowerShell > 'Run as Administrator', and re-run." -ForegroundColor Yellow
    Write-Host "         Pass -SkipPrereqs to skip the install steps if already done." -ForegroundColor Yellow
    Write-Host ""
  }
}

# --- 0. Early sanity -----------------------------------------------------
Write-Host "--- 0. Environment check ---" -ForegroundColor Cyan
Write-Host "PowerShell version: $($PSVersionTable.PSVersion)"
Write-Host "OS:                 $([Environment]::OSVersion.VersionString)"
Check-Admin

if (-not $SkipPrereqs) {
  if (-not (Test-Command "winget")) {
    Write-Host ""
    Write-Host "winget is not available in this shell." -ForegroundColor Red
    Write-Host "Install 'App Installer' from the Microsoft Store, or install prereqs manually." -ForegroundColor Red
    Write-Host "Alternatively pass -SkipPrereqs and install Rust/Node/VS Build Tools yourself." -ForegroundColor Yellow
    Stop-Transcript | Out-Null
    Pause-AtEnd
    exit 1
  }
}

# --- 1. Rust toolchain ----------------------------------------------------
Write-Host ""
Write-Host "--- 1. Rust toolchain ---" -ForegroundColor Cyan
if (-not $SkipPrereqs -and -not (Test-Command "cargo")) {
  Write-Host "Installing Rust (rustup)..." -ForegroundColor Yellow
  Invoke-Winget "Rustlang.Rustup"
}
Refresh-Path
if (-not (Test-Command "cargo")) {
  Write-Host "cargo still not on PATH. Open a NEW PowerShell window and re-run." -ForegroundColor Red
  Stop-Transcript | Out-Null
  Pause-AtEnd
  exit 1
}
& rustup default stable | Out-Null
Write-Host "Rust: $(& rustc --version)" -ForegroundColor Green

# --- 2. Node.js -----------------------------------------------------------
Write-Host ""
Write-Host "--- 2. Node.js ---" -ForegroundColor Cyan
if (-not $SkipPrereqs -and -not (Test-Command "node")) {
  Invoke-Winget "OpenJS.NodeJS.LTS"
}
Refresh-Path
if (-not (Test-Command "node")) {
  Write-Host "node still not on PATH. Install Node.js LTS manually from https://nodejs.org/ and re-run." -ForegroundColor Red
  Stop-Transcript | Out-Null
  Pause-AtEnd
  exit 1
}
Write-Host "Node:  $(& node --version)" -ForegroundColor Green
Write-Host "npm:   $(& npm --version)" -ForegroundColor Green

# --- 3. MSVC Build Tools --------------------------------------------------
Write-Host ""
Write-Host "--- 3. Microsoft C++ Build Tools ---" -ForegroundColor Cyan
if (-not $SkipPrereqs -and -not (Test-Command "link.exe")) {
  Write-Host "Installing VS 2022 Build Tools (large download, ~3 GB — this step can take 10+ min)..." -ForegroundColor Yellow
  Invoke-Winget "Microsoft.VisualStudio.2022.BuildTools" @(
    "--override",
    "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --includeRecommended"
  )
  Refresh-Path
} elseif (Test-Command "link.exe") {
  Write-Host "MSVC link.exe already on PATH." -ForegroundColor Green
} else {
  Write-Host "Skipping MSVC install (per -SkipPrereqs)." -ForegroundColor DarkGray
}

# --- 4. WebView2 runtime --------------------------------------------------
Write-Host ""
Write-Host "--- 4. WebView2 runtime ---" -ForegroundColor Cyan
$webview2Key = "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
if (-not $SkipPrereqs -and -not (Test-Path $webview2Key)) {
  Invoke-Winget "Microsoft.EdgeWebView2Runtime"
} else {
  Write-Host "WebView2 runtime present." -ForegroundColor Green
}

# --- 5. WiX Toolset (optional — Tauri downloads it on demand) -------------
Write-Host ""
Write-Host "--- 5. WiX Toolset ---" -ForegroundColor Cyan
Write-Host "Tauri downloads WiX automatically on first .msi build. Skipping explicit install." -ForegroundColor DarkGray

# --- 6. npm install -------------------------------------------------------
Write-Host ""
Write-Host "--- 6. npm install ---" -ForegroundColor Cyan
& npm install
if ($LASTEXITCODE -ne 0) {
  Write-Host "npm install failed with exit code $LASTEXITCODE." -ForegroundColor Red
  Stop-Transcript | Out-Null
  Pause-AtEnd
  exit 1
}

# --- 7. Icons -------------------------------------------------------------
Write-Host ""
Write-Host "--- 7. App icons ---" -ForegroundColor Cyan
$iconDir = Join-Path $repoRoot "src-tauri\icons"
$iconIco = Join-Path $iconDir "icon.ico"
$sourcePng = Join-Path $iconDir "source.png"

if (-not $SkipIcons -and -not (Test-Path $iconIco)) {
  New-Item -ItemType Directory -Force -Path $iconDir | Out-Null

  if (-not (Test-Path $sourcePng)) {
    Write-Host "Generating 1024x1024 placeholder PNG via .NET (no source.png provided)..." -ForegroundColor Yellow
    Add-Type -AssemblyName System.Drawing
    $bmp = New-Object System.Drawing.Bitmap 1024, 1024
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 44, 111, 216))
    $font = New-Object System.Drawing.Font("Segoe UI", 380, [System.Drawing.FontStyle]::Bold)
    $brush = [System.Drawing.Brushes]::White
    $fmt = New-Object System.Drawing.StringFormat
    $fmt.Alignment = [System.Drawing.StringAlignment]::Center
    $fmt.LineAlignment = [System.Drawing.StringAlignment]::Center
    $g.DrawString("S", $font, $brush, (New-Object System.Drawing.RectangleF 0, 0, 1024, 1024), $fmt)
    $bmp.Save($sourcePng, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Write-Host "Placeholder written to $sourcePng" -ForegroundColor DarkYellow
  }

  Write-Host "Running 'npx @tauri-apps/cli icon'..." -ForegroundColor Yellow
  & npx --yes @tauri-apps/cli icon $sourcePng
  if ($LASTEXITCODE -ne 0) {
    Write-Host "Icon generation failed (exit $LASTEXITCODE). Put a real PNG at $sourcePng and re-run." -ForegroundColor Red
    Stop-Transcript | Out-Null
    Pause-AtEnd
    exit 1
  }
} else {
  Write-Host "Icons already present (or --SkipIcons set)." -ForegroundColor Green
}

# --- 8. Build -------------------------------------------------------------
Write-Host ""
Write-Host "--- 8. Building release installer ---" -ForegroundColor Cyan
Write-Host "(First build ~15-20 min while crates compile. Subsequent builds ~1 min.)" -ForegroundColor DarkGray
& npm run tauri:build
if ($LASTEXITCODE -ne 0) {
  Write-Host ""
  Write-Host "Build failed. See errors above (full log: $logPath)." -ForegroundColor Red
  Stop-Transcript | Out-Null
  Pause-AtEnd
  exit 1
}

# --- 9. Surface outputs ---------------------------------------------------
$bundleDir = Join-Path $repoRoot "src-tauri\target\release\bundle"
Write-Host ""
Write-Host "===========================================" -ForegroundColor Cyan
Write-Host "Build complete. Outputs:" -ForegroundColor Cyan
Get-ChildItem -Path $bundleDir -Recurse -Include "*.msi", "*.exe" -ErrorAction SilentlyContinue | ForEach-Object {
  Write-Host "  $($_.FullName)" -ForegroundColor Green
}
Write-Host "===========================================" -ForegroundColor Cyan

Write-Host ""
Write-Host "To INSTALL (and reliably refresh the icon), run:" -ForegroundColor DarkGray
Write-Host "   .\scripts\reinstall.ps1" -ForegroundColor Cyan
Write-Host "(That uninstalls the old version, clears Windows icon caches, installs" -ForegroundColor DarkGray
Write-Host " this new MSI, then clears caches again. Plain double-clicking the MSI" -ForegroundColor DarkGray
Write-Host " does an in-place upgrade that keeps the old cached icon.)" -ForegroundColor DarkGray

Stop-Transcript | Out-Null
Pause-AtEnd
