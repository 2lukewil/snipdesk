# Auto-Update — How it works

> **Status:** Shipping. CI-driven releases via `.github/workflows/release.yml`.

## The flow at a glance

```
You: bump version, git tag v1.0.1, git push --tags
     │
     ▼
GitHub Actions (release.yml on tag push)
     │
     ├─ npm ci, npm run build
     ├─ tauri build (offline, signed via env-var key)
     ├─ tauri build --features teams (Teams, signed)
     ├─ generate-manifest.ps1 (emits two manifest JSONs)
     └─ softprops/action-gh-release publishes the tag with all six files
            │
            ▼
GitHub Releases (release marked Latest by default)
            │
            ▼
Every running SnipDesk instance, on next launch:
  • Hits releases/latest/download/snipdesk-update.json (or teams equivalent)
  • Sees newer version available → toast: "v1.0.1 available · Install / Later"
  • User clicks Install → download, verify Ed25519 signature, install, relaunch
```

End-to-end: ~3-5 minutes from `git push --tags` to the release being live. Clients pick up within whatever their next launch is.

## How the cryptography works

When you build with the `TAURI_SIGNING_PRIVATE_KEY` env var set, Tauri's bundler signs each NSIS installer with your Ed25519 private key, producing a `<installer>.sig` file alongside it. The `generate-manifest.ps1` helper reads each `.sig` and splices it into a manifest JSON pointing at the installer's GitHub Releases URL.

When a client polls the manifest, it parses the version + URL + signature, downloads the installer, streams the bytes through an Ed25519 verifier using the public key baked into the binary at compile time (in `tauri.conf.json`'s `plugins.updater.pubkey`), and rejects the file if the signature doesn't match. Tampered bundles never install.

This signature is **independent of Authenticode / SmartScreen.** SmartScreen warnings on first install are a separate Windows trust concern, fixed by buying an Authenticode cert, which is on the post-1.0 roadmap and not blocking auto-update.

## One-time setup

You do this once. After that, every release is `git tag && git push`.

### 1. Generate the Ed25519 keypair locally

```powershell
npx @tauri-apps/cli signer generate -w $HOME\.snipdesk-update.key
```

Pick a passphrase you'll remember. The command emits two files:

- `~\.snipdesk-update.key` — private. Encrypted-at-rest with your passphrase. **Save in your password manager + offline backup.** Losing this means you cannot ship signed updates and have to break the auto-update chain.
- `~\.snipdesk-update.key.pub` — public. Goes into `tauri.conf.json`.

### 2. Embed the public key in the build

Open `~\.snipdesk-update.key.pub`, copy the contents (a single base64 string), paste it into `src-tauri/tauri.conf.json` replacing the `REPLACE_WITH_TAURI_SIGNER_PUBLIC_KEY` placeholder:

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

Commit the change. From this point on, every build embeds the public key so the runtime updater can verify signatures.

### 3. Add the private key + passphrase as GitHub secrets

In your GitHub repo settings → Secrets and variables → Actions → New repository secret:

- `TAURI_SIGNING_PRIVATE_KEY` — paste the **contents** of `~\.snipdesk-update.key` (open it in Notepad, copy everything, paste). It's already encrypted by the passphrase, so storing it as-is is fine.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — the passphrase you chose during generation.

GitHub encrypts both at rest and never logs them.

### 4. (Optional) Smoke-test the workflow

Push a throwaway tag to verify the pipeline:

```powershell
git tag v0.0.0-test
git push --tags
```

Watch the Actions tab. If the workflow goes green, you'll get a (probably ugly) release at `https://github.com/2lukewil/snipdesk/releases/tag/v0.0.0-test`. Delete it (`gh release delete v0.0.0-test --cleanup-tag`) once you've confirmed the artifacts look right.

## Per-release flow

After setup is done, releases are three commands:

```powershell
# 1. Bump the version in three places (in lockstep):
#      Cargo.toml             → [workspace.package].version
#      package.json           → version
#      src-tauri/tauri.conf.json → version
#    (A small script in scripts/ could automate this — TODO if releases get frequent.)

# 2. Commit the bump and push:
git add -A
git commit -m "Bump version to 1.0.1"
git push

# 3. Tag and push the tag:
git tag v1.0.1
git push --tags
```

Step 3 fires `.github/workflows/release.yml`. ~3-5 minutes later your release is live and clients start picking it up on their next launch.

## Manual fallback

If CI is broken and you need to ship anyway, the manual local path:

```powershell
cd E:\snipdesk

# Build both flavors with the signing env vars set so .sig files appear.
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content $HOME\.snipdesk-update.key -Raw
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "<your-passphrase>"

npx @tauri-apps/cli build --bundles nsis
npm run tauri:build:teams -- --bundles nsis

# The Teams build self-renames its installer to SnipDesk-Teams-setup.exe.
# The offline build doesn't, so normalize it the same way release.yml does
# (generate-manifest.ps1 expects the canonical names):
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

Same outputs as the CI path, just produced on your machine.

## What clients see

When `auto_check_updates` is on (default), every launch silently fetches the manifest. If the version is newer than `CARGO_PKG_VERSION`, a non-blocking toast appears in the status bar:

> **SnipDesk 1.0.1 is available.** Install and restart · Later

Click "Install and restart" → progress shown in the status bar (downloaded byte count) → silent install → relaunch. About 30 seconds total on a normal connection.

If the user clicks Later, the toast clears and they get re-prompted on the next launch. There's no nagging within a session.

If the network is unreachable, the check fails silently — `console.warn` only, no user-facing error. Manual "Check for updates" in Settings → About surfaces errors loudly.

## Windows install path caveats

NSIS is the auto-update target. NSIS installs per-user under `%LOCALAPPDATA%\Programs\` without admin elevation, so the updater can replace files silently. **MSI installs** (under `Program Files`) require admin elevation on every update — not silently installable. We don't ship MSI for auto-update; the offline build's NSIS installer is what end users should grab.

If a user originally installed via an MSI (e.g. some IT departments require it for deployment), they'll need to uninstall and reinstall via the NSIS installer once to get into the auto-update chain. Document this in the README install section if it ever becomes a real friction point.

## Troubleshooting

**"Invalid signature" on client.** Either you bumped the keypair without redeploying the public key in `tauri.conf.json`, or `TAURI_SIGNING_PRIVATE_KEY` in CI doesn't match the embedded public key. Regenerate one to match the other.

**Release workflow fails on `tauri build`.** Most common cause: the TAURI_SIGNING_PRIVATE_KEY secret has whitespace at the start/end (Notepad's CRLF + trailing newline can do this). Re-paste it exactly with no surrounding whitespace.

**Workflow succeeds but client never sees the update.** Check the release was marked "Latest" in GitHub UI (the workflow does this automatically; if it ended up as a draft or pre-release, the `releases/latest/download/` URL won't redirect to it). Also check the manifest URL in `tauri.conf.json` matches your repo path (`2lukewil/snipdesk` etc.).

**Update toast doesn't appear at all.** Verify `auto_check_updates` is on in Settings → General. The plugin's `check()` returns `null` when the manifest version equals or is older than the running binary's version, so make sure you actually bumped the version in all three places.

## Future improvements (deferred)

- Authenticode code signing for Windows SmartScreen (separate concern, ROADMAP.md).
- Update channels (stable / beta) — add a `update_channel` setting that swaps the endpoint URL.
- Background re-check while running (currently only on launch).
- Delta updates (only download changed bytes) — Tauri doesn't support this natively yet.
- A version-bump helper script so the three-file dance becomes one command.

---

*Doc owner: Lucas Wilson.*
