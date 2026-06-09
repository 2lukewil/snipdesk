# SnipDesk

A fast, searchable snippet launcher for support agents. Hit a global hotkey, type a few characters, press Enter - the canned reply gets pasted into whatever window you were just using.

Built with Tauri (Rust + web UI): a small (~5-10 MB), native binary for Windows, macOS, and Linux that starts instantly and runs from the system tray. Ships in two editions: **Lite** (free, offline) and **Teams** (server-backed sync + shared library + SSO + admin dashboard).

> ## Documentation
>
> Full, searchable docs live at **<https://2lukewil.github.io/snipdesk/>** (source: [`docs/`](docs/)).
>
> - **End users:** [Getting started](https://2lukewil.github.io/snipdesk/getting-started)
> - **Self-hosters:** [Docker quickstart](https://2lukewil.github.io/snipdesk/docker-quickstart) | [Production deploy](https://2lukewil.github.io/snipdesk/deploy)
> - **Developers:** [Build from source](https://2lukewil.github.io/snipdesk/build) | [Whitelabel](https://2lukewil.github.io/snipdesk/whitelabel) | [Releases](https://2lukewil.github.io/snipdesk/auto-update)

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
├── scripts/                  # Build & release helpers
├── docs/                     # Markdown source for the docs site (vitepress)
└── Cargo.toml                # Workspace root
```

The frontend calls Rust via `invoke("command_name", args)`; all file I/O, SQLite, clipboard, hotkey registration, and key simulation live in Rust. The Teams server is a separate binary (`snipdesk-server`) that you'd host yourself.

## License

MIT. See [LICENSE](LICENSE).
