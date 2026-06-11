# CI reference: GitLab pipeline for Linux runners

A complete, commented `.gitlab-ci.yml` for building and releasing
SnipDesk from GitLab with Linux-only runners. This page is a
**reference, not active CI** - the pipeline file belongs to whoever
operates the GitLab project, and they should adapt registry names,
runner tags, and caching to their environment. Every build command
here has been executed and verified on a Linux container; the
environment-specific parts (runners, registries, secrets storage)
are the only things left to adapt.

What the pipeline produces:

- **Every push**: format check, lints, full test suite, frontend
  build. Fails fast on anything main shouldn't carry.
- **`server-v*` tags**: the server Docker image, pushed to the
  project's container registry. Deploy per
  [Production deployment](/deploy) (the Kubernetes manifests there
  are the reference for Helm values).
- **`v*` tags**: the Windows Teams installer, cross-compiled on
  Linux, signed for auto-update, published with its manifest to the
  project's generic package registry.

## One-time setup

### CI/CD variables

Settings -> CI/CD -> Variables. Mark all of these **Masked** and
**Protected** (and protect the `v*` / `server-v*` tag patterns under
Settings -> Repository -> Protected tags so only release tags see
them):

| Variable | What it is |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | Contents of the minisign private key from `npx tauri signer generate`. Generate a fresh keypair for the internal fleet; do NOT reuse any other deployment's key. Required by every client build - the bundler errors without it. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The passphrase chosen at key generation. |
| `UPDATER_PUBKEY` | The matching public key (`.pub` file contents). Baked into the client at build time so it only accepts updates signed by the key above. Not secret, but keeping it next to the others avoids drift. |
| `UPDATER_ENDPOINT` | Where built clients poll for updates. With the registry-hosted manifest below: `https://gitlab.com/api/v4/projects/<PROJECT_ID>/packages/generic/snipdesk-client/latest/snipdesk-teams-update.json` |
| `DEFAULT_SERVER_URL` | The snipdesk-server URL baked into the client so end users never type it (Settings hides the field). Example: `https://snippets.example.com` |

Baking the URL is optional. The runtime alternative: deploy
`C:\ProgramData\snipdesk\config.json` containing
`{ "server_url": "https://snippets.example.com" }` to each machine.
It locks and hides the URL exactly like a baked build, and if the
server address ever changes you edit that one managed file instead
of cutting a new client release. No management software needed -
creating the folder doesn't require admin rights, so this one-liner
(run once per machine, shippable to the team as a `.cmd`) does it:

```powershell
New-Item -ItemType Directory -Force "$env:ProgramData\snipdesk" | Out-Null; Set-Content "$env:ProgramData\snipdesk\config.json" '{ "server_url": "https://snippets.example.com" }' -Encoding ascii
```

The `SNIPDESK_SERVER_URL` environment variable works too (set
machine-wide, not via a wrapper script - autostart launches the
real exe and would bypass a wrapper). Precedence: env var, then
config.json, then the baked value.

No registry credentials are needed: jobs push to the project's own
container/package registries with the built-in `CI_JOB_TOKEN`.

### Package registry visibility

The desktop updater fetches the manifest **unauthenticated**. On a
private project the generic package registry requires a token by
default, which would break every update check. Flip the switch at
Settings -> General -> Visibility -> "Allow anyone to pull from
Package Registry" (packages become readable without exposing the
source). If policy forbids that, mirror the manifest + installer to
any internal static host instead and point `UPDATER_ENDPOINT` there;
the upload step below is the only thing that changes.

### Version/tag discipline

Tags drive releases and **must match the versions committed in the
tree**, or running clients end up in an update loop (binary reports
one version, manifest claims another). The pipeline guards this and
fails the build on drift:

- `v1.2.3` must equal the `version` in `src-tauri/Cargo.toml`,
  `package.json`, AND `src-tauri/tauri.conf.json`.
- `server-v1.2.3` must equal the `version` in
  `crates/snipdesk-server/Cargo.toml`.

## The pipeline

```yaml
# .gitlab-ci.yml - reference pipeline for SnipDesk on Linux runners.
#
# Image notes: rust:1.88-bookworm matches the repo's
# rust-toolchain.toml pin. Bump the image, the toolchain file, and
# the cargo-xwin version TOGETHER (cargo-xwin 0.22+ needs rustc
# 1.89; 0.19.2 matches 1.88).

stages:
  - verify
  - release

# Toolchain layers reused by the Rust jobs. Installing Node and the
# system packages on every run costs a few minutes; once the
# pipeline shape settles, bake them into a small custom image and
# swap it in here.
.rust-node:
  image: rust:1.88-bookworm
  before_script:
    - apt-get update
    - apt-get install -y nsis lld llvm clang libayatana-appindicator3-dev
    # Node 20 (bookworm's packaged node is too old for vite 6)
    - curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
    - apt-get install -y nodejs
  cache:
    key: cargo-$CI_COMMIT_REF_SLUG
    paths:
      - .cargo/
      - target/
  variables:
    CARGO_HOME: $CI_PROJECT_DIR/.cargo

# ---- Every push: the same gate the repo's pre-push hook enforces ----
verify:
  extends: .rust-node
  stage: verify
  script:
    - npm ci
    - npm run build            # tauri's build script expects dist/ to exist
    - cargo fmt --all --check
    - cargo clippy --workspace --all-targets -- -D warnings
    - cargo test --workspace

# ---- server-v* tags: build + push the server image ----
server-image:
  stage: release
  image: docker:27
  services:
    - docker:27-dind
  rules:
    - if: '$CI_COMMIT_TAG =~ /^server-v/'
  script:
    # Tag-vs-crate version guard (see "Version/tag discipline").
    - VERSION="${CI_COMMIT_TAG#server-v}"
    - CRATE_VERSION=$(grep -m1 '^version' crates/snipdesk-server/Cargo.toml | cut -d'"' -f2)
    - |
      if [ "$VERSION" != "$CRATE_VERSION" ]; then
        echo "tag $CI_COMMIT_TAG != crate version $CRATE_VERSION - bump Cargo.toml first"
        exit 1
      fi
    - docker login -u "$CI_REGISTRY_USER" -p "$CI_JOB_TOKEN" "$CI_REGISTRY"
    - docker build -t "$CI_REGISTRY_IMAGE/snipdesk-server:$VERSION"
                   -t "$CI_REGISTRY_IMAGE/snipdesk-server:latest" .
    - docker push "$CI_REGISTRY_IMAGE/snipdesk-server:$VERSION"
    - docker push "$CI_REGISTRY_IMAGE/snipdesk-server:latest"

# ---- v* tags: Windows Teams installer, cross-compiled + signed ----
client-windows:
  extends: .rust-node
  stage: release
  rules:
    - if: '$CI_COMMIT_TAG =~ /^v/'
  variables:
    XWIN_ACCEPT_LICENSE: "1"
    # Bake the fleet's server URL so users never type it.
    SNIPDESK_DEFAULT_SERVER_URL: $DEFAULT_SERVER_URL
    # Only needed on memory-constrained runners; harmless otherwise.
    # CARGO_BUILD_JOBS: "4"
  script:
    # Tag-vs-config guard across all three client version files.
    - VERSION="${CI_COMMIT_TAG#v}"
    - CARGO_V=$(grep -m1 '^version' src-tauri/Cargo.toml | cut -d'"' -f2)
    - PKG_V=$(node -p "require('./package.json').version")
    - CONF_V=$(node -p "require('./src-tauri/tauri.conf.json').version")
    - |
      for v in "$CARGO_V" "$PKG_V" "$CONF_V"; do
        if [ "$VERSION" != "$v" ]; then
          echo "tag $CI_COMMIT_TAG disagrees with committed versions" \
               "(Cargo.toml=$CARGO_V package.json=$PKG_V tauri.conf.json=$CONF_V)"
          exit 1
        fi
      done
    - rustup target add x86_64-pc-windows-msvc
    - cargo install cargo-xwin --version 0.19.2 --locked
    - npm ci
    # Teams build, mirroring scripts/build-teams.mjs with cross flags.
    # The second --config merges over the first: it points the baked
    # updater at THIS project's manifest + key instead of the public
    # upstream feed (see docs/build.md, "Internal fleets").
    - npx vite build --mode teams
    - >
      npx tauri build --features teams
      --config src-tauri/tauri.teams.conf.json
      --config "{\"plugins\":{\"updater\":{\"endpoints\":[\"$UPDATER_ENDPOINT\"],\"pubkey\":\"$UPDATER_PUBKEY\"}}}"
      --runner cargo-xwin --target x86_64-pc-windows-msvc
    # Collect artifacts. Raw `tauri build` names the installer
    # SnipDesk_<version>_x64-setup.exe; the canonical
    # SnipDesk-Teams-setup.exe rename normally happens in the
    # scripts/build-teams.mjs wrapper, which this job bypasses to
    # add the cross flags - so rename here. (Caught by running this
    # exact job in a container; without the rename the collect step
    # fails.)
    - BUNDLE=target/x86_64-pc-windows-msvc/release/bundle/nsis
    - mv "$BUNDLE/SnipDesk_${VERSION}_x64-setup.exe" "$BUNDLE/SnipDesk-Teams-setup.exe"
    - mv "$BUNDLE/SnipDesk_${VERSION}_x64-setup.exe.sig" "$BUNDLE/SnipDesk-Teams-setup.exe.sig"
    # Update manifest: same shape scripts/generate-manifest.ps1
    # emits, written with plain shell so the job needs no pwsh.
    - PKG_BASE="$CI_API_V4_URL/projects/$CI_PROJECT_ID/packages/generic/snipdesk-client"
    - SIG=$(cat "$BUNDLE/SnipDesk-Teams-setup.exe.sig")
    - |
      cat > snipdesk-teams-update.json <<EOF
      {
        "version": "$VERSION",
        "pub_date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
        "platforms": {
          "windows-x86_64": {
            "signature": "$SIG",
            "url": "$PKG_BASE/$VERSION/SnipDesk-Teams-setup.exe"
          }
        }
      }
      EOF
    # Publish: immutable copies under the version, plus the
    # manifest again under the stable "latest" channel the baked
    # endpoint polls. Requires "duplicate package uploads" left
    # enabled for this package (the GitLab default) so "latest"
    # can be overwritten each release.
    - 'curl --fail -H "JOB-TOKEN: $CI_JOB_TOKEN" --upload-file "$BUNDLE/SnipDesk-Teams-setup.exe" "$PKG_BASE/$VERSION/SnipDesk-Teams-setup.exe"'
    - 'curl --fail -H "JOB-TOKEN: $CI_JOB_TOKEN" --upload-file snipdesk-teams-update.json "$PKG_BASE/$VERSION/snipdesk-teams-update.json"'
    - 'curl --fail -H "JOB-TOKEN: $CI_JOB_TOKEN" --upload-file snipdesk-teams-update.json "$PKG_BASE/latest/snipdesk-teams-update.json"'
  artifacts:
    paths:
      - target/x86_64-pc-windows-msvc/release/bundle/nsis/SnipDesk-Teams-setup.exe
      - snipdesk-teams-update.json
    expire_in: 30 days
```

## Things worth knowing before the first run

**The bundler-type warning is expected.** Every Linux-host client
build prints *"Failed to add bundler type to the binary ... Updater
plugin may not be able to update this package"*. Verified harmless
for this repo's manifest shape - the full analysis is in
[Build from source](/build#cross-compile-windows-installers-from-linux-experimental).
Don't switch the manifest to `windows-x86_64-nsis`-style keys
without re-reading that section.

**First client build is slow.** cargo-xwin downloads the Microsoft
SDK headers on first run (cached afterwards), and `cargo install
cargo-xwin` compiles from source. With the cargo cache configured
above, subsequent builds drop to normal compile times. If release
latency matters, bake cargo-xwin and the apt packages into a custom
runner image.

**The release stays a two-tag dance.** Client and server version
independently: bump the version files, commit, tag `v*` and/or
`server-v*`, push tags. Nothing else. The guards catch a missed
bump before anything is signed or pushed.

**Run one manual update cycle before the fleet relies on it.**
Install a Linux-built installer on a Windows machine, tag the next
version, and watch the in-app update complete. Cross-compiled
auto-update is verified for this repo, but it is the one path worth
re-proving inside the real environment (real registry URL, real
keys) once.

**Lite edition:** the pipeline above builds Teams only - an internal
fleet always pairs the client with the server, so a Lite (offline)
build would be CI minutes nobody installs. Add a mirrored job
without `--features teams` if one is ever wanted.

**Server deploy questions** (Helm values, Secrets, probes, SQLite
constraints) are answered in [Production deployment](/deploy) -
the Kubernetes reference manifests there enumerate every value the
chart needs.
