# Brand bundle template

Reference layout for a per-customer whitelabel build. Copy this directory
to a sibling under `brands/<your-slug>/` (or anywhere on disk), edit
`brand.json`, drop in your icon and installer assets, then pack with
`scripts/pack-brand.mjs` and push to a GitHub Secret.

```
brands/
├── _template/      <- this, tracked as the reference
└── <your-slug>/    <- gitignored; your real bundle
```

**Full walkthrough, per-field reference, and the CI flow:**
<https://2lukewil.github.io/snipdesk/whitelabel>

## Bootstrap

```bash
# bash / git bash
cp -r brands/_template brands/acme
```

```powershell
# PowerShell
Copy-Item -Recurse brands/_template brands/acme
```

## What lives here

| File | Purpose |
| --- | --- |
| `brand.json` | Name, slug, identifier, deep-link scheme, server URL, installer asset filenames. |
| `icon.png` | Source app icon; Tauri downscales into platform-specific sets at build time. |
| `installer-assets/` | NSIS installer chrome (header / sidebar BMPs, installer ICO, license RTF). Each file is optional and falls back to `src-tauri/installer-defaults/`. |

## Build locally

```bash
npm run tauri:build:teams -- --whitelabel=acme
```

Whitelabel ships Teams-only. The vanilla Lite build is unaffected by
brand bundles. See the docs link above for the env-var alternative, the
pack-and-push flow, and the per-field reference for `brand.json`.
