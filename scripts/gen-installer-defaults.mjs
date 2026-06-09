#!/usr/bin/env node
//
// Regenerate the default NSIS installer chrome bitmaps for the
// project. Writes:
//   src-tauri/installer-defaults/header.bmp   (150 x 57, accent blue)
//   src-tauri/installer-defaults/sidebar.bmp  (164 x 314, vertical gradient)
//
// Both are 24-bit uncompressed BMPs (NSIS won't accept PNG/JPG for
// the chrome bitmap fields). The installer.ico and license.rtf
// alongside them are static assets - this script only handles the
// two bitmaps because they're the ones easiest to author
// procedurally without an image-editing tool.
//
// Run when you want to tweak the default look:
//   node scripts/gen-installer-defaults.mjs
//
// Customer whitelabel builds override these per-asset via their
// brand bundle's installer.* fields; the assets this script
// produces are the project's stock fallback (see
// src-tauri/installer-defaults/README.md for the full picture).

import { writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

// `import.meta.dirname` is the Node 20.11+ canonical replacement for the
// older `dirname(fileURLToPath(import.meta.url))` polyfill.
const repoRoot = resolve(import.meta.dirname, "..");
const outDir = join(repoRoot, "src-tauri", "installer-defaults");

// Project palette. Same accent the dashboard CSS uses (var(--accent)
// resolves to #4c9aff in the dark theme); dark navy matches the
// dark-theme background. Subtle gradient between them on the sidebar
// keeps the bitmap looking deliberate without locking in any wordmark.
const ACCENT = [0x4c, 0x9a, 0xff]; // R G B
const DARK = [0x1a, 0x1c, 0x20];

// ---- BMP encoding ----
//
// 24-bit uncompressed BMP layout:
//   14-byte file header (magic + size + offset)
//   40-byte DIB header (BITMAPINFOHEADER)
//   pixel data: bottom-up rows, BGR order, each row padded to a
//               4-byte boundary

function bmpHeaders(width, height, pixelDataSize) {
  const buf = Buffer.alloc(14 + 40);
  buf.write("BM", 0);
  buf.writeUInt32LE(14 + 40 + pixelDataSize, 2);
  buf.writeUInt32LE(0, 6); // reserved
  buf.writeUInt32LE(14 + 40, 10); // pixel data offset
  buf.writeUInt32LE(40, 14); // DIB header size
  buf.writeInt32LE(width, 18);
  buf.writeInt32LE(height, 22); // positive = bottom-up
  buf.writeUInt16LE(1, 26); // planes
  buf.writeUInt16LE(24, 28); // bits per pixel
  buf.writeUInt32LE(0, 30); // compression (BI_RGB)
  buf.writeUInt32LE(pixelDataSize, 34);
  buf.writeInt32LE(2835, 38); // ~72 DPI horizontal
  buf.writeInt32LE(2835, 42); // ~72 DPI vertical
  buf.writeUInt32LE(0, 46); // colors in palette
  buf.writeUInt32LE(0, 50); // important colors
  return buf;
}

function writeBmp(path, width, height, pixelAt) {
  const rowStride = Math.ceil((width * 3) / 4) * 4;
  const pixels = Buffer.alloc(rowStride * height);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      // BMP rows are bottom-up: y=0 in the file is the bottom row
      // of the image. Pass the visual y (top-down) to pixelAt so
      // the callback writes "natural" gradients.
      const visualY = height - 1 - y;
      const [r, g, b] = pixelAt(x, visualY);
      const off = y * rowStride + x * 3;
      pixels[off] = b;
      pixels[off + 1] = g;
      pixels[off + 2] = r;
    }
  }
  writeFileSync(path, Buffer.concat([bmpHeaders(width, height, pixels.length), pixels]));
}

function lerp(a, b, t) {
  return Math.round(a + (b - a) * t);
}

// ---- header.bmp: 150x57 solid accent ----
// NSIS draws this in the top-right of the installer chrome on most
// page styles. Solid accent reads cleanly; any text would render
// at ~57px tall which is too cramped to look polished.
const headerPath = join(outDir, "header.bmp");
writeBmp(headerPath, 150, 57, () => ACCENT);
console.log(`wrote ${headerPath}`);

// ---- sidebar.bmp: 164x314 vertical gradient ----
// Shown on the Welcome + Finish pages. Vertical gradient from
// accent (top) to dark (bottom) reads as a quietly branded panel
// without committing to a specific wordmark - which keeps the
// asset reusable across the project's editions.
const sidebarPath = join(outDir, "sidebar.bmp");
writeBmp(sidebarPath, 164, 314, (_x, y) => {
  const t = y / (314 - 1); // 0 at top, 1 at bottom
  return [
    lerp(ACCENT[0], DARK[0], t),
    lerp(ACCENT[1], DARK[1], t),
    lerp(ACCENT[2], DARK[2], t),
  ];
});
console.log(`wrote ${sidebarPath}`);
