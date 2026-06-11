# Auto-update and release flow

How tagged releases ship to running installations.

`.github/workflows/release.yml` fires on a `v*` tag push, builds
and signs both editions, generates update manifests, and publishes
a GitHub release. Every running SnipDesk instance polls the manifest
on next launch and offers an in-app install.

## The flow at a glance

```
git tag v1.0.1 && git push --tags
     │
     ▼
GitHub Actions (release.yml on tag push)
     │
     ├─ npm ci, npm run build
     ├─ tauri build (Lite, signed via env-var key)
     ├─ tauri build --features teams (Teams, signed)
     ├─ scripts/generate-manifest.ps1 (emits two manifest JSONs)
     └─ softprops/action-gh-release publishes the tag with all six files
            │
            ▼
GitHub Releases (marked Latest by default)
            │
            ▼
Every running SnipDesk instance, on next launch:
  - Polls releases/latest/download/snipdesk-update.json (or the Teams manifest)
  - Newer version available -> toast: "v1.0.1 available | Install / Later"
  - User clicks Install -> download, verify Ed25519 signature, install, relaunch
```

End-to-end: roughly three to five minutes from `git push --tags` to
the release being live. Clients pick it up on their next launch.

## How the signature works

When the workflow builds with `TAURI_SIGNING_PRIVATE_KEY` set,
Tauri's bundler signs each NSIS installer with an Ed25519 private
key, producing a `<installer>.sig` file alongside it.
`generate-manifest.ps1` reads each `.sig` and splices it into a
manifest JSON pointing at the installer's GitHub Releases URL.

When a client polls the manifest, it parses the version + URL +
signature, downloads the installer, streams the bytes through an
Ed25519 verifier using the public key baked into the binary at
compile time (in `tauri.conf.json` at `plugins.updater.pubkey`),
and rejects the file if the signature doesn't match. Tampered
bundles never install.

The signature is independent of Authenticode / SmartScreen.
SmartScreen warnings on first install are a separate Windows trust
concern handled by buying an Authenticode certificate; it does not
block the auto-update chain.

## One-time signing-key setup

Once per repository. After this, every release is `git tag && git push`.

### 1. Generate the Ed25519 keypair

```powershell
npx @tauri-apps/cli signer generate -w $HOME\.snipdesk-update.key
```

Pick a passphrase. The command emits two files:

- `~\.snipdesk-update.key` (private). Encrypted at rest with the
  passphrase. Store in a password manager plus an offline backup.
  Losing this means the auto-update chain breaks: a new keypair
  has to be embedded in a fresh release, and existing installs
  can't auto-upgrade through the gap.
- `~\.snipdesk-update.key.pub` (public). Goes into `tauri.conf.json`.

### 2. Embed the public key

Copy the contents of `~\.snipdesk-update.key.pub` into
`src-tauri/tauri.conf.json` at `plugins.updater.pubkey`, replacing
the `REPLACE_WITH_TAURI_SIGNER_PUBLIC_KEY` placeholder:

```json
"plugins": {
  "updater": {
    "active": true,
    "endpoints": ["https://github.com/2lukewil/snipdesk/releases/latest/download/snipdesk-update.json"],
    "dialog": false,
    "pubkey": "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1Ymxpa..."
  }
}
```

Commit the change. Every build from now on embeds the public key
so the runtime updater can verify signatures.

### 3. Add the private key + passphrase as GitHub secrets

Repo Settings -> Secrets and variables -> Actions -> New repository
secret:

- `TAURI_SIGNING_PRIVATE_KEY`: the contents of
  `~\.snipdesk-update.key` (open in Notepad, copy everything,
  paste). It's already passphrase-encrypted so storing the file
  contents as-is is fine.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: the passphrase chosen during
  generation.

### 4. Smoke-test the workflow

Push a throwaway tag to confirm the pipeline:

```powershell
git tag v0.0.0-test
git push --tags
```

Watch the Actions tab. A green workflow ends with a release at
`https://github.com/2lukewil/snipdesk/releases/tag/v0.0.0-test`.
Delete it once the artifacts look right:

```powershell
gh release delete v0.0.0-test --cleanup-tag
```

## Per-release flow

Three commands per release. All three version files MUST match the
tag exactly. The release workflow runs a CI guard step that fails
the build with an actionable error if any one of them drifts, but
catching it locally is faster than waiting for CI.

```powershell
# 1. Bump the version in three places (in lockstep):
#      src-tauri/Cargo.toml      -> [package].version
#      package.json              -> version
#      src-tauri/tauri.conf.json -> version
#
# Then bump the Cargo lockfile so the version embedded in
# src-tauri's lock entry matches:
cargo update --workspace --offline

# Confirm everything aligns before tagging - all three lines must
# show the same version. (Select-String is the stock-PowerShell
# equivalent of grep; use grep if you're in Git Bash.)
Select-String '^version' src-tauri/Cargo.toml
Select-String '"version"' package.json
Select-String '"version"' src-tauri/tauri.conf.json

# 2. Commit the bump and push:
git add -A
git commit -m "Bump version to 1.0.1"
git push

# 3. Tag and push the tag:
git tag v1.0.1
git push --tags
```

Step 3 fires `.github/workflows/release.yml`. The first step of
that workflow re-checks the same alignment (tag vs. all three
config files); a mismatch fails the build before any binary is
signed. Three to five minutes later (when the versions line up)
the release is live and clients start picking it up on their next
launch.

::: warning Why this matters
The desktop updater compares `CARGO_PKG_VERSION` (baked into the
binary at build time, sourced from `src-tauri/Cargo.toml`) against
the version field in the manifest the workflow generates (sourced
from the tag). If you tag `v1.0.1` without bumping the config
files, the binary still reports `1.0.0`, the manifest says `1.0.1`,
and every install loops: "1.0.1 available -> install -> binary
still reports 1.0.0 -> 1.0.1 available -> ...". The CI guard
catches this; don't bypass it.
:::

## Manual fallback (CI down)

The local equivalent, used when CI is broken and a release has to
ship anyway:

```powershell
cd E:\snipdesk

# One-time: copy .env.example to .env and fill in
# TAURI_SIGNING_PRIVATE_KEY_PATH + TAURI_SIGNING_PRIVATE_KEY_PASSWORD.
# Both npm scripts below pick up .env via scripts/load-env.mjs.

npm run tauri:build -- --bundles nsis
npm run tauri:build:teams -- --bundles nsis

# Teams build self-renames its installer to SnipDesk-Teams-setup.exe.
# Normalize the Lite output to the canonical name generate-manifest.ps1
# expects:
$nsis = "target\release\bundle\nsis"
$lite = Get-ChildItem $nsis -Filter "SnipDesk Lite_*_x64-setup.exe" | Select-Object -First 1
Rename-Item "$($lite.FullName).sig" "SnipDesk-Lite-setup.exe.sig"
Rename-Item $lite.FullName "SnipDesk-Lite-setup.exe"

# Generate manifests:
.\scripts\generate-manifest.ps1 -Version 1.0.1 -Notes "Release notes here"

# Create the release with gh CLI:
gh release create v1.0.1 -t "SnipDesk 1.0.1" --generate-notes `
  target\release\bundle\nsis\SnipDesk-Lite-setup.exe `
  target\release\bundle\nsis\SnipDesk-Lite-setup.exe.sig `
  target\release\bundle\nsis\SnipDesk-Teams-setup.exe `
  target\release\bundle\nsis\SnipDesk-Teams-setup.exe.sig `
  snipdesk-update.json `
  snipdesk-teams-update.json
```

Same outputs as the CI path, produced locally.

## What end users see

With `auto_check_updates` on (the default), every launch silently
fetches the manifest. If the version is newer than the running
binary, a non-blocking toast appears in the status bar:

> **SnipDesk 1.0.1 is available.** Install and restart | Later

Click *Install and restart* and the install runs through the status
bar (download progress, silent install, relaunch) in about thirty
seconds on a normal connection. The installer itself never shows a
window: `plugins.updater.windows.installMode` is set to `quiet` in
`tauri.conf.json`, so the NSIS run is fully invisible and the only
UI the user sees is the client's own status bar.

Click *Later* and the toast clears. The next launch re-prompts.
No nagging within a session.

If the network is unreachable, the check fails silently
(`console.warn` only, no user-facing error). The manual *Check for
updates* button in Settings -> About surfaces errors loudly.

## Windows install path caveats

NSIS is the auto-update target. NSIS installs per-user under
`%LOCALAPPDATA%\Programs\` without admin elevation, so the updater
can replace files silently. **MSI installs** (under `Program Files`)
require admin elevation on every update and are not silently
installable. SnipDesk doesn't ship MSI for auto-update; the
released NSIS installer is what end users should grab.

If a user originally installed via MSI (some IT departments require
it), they need to uninstall and reinstall via the NSIS installer
once to enter the auto-update chain.

## Troubleshooting

**Invalid signature on client.** Either the keypair was rotated
without redeploying the public key in `tauri.conf.json`, or
`TAURI_SIGNING_PRIVATE_KEY` in CI doesn't match the embedded public
key. Regenerate one to match the other.

**Release workflow fails on `tauri build`.** Most common cause: the
`TAURI_SIGNING_PRIVATE_KEY` secret has whitespace at the start or
end (a Notepad CRLF + trailing newline will do this). Re-paste it
with no surrounding whitespace.

**Workflow succeeds but the client never sees the update.** Check
the release was marked "Latest" in the GitHub UI (the workflow does
this automatically; if it landed as a draft or pre-release, the
`releases/latest/download/` URL won't redirect to it). Also confirm
the manifest URL in `tauri.conf.json` matches the repo path.

**Update check fails with "could not fetch a valid release JSON"
right after a server release.** The `releases/latest/download/` URL
follows GitHub's repo-wide Latest badge, which must always sit on a
client release (the one carrying the manifest files). The server
workflow sets `make_latest: false` for exactly this reason; if a
server release ever ends up holding the badge (manual release,
older workflow), move it back with
`gh release edit v<client-version> --latest`.

**Update toast doesn't appear.** Verify `auto_check_updates` is on
in Settings -> General. The updater's `check()` returns `null` when
the manifest version equals or is older than the running binary's,
so confirm the version was actually bumped in all three files
(`src-tauri/Cargo.toml`, `package.json`, `src-tauri/tauri.conf.json`).

## Deferred

- Authenticode code signing for Windows SmartScreen (separate
  certificate purchase; doesn't block auto-update).
- Update channels (stable / beta) via an `update_channel` setting
  that swaps the endpoint URL.
- Background re-check while the app is running (currently checks
  only at launch).
- Delta updates (only download changed bytes). Tauri doesn't
  support this natively yet.
- A version-bump helper script so the three-file dance becomes one
  command.
