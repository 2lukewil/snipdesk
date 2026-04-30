# SnipDesk

A fast, searchable snippet launcher for support agents. Hit a global hotkey, type a few characters, press Enter — the canned reply gets pasted into whatever window you were just using.

Built with Tauri (Rust + web UI). Shipped binary is small (~5–10 MB), native on Windows/macOS/Linux, and starts instantly.

## Features

- **Global hotkey** — default `Alt+Space` (configurable). Toggles the launcher from anywhere.
- **Fuzzy-ish search** across title, body, and tags. Type to filter, ↑/↓ to navigate, Enter to paste.
- **Auto-paste OR copy-only** — choose in settings. Auto-paste hides the window, returns focus to the previous app, and pastes via WM_PASTE on Windows / Ctrl+V simulation elsewhere.
- **Categories / tags** with a filter strip at the top.
- **Variables / placeholders** — put `{customer_name}` or `{invoice_id}` in a snippet body and the app prompts for each value before pasting.
- **Usage counter** — most-used snippets bubble to the top.
- **Import / export** as JSON or CSV, plus PhraseExpress `.pex` XML import. Modern `.pexdb` databases are encrypted by PhraseExpress; use the XML export.
- **System tray** — runs quietly in the background, click the tray icon to open.
- **SQLite storage** in the OS app-data directory. Survives restarts, easy to back up.

## Architecture

```
snipdesk/
├── src/                          # Frontend (HTML/CSS/vanilla JS, Vite-bundled)
│   ├── index.html
│   ├── main.js                   # UI state, invoke() calls to Rust
│   └── styles.css
├── crates/
│   ├── snipdesk-core/            # Offline engine: DB, paste, settings, backups,
│   │                             # logging, .pex/.pexdb import. No networking.
│   └── snipdesk-teams/           # Network features behind the `teams` Cargo
│                                 # feature. Pulls in `ureq`.
├── src-tauri/                    # Tauri shell: entry point, IPC, tray, hotkey,
│   ├── src/                      # MSI bundling.
│   │   ├── main.rs
│   │   ├── lib.rs                # Setup; #[cfg(feature = "teams")] gates apply.
│   │   └── commands.rs
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   └── capabilities/default.json
├── scripts/
│   ├── build-windows.ps1         # Free build (winget + tauri build)
│   └── build-teams.mjs           # Teams build orchestrator
├── Cargo.toml                    # Workspace root
├── package.json
├── vite.config.js                # Injects __SNIPDESK_TEAMS_BUILD__
└── README.md
```

The frontend calls Rust via `invoke("command_name", args)`. All file I/O, SQLite, clipboard, hotkey registration, and key-simulation live in Rust.

### Build flavors

The same source tree produces two flavors:

- **Free (offline)** — default. The free build's dep graph contains no networking code: `cargo tree --no-default-features` shows no `ureq` and no `snipdesk-teams`. The Team Library UI is dead-code-eliminated from the bundle because Vite substitutes `__SNIPDESK_TEAMS_BUILD__ = false` and esbuild folds the `if (false)` branches.
- **Teams** — built with `--features teams`. Adds an HTTPS shared-library fetch, a settings tab, and a background sync thread. Same SQLite, same hotkey, same UX otherwise.

Switching between them is a build-time flag. Feature-gated Rust never reaches the compiler in the free build, and the gated frontend never reaches the bundle, so keeping both in one repo costs nothing in the shipped offline binary.

## Prerequisites

- **Rust** (stable): https://rustup.rs/
- **Node.js** 18+ (LTS): https://nodejs.org/
- **Platform deps**:
  - **Windows**: MSVC C++ Build Tools, WebView2 (preinstalled on Windows 11; `build-windows.ps1` installs both via winget if missing).
  - **macOS**: Xcode CLT — `xcode-select --install`.
  - **Linux**: `webkit2gtk-4.1`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`, `libssl-dev`. See https://tauri.app/start/prerequisites/ per distro.

## Setup

```bash
cd snipdesk
npm install
```

App icons live in `src-tauri/icons/`. Generate them from a single PNG:

```bash
npx @tauri-apps/cli icon path/to/source-1024.png
```

That produces `icon.ico`, `icon.icns`, and the `*.png` sizes referenced in `tauri.conf.json`.

## Run in development

```bash
npm run tauri:dev
```

First run takes a few minutes (Rust compiles ~300 crates). Subsequent runs only rebuild changed code. The window starts hidden — press `Alt+Space` to open it, or click the tray icon.

## Build a release binary

### Free build

```powershell
# Windows, one command (run from elevated PowerShell):
Set-ExecutionPolicy -Scope Process Bypass
.\scripts\build-windows.ps1
```

The script installs prereqs via winget, runs `npm install`, generates icons if missing, and builds the `.msi`. First run is ~15–20 min; subsequent runs ~1 min.

For other platforms or manual builds:

```bash
npm run tauri:build
```

Output: `src-tauri/target/release/bundle/`

- Windows: `.msi` + `.exe`
- macOS: `.app` + `.dmg`
- Linux: `.deb` / `.rpm` / `.AppImage`

### Teams build

```bash
npm run tauri:build:teams
```

This runs `scripts/build-teams.mjs`, which builds the frontend with `vite build --mode teams`, then runs `tauri build --features teams` with `beforeBuildCommand` overridden so Tauri doesn't re-run vite in free mode and clobber the bundle.

To debug the orchestration, run the halves separately:

```bash
npm run build:teams
cd src-tauri
cargo build --release --features teams
```

Output goes to the same `src-tauri/target/release/bundle/` path. Identifier and product name match the free build for now; change them in `tauri.conf.json` when we ship a side-by-side install.

### Verifying the offline guarantee

```bash
cargo tree --manifest-path src-tauri/Cargo.toml --no-default-features
```

`ureq` and `snipdesk-teams` should both be absent. If either shows up, something has been added to the non-optional dependency list.

### CI releases

`.github/workflows/release.yml` builds the Windows installer on tag push (`git tag v1.0.0 && git push --tags`) and attaches it to the release. To produce both flavors, duplicate the build job with the Teams orchestrator and upload both MSIs.

## Usage

| Key | Action |
| --- | --- |
| `Alt+Space` | Toggle launcher (global) |
| Type in search | Filter by title / body / tags |
| `↑` / `↓` | Navigate list |
| `Enter` | Paste selected snippet (or copy, depending on settings) |
| `Shift+Enter` | Copy to clipboard only |
| `Ctrl+N` | New snippet |
| `Ctrl+E` | Edit selected |
| `Delete` | Delete selected |
| `Ctrl+,` | Open settings |
| `Esc` | Clear search/filter, or hide window |

## Data location

SQLite database + JSON settings live under the OS app-data folder:

- Windows: `%APPDATA%\com.shockbyte.snipdesk\`
- macOS: `~/Library/Application Support/com.shockbyte.snipdesk/`
- Linux: `~/.local/share/com.shockbyte.snipdesk/`

Back up `snippets.db` to migrate machines, or use **Settings → Export**.

## Variables

A snippet body can contain `{variable_name}` placeholders (letters, digits, `_`, `-`). On paste, a prompt appears for each one.

```
Hi {customer_name},

Your refund for invoice #{invoice_id} has been processed. It should show up
on {payment_method} within 3-5 business days.
```

## Migrating from PhraseExpress

Use PhraseExpress's XML export, not the `.pexdb`:

1. In PhraseExpress, open the phrase file to migrate.
2. **File → Export → "Phrase file (`*.pex`)"** and save it anywhere.
3. In SnipDesk, **Settings → Import…**, pick the `.pex`. Folder names become tags; autotext shortcuts are preserved as `shortcut:<text>` tags.

PhraseExpress v16+ encrypts the `.pexdb` on disk (no SQLite magic header, maximum entropy) and there's no third-party way to decrypt it without the PhraseExpress master key. The importer still accepts older unencrypted `.pexdb` files and surfaces a clear error pointing at the XML path if it sees an encrypted one.

Unrecognized rows are skipped rather than aborting the whole import. After import, eyeball the list and clean up anything that came through wrong.

## Roadmap: WHMCS / browser auto-fill

See [docs/browser-integration.md](docs/browser-integration.md) for the full design. Short version:

1. **Phase B — window-title parser**: read the active browser tab's title, regex for ticket/invoice IDs, pre-fill the variable prompt.
2. **Phase C — WHMCS Admin API**: settings panel adds WHMCS API credentials (OS keychain), and matching variables (`{customer_name}`, `{service_type}`, `{cancellation_date}`, etc.) pre-fill from WHMCS. Agent can override.
3. **Phase D — browser extension + native messaging**: only if/when we need non-WHMCS context.

Variable substitution goes through a `HashMap<String, String>` from JS to the `use_snippet` command, so a provider just needs to populate that map before the modal opens.

## Why Tauri

Other options considered:

- **Electron** — more accessible for JS-only teams, but ~150 MB binaries and higher RAM use.
- **PyQt / Tkinter** — fastest to prototype, but packaging + global-hotkey support are weaker.
- **AutoHotkey** — extremely light on Windows, but single-platform and the UI is crude.

Tauri wins on binary size, native feel, cross-platform reach, and the Rust side makes SQLite + keyboard simulation trivial.

## Known follow-ups

- App icons aren't bundled in the repo. Run `npx @tauri-apps/cli icon <path>` once to generate them.
- Two-way sync (agents publishing edits back, conflict resolution, SSO-gated dashboards) is the next milestone for `snipdesk-teams`. Today it's pull-only.
- Linux auto-paste uses X11 (`enigo`); Wayland users may need to fall back to copy-only mode.
- Side-by-side install of free + Teams MSIs needs different identifiers / product names. Deferred until Teams ships.
