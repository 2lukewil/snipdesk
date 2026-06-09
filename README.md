# SnipDesk

A fast, searchable snippet launcher for support agents. Hit a global hotkey, type a few characters, press Enter - the canned reply gets pasted into whatever window you were just using.

Built with Tauri (Rust + web UI): a small (~5-10 MB), native binary for Windows, macOS, and Linux that starts instantly and runs from the system tray.

## Features

### Desktop launcher (both editions)

- **Global hotkey** - default `Alt+Space` (configurable). Toggles the launcher from anywhere.
- **Fast search** across title, body, and tags. Type to filter, ↑/↓ to navigate, Enter to paste.
- **Auto-paste or copy-only** - auto-paste returns focus to your previous window and pastes for you; or just copy to the clipboard.
- **Folders & tags** for organizing snippets, with a filter strip and drag-and-drop reorganization.
- **Variables** - put `{customer_name}` or `{invoice_id}` in a snippet and the app prompts for each value before pasting. The prompt remembers previously-used values per snippet+variable.
- **Usage counter** - most-used snippets bubble to the top.
- **Import / export** as JSON or CSV.
- **Local SQLite storage** in the OS app-data folder. Survives restarts; rolling backups in `backups/`.
- **Automatic updates** - new versions install in the background on launch.

### Teams edition adds

- **Server-backed sync** of personal snippets across devices. AES-256-GCM encryption at rest on the server; a database dump reveals nothing without the master key.
- **Sign-in options**: email + password OR Google Workspace SSO (OIDC).
- **Persistent login**: the server rotates session tokens automatically so a daily user stays signed in indefinitely.
- **Shared team library**: admin-curated snippets that appear in every member's launcher with a cloud glyph. Mixed into the All view and folder views; toggle to hide them is in Settings.
- **Trash + restore**: deleted snippets stay recoverable for 90 days (configurable) via a per-user trash panel.
- **Admin dashboard**: browser-based at `https://<server>/`. Users + roles, shared library CRUD with folders / drag-drop / inline edit, server-wide stats. Members are blocked.
- **Real-time propagation**: role changes and disabled accounts take effect on the next API call, not at JWT expiry. Disabled users get signed out from the desktop automatically.

## Install

Download the latest installer from the [Releases](https://github.com/2lukewil/snipdesk/releases) page and run it. It installs per-user (no admin prompt) and updates itself automatically on future launches.

The window starts hidden - press `Alt+Space` or click the tray icon to open it.

> Two editions are published: **SnipDesk Lite** (free, offline) and **SnipDesk Teams** (server-backed sync + shared library + SSO). Most individual users want Lite; Teams is for organizations running their own snipdesk-server.

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

- **Rust** (stable) - https://rustup.rs/ (the pinned toolchain installs automatically from `rust-toolchain.toml`).
- **Node.js 20+** - https://nodejs.org/ (CI builds on Node 24).
- **Platform dependencies**:
  - **Windows**: MSVC C++ Build Tools and WebView2 (preinstalled on Windows 11; `scripts/build-windows.ps1` installs both via winget if missing).
  - **macOS**: Xcode Command Line Tools - `xcode-select --install`.
  - **Linux**: `webkit2gtk-4.1`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`, `libssl-dev`. See https://tauri.app/start/prerequisites/.

### Develop

```bash
npm install
npm run tauri:dev
```

First run takes a few minutes while Rust compiles its dependencies; later runs only rebuild what changed.

### Develop the Teams edition

The Teams build needs the cargo feature plus the Teams Tauri config; one npm script bundles both:

```bash
npm run tauri:dev:teams
```

You'll need a snipdesk-server running locally (see below) to actually sign in.

### Build

```bash
npm run tauri:build            # free (Lite) edition
npm run tauri:build:teams      # Teams edition
```

Output lands in `target/release/bundle/` (the workspace target is at the repo root): `.msi`/`.exe` on Windows, `.app`/`.dmg` on macOS, `.deb`/`.rpm`/`.AppImage` on Linux.

On Windows you can also run `scripts/build-windows.ps1` from an elevated PowerShell to install prerequisites and build in one step.

### Editions

The same source tree produces both editions; which one you get is a build-time flag:

- **Lite (default)** - fully offline. Feature-gated network code never reaches the compiler, and the Team Library UI is stripped from the bundle.
- **Teams (`--features teams`)** - adds an HTTPS shared-library sync, a settings tab, and a background sync thread.

Teams-specific config (product name, identifier, updater endpoint) lives in `src-tauri/tauri.teams.conf.json`.

The offline edition pulls in no team-sync networking code. To verify the invariant:

```bash
cargo tree --manifest-path src-tauri/Cargo.toml --no-default-features
```

`ureq` and `snipdesk-teams` should both be absent. (The auto-updater is the one intentional outbound connection - it polls the GitHub releases manifest on launch via `tauri-plugin-updater`.)

### Releases & auto-update

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which builds and signs both editions, generates update manifests, and publishes a GitHub release. Clients pick it up on their next launch. See [docs/auto-update.md](docs/auto-update.md) for the full release process and one-time signing-key setup.

## Architecture

```
snipdesk/
├── src/                      # Frontend: index.html, main.js, styles.css (Vite-bundled)
├── crates/
│   ├── snipdesk-core/        # Offline engine: DB, paste, settings, backups, logging
│   ├── snipdesk-teams/       # Server-sync client (Teams-only): API, sync engine, keychain
│   └── snipdesk-server/      # Self-hosted backend (Axum + SQLite + JWT + AES-GCM + htmx)
├── src-tauri/                # Tauri shell: entry point, IPC commands, tray, hotkey, bundling
│   ├── tauri.conf.json       # Base (Lite) config
│   └── tauri.teams.conf.json # Teams overrides (deep-link scheme, identifier, updater)
├── scripts/                  # Build & release helpers
├── docs/                     # Design notes, deployment guide, user docs
└── Cargo.toml                # Workspace root
```

The frontend calls Rust via `invoke("command_name", args)`; all file I/O, SQLite, clipboard, hotkey registration, and key simulation live in Rust. The Teams server is a separate binary (`snipdesk-server`) that you'd host yourself.

## Self-hosting the Teams server

If you want server-backed sync, shared library, and SSO, run `snipdesk-server` on a box you control. See:

- [docs/docker-quickstart.md](docs/docker-quickstart.md) - 5-minute fresh-machine-to-working-dashboard walkthrough
- [docs/deploy.md](docs/deploy.md) - production deployment walkthrough (TLS, reverse proxy, OIDC setup, backups, whitelabel)
- [docs/server-design.md](docs/server-design.md) - architecture, schema, security posture, sync algorithm

Quick local dev:

```bash
cd crates/snipdesk-server
cargo run -p snipdesk-server -- gen-key        # > master_key
cargo run -p snipdesk-server -- gen-jwt-secret # > jwt_secret
cp snipdesk-server.example.toml snipdesk-server.toml   # edit in the two secrets
cargo run -p snipdesk-server -- --config snipdesk-server.toml
```

Now visit http://127.0.0.1:8080/ in a browser to see the admin dashboard, or point a Teams desktop client at `http://127.0.0.1:8080` to sync.

## User documentation

- [docs/getting-started.md](docs/getting-started.md) - first-time setup guide for end users.
- [docs/deploy.md](docs/deploy.md) - server deployment for admins.

## Roadmap

Planned work is tracked in [docs/ROADMAP.md](docs/ROADMAP.md). Notable deferred items: end-to-end encryption (v2, design path in `docs/server-design.md`), conflict-loser preservation on concurrent edits (v1.1), browser/WHMCS auto-fill (design in [docs/browser-integration.md](docs/browser-integration.md)).
