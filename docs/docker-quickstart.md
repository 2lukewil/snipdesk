# Quickstart: snipdesk-server in Docker

Five steps. Should take ~5 minutes from a fresh machine to a
working dashboard you can sign in to. For TLS, Google SSO,
whitelabel, backups, retention tuning, and everything else, see
the [production deploy guide](/deploy) once the basic flow works.

## You'll need

- Docker (Desktop on Windows / macOS, Engine on Linux)
- A free TCP port to bind to (the guide uses 8080)
- ~50 MB of disk for the image, plus whatever your snippet
  library grows into

## 1. Make a working directory

A folder to hold your config + data volume.

```powershell
# PowerShell
New-Item -ItemType Directory -Force snipdesk-server | Out-Null
cd snipdesk-server
New-Item -ItemType Directory -Force data | Out-Null
```

```bash
# bash / zsh
mkdir -p snipdesk-server/data && cd snipdesk-server
```

## 2. Generate a master encryption key

The server uses AES-256-GCM for personal snippet bodies. The key
never leaves the operator's environment; **save the output
somewhere safe** (password manager, secrets store) - losing it
makes existing encrypted rows unreadable.

```powershell
# PowerShell
$key = docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-key
Write-Host "Master key (save this!): $key"
$key | Set-Clipboard
```

```bash
# bash / zsh
key=$(docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-key)
echo "Master key (save this!): $key"
```

::: warning Keep this terminal open through step 4
The `$key` variable only exists in the current shell. If you close
it (or run step 4 in a different terminal), the container starts
with an empty key and fails at boot. Save the printed key to your
password manager now; if you do switch shells, paste it back as a
literal in the step 4 command instead of `$key`.
:::

## 3. Write the minimum-viable config

The config needs a JWT secret (the signing key for session tokens;
the server refuses to start without one). Generate it, then write
the config file in one go:

```powershell
# PowerShell
$jwt = docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-jwt-secret
@"
bind_addr = "0.0.0.0:8080"
data_dir = "/var/lib/snipdesk"
jwt_secret = "$jwt"
"@ | Out-File -Encoding utf8 snipdesk-server.toml
```

```bash
# bash / zsh
jwt=$(docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-jwt-secret)
cat > snipdesk-server.toml <<EOF
bind_addr = "0.0.0.0:8080"
data_dir = "/var/lib/snipdesk"
jwt_secret = "$jwt"
EOF
```

That's enough to boot in password-only mode. The master key from
step 2 deliberately stays OUT of this file - it travels as the
`SNIPDESK_MASTER_KEY` environment variable in step 4, which takes
precedence over anything in the config. OIDC, brand, retention
tuning, CORS, etc. are all optional and have sensible defaults -
see [the production deploy guide](/deploy) and the
[example.toml](https://github.com/2lukewil/snipdesk/blob/main/crates/snipdesk-server/snipdesk-server.example.toml)
for the full schema when you're ready.

## 4. Run the container

```powershell
# PowerShell
docker run -d `
  --name snipdesk-server `
  --restart unless-stopped `
  -p 127.0.0.1:8080:8080 `
  -v "${PWD}/data:/var/lib/snipdesk" `
  -v "${PWD}/snipdesk-server.toml:/etc/snipdesk/config.toml:ro" `
  -e "SNIPDESK_MASTER_KEY=$key" `
  ghcr.io/2lukewil/snipdesk/snipdesk-server:latest
```

```bash
# bash / zsh
docker run -d \
  --name snipdesk-server \
  --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v "$PWD/data:/var/lib/snipdesk" \
  -v "$PWD/snipdesk-server.toml:/etc/snipdesk/config.toml:ro" \
  -e "SNIPDESK_MASTER_KEY=$key" \
  ghcr.io/2lukewil/snipdesk/snipdesk-server:latest
```

Verify it's running:

```
docker logs snipdesk-server
```

You should see `snipdesk-server listening on 0.0.0.0:8080` and a
few migration lines. The `0.0.0.0` is the in-container bind
address; reach the server from the host at `http://127.0.0.1:8080`
(that's what `-p 127.0.0.1:8080:8080` set up). If you instead see
a config error, it'll tell you exactly what to fix; re-run after
the fix.

## 5. Create your first admin

Open http://127.0.0.1:8080 in a browser. While the server has zero
accounts, that page is a **first-time setup form**: enter your
name, email, and a password (10+ characters), submit, and you land
in the dashboard as the server's administrator. Once any account
exists the form disappears permanently and `/` becomes a normal
login page.

(When the server runs directly on your machine rather than in
Docker, it opens this page in your default browser on first boot;
in Docker you open it yourself - the boot log prints the URL.)

Everyone who signs up after this (desktop client **Create
account**, or another admin adding them) lands as a regular
`member`; promote from the dashboard's Users page or via the CLI.

Confirm via the in-container CLI:

```
docker exec -it snipdesk-server snipdesk-server \
  --config /etc/snipdesk/config.toml users list
```

## 6. Look around

You should be on the Users page; the nav also has Library, Stats,
and Audit.

The desktop client (any SnipDesk Teams build) can now point at
this server: Settings -> Server -> Server URL =
`http://127.0.0.1:8080` (or your reverse-proxied URL).

## Recommended: docker-compose instead of docker run

Once you've confirmed the above works, the compose form is easier
to maintain (config + env are all in one file, restarts pick up
changes cleanly). Two files in the same directory:

**1. Save the following as `docker-compose.yml`** (Compose looks
for this filename automatically, so it must be named exactly this
or `compose.yaml`):

```yaml
services:
  snipdesk-server:
    image: ghcr.io/2lukewil/snipdesk/snipdesk-server:latest
    container_name: snipdesk-server
    restart: unless-stopped
    ports:
      - "127.0.0.1:8080:8080"
    volumes:
      - ./data:/var/lib/snipdesk
      - ./snipdesk-server.toml:/etc/snipdesk/config.toml:ro
    environment:
      SNIPDESK_MASTER_KEY: "${SNIPDESK_MASTER_KEY}"
      RUST_LOG: "info,sqlx=warn,tower_http=info"
```

Without `container_name`, Compose generates
`<directory>-<service>-1` (for example
`snipdesk-server-snipdesk-server-1` when the compose file lives in
a folder called `snipdesk-server`). Pinning the name keeps `docker
ps` and `docker logs` commands short.

Whitelabel deployments swap the image line for their per-customer
tag (`ghcr.io/2lukewil/snipdesk/snipdesk-server-<slug>:latest`)
and rename the config volume to match the file they wrote in
step 3. Everything else stays the same.

**2. Save the master key as `.env`** in the same directory so
Compose can interpolate `${SNIPDESK_MASTER_KEY}` at startup:

```
SNIPDESK_MASTER_KEY=<the key from step 2>
```

If you already have a container from the docker-run step above,
stop it first so the port and name aren't taken:

```
docker rm -f snipdesk-server
```

Then:

```
docker compose up -d
docker compose logs -f snipdesk-server
```

Updating: `docker compose pull && docker compose up -d`. The
running container's dashboard shows a banner when a newer
`server-v*` release exists, so you'll see it before you need it.

## Server admin commands

Everything operational happens via `docker exec` against the
running container. The pattern:

```
docker exec -it snipdesk-server snipdesk-server \
  --config /etc/snipdesk/config.toml <subcommand>
```

Available subcommands:

| Command | What it does |
| --- | --- |
| `users list` | List every account with role, status, snippet count |
| `users promote <email>` | Promote a user to admin |
| `users demote <email>` | Demote an admin to member (refuses if it would leave zero admins) |
| `users disable <email>` | Disable account |
| `users enable <email>` | Re-enable a disabled account |
| `users delete <email>` | Permanently delete (cascades to their snippets; prompts for confirmation) |
| `users reset-password <email>` | Set a new password (prompts on stdin) |
| `users info <email>` | Diagnostic dump: id, role, snippet count, first few snippet ids |
| `gen-key` | Print a fresh master encryption key (does NOT touch a running server) |
| `gen-jwt-secret` | Print a fresh JWT signing secret |

## Next steps

When you're ready to put this in front of real users:

- **TLS + reverse proxy**: [production deploy guide](/deploy#6-put-tls-in-front) (Caddy + nginx walkthroughs)
- **Google Workspace SSO**: [production deploy guide](/deploy#google-workspace)
- **Keycloak / generic OIDC SSO**: [production deploy guide](/deploy#keycloak-or-any-compliant-oidc-idp)
- **Backups + retention**: [production deploy guide](/deploy#operations)
- **Per-customer whitelabel images**: [whitelabel brand bundles](/whitelabel)
- **Production security checklist**: [production deploy guide](/deploy#security-posture)

## Troubleshooting

**Container exits immediately with `read config /etc/snipdesk/config.toml`**:
your config volume isn't mounted, or the path on the host doesn't
exist. The error now prints the full docker-run command to fix it;
follow that and you're back on track.

**Container exits with `Is a directory (os error 21)` reading the
config**: classic Docker bind-mount gotcha on Windows / macOS.
When `-v "${PWD}/your-config.toml:/etc/snipdesk/config.toml:ro"`
is given and the host path *doesn't exist as a file*, Docker
silently creates it as an empty *directory* and mounts that. The
server then reads the path and gets EISDIR.

Fix:

```powershell
# PowerShell - verify it's a file with content
Get-ChildItem your-config.toml
Get-Content your-config.toml

# If you instead see a directory (mode d----), Docker auto-created it.
# Remove it, then recreate the config as a real file by re-running
# step 3 (the config must include jwt_secret or the boot fails on
# the next error instead).
Remove-Item -Recurse -Force your-config.toml
```

Then `docker rm -f snipdesk-server` and re-run the docker command.

**Container exits with a master-key error**: the
`SNIPDESK_MASTER_KEY` env var either wasn't passed in (`-e
SNIPDESK_MASTER_KEY=...`) or is the wrong shape (not base64 of 32
bytes). Re-run step 2 + 4. If you opened a fresh shell since
generating the key, the `$key` variable is empty; either save the
key to your password manager and paste it back as a literal, or
regenerate (only safe on a clean install with no encrypted data
yet).

**`docker run` says the name is already in use**: a previous run
crashed and the dead container still holds the name. Force-remove
it and re-run:

```
docker rm -f snipdesk-server
```

**`gen-key` output looks like a base64 string with `+` / `/`**:
that's normal. Pass the whole string verbatim; don't quote it
unnecessarily in your shell or strip characters.

**`docker compose pull` says "no configuration file provided: not
found"**: Compose looks for a `docker-compose.yml` (or
`compose.yaml`) in the current directory. You haven't saved the
YAML from the Compose section above to a file yet. Save it
verbatim as `docker-compose.yml` in your working directory, save
your master key to a sibling `.env` file as
`SNIPDESK_MASTER_KEY=<key>`, then re-run.

**Want the example TOML inside the container?** It's there:

```
docker cp snipdesk-server:/etc/snipdesk/config.toml.example .
```

(Available in `server-v0.1.1` and later. Earlier images don't
ship the example file; grab it from the repo instead.)
