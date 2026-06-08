# Installer asset drop-zone

Place the per-customer NSIS installer chrome here. Each file is
optional - declared-but-missing files fall back to the project
default at build time.

| Filename        | Format        | Standard dims |
| --------------- | ------------- | ------------- |
| `header.bmp`    | 24-bit BMP    | 150 x 57      |
| `sidebar.bmp`   | 24-bit BMP    | 164 x 314     |
| `installer.ico` | .ico (multi-res) | n/a        |
| `license.rtf`   | plain text or RTF | n/a       |

NSIS rejects PNG/JPG for the bitmap fields; convert to 24-bit BMP
first.
