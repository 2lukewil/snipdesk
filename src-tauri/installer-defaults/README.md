# Default NSIS installer chrome

The project's stock Windows-installer assets live here. They ship with
every vanilla build and act as the per-field fallback when a whitelabel
build doesn't provide its own override.

Expected files: `header.bmp`, `sidebar.bmp`, `installer.ico`,
`license.rtf`. Asset specs (formats, dimensions, conversion command)
are documented once on the docs site:
<https://2lukewil.github.io/snipdesk/whitelabel#step-3-drop-in-the-asset-files>

## Wiring into `tauri.conf.json`

Add the relevant fields to `src-tauri/tauri.conf.json`. Header, sidebar,
and installer icon live under `bundle.windows.nsis`; the license file is
a top-level `bundle.licenseFile` (Tauri's NSIS bundler picks it up from
there, and the same field covers any other installer format we might
add later).

```json
"bundle": {
  "...": "(existing fields)",
  "windows": {
    "nsis": {
      "headerImage": "installer-defaults/header.bmp",
      "sidebarImage": "installer-defaults/sidebar.bmp",
      "installerIcon": "installer-defaults/installer.ico"
    }
  },
  "licenseFile": "installer-defaults/license.rtf"
}
```

Paths are resolved by Tauri relative to `src-tauri/`. Omit any field
you don't want to override; Tauri's built-in NSIS default fills in for it.

## Interaction with whitelabel builds

`scripts/brand.mjs` (for whitelabel builds) only patches the fields a
customer's `brand.json` explicitly provides an `installer.<field>` entry
for AND whose file is actually present in their bundle. Any other field
keeps its tracked value above. The matrix:

- Whitelabel declares + ships the asset: their file wins.
- Whitelabel declares but file missing: warning + the default above wins.
- Whitelabel silent on that field: the default above wins.
- No default wired here either: Tauri's built-in fallback applies.

So a whitelabel can selectively rebrand (e.g. just the sidebar) without
having to ship a full asset set.
