# Deploying snipdesk-server

Production deployment guide for `snipdesk-server`. For a five-minute
walk from a fresh machine to a working dashboard, see
[Docker quickstart](/docker-quickstart). For the architecture this
deployment is running, see [Server architecture](/server-design).

Audience: an operator setting up a real Teams deployment for their
organisation. Assumes familiarity with Docker, reverse proxies, and
TLS.

## What you're deploying

One binary (`snipdesk-server`) backed by a single SQLite file. No
external dependencies. Talks HTTP on a configurable port; you
terminate TLS at a reverse proxy in front of it.

Endpoints:

- `/api/health` - liveness probe (200 OK / 503 when DB unreachable).
- `/api/auth/*`, `/api/me`, `/api/snippets/*`, `/api/library/*`,
  `/api/admin/users/*` - JSON API for the desktop client.
- `/` and `/dashboard/*` - htmx admin dashboard, cookie-authed,
  admin-only.
- `/static/*` - vendored htmx + dashboard CSS, baked into the binary.

## 1. Pick where it runs

Anywhere that runs Linux containers (or that you can drop a static
Rust binary on). For most teams that means:

- **A small VM** (1 vCPU / 1 GB RAM is plenty for hundreds of users
  in tests). DigitalOcean, Hetzner, Vultr, AWS Lightsail all fine.
- **Your existing Kubernetes cluster**, if you have one.
- **A box under someone's desk**, if you trust your office network.

Resource shape: the server is overwhelmingly idle. Hot path is one
SQLite query per API call plus AES-GCM on writes. RAM scales with
concurrent OIDC sign-ins (~1 KB per pending auth, capped at 1024).
Disk grows with snippets - assume a few hundred bytes per snippet
including the ciphertext+nonce overhead.

## 2. Plan the persistent state

Two things must survive process restarts and image rebuilds:

- **The SQLite database** at `<data_dir>/snipdesk.db` (plus the WAL
  and shared-memory files in the same directory). Losing this loses
  every user's snippets.
- **The master encryption key.** Losing this makes every encrypted
  personal snippet permanently unreadable, even if you still have the
  DB. Generate once, store somewhere safe, rotate only with a
  documented procedure (see "Key rotation" below).

Treat the master key like a password manager root key: backed up
offline, multiple custodians, never committed.

## 3. Generate the secrets

The image ships with two one-shot subcommands that print a fresh
secret and exit. `docker run --rm` runs them without leaving a
container behind:

```
docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-key
docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-jwt-secret
```

Each prints one base64 line to stdout. Pipe to your secret store
of choice, or capture in a shell variable:

```powershell
# PowerShell
$masterKey = docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-key
$jwtSecret = docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-jwt-secret
```

```bash
# bash / zsh
master_key=$(docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-key)
jwt_secret=$(docker run --rm ghcr.io/2lukewil/snipdesk/snipdesk-server:latest gen-jwt-secret)
```

For a whitelabel image, substitute `snipdesk-server-<slug>` in the
image path.

The master key is the more sensitive of the two:

- Lose the JWT secret: users get bounced on the next request, fix
  by putting the secret back. No data lost.
- Lose the master key: every encrypted personal snippet is now
  unrecoverable. Library snippets are still readable (they're
  plaintext at rest).

## 4. Write the config

Create `snipdesk-server.toml` in the working directory you'll mount
into the container (typically alongside your `docker-compose.yml`):

```toml
# Public bind address. 0.0.0.0:8080 listens on all interfaces; the
# reverse proxy in front terminates TLS.
bind_addr = "0.0.0.0:8080"

# Where the SQLite DB lives. In Docker, mount this from a named
# volume or host path so the data survives container restarts.
data_dir = "/var/lib/snipdesk"

# HS256 signing secret for session JWTs. Output of
# `docker run --rm <image> gen-jwt-secret`.
jwt_secret = "<base64 from gen-jwt-secret>"

# How long soft-deleted snippets stay around before the hourly purge
# drops them. 90 days is the default and the right answer unless
# users routinely sync from devices that have been offline longer.
# Set to 0 to disable purge entirely.
tombstone_retention_days = 90

# Set true when the dashboard is reachable over HTTPS. The session
# cookie gets the Secure attribute so browsers won't send it over
# plain HTTP at all. Leave false for local-only dev.
secure_cookies = true

[crypto]
# AES-256-GCM key, base64. Source priority: SNIPDESK_MASTER_KEY env
# var > master_key_file > master_key (inline). For container
# deployments, prefer the env var (orchestrator manages the secret)
# or a mounted file (e.g. Docker secret).
master_key = "<base64 from gen-key>"
# master_key_file = "/run/secrets/snipdesk-master-key"

# Optional: "Sign in with Google" via OIDC. Omit this section for
# password-only.
[oidc.google]
client_id = "<from Google Cloud Console>"
client_secret = "<from Google Cloud Console>"
redirect_uri = "https://snippets.yourcompany.com/api/auth/oidc/google/callback"
# Workspace lock: reject any token whose hd claim doesn't match.
# Comment out for "any Google account allowed" mode.
required_hd = "yourcompany.com"
# Softer fallback: allow emails whose domain matches one of these.
allowed_email_domains = ["yourcompany.com"]

# Optional: "Sign in with SSO" against a self-hosted Keycloak (or
# any compliant OIDC IdP). Independent of [oidc.google] - configure
# one, both, or neither.
[oidc.keycloak]
client_id = "<from your Keycloak realm client>"
client_secret = "<from your Keycloak realm client>"
issuer_url = "https://kc.yourcompany.com/realms/main"
redirect_uri = "https://snippets.yourcompany.com/api/auth/oidc/keycloak/callback"
# Optional: restrict sign-in to users who hold this realm role.
# required_realm_role = "snipdesk-user"
# Optional: realm role that promotes the user to admin in SnipDesk.
# Re-checked on every sign-in (losing the role demotes them).
# admin_role = "snipdesk-admin"
# Optional button label. Falls back to "Sign in with SSO".
display_name = "Sign in with Acme SSO"
```

Keep this file out of source control. The full reference config
([`snipdesk-server.example.toml`](../crates/snipdesk-server/snipdesk-server.example.toml))
lives in the repo, and the image ships a copy at
`/etc/snipdesk/config.toml.example`. To grab it from a running
container:

```
docker cp snipdesk-server:/etc/snipdesk/config.toml.example .
```

## 5. Set up Google OIDC (optional, recommended)

Skip this section if you're running password-only.

1. https://console.cloud.google.com/ -> create or select a project
   you'll dedicate to SnipDesk.
2. **APIs & Services -> OAuth consent screen.** User type
   "External" for personal projects, "Internal" if you're using a
   Workspace-owned GCP project (the latter restricts at the
   consent-screen level too, not just `required_hd`). Add scopes
   `openid`, `email`, `profile`. Add yourself as a test user if the
   app is still in Testing.
3. **APIs & Services -> Credentials -> Create credentials -> OAuth
   client ID.** Application type Web application. Add an
   **Authorized redirect URI** that matches `redirect_uri` in your
   config exactly - typically your production URL plus
   `/api/auth/oidc/google/callback`. For local dev you can add
   `http://127.0.0.1:8080/api/auth/oidc/google/callback` as a
   second URI. (The legacy unscoped path `/api/auth/oidc/callback`
   still works as a Google shim if you have it registered already;
   the per-provider path is the new canonical surface.)
4. Copy the `client_id` and `client_secret` into the config.
5. **Decide on Workspace lockdown.** `required_hd` is the strict
   knob: Google sets the `hd` claim on tokens issued to Workspace
   members, and the server rejects any token whose `hd` doesn't
   match. Set it and only the matching Workspace can sign in. Leave
   it unset and any Google account that passes the consent screen
   can sign up.
6. **Bootstrap the first admin.** The first user who signs up (via
   email/password OR OIDC) is auto-promoted to admin. Sign in as
   yourself before sharing the URL with the team, so you control the
   admin role from the start.

## 5a. Set up Keycloak SSO (optional)

Skip this section unless you're running a self-hosted Keycloak
(or any compliant OIDC IdP, e.g. Authentik, Authelia). Independent
of Google: configure one, both, or neither.

1. In the Keycloak admin console, pick the realm your users live
   in (or create a fresh `snipdesk` realm).
2. **Clients -> Create client.**
   - Client type: **OpenID Connect**.
   - Client ID: `snipdesk` (whatever you like; it goes into
     `client_id` in the config).
   - Client authentication: **On** (this server uses the
     confidential-client flow with a `client_secret`).
   - Authentication flow: leave **Standard flow** enabled;
     untick Direct access grants and Implicit flow.
3. **Settings on the new client:**
   - Valid Redirect URIs: add your production URL plus
     `/api/auth/oidc/keycloak/callback`. For local dev add
     `http://127.0.0.1:8080/api/auth/oidc/keycloak/callback`
     as a second entry.
   - Web origins: copy the redirect URIs (or `+` to inherit).
   - Leave the rest at defaults.
4. **Credentials tab on the client:** copy the **Client secret**
   into `client_secret` in your `[oidc.keycloak]` block. Treat it
   like a password.
5. **issuer_url:** the URL of your realm WITHOUT the
   `.well-known/openid-configuration` suffix - the openidconnect
   crate appends it. Example: `https://kc.yourcompany.com/realms/main`.
6. **Optional: realm-role gating.** When `[oidc.keycloak]
   required_realm_role` is set, only users who hold that realm
   role can sign in. Create the role under Realm roles, assign it
   to the groups / users who should have access.
7. **Optional: admin role mapping.** `admin_role` is a realm role
   that, when present on the user's ID token, sets `role = admin`
   in the SnipDesk users table. The check runs on every sign-in -
   removing the role in Keycloak demotes the user the next time
   they sign in. Without this, SnipDesk admin status is managed
   exclusively from the dashboard / CLI (same as the Google path).
8. **Display name.** The desktop and dashboard buttons read
   `display_name` from the config; fall back is "Sign in with SSO".
   Use whatever your team recognises (e.g. "Sign in with Okta",
   "Sign in with Acme SSO").

After restart, the admin dashboard's login page shows a "Sign in
with <display_name>" button under the password form, and the
desktop client's Team Library tab renders the same button alongside
the password form. Both flows share the IdP-side callback URL,
so you only register one redirect URI per provider.

### Dashboard SSO

The admin dashboard accepts the same OIDC providers as the desktop
client. The button stack appears on the login page (`/`) under the
password form whenever any provider is configured. This closes the
gap where an OIDC-only user (no password set on their account)
would otherwise be unable to reach `/dashboard` at all.

Non-admin members who try the dashboard SSO flow still get bounced
to the "members can't access the dashboard" page - admin gating is
unchanged. The IdP-side callback URL is the same as the desktop
flow (`/api/auth/oidc/<provider>/callback`); the start endpoint
(`/dashboard/oidc/<provider>/start`) is what tells the server to
finish by setting the session cookie instead of firing the desktop
deep link.

## 6. Container deployment (recommended)

Pre-built images are published to GHCR on every `server-v*` tag:
`ghcr.io/2lukewil/snipdesk/snipdesk-server:latest` (vanilla) or
`ghcr.io/2lukewil/snipdesk/snipdesk-server-<slug>:latest`
(whitelabel). Pull, don't build, unless you have a reason to.

Minimal `docker-compose.yml`:

```yaml
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
      # The master key prefers the env var over an inline
      # `master_key` in the TOML: orchestrator-managed and easier
      # to keep out of configuration-management.
      SNIPDESK_MASTER_KEY: "${SNIPDESK_MASTER_KEY}"
      RUST_LOG: "info,sqlx=warn,tower_http=info"
```

Save the master key in a sibling `.env` so Compose can interpolate
`${SNIPDESK_MASTER_KEY}` at startup:

```
SNIPDESK_MASTER_KEY=<base64 from step 3>
```

The image's default command is
`snipdesk-server --config /etc/snipdesk/config.toml`, so the
compose file doesn't need a `command:` override as long as the
config is mounted at that path.

The container exposes only `127.0.0.1:8080` to the host. Put it
behind a reverse proxy (next section); don't open 8080 to the
internet.

To build the image locally instead of pulling:

```
docker build -t snipdesk-server:local -f Dockerfile .
```

## 7. Reverse proxy + TLS

Pick whichever you already run. Two complete examples follow.

### Caddy

`Caddyfile`:

```
snippets.yourcompany.com {
    encode gzip
    reverse_proxy 127.0.0.1:8080
}
```

Caddy provisions a Let's Encrypt cert on first boot. That's the
entire config.

### nginx

```nginx
server {
    listen 443 ssl http2;
    server_name snippets.yourcompany.com;

    ssl_certificate     /etc/letsencrypt/live/snippets.yourcompany.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/snippets.yourcompany.com/privkey.pem;

    # 2 MiB matches the server's own body cap so the proxy doesn't
    # buffer a request the upstream is going to reject anyway.
    client_max_body_size 2m;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
    }
}

server {
    listen 80;
    server_name snippets.yourcompany.com;
    return 301 https://$host$request_uri;
}
```

(Cloudflare in front works fine too. Set the origin certificate or
flexible-SSL toggle to taste; `secure_cookies = true` in the server
config is what really matters.)

## 7a. Cross-origin web clients (CORS)

CORS is off by default and only needs to be enabled when a separate
web frontend on a different origin needs to call `/api/*`. The
default topology (desktop client + admin dashboard, both same-origin
with the server) never triggers a CORS preflight: same-origin
requests skip it, the dashboard's htmx posts authenticate via cookie,
and the desktop JSON API uses `Authorization: Bearer`.

To enable CORS for a web client on a different origin, add to the
config:

```toml
cors_allowed_origins = [
    "https://app.example.com",
    "http://localhost:5173",   # dev only
]
```

Each entry must include the scheme and (if non-default) the port.
The server mounts a `tower_http::cors::CorsLayer` per entry:

- Methods: all (GET, POST, PUT, PATCH, DELETE).
- Headers: all (so `Authorization`, `Content-Type` etc. flow through).
- Credentials: allowed. The JSON API uses bearer tokens not cookies,
  but the dashboard's session cookie also wants this so a future
  web frontend can hit `/dashboard/*` from the same origin set.

Operational notes:

- Restart the server to pick up changes; CORS is read at boot.
- Bad origin strings (typos, missing scheme) are logged at WARN and
  silently dropped from the list. If all origins fail to parse, the
  CORS layer isn't mounted at all - the server logs that too.
- Wildcard (`"*"`) is intentionally not special-cased. List every
  origin you want; if you really need wildcard behaviour, set up a
  reverse proxy that strips the `Origin` header and serves the API
  same-origin via path-rewrite instead.

Leave the list empty for any deployment that doesn't have a
separate web client - empty is a hard "no CORS layer mounted,"
matching v1's tighter security posture.

## 8. First boot + admin signup

```
docker compose up -d
docker compose logs -f snipdesk-server
```

Expect:

```
INFO snipdesk-server listening on 0.0.0.0:8080
INFO master key loaded; preparing database
INFO tombstone purge task starting (will sweep hourly)
```

Then in a browser, hit `https://<your-host>/` - the dashboard's
login page. Sign up. You're admin.

After bootstrap, configure your desktop client to point at the same
URL.

## 9. Operations

### Health / monitoring

Point your uptime check at `GET /api/health`. 200 with a JSON body
of `{ "status": "ok", "db": true, ... }` means alive; 503 means the
DB ping failed (disk full, file corruption, container restarting).

For richer monitoring: `RUST_LOG=info` produces structured JSON logs
on stdout (when not attached to a TTY - the dev terminal switches to
human-readable format automatically). Ship to Loki / Vector / Datadog
via the platform's standard container-log shipper.

### Backups

Two strategies, pick one:

**Option A: filesystem snapshots.** Stop the container briefly, copy
`data/snipdesk.db` (plus `.db-wal` and `.db-shm` if present), restart.
Works for any host. Daily cron job is plenty for an internal tool.

**Option B: SQLite `.backup`**. `sqlite3 /var/lib/snipdesk/snipdesk.db
".backup '/backups/snipdesk-$(date +%Y%m%d).db'"`. Doesn't require
a stop; SQLite handles the consistency.

You must back up the master key separately. A DB without the key is
useless for personal snippets (library snippets stay readable).

### CLI / interactive console

User-management commands run inside the running container via
`docker compose exec`. The pattern:

```
docker compose exec snipdesk-server snipdesk-server --config /etc/snipdesk/config.toml <subcommand>
```

Common subcommands:

```
docker compose exec snipdesk-server snipdesk-server --config /etc/snipdesk/config.toml users list
docker compose exec snipdesk-server snipdesk-server --config /etc/snipdesk/config.toml users promote alice@example.com
docker compose exec snipdesk-server snipdesk-server --config /etc/snipdesk/config.toml users reset-password alice@example.com
```

If the container is started attached to a TTY (`docker compose up`
without `-d`), the server also drops into an interactive console
that accepts the same `users list`, `users promote <email>`,
`stop`, etc. inputs directly. Useful for incident response, less
useful for routine ops.

### Updates

Tagged releases at `server-v*` publish a new image to
`ghcr.io/2lukewil/snipdesk-server`. When a whitelabel brand bundle
is configured in CI (`BRAND_BUNDLE_WHITELABEL` secret), the same
tag push also produces `ghcr.io/2lukewil/snipdesk-server-<slug>`
with the customer's brand baked in via Dockerfile build-args -
operators pulling the per-customer image get the right branding
on every update without ever touching server config. Migrations
run automatically on boot. The checksum-repair logic in `db.rs`
handles comment-only edits to applied migrations cleanly; real
schema changes only land via new migration files.

#### Whitelabel: hands-off Docker deploy

Per-customer images bake the brand name + OIDC deep-link scheme
allowlist into `SNIPDESK_BRAND_NAME` and
`SNIPDESK_OIDC_ALLOWED_SCHEMES` environment variables at image
build time. The server reads them at startup with env > TOML
precedence (mirroring `SNIPDESK_MASTER_KEY`), so the operator's
mounted TOML only needs the deployment-specific knobs and
secrets - never brand fields. A `docker pull` preserves the env
because it lives on the image, so brand sticks across updates.

Example `docker-compose.yml` for the customer:

```yaml
services:
  snipdesk-server:
    image: ghcr.io/2lukewil/snipdesk-server-acme:latest
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

The mounted TOML can omit `[brand]` and `[oidc].allowed_deep_link_schemes`
entirely; the image's env supplies them. A `docker compose pull`
preserves the baked env because it lives on the image, so brand
stays through every update.

The running server polls the GitHub releases feed every 6 hours by
default (configurable via `[updater]` in the TOML, off via
`enabled = false`). When a newer `server-v*` tag is found the
dashboard renders a banner under the top nav linking to the
release notes, and an info log fires. The banner is the operator
signal that it's time to pull.

#### Pulling an update

When the dashboard banner appears, or any time you want to roll
forward:

```
docker compose pull
docker compose up -d
```

That's the whole flow. Pinning to `:latest` means a `pull` always
fetches whatever the most recent `server-v*` release built; pin to
a specific version tag if you'd rather control rollout windows
explicitly. A pull + restart is on the order of seconds for
snipdesk-server (small Rust binary, no `node_modules`). The active
SQLite connection is dropped during restart; any in-flight admin
POST gets a transient connection error and a retry. Persistent
client sync resumes on the next poll.

#### Automating it

If you'd rather have the pull happen on a cadence without thinking
about it, any image-update tool your fleet already uses works:
Diun for notifications, Renovate + a CI redeploy job, your
hypervisor's scheduled-task runner, or a plain cron firing the
two commands above. The in-server poller + dashboard banner are
the canonical signal regardless of which automation (if any) you
wire up.

#### Kubernetes

Same shape: use `imagePullPolicy: Always` on the `:latest` tag and
either a redeployment trigger (Keel, Argo CD image-updater, or a
simple CI step on tag push) to bounce the pod, or pin to specific
version tags and roll forward via your normal manifest update
flow. The in-server poller + dashboard banner work the same way
inside a pod as in a compose container.

#### Rollback

If a release breaks something, pin to the previous version tag
and bring the container back up:

```
# In your docker-compose.yml, swap :latest for the known-good tag.
# Example:
#   image: ghcr.io/2lukewil/snipdesk-server:server-v0.1.4
docker compose down
docker compose pull
docker compose up -d
```

The SQLite data file is unaffected: rollback is a pure binary swap.
Schema migrations only ever add (the in-tree migrations are append-
only), so an older binary against a newer schema typically still
works against the columns it knows about. If the older binary
genuinely can't read the newer schema, restore the most recent
backup of `snipdesk.db` from before the failed upgrade, then start
the older image against it.

### Audit log

Every admin mutation (user create/update/delete, library
create/update/delete) lands in the `audit_log` table with the
actor's id + email, the action, the target, and a small JSON
details blob. View at `/dashboard/audit` (admins only); 50 entries
per page, newest first.

The table is append-only from the application side - no UPDATE or
DELETE paths. Rows survive a `user.delete` because `actor_email`
is denormalised (the FK has `ON DELETE SET NULL` for `actor_id`).
If you need to prune for retention, do it out-of-band:

```sh
sqlite3 /var/lib/snipdesk/snipdesk.db \
  "DELETE FROM audit_log WHERE at < strftime('%s', 'now', '-1 year');"
```

The app side won't notice; the dashboard just shows newer entries.

### Tombstone purge

The hourly background sweep deletes `is_deleted = 1` snippets older
than `tombstone_retention_days`. Default 90 days. If you need to
hold deleted data longer (legal hold, audit), bump the value and
restart. Set to `0` to disable purging entirely.

### Key rotation

JWT secret rotation is cheap: swap `jwt_secret` in the config and
restart. Every active session is invalidated and users sign back in.

Master encryption key rotation is more involved. The schema records
`key_version` per row to support online rotation, but the in-server
re-encryption command for v1.0 is not exposed. Treat the master key
as "set once, never rotate" unless an operator is comfortable
scripting against the encrypt/decrypt functions in
`crates/snipdesk-server/src/crypto.rs`. This matches the posture of
encryption-at-rest keys in most managed databases.

### Disaster recovery

A user who has signed in on a desktop client has the entire library
of their snippets cached locally in their `app_data_dir`. If the
server permanently dies, those caches survive - users have local
copies. Bring up a fresh server; users re-sign-up; ask each to
**Upload existing snippets** during the migration prompt.

The library (shared snippets) is harder - it lives only on the
server. Your backup strategy covers this; without backups, library
content needs to be recreated manually.

## 10. Security posture

What protects user data in v1.0:

- **In transit:** TLS, at the reverse proxy.
- **At rest, personal snippets:** AES-256-GCM with a server-held
  master key. DB dumps reveal nothing without the key.
- **At rest, library snippets:** plaintext, intentionally. Library
  content is shared content (canned replies every signed-in member
  needs to read).
- **API authorisation:** every personal-snippet endpoint enforces
  `owner_id == authenticated_user.id`. Cross-user access via the
  documented API is impossible.
- **Admin dashboard:** never exposes personal snippet bodies. Admin
  views are counts, timestamps, account metadata, and the audit log.
- **OIDC token compromise:** an attacker with a stolen JWT can read
  the victim's snippets via the API. Mitigations: 30-day rolling
  TTL, plus admins can disable a user from the dashboard to
  invalidate active sessions on the next request.
- **Server compromise (shell access):** an attacker with the master
  key plus the DB can decrypt all personal snippets. This is the
  explicit v1.0 trust boundary: the operator is inside it.

For an end-to-end model where operators are outside the trust
boundary (SaaS deployments, regulated customer environments), see
the *Future: end-to-end encryption* section of
[Server architecture](/server-design#future-end-to-end-encryption).
The v1 schema is forward-compatible with the upgrade.

## 11. Troubleshooting

**"no master encryption key configured"** at startup: the server
couldn't find a key via any source. Either `SNIPDESK_MASTER_KEY` env
var is unset, `master_key_file` points at something unreadable, or
`master_key` is missing from the config. See section 3.

**"migration N was previously applied but has been modified"**:
shouldn't happen with the in-tree migrations (the self-repair in
`db.rs` handles comment-only edits). If you see this, a custom
migration file is divergent from what was first applied - inspect,
fix, restart.

**Dashboard says "members can't access the dashboard"** when you
sign in: your account is `member` not `admin`. Promote via the CLI:

```
docker compose exec snipdesk-server snipdesk-server --config /etc/snipdesk/config.toml users promote you@example.com
```

**OIDC returns `redirect_uri_mismatch`**: the `redirect_uri` in your
config doesn't EXACTLY match an Authorized Redirect URI in the
Google Cloud Console. Add it there.

**"this server doesn't have Google OIDC configured"** on the
desktop's Sign in with Google: your config is missing the
`[oidc.google]` section. Add it (section 5).

**Server starts but immediately exits with a SQLite error**: usually
`data_dir` is unwritable. Check container volume mounts and host
file permissions.

**Snippets aren't syncing on a client**: check the client's local
`high_water_mark` sync state. The server-side `/api/snippets` and
`/api/library` log lines (`tracing::info!` at debug level) show
`since=N returned=M` - if `returned=0` even when there should be
data, the client's cursor is past the server's data.

