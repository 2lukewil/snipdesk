# SnipDesk

A fast, searchable snippet launcher for support agents. Hit a global hotkey, type a few characters, press Enter — the canned reply gets pasted into whatever window you were just using.

Built with Tauri (Rust + web UI): a small (~5–10 MB), native binary for Windows, macOS, and Linux that starts instantly and runs from the system tray.

## Features

- **Global hotkey** — default `Alt+Space` (configurable). Toggles the launcher from anywhere.
- **Fast search** across title, body, and tags. Type to filter, ↑/↓ to navigate, Enter to paste.
- **Auto-paste or copy-only** — auto-paste returns focus to your previous window and pastes for you; or just copy to the clipboard.
- **Folders & tags** for organizing snippets, with a filter strip.
- **Variables** — put `{customer_name}` or `{invoice_id}` in a snippet and the app prompts for each value before pasting.
- **Usage counter** — most-used snippets bubble to the top.
- **Import / export** as JSON or CSV.
- **Local SQLite storage** in the OS app-data folder. Survives restarts, easy to back up.
- **Automatic updates** — new versions install in the background on launch.

## Install

Download the latest installer from the [Releases](https://github.com/2lukewil/snipdesk/releases) page and run it. It installs per-user (no admin prompt) and updates itself automatically on future launches.

The window starts hidden — press `Alt+Space` or click the tray icon to open it.

> Two editions are published: **SnipDesk Lite** (free, offline) and **SnipDesk** (Teams, adds a shared snippet library). Most users want Lite.

To build from source instead, see [Building from source](#building-from-source).

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

### Variables

A snippet body can contain `{variable_name}` placeholders (letters, digits, `_`, `-`). On paste, you're prompted for each one:

```
Hi {customer_name},

Your refund for invoice #{invoice_id} has been processed. It should show up
on {payment_method} within 3-5 business days.
```

## Data location

Your snippet database and settings live under the OS app-data folder, keyed by the build's identifier (`com.snipdesk.lite` for the free build, `com.snipdesk.teams` for Teams):

- Windows: `%APPDATA%\com.snipdesk.lite\`
- macOS: `~/Library/Application Support/com.snipdesk.lite/`
- Linux: `~/.local/share/com.snipdesk.lite/`

Back up `snippets.db` to migrate machines, or use **Settings → Export**.

## Building from source

### Prerequisites

- **Rust** (stable) — https://rustup.rs/ (the pinned toolchain installs automatically from `rust-toolchain.toml`).
- **Node.js 20+** — https://nodejs.org/ (CI builds on Node 24).
- **Platform dependencies**:
  - **Windows**: MSVC C++ Build Tools and WebView2 (preinstalled on Windows 11; `scripts/build-windows.ps1` installs both via winget if missing).
  - **macOS**: Xcode Command Line Tools — `xcode-select --install`.
  - **Linux**: `webkit2gtk-4.1`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`, `libssl-dev`. See https://tauri.app/start/prerequisites/.

### Develop

```bash
npm install
npm run tauri:dev
```

First run takes a few minutes while Rust compiles its dependencies; later runs only rebuild what changed.

### Build

```bash
npm run tauri:build            # free (Lite) edition
npm run tauri:build:teams      # Teams edition
```

Output lands in `target/release/bundle/` (the workspace target is at the repo root): `.msi`/`.exe` on Windows, `.app`/`.dmg` on macOS, `.deb`/`.rpm`/`.AppImage` on Linux.

On Windows you can also run `scripts/build-windows.ps1` from an elevated PowerShell to install prerequisites and build in one step.

### Editions

The same source tree produces both editions; which one you get is a build-time flag:

- **Lite (default)** — fully offline. Feature-gated network code never reaches the compiler, and the Team Library UI is stripped from the bundle.
- **Teams (`--features teams`)** — adds an HTTPS shared-library sync, a settings tab, and a background sync thread.

Teams-specific config (product name, identifier, updater endpoint) lives in `src-tauri/tauri.teams.conf.json`.

The offline edition pulls in no team-sync networking code. To verify the invariant:

```bash
cargo tree --manifest-path src-tauri/Cargo.toml --no-default-features
```

`ureq` and `snipdesk-teams` should both be absent. (The auto-updater is the one intentional outbound connection — it polls the GitHub releases manifest on launch via `tauri-plugin-updater`.)

### Releases & auto-update

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which builds and signs both editions, generates update manifests, and publishes a GitHub release. Clients pick it up on their next launch. See [docs/auto-update.md](docs/auto-update.md) for the full release process and one-time signing-key setup.

## Architecture

```
snipdesk/
├── src/                      # Frontend: index.html, main.js, styles.css (Vite-bundled)
├── crates/
│   ├── snipdesk-core/        # Offline engine: DB, paste, settings, backups, logging
│   └── snipdesk-teams/       # Team-library sync, behind the `teams` Cargo feature
├── src-tauri/                # Tauri shell: entry point, IPC commands, tray, hotkey, bundling
│   ├── tauri.conf.json       # Base (Lite) config
│   └── tauri.teams.conf.json # Teams overrides
├── scripts/                  # Build & release helpers
├── docs/                     # Design notes and the release/auto-update guide
└── Cargo.toml                # Workspace root
```

The frontend calls Rust via `invoke("command_name", args)`; all file I/O, SQLite, clipboard, hotkey registration, and key simulation live in Rust.

## Roadmap

Planned work — browser/WHMCS auto-fill of snippet variables, Teams two-way sync, and more — is tracked in [docs/ROADMAP.md](docs/ROADMAP.md), with the integration design in [docs/browser-integration.md](docs/browser-integration.md).
