# Whitelabel: brand-rebuilt installers and server images

SnipDesk ships a per-customer whitelabel pipeline: the same tracked source
tree rebuilds with a different display name, icon, installer chrome,
deep-link scheme, OIDC config, and server image without forking the repo.
Customer brand bundles live **outside** the tracked tree (gitignored under
`brands/`) and are fed to CI via a single base64-encoded GitHub Secret.

## What gets rebranded

**Client (desktop app)**
- Window title, tray menu, About panel display name
- App icon (Windows ICO / macOS ICNS / Linux PNGs)
- NSIS installer header bitmap, sidebar bitmap, installer icon, license RTF
- Tauri bundle identifier (`com.<customer>.snippets.teams`)
- Deep-link scheme used for OIDC callbacks
- Pre-filled server URL in Settings
- Optional SSO-only mode (hides password sign-in)
- Auto-update manifest URL

**Server (Docker image)**
- Display name baked into the admin dashboard, sign-in flows, etc.
- OIDC allowed-scheme list (so the brand's deep link survives the round trip)
- Image tag: `ghcr.io/2lukewil/snipdesk/snipdesk-server-<slug>:<version>`

Whitelabel is Teams-only. Customer deployments always pair the desktop
client with a snipdesk-server, so a per-customer Lite (offline-only) build
would be CI minutes spent on a binary nobody installs. The upstream Lite
build is unaffected.

## The flow at a glance

```
brands/_template/  -- copy to a gitignored sibling --> brands/<customer>/
brands/<customer>/ -- node scripts/pack-brand.mjs --> <slug>-bundle.b64
<slug>-bundle.b64  -- paste into repo Secret BRAND_BUNDLE_<SLUG>
git tag v1.2.3     -- triggers branded Teams installer alongside vanilla
git tag server-v0.2.0 -- triggers branded Docker image on GHCR
```

The five steps in detail below.

## Step 1. Bootstrap a bundle from the template

```bash
# bash / git bash
cp -r brands/_template brands/acme
```

```powershell
# PowerShell
Copy-Item -Recurse brands/_template brands/acme
```

Everything under `brands/` except `_template/` is gitignored, so your
customer-specific bundle stays out of the repo automatically.

## Step 2. Edit `brand.json`

Open `brands/<customer>/brand.json`. Every field, in detail:

| Field | Required | What it does |
| --- | --- | --- |
| `name` | yes | Display name shown in window title, tray menu, About panel. |
| `slug` | yes | Filename-safe identifier used for the installer filename (e.g. `Acme-Teams-setup.exe`) and the manifest filename in the updater URL. |
| `teams_identifier` | yes | Reverse-DNS bundle id the OS treats as unique (e.g. `com.acme.snippets.teams`). |
| `identifier` | no | Optional Lite-edition bundle id. Ignored by the release pipeline but accepted for local experimentation. |
| `icon_source` | yes | Filename of the source PNG inside the bundle, typically `icon.png`. Tauri downscales it into platform-specific icon sets at build time. |
| `teams_updater_url` | yes | Where the customer's installation polls for updates. Convention is a per-customer manifest filename inside this project's GitHub releases (e.g. `snipdesk-acme-teams-update.json`). |
| `deep_link_scheme` | yes | Custom URL scheme for the OIDC callback (e.g. `acme`). Must also be registered on the OAuth provider side (e.g. Google Cloud Console authorised redirect URIs) and on the server side via `[oidc].allowed_deep_link_schemes`. |
| `server_url` | yes | Default snipdesk-server URL pre-filled in Settings. End users can override. |
| `sso_only` | no | When `true`, hides username/password sign-in in the desktop client. End users can override in Settings. Defaults to `false`. |
| `installer.header_image` | no | Bare filename inside `installer-assets/` for the NSIS header image. |
| `installer.sidebar_image` | no | Bare filename inside `installer-assets/` for the NSIS sidebar image. |
| `installer.installer_icon` | no | Bare filename inside `installer-assets/` for the NSIS installer `.ico`. |
| `installer.license_file` | no | Bare filename inside `installer-assets/` for the license text shown by the installer. |

Each `installer.*` entry is independent: declare only the ones you want
to override. Missing entries (or declared-but-missing files) fall back to
the project defaults in `src-tauri/installer-defaults/`, so a customer can
ship just a sidebar override without the full set.

A complete example:

```json
{
  "name": "Acme Snippets",
  "slug": "acme",
  "teams_identifier": "com.acme.snippets.teams",
  "icon_source": "icon.png",
  "teams_updater_url": "https://github.com/2lukewil/snipdesk/releases/latest/download/snipdesk-acme-teams-update.json",
  "deep_link_scheme": "acme",
  "server_url": "https://snippets.acme.com",
  "sso_only": false,
  "installer": {
    "header_image": "header.bmp",
    "sidebar_image": "sidebar.bmp",
    "installer_icon": "installer.ico",
    "license_file": "license.rtf"
  }
}
```

## Step 3. Drop in the asset files

Replace `icon.png` and any files under `installer-assets/`:

| File | Format | Standard dimensions |
| --- | --- | --- |
| `icon.png` | PNG with transparency | 1024 x 1024 ideal (Tauri downscales) |
| `installer-assets/header.bmp` | 24-bit BMP | 150 x 57 |
| `installer-assets/sidebar.bmp` | 24-bit BMP | 164 x 314 |
| `installer-assets/installer.ico` | `.ico` (16/32/48/256 multi-res) | n/a |
| `installer-assets/license.rtf` | plain text or RTF | n/a |

NSIS rejects PNG/JPG for the bitmap fields. Convert with:

```
magick in.png -type truecolor -depth 24 out.bmp
```

The `.ico` should bundle multiple resolutions so Windows SmartScreen and
Explorer render crisply at any size.

Every installer asset is optional. If you skip a file, the bundled
defaults in `src-tauri/installer-defaults/` are used instead.

## Step 4. Pack and push to a Secret

```
node scripts/pack-brand.mjs brands/acme
```

The script tars + base64-encodes the bundle, writes the output to your OS
temp dir, and prints a one-line shell command to copy it to your clipboard.
Paste the result into a repository Secret named (by convention)
`BRAND_BUNDLE_<UPPERCASE_SLUG>`. Repo Settings -> Secrets and variables ->
Actions -> New repository secret.

The script's clipboard one-liner for each shell:

```powershell
# PowerShell
Get-Content "$env:TEMP\snipdesk-brand-bundles\acme-bundle.b64" -Raw | Set-Clipboard
```

```bash
# macOS:           pbcopy < /tmp/snipdesk-brand-bundles/acme-bundle.b64
# Linux (Wayland): wl-copy < /tmp/snipdesk-brand-bundles/acme-bundle.b64
# Linux (X11):     xclip -selection clipboard < /tmp/snipdesk-brand-bundles/acme-bundle.b64
```

### Manual pack (no Node available)

If you ever need to pack without the script (debugging, fresh box, etc.):

```powershell
# PowerShell
tar -czf bundle.tgz -C brands/acme .
[Convert]::ToBase64String([IO.File]::ReadAllBytes("bundle.tgz")) | Set-Clipboard
```

```bash
# bash + Linux clipboard tool (wl-copy for Wayland, xclip for X11)
tar -czf bundle.tgz -C brands/acme .
base64 -w0 bundle.tgz | wl-copy
```

The script is just a friendlier wrapper around those two steps.

## Step 5. Tag a release

```
git tag v1.2.3                # desktop client release
git push --tags

git tag server-v0.2.0         # server image release
git push --tags
```

Tag pushes pick up the secret automatically. The branded artifacts ship
in the same GitHub release as the vanilla Lite + Teams installers
(desktop) or land on GHCR (server). The server image bakes the brand
name and OIDC scheme in as environment variables, so a customer's
`docker compose pull && up -d` keeps the brand intact without any
config change on their side.

## Build a branded client locally

```bash
npm run tauri:build:teams -- --whitelabel=acme
```

`--whitelabel=<slug>` resolves to `brands/<slug>/brand.json`. The short
alias `--wl=acme` works too. A full path
(`--whitelabel=/abs/path/brand.json`, or a relative one with a slash in
it) is accepted when the bundle lives outside `brands/`. Omit the flag
for a vanilla build.

The Lite wrapper (`npm run tauri:build`) also accepts the flag for local
experimentation, but the CI release pipeline never builds Lite customer
installers. If you build one locally you'll get a warning suggesting the
Teams path; silence it with `BRAND_LITE_OK=1` if you genuinely want a
Lite customer build for testing.

### Env-var alternative

The CLI flag is the modern entry point; the older env-var form still
works (useful in CI / scripts):

```powershell
# PowerShell
$env:BRAND_CONFIG = "brands/acme/brand.json"; npm run tauri:build:teams
```

```bash
# bash / zsh
BRAND_CONFIG=brands/acme/brand.json npm run tauri:build:teams
```

Either form, the underlying `scripts/brand.mjs` substitutes the brand
strings, runs `tauri icon` against `icon.png` to expand the
platform-specific app-icon set, copies the present installer assets into
`src-tauri/installer-assets/`, JSON-patches `tauri.conf.json`, runs the
build, then restores everything. `git status` is clean before and after.

## Updating an existing customer

Same loop. The brand bundle and the GitHub Secret are not coupled to a
version, so:

1. Edit `brands/<customer>/` on disk.
2. Re-run `node scripts/pack-brand.mjs brands/<customer>`.
3. Paste the new base64 over the existing secret value (GitHub Settings ->
   Secrets and variables -> Actions -> `BRAND_BUNDLE_<SLUG>` -> Update).
4. Next tag push rebuilds with the new values.

No git diff, no PR, no release notes referencing the customer name. The
upstream tree stays brand-neutral.

## What stays out of the repo

The tracked tree never contains a real customer's brand assets, icons,
names, or server URL. The invariants:

- `brands/_template/` is tracked as a reference layout.
- `brands/*` is gitignored (the `!brands/_template/` exception keeps the
  template visible to new contributors).
- The brand bundle is fed to CI through `BRAND_BUNDLE_<SLUG>` secrets.
- `git status` is clean before and after a local branded build.

If you ever see a real customer name or icon in `git status` output, the
`scripts/brand.mjs` restore phase didn't finish. Re-run the build to
clean up, or `git checkout -- src-tauri/tauri.conf.json src-tauri/icons/`.

## Where the source lives

- Brand template (the only tracked example): [`brands/_template/`](../brands/_template/)
- Build orchestrator: [`scripts/brand.mjs`](../scripts/brand.mjs)
- Packer (tar + base64): [`scripts/pack-brand.mjs`](../scripts/pack-brand.mjs)
- Server brand env vars baked at image build: [`Dockerfile`](../Dockerfile)
- CI matrix: [`.github/workflows/release.yml`](../.github/workflows/release.yml),
  [`.github/workflows/release-server.yml`](../.github/workflows/release-server.yml)
