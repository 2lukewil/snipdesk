# Quickstart: snipdesk-server in Docker

Five steps. Should take ~5 minutes from a fresh machine to a
working dashboard you can sign in to. For TLS, Google SSO,
whitelabel, backups, retention tuning, and everything else, see
[deploy.md](deploy.md) once the basic flow works.

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
somewhere safe** (password manager, secrets store) — losing it
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

## 3. Write the minimum-viable config

```powershell
# PowerShell
@'
bind_addr = "0.0.0.0:8080"
data_dir = "/var/lib/snipdesk"
'@ | Out-File -Encoding utf8 snipdesk-server.toml
```

```bash
# bash / zsh
cat > snipdesk-server.toml <<'EOF'
bind_addr = "0.0.0.0:8080"
data_dir = "/var/lib/snipdesk"
EOF
```

That's enough to boot. JWT secret, OIDC, brand, retention tuning,
CORS, etc. are all optional and have sensible defaults — see
[deploy.md](deploy.md) and the
[example.toml](../crates/snipdesk-server/snipdesk-server.example.toml)
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

You should see `snipdesk-server listening on 0.0.0.0:8080` and
a few migration lines. If you instead see a config error, it'll
tell you exactly what to fix — re-run after the fix.

## 5. Create your first admin

The server ships with no users; the first account that signs up
through `POST /api/auth/signup` is auto-promoted to admin. Two
ways to hit that endpoint:

**Easiest**: install a Teams desktop client (your own SnipDesk
Teams build or a whitelabel one), point it at the server URL
(Settings -> Team Library), and click **Create account**. The
account you create there is the first admin.

**Via the CLI on your host**:

```powershell
# PowerShell
$body = @{
  email = "you@example.com"
  password = "your-password-here"
  display_name = "Your Name"
} | ConvertTo-Json
Invoke-RestMethod -Uri http://127.0.0.1:8080/api/auth/signup `
                  -Method POST `
                  -ContentType "application/json" `
                  -Body $body
```

```bash
# bash / zsh
curl -X POST http://127.0.0.1:8080/api/auth/signup \
  -H 'Content-Type: application/json' \
  -d '{"email":"you@example.com","password":"your-password-here","display_name":"Your Name"}'
```

A successful response includes a session token; the first signup
gets `role=admin` automatically because the table was empty.
Subsequent signups land as `member` and need an existing admin to
promote them.

Confirm via the in-container CLI:

```
docker exec -it snipdesk-server snipdesk-server \
  --config /etc/snipdesk/config.toml users list
```

## 6. Sign in

Open http://127.0.0.1:8080 in a browser. Log in with the email +
password from step 5. You should land on the Users page; the nav
also has Library, Stats, and Audit.

The desktop client (any SnipDesk Teams build) can now point at
this server: Settings → Team Library → Server URL =
`http://127.0.0.1:8080` (or your reverse-proxied URL).

## Recommended: docker-compose instead of docker run

Once you've confirmed the above works, the compose form is easier
to maintain (config + env are all in one file, restarts pick up
changes cleanly):

```yaml
# docker-compose.yml
services:
  snipdesk-server:
    image: ghcr.io/2lukewil/snipdesk/snipdesk-server:latest
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

Put the master key in a sibling `.env` file (`SNIPDESK_MASTER_KEY=...`)
or your secrets store, then:

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

- **TLS + reverse proxy**: [deploy.md §7](deploy.md) (Caddy +
  nginx walkthroughs)
- **Google Workspace SSO**: [deploy.md §5](deploy.md)
- **Backups + retention**: [deploy.md §9](deploy.md)
- **Per-customer whitelabel images**: [brands/_template/README.md](../brands/_template/README.md)
- **Production security checklist**: [deploy.md §10](deploy.md)

## Troubleshooting

**Container exits immediately with `read config /etc/snipdesk/config.toml`** —
your config volume isn't mounted, or the path on the host doesn't
exist. The error now prints the full docker-run command to fix it;
follow that and you're back on track.

**Container exits with a master-key error** — the
`SNIPDESK_MASTER_KEY` env var either wasn't passed in (`-e
SNIPDESK_MASTER_KEY=...`) or is the wrong shape (not base64 of 32
bytes). Re-run step 2 + 4.

**`gen-key` output looks like a base64 string with `+` / `/`** —
that's normal. Pass the whole string verbatim; don't quote it
unnecessarily in your shell or strip characters.

**Want the example TOML inside the container?** It's there:

```
docker cp snipdesk-server:/etc/snipdesk/config.toml.example .
```

(Available in `server-v0.1.1` and later. Earlier images don't
ship the example file; grab it from the repo instead.)
