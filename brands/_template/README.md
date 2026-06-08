# Brand bundle template

A starting layout for a per-customer build. Copy this directory to
a sibling under `brands/` (or anywhere on disk), edit the values,
drop in the actual asset files, then point `BRAND_CONFIG` at the
new `brand.json` when you build.

```
brands/
├── _template/      <- this, tracked as the reference
└── <your-slug>/    <- gitignored; copy of this with real values
```

## Bootstrap a new bundle

```bash
cp -r brands/_template brands/acme
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
BRAND_CONFIG=brands/acme/brand.json npm run tauri:build
BRAND_CONFIG=brands/acme/brand.json npm run tauri:build:teams
```

`scripts/brand.mjs` substitutes the brand strings, runs
`tauri icon` against `icon.png` to expand the platform-specific
app-icon set, copies the present installer assets into
`src-tauri/installer-assets/`, JSON-patches `tauri.conf.json`,
runs the build, then restores everything. `git status` is clean
before and after.

## Ship to CI

Pack the bundle and paste the base64 into the GitHub Secret
`BRAND_BUNDLE_WHITELABEL`:

```bash
tar -czf bundle.tgz -C brands/acme .
base64 -w0 bundle.tgz | wl-copy   # or write to a file and paste
```

The release workflow auto-detects the secret on the next tag push
and produces the customer's installers + signed update manifests
alongside the vanilla artifacts in the same GitHub release.

Updating the customer = edit files here, re-tar + re-base64,
paste over the secret value.
