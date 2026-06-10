# Building from source

Everything you need to take a fresh clone to a working installer
(or to a hot-reload dev loop). For the release pipeline that
auto-publishes signed installers on a tag push, see
[Releases & auto-update](/auto-update). For the docs site
itself, see the bottom of this page.

## Prerequisites

- **Rust** (stable). Install via [rustup](https://rustup.rs/); the
  pinned toolchain installs automatically from `rust-toolchain.toml`.
- **Node.js 20+**. CI builds on Node 24; anything 20 or newer works
  locally.
- **Platform dependencies**:
  - **Windows**: MSVC C++ Build Tools and WebView2. WebView2 is
    preinstalled on Windows 11. `scripts/build-windows.ps1` installs
    both via winget if missing.
  - **macOS**: Xcode Command Line Tools (`xcode-select --install`).
  - **Linux**: `webkit2gtk-4.1`, `libgtk-3-dev`,
    `libayatana-appindicator3-dev`, `librsvg2-dev`, `libssl-dev`. The
    full list is at [tauri.app/start/prerequisites/](https://tauri.app/start/prerequisites/).

## Develop the desktop app

### Lite (default, offline)

```bash
npm install
npm run tauri:dev
```

First run takes a few minutes while Rust compiles dependencies;
later runs only rebuild what changed.

If `tauri:dev` fails on first run, the most common causes are
missing platform dependencies. See
[Tauri prerequisites](https://tauri.app/start/prerequisites/) for
the canonical platform-by-platform list. On Windows, the usual
culprit is WebView2 (preinstalled on Windows 11); on Linux, the
GTK/WebKit packages above.

### Teams

The Teams build needs the cargo feature plus the Teams Tauri config;
one npm script bundles both:

```bash
npm run tauri:dev:teams
```

You'll need a snipdesk-server running locally (see below) to actually
sign in.

## Build installers

```bash
npm run tauri:build            # Lite edition (free, offline)
npm run tauri:build:teams      # Teams edition
```

Output lands in `target/release/bundle/` (the workspace `target/` is
at the repo root): `.msi` / `.exe` on Windows, `.app` / `.dmg` on
macOS, `.deb` / `.rpm` / `.AppImage` on Linux.

On Windows you can also run `scripts/build-windows.ps1` from an
elevated PowerShell to install prerequisites and build in one step.

## Editions

The same source tree produces both editions; which one you get is a
build-time flag:

- **Lite (default)** is fully offline. Feature-gated network code
  never reaches the compiler, and the Team Library UI is stripped
  from the bundle.
- **Teams (`--features teams`)** adds an HTTPS shared-library sync,
  a Settings tab, and a background sync thread.

Teams-specific config (product name, identifier, updater endpoint)
lives in `src-tauri/tauri.teams.conf.json`.

The offline edition pulls in no team-sync networking code. To verify
the invariant:

```bash
cargo tree --manifest-path src-tauri/Cargo.toml --no-default-features
```

`ureq` and `snipdesk-teams` should both be absent. (The auto-updater
is the one intentional outbound connection. It polls the GitHub
releases manifest on launch via `tauri-plugin-updater`.)

## Run snipdesk-server locally

The Teams desktop client needs a running server to sign in to. For
production deploys see [Docker quickstart](/docker-quickstart) and
[Production deployment](/deploy). The five-line local dev loop:

```bash
cd crates/snipdesk-server
cargo run -p snipdesk-server -- gen-key         # save the master key
cargo run -p snipdesk-server -- gen-jwt-secret  # save the JWT secret
cp snipdesk-server.example.toml snipdesk-server.toml   # edit in the two secrets
cargo run -p snipdesk-server -- --config snipdesk-server.toml
```

Visit `http://127.0.0.1:8080/` in a browser for the admin dashboard,
or point a Teams desktop client at `http://127.0.0.1:8080` to sync.

## Bake a default server URL

Deployment builds can carry the organisation's server URL inside
the binary, so end users never see or type it. Set the
`SNIPDESK_DEFAULT_SERVER_URL` environment variable when compiling:

```bash
SNIPDESK_DEFAULT_SERVER_URL=https://snippets.yourcompany.com npm run tauri:build:teams
```

What a baked URL changes in the client:

- The server URL field disappears from Settings and onboarding;
  the configured host is shown as a read-only label instead.
- The baked value is authoritative. On every launch the client
  adopts it over whatever an earlier release persisted, so moving
  the fleet to a new URL is: change the variable, push a release
  tag, and installed clients pick up the new URL through
  auto-update. (Users sign in again after a URL change - the auth
  token is keyed to the server it was issued by.)
- A stock build (variable unset) behaves exactly as before: the
  user types the URL themselves and it persists normally.

In CI, define the variable where your pipeline runs the build:
GitHub Actions env/secret, GitLab CI/CD variable, or a plain
`export` in the job script. Forks that mirror this repo get a
baked-URL build with zero source changes - the URL lives in the
pipeline configuration, not the tree.

A whitelabel brand bundle's `server_url` field does the same thing
and takes precedence over the environment variable when both are
present.

## Whitelabel (per-customer builds)

Building a customer-branded installer or server image is its own flow.
See [Whitelabel brand bundles](/whitelabel) for the full walkthrough.

The short version:

```bash
npm run tauri:build:teams -- --whitelabel=<slug>
```

Whitelabel is Teams-only. A customer brand bundle lives outside the
tracked tree (gitignored under `brands/<slug>/`) and is fed to CI via
a single base64 GitHub Secret. The tracked tree stays brand-neutral.

## Pre-push checks

A pre-push git hook in `.githooks/` runs `cargo fmt --all --check`
and `cargo clippy --workspace --all-targets -- -D warnings` before
your push leaves your machine. The hook is activated on `npm install`
via `scripts/setup-hooks.mjs`; if you ever skip it (`git push
--no-verify`), the CI workflow runs the same checks on the server
side and will fail the build.

## Build this docs site

```bash
npm run docs:dev       # hot-reload preview
npm run docs:build     # static build to docs/.vitepress/dist/
npm run docs:preview   # serve the built site
```

The site is published from `main` by
[`.github/workflows/docs.yml`](https://github.com/2lukewil/snipdesk/blob/main/.github/workflows/docs.yml)
whenever any file under `docs/` changes. The live site is at
[2lukewil.github.io/snipdesk](https://2lukewil.github.io/snipdesk/).
