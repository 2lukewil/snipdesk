# Whitelabel: brand-rebuilt installers and server images

SnipDesk ships a per-customer whitelabel pipeline: the same tracked source
tree rebuilds with a different display name, icon, installer chrome, deep-link
scheme, OIDC config, and server image without forking the repo. Customer
brand bundles live **outside** the tracked tree (gitignored under `brands/`)
and are fed to CI via a single base64-encoded GitHub Secret.

This page is the conceptual overview. The exhaustive field reference lives
next to the template files in
[`brands/_template/README.md`](../brands/_template/README.md), and the
packing script (`scripts/pack-brand.mjs`) prints its own usage hints.

## What gets rebranded

- **Client (desktop app)**
  - Window title, tray menu, About panel display name
  - App icon (Windows ICO / macOS ICNS / Linux PNGs)
  - NSIS installer header bitmap, sidebar bitmap, installer icon, license RTF
  - Tauri bundle identifier (`com.<customer>.snippets.teams`)
  - Deep-link scheme used for OIDC callbacks
  - Pre-filled server URL in Settings
  - Optional SSO-only mode (hides password sign-in)
  - Auto-update manifest URL

- **Server (Docker image)**
  - Display name baked into the admin dashboard, sign-in flows, etc.
  - OIDC allowed-scheme list (so the brand's deep link survives the round trip)
  - Image tag: `ghcr.io/2lukewil/snipdesk/snipdesk-server-<slug>:<version>`

Whitelabel is Teams-only. Customer deployments always pair the desktop
client with a snipdesk-server, so a per-customer Lite (offline-only) build
would be CI minutes spent on a binary nobody installs. The upstream Lite
build is unaffected.

## The flow at a glance

```
brands/_template/         (tracked; reference layout)
        |
        |  cp -r to a gitignored sibling
        v
brands/<customer>/        (gitignored; your real bundle)
   - brand.json           (display name, slug, scheme, server URL, ...)
   - icon.png             (source icon; Tauri downscales)
   - installer-assets/    (NSIS chrome, ICO, license)
        |
        |  node scripts/pack-brand.mjs brands/<customer>
        v
<slug>-bundle.b64         (in your OS temp dir, copied to clipboard)
        |
        |  paste into repo Secret named BRAND_BUNDLE_<SLUG>
        v
GitHub Actions
        |
        +-- on `v*` tag    -> branded Teams installer alongside vanilla artifacts
        +-- on `server-v*` -> branded Docker image with brand env baked in
```

## Step-by-step

### 1. Bootstrap a bundle from the template

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

### 2. Edit `brand.json`

Open `brands/acme/brand.json`. The
[brand template README](../brands/_template/README.md) documents every field;
the must-fills are:

- `name`: display name (window title, tray menu, etc.)
- `slug`: filename-safe identifier (becomes `<slug>` in artifact filenames)
- `teams_identifier`: reverse-DNS bundle id, e.g. `com.acme.snippets.teams`
- `deep_link_scheme`: OIDC callback URL scheme (must also be allowed on the
  server side and registered with your OAuth provider)
- `server_url`: pre-filled server URL in Settings (end users can override)
- `teams_updater_url`: per-customer manifest filename inside your GitHub releases

### 3. Drop in the asset files

Replace `icon.png` and the files under `installer-assets/`. NSIS rejects
PNG/JPG for the installer bitmaps. Convert with:

```
magick in.png -type truecolor -depth 24 out.bmp
```

Every installer asset is optional. If you skip a file, the bundled defaults
in `src-tauri/installer-defaults/` are used instead.

### 4. Pack and push to a Secret

```
node scripts/pack-brand.mjs brands/acme
```

The script tars + base64-encodes the bundle, writes the output to your OS
temp dir, and prints a one-line shell command to copy it to your clipboard.
Paste the result into a repository Secret named (by convention)
`BRAND_BUNDLE_<UPPERCASE_SLUG>`. Repo Settings -> Secrets and variables ->
Actions -> New repository secret.

### 5. Tag a release

```
git tag v1.2.3                # desktop client release
git push --tags

git tag server-v0.2.0         # server image release
git push --tags
```

Tag pushes pick up the secret automatically. The branded artifacts ship in
the same GitHub release as the vanilla Lite + Teams installers (desktop)
or land on GHCR (server). Subsequent updates are the same loop: edit, re-pack,
paste over the secret value, tag.

### Build a branded client locally

```bash
npm run tauri:build:teams -- --whitelabel=acme
```

The short alias `--wl=acme` works too. A full path (`--whitelabel=/abs/path/brand.json`)
is accepted when the bundle lives outside `brands/`. Omit the flag for a
vanilla build.

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

The tracked tree never contains a real customer's brand assets, icons, names,
or server URL. The invariant:

- `brands/_template/` is tracked as a reference layout.
- `brands/*` is gitignored (the `!brands/_template/` exception keeps the template).
- The brand bundle is fed to CI through `BRAND_BUNDLE_<SLUG>` secrets.
- `git status` is clean before and after a local branded build.

If you ever see a real customer name or icon in `git status` output, the
`scripts/brand.mjs` restore phase didn't finish. Re-run the build to clean
up, or `git checkout -- src-tauri/tauri.conf.json src-tauri/icons/`.

## Where the source lives

- Brand template + per-field reference: [`brands/_template/README.md`](../brands/_template/README.md)
- Build orchestrator: [`scripts/brand.mjs`](../scripts/brand.mjs)
- Packer (tar + base64): [`scripts/pack-brand.mjs`](../scripts/pack-brand.mjs)
- Server brand env vars baked at image build: [`Dockerfile`](../Dockerfile)
- CI matrix: [`.github/workflows/release.yml`](../.github/workflows/release.yml),
  [`.github/workflows/release-server.yml`](../.github/workflows/release-server.yml)
