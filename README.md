# SnipDesk

Fast snippet launcher for support agents. Hit a hotkey, type a few characters, press Enter - the canned reply drops into whatever field you were just in. It comes as a native desktop app and as a Chrome extension; both organize snippets into folders with tags and can share a synced team library.

The desktop app is built with Tauri (Rust + web UI): a small (~5-10 MB) native binary for Windows, macOS, and Linux that starts instantly and runs from the system tray. It ships in two editions: **Lite** (free, offline, snippets live on the device) and **Teams** (server-backed sync across devices, shared team library, single sign-on, admin dashboard). The browser extension carries the same launcher and snippet manager into web fields, working offline with an optional sign-in for sync and the team library.

## Components

The repo holds one server and two independent clients that talk to it over HTTP. Each is self-contained; the clients share no code with each other.

- **Server** (`crates/snipdesk-server`) - self-hosted backend: API, team library, SSO, admin dashboard.
- **Desktop client** (`src/` + `src-tauri/`) - the Tauri app above; pastes into any window.
- **Browser client** (`extension/`) - a Chrome (MV3) extension that inserts snippets into web fields. Runs on any OS with no install friction. See [`extension/README.md`](extension/README.md).

> ## Documentation
>
> Full, searchable docs live at **<https://2lukewil.github.io/snipdesk/>** (source: [`docs/`](docs/)).
>
> - **End users:** [Getting started](https://2lukewil.github.io/snipdesk/getting-started)
> - **Self-hosters:** [Docker quickstart](https://2lukewil.github.io/snipdesk/docker-quickstart) | [Production deploy](https://2lukewil.github.io/snipdesk/deploy)
> - **Developers:** [Build from source](https://2lukewil.github.io/snipdesk/build) | [Whitelabel](https://2lukewil.github.io/snipdesk/whitelabel) | [Releases](https://2lukewil.github.io/snipdesk/auto-update) | [Architecture](https://2lukewil.github.io/snipdesk/server-design)

## Install

Grab the latest installer from the [Releases](https://github.com/2lukewil/snipdesk/releases) page. Per-user install, no admin prompt, self-updating on launch. The window starts hidden; press `Alt+Space` or click the tray icon to open it.

## Developer quickstart

```bash
git clone https://github.com/2lukewil/snipdesk.git
cd snipdesk
npm install
npm run tauri:dev            # Lite (offline) edition
npm run tauri:dev:teams      # Teams edition (needs a local server)
```

First run takes a few minutes while Rust compiles dependencies. See [Build from source](https://2lukewil.github.io/snipdesk/build) for prerequisites, edition flags, and the local snipdesk-server loop.

## Repository layout

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
├── extension/                # Browser client: Chrome MV3 extension (self-contained, own build)
├── scripts/                  # Build & release helpers
├── docs/                     # Markdown source for the docs site (vitepress)
└── Cargo.toml                # Workspace root
```

The frontend calls Rust via `invoke("command_name", args)`; all file I/O, SQLite, clipboard, hotkey registration, and key simulation live in Rust. The Teams server is a separate binary (`snipdesk-server`) that you'd host yourself.

## License

MIT. See [LICENSE](LICENSE).
