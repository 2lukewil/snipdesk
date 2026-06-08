# Default NSIS installer chrome

Drop the project's default Windows-installer assets in this
directory and reference them from `src-tauri/tauri.conf.json`'s
`bundle.windows.nsis` block. They ship with every stock build and
act as the per-field fallback when a whitelabel build doesn't
provide its own override.

## Expected filenames + formats

| File           | Format         | Standard dimensions |
| -------------- | -------------- | ------------------- |
| `header.bmp`   | 24-bit BMP     | 150 x 57            |
| `sidebar.bmp` | 24-bit BMP     | 164 x 314           |
| `installer.ico`| .ico (16/32/48/256 multi-res ideal) | n/a |
| `license.rtf` | plain text or RTF | n/a              |

NSIS literally rejects PNG/JPG for the bitmap fields; convert to
24-bit BMP first (`magick in.png -type truecolor -depth 24 out.bmp`
or similar). The `.ico` should bundle multiple resolutions so
SmartScreen and Explorer render crisply at any size.

## Wiring

Add a `bundle.windows.nsis` block to `src-tauri/tauri.conf.json`
inside `bundle`:

```json
"bundle": {
  "...": "(existing fields)",
  "windows": {
    "nsis": {
      "headerImage":   "installer-defaults/header.bmp",
      "sidebarImage":  "installer-defaults/sidebar.bmp",
      "installerIcon": "installer-defaults/installer.ico",
      "license":       "installer-defaults/license.rtf"
    }
  }
}
```

The paths are resolved by Tauri relative to `src-tauri/`. Omit any
field you don't want to override; Tauri's built-in NSIS default
fills in for it.

## Interaction with whitelabel builds

`scripts/brand.mjs` (for whitelabel builds) only patches the
fields a customer's `brand.json` explicitly provides an
`installer.<field>` entry for AND whose file is actually present in
their bundle. Any other field keeps its tracked value above. So
the matrix is:

- whitelabel declares + ships the asset: their file wins
- whitelabel declares but file missing: warning + the default
  above wins
- whitelabel silent on that field: the default above wins
- no default wired here either: Tauri's built-in fallback applies

This means a whitelabel can selectively rebrand (e.g., just the
sidebar) without having to ship a full asset set.
