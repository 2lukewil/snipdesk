# Brand bundle template

A starting layout for a per-customer build. Copy this directory to
a sibling under `brands/` (or anywhere on disk), edit the values,
drop in the actual asset files, then build with `--whitelabel=<slug>`.

```
brands/
├── _template/      <- this, tracked as the reference
└── <your-slug>/    <- gitignored; copy of this with real values
```

## Bootstrap a new bundle

```bash
# bash / git bash
cp -r brands/_template brands/acme
```

```powershell
# PowerShell
Copy-Item -Recurse brands/_template brands/acme
```

Then edit `brands/acme/brand.json`:

- `name` -> display name shown in window title, tray menu, About panel.
- `slug` -> filename-safe identifier used for installer filenames
  (e.g. `Acme-Lite-setup.exe`) and the manifest filename in the
  updater URL.
- `identifier` / `teams_identifier` -> reverse-DNS bundle ids the
  OS treats as unique. Lite + Teams differ so both can be
  installed side-by-side.
- `updater_url` / `teams_updater_url` -> where the customer's
  installation polls for updates. Default convention is a
  per-customer manifest filename inside this project's GitHub
  releases (`snipdesk-<slug>-update.json`).
- `deep_link_scheme` -> custom URL scheme for OIDC callback. Must
  also be registered on the OAuth provider side (e.g. Google
  Cloud Console authorised redirect URIs).
- `server_url` -> default snipdesk-server URL pre-filled in
  Settings. End users can override.
- `sso_only` -> when true, hides username/password sign-in in the
  desktop client. End users can override in Settings.
- `installer.*` -> bare filenames inside `installer-assets/`.
  Each field is independent: declare only the ones you want to
  override; missing entries (or declared-but-missing files) fall
  back to the project's defaults in `src-tauri/installer-defaults/`.

## Assets

Replace the placeholder PNG and drop your installer chrome into
`installer-assets/`:

| File                     | Format                              | Standard dims |
| ------------------------ | ----------------------------------- | ------------- |
| `icon.png`               | PNG with transparency               | 1024 x 1024 ideal (Tauri downscales) |
| `installer-assets/header.bmp`    | 24-bit BMP                  | 150 x 57      |
| `installer-assets/sidebar.bmp`   | 24-bit BMP                  | 164 x 314     |
| `installer-assets/installer.ico` | .ico (16/32/48/256 multi-res) | n/a       |
| `installer-assets/license.rtf`   | plain text or RTF            | n/a           |

NSIS rejects PNG/JPG for the bitmap fields; convert to BMP with
e.g. `magick in.png -type truecolor -depth 24 out.bmp`.

## Build locally

```bash
npm run tauri:build -- --whitelabel=acme
npm run tauri:build:teams -- --whitelabel=acme
```

`--whitelabel=<slug>` resolves to `brands/<slug>/brand.json`. The
short alias `--wl=acme` works too, and you can pass a full path
(`--whitelabel=/abs/path/brand.json` or a relative one with a
slash in it) when the bundle lives outside `brands/`. The build
script itself runs vanilla when the flag is omitted.

If you'd rather set the environment variable directly (useful in
CI / scripts), the older form still works:

```powershell
# PowerShell
$env:BRAND_CONFIG = "brands/acme/brand.json"; npm run tauri:build:teams
```

```bash
# bash / zsh
BRAND_CONFIG=brands/acme/brand.json npm run tauri:build:teams
```

Either way, `scripts/brand.mjs` substitutes the brand strings,
runs `tauri icon` against `icon.png` to expand the
platform-specific app-icon set, copies the present installer
assets into `src-tauri/installer-assets/`, JSON-patches
`tauri.conf.json`, runs the build, then restores everything.
`git status` is clean before and after.

## Ship to CI

`scripts/pack-brand.mjs` tars + base64-encodes a bundle directory
and prints both file paths + a cross-platform clipboard one-liner:

```
node scripts/pack-brand.mjs brands/acme
```

The script writes `<slug>-bundle.b64` to your OS temp dir and
shows you exactly what to do next. Paste the contents into a
GitHub repository secret named `BRAND_BUNDLE_WHITELABEL`
(repo Settings -> Secrets and variables -> Actions -> New
repository secret). The clipboard line for your shell:

```powershell
# PowerShell
Get-Content "$env:TEMP\snipdesk-brand-bundles\acme-bundle.b64" -Raw | Set-Clipboard
```

```bash
# macOS:           pbcopy < /tmp/snipdesk-brand-bundles/acme-bundle.b64
# Linux (Wayland): wl-copy < /tmp/snipdesk-brand-bundles/acme-bundle.b64
# Linux (X11):     xclip -selection clipboard < /tmp/snipdesk-brand-bundles/acme-bundle.b64
```

Once the secret is in place, every subsequent tag push triggers:

- **Desktop** (on a `v*` tag): customer-branded Lite + Teams
  installers + signed update manifests alongside the vanilla
  artifacts in the same GitHub release.
- **Server** (on a `server-v*` tag): a per-customer Docker image
  at `ghcr.io/2lukewil/snipdesk/snipdesk-server-<slug>:<version>`
  + `:latest` with the brand name + OIDC scheme baked in as env
  vars. Customer's `docker-compose.yml` pulls that image and
  never has to think about brand config; a routine
  `docker compose pull && up -d` keeps the brand intact because
  the env lives on the image.

Updating the customer is the same loop: edit files here, re-run
`pack-brand.mjs`, paste over the existing secret value. Next tag
push rebuilds everything with the new values.

### One-liner if you don't want the script

If you ever need to pack manually (no Node available, debugging,
etc.):

```powershell
# PowerShell - tar + base64 of a brand bundle, copied to clipboard
tar -czf bundle.tgz -C brands/acme .
[Convert]::ToBase64String([IO.File]::ReadAllBytes("bundle.tgz")) | Set-Clipboard
```

```bash
# bash + Linux clipboard tool (wl-copy for Wayland, xclip for X11)
tar -czf bundle.tgz -C brands/acme .
base64 -w0 bundle.tgz | wl-copy
```

The script is just a friendlier wrapper around the same two steps.
