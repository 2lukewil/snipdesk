# Deploying snipdesk-server

Production deployment guide for `snipdesk-server`. For a five-minute
local trial run, see [Docker quickstart](/docker-quickstart). For the
architecture this deployment runs, see
[Server architecture](/server-design).

Audience: an operator setting up a real Teams deployment for their
organisation. Assumes familiarity with Docker, reverse proxies, and
TLS.

The guide is two halves. **Steps 1-7** are the setup path: follow
them top to bottom and you end with a TLS-fronted server and an
admin account. **Operations, security posture, and troubleshooting**
after that are reference material for the running system.

## What you're deploying

One binary (`snipdesk-server`) backed by a single SQLite file. No
external dependencies. Talks HTTP on a configurable port; you
terminate TLS at a reverse proxy in front of it.

By the end of step 5, one directory on the host holds the entire
deployment:

```
snipdesk/                   # any name, anywhere on the host
├── docker-compose.yml      # written in step 5
├── snipdesk-server.toml    # written in step 4 (keep out of git)
├── .env                    # written in step 5 (keep out of git)
└── data/                   # created in step 2
    └── snipdesk.db         # created by the server on first boot
```

Two of those must survive restarts, image rebuilds, and host moves:

- **`data/`** holds the SQLite database (plus its WAL and
  shared-memory sidecar files). Losing it loses every user's
  snippets.
- **The master encryption key** (stored in `.env`) decrypts personal
  snippets. Losing it makes every encrypted snippet permanently
  unreadable even if you still have the database. Back it up
  offline, like a password manager root key: multiple custodians,
  never committed.

## 1. Pick where it runs

Anywhere that runs Linux containers (or that you can drop a static
Rust binary on):

- **A small VM** (1 vCPU / 1 GB RAM is plenty for hundreds of users
  in tests). DigitalOcean, Hetzner, Vultr, AWS Lightsail all fine.
- **Your existing Kubernetes cluster**, if you have one.
- **A box under someone's desk**, if you trust your office network.

The server is overwhelmingly idle. Hot path is one SQLite query per
API call plus AES-GCM on writes. Disk grows with snippets; assume a
few hundred bytes per snippet.

## 2. Create the server directory

Everything in this guide happens inside one directory. Create it
along with the `data/` folder the database will live in:

```bash
mkdir -p snipdesk/data
cd snipdesk
```

```powershell
# PowerShell
New-Item -ItemType Directory -Force snipdesk\data
cd snipdesk
```

`data/` is what step 5 mounts at `/var/lib/snipdesk` inside the
container; the server creates `snipdesk.db` in it on first boot.
Every command from here on runs from inside `snipdesk/`.

## 3. Generate the secrets

The server needs two secrets. The image ships one-shot subcommands
that print a fresh value and exit:

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

(For a whitelabel image, substitute `snipdesk-server-<slug>` in the
image path.)

They differ in blast radius, so store them accordingly:

- **JWT secret** (goes in the config file, step 4): losing it just
  bounces every session; put it back or generate a new one and
  users sign in again. No data lost.
- **Master key** (goes in `.env`, step 5): losing it makes every
  encrypted personal snippet unrecoverable. **Copy it somewhere
  safe now** (a password manager entry or your org's secret store)
  before moving on.

## 4. Write the config

Create `snipdesk-server.toml` in the current directory. This is the
complete config for a password-only deployment:

```toml
bind_addr = "0.0.0.0:8080"
data_dir = "/var/lib/snipdesk"
jwt_secret = "<paste the gen-jwt-secret output>"

# The dashboard session cookie only travels over HTTPS. Keep true
# in production; set false only for plain-HTTP local testing.
secure_cookies = true
```

Notes:

- The **master key does not go in this file.** It arrives via the
  `SNIPDESK_MASTER_KEY` environment variable in step 5, which keeps
  the most dangerous secret out of the config file entirely.
- `data_dir` is the path **inside the container**. The compose file
  in step 5 maps your `data/` folder onto it; don't change one
  without the other.
- SSO (Google, Keycloak) is added in step 7, after the server is
  running. Every other knob (retention, CORS, stats, branding,
  update checks) has a sensible default and is documented in the
  [reference config](https://github.com/2lukewil/snipdesk/blob/main/crates/snipdesk-server/snipdesk-server.example.toml).

Keep this file out of source control: it contains the JWT secret.

### No config file at all (Kubernetes / Helm)

Every practical config field has a `SNIPDESK_*` environment
variable, and the config file is optional when `SNIPDESK_JWT_SECRET`
is set - the natural shape for Helm charts where values flow in as
env and secrets rather than mounted files. Precedence is
env > TOML > default everywhere, so the two styles also mix (file
for the stable knobs, env for the secrets).

| Variable | Maps to |
| --- | --- |
| `SNIPDESK_BIND_ADDR` | `bind_addr` (default `0.0.0.0:8080`) |
| `SNIPDESK_DATA_DIR` | `data_dir` - point at your persistent volume mount |
| `SNIPDESK_JWT_SECRET` | `jwt_secret` (required; also the env-only-mode signal) |
| `SNIPDESK_MASTER_KEY` | the master encryption key (no TOML needed) |
| `SNIPDESK_SECURE_COOKIES` | `secure_cookies` (`true`/`false`) |
| `SNIPDESK_TOMBSTONE_RETENTION_DAYS` | `tombstone_retention_days` |
| `SNIPDESK_CORS_ALLOWED_ORIGINS` | `cors_allowed_origins` (comma-separated) |
| `SNIPDESK_BRAND_NAME` | `[brand].name` |
| `SNIPDESK_OIDC_ALLOWED_SCHEMES` | `[oidc].allowed_deep_link_schemes` (comma-separated) |
| `SNIPDESK_OIDC_GOOGLE_*` | `[oidc.google]`. Required set to enable: `SNIPDESK_OIDC_GOOGLE_CLIENT_ID`, `SNIPDESK_OIDC_GOOGLE_CLIENT_SECRET`, `SNIPDESK_OIDC_GOOGLE_REDIRECT_URI`. Optional gating: `SNIPDESK_OIDC_GOOGLE_REQUIRED_HD`, `SNIPDESK_OIDC_GOOGLE_ALLOWED_EMAIL_DOMAINS` |
| `SNIPDESK_OIDC_KEYCLOAK_*` | `[oidc.keycloak]`. Required set to enable: `SNIPDESK_OIDC_KEYCLOAK_CLIENT_ID`, `SNIPDESK_OIDC_KEYCLOAK_CLIENT_SECRET`, `SNIPDESK_OIDC_KEYCLOAK_ISSUER_URL`, `SNIPDESK_OIDC_KEYCLOAK_REDIRECT_URI`. Optional: `SNIPDESK_OIDC_KEYCLOAK_REQUIRED_REALM_ROLE`, `SNIPDESK_OIDC_KEYCLOAK_ADMIN_ROLE`, `SNIPDESK_OIDC_KEYCLOAK_ALLOWED_EMAIL_DOMAINS`, `SNIPDESK_OIDC_KEYCLOAK_DISPLAY_NAME` |
| `SNIPDESK_UPDATER_ENABLED` | `[updater].enabled` - set `false` for zero outbound HTTP from the server |
| `SNIPDESK_OPEN_BROWSER` | set `false` to stop a zero-account server from opening the first-run setup page in the local browser (containers never open one; this is for bare-host and scripted runs) |

The remaining tuning tables (`[stats]`, `[fx]`, the rest of
`[updater]`) stay TOML-only; deployments that tune those mount a
file.

A minimal Kubernetes-style env set:

```yaml
env:
  - name: SNIPDESK_DATA_DIR
    value: /var/lib/snipdesk          # your PVC mountPath
  - name: SNIPDESK_SECURE_COOKIES
    value: "true"
  - name: SNIPDESK_JWT_SECRET
    valueFrom:
      secretKeyRef: { name: snipdesk, key: jwt-secret }
  - name: SNIPDESK_MASTER_KEY
    valueFrom:
      secretKeyRef: { name: snipdesk, key: master-key }
```

An OIDC provider enabled from env needs its full required set
(listed above); an incomplete set logs a warning at boot and leaves
the provider disabled rather than half-configured.

Two Kubernetes-specific notes:

- **The container runs as UID 10001 (non-root).** A PVC mounts
  root-owned by default, so without an ownership fix the server
  can't create its SQLite file and exits with a data_dir-unwritable
  error. Set the pod security context and the volume is writable:

  ```yaml
  securityContext:
    runAsUser: 10001
    runAsGroup: 10001
    fsGroup: 10001
  ```

- **Claim the first admin before exposing the Ingress.** A fresh
  database serves the create-first-admin form to whoever reaches
  `/` first. Port-forward and submit it before the Service is
  reachable from outside:

  ```
  kubectl port-forward deploy/snipdesk-server 8080:8080
  # then open http://127.0.0.1:8080/ and create the admin account
  ```

Liveness/readiness probes point at `GET /api/health` (200 healthy,
503 when the database is unreachable).

### Kubernetes reference manifests

The complete set of objects a deployment needs, wired together.
Use directly with `kubectl apply`, or as the specification a Helm
chart's templates should produce - every value a chart needs to
expose appears here. TLS/Ingress is omitted (use whatever your
cluster already runs; point it at the Service below and set
`SNIPDESK_SECURE_COOKIES=true`, which this spec already does).

```yaml
# Secrets first. Generate the two values with:
#   docker run --rm <image> gen-jwt-secret
#   docker run --rm <image> gen-key
# kubectl create secret generic snipdesk \
#   --from-literal=jwt-secret=<...> --from-literal=master-key=<...>
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: snipdesk-data
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi          # SQLite; grows slowly, see sizing notes
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: snipdesk-server
spec:
  replicas: 1               # MUST stay 1: SQLite + in-process OIDC state
  strategy:
    type: Recreate          # two pods can't share the SQLite file
  selector:
    matchLabels: { app: snipdesk-server }
  template:
    metadata:
      labels: { app: snipdesk-server }
    spec:
      securityContext:
        runAsUser: 10001    # the image's non-root user
        runAsGroup: 10001
        fsGroup: 10001      # makes the PVC writable
      containers:
        - name: snipdesk-server
          image: <your-registry>/snipdesk-server:<tag>
          ports:
            - containerPort: 8080
          env:
            - name: SNIPDESK_DATA_DIR
              value: /var/lib/snipdesk
            - name: SNIPDESK_SECURE_COOKIES
              value: "true"
            - name: SNIPDESK_JWT_SECRET
              valueFrom:
                secretKeyRef: { name: snipdesk, key: jwt-secret }
            - name: SNIPDESK_MASTER_KEY
              valueFrom:
                secretKeyRef: { name: snipdesk, key: master-key }
            # Optional from here down. Keycloak SSO:
            # - name: SNIPDESK_OIDC_KEYCLOAK_CLIENT_ID
            #   value: snipdesk
            # - name: SNIPDESK_OIDC_KEYCLOAK_CLIENT_SECRET
            #   valueFrom:
            #     secretKeyRef: { name: snipdesk, key: keycloak-secret }
            # - name: SNIPDESK_OIDC_KEYCLOAK_ISSUER_URL
            #   value: https://kc.yourcompany.com/realms/main
            # - name: SNIPDESK_OIDC_KEYCLOAK_REDIRECT_URI
            #   value: https://snippets.yourcompany.com/api/auth/oidc/keycloak/callback
            # No outbound HTTP at all:
            # - name: SNIPDESK_UPDATER_ENABLED
            #   value: "false"
          volumeMounts:
            - name: data
              mountPath: /var/lib/snipdesk
          livenessProbe:
            httpGet: { path: /api/health, port: 8080 }
            initialDelaySeconds: 5
            periodSeconds: 15
          readinessProbe:
            httpGet: { path: /api/health, port: 8080 }
            initialDelaySeconds: 3
            periodSeconds: 10
          resources:
            requests: { cpu: 50m, memory: 64Mi }
            limits: { memory: 256Mi }
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: snipdesk-data
---
apiVersion: v1
kind: Service
metadata:
  name: snipdesk-server
spec:
  selector: { app: snipdesk-server }
  ports:
    - port: 8080
      targetPort: 8080
```

After the first apply: `kubectl port-forward deploy/snipdesk-server
8080:8080`, open `http://127.0.0.1:8080/`, and create the admin
account before wiring up the Ingress.

## 5. Boot it

Create `docker-compose.yml`:

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

Create `.env` next to it so Compose can fill in the variable
(this file is why the master key never has to appear in the
compose file or the TOML):

```
SNIPDESK_MASTER_KEY=<paste the gen-key output>
```

Start it and watch the log:

```
docker compose up -d
docker compose logs -f snipdesk-server
```

A healthy boot looks like:

```
INFO snipdesk-server listening on 0.0.0.0:8080
INFO master key loaded; preparing database
INFO tombstone purge task starting (will sweep hourly)
```

Verify from the host, then claim the admin account:

1. `curl http://127.0.0.1:8080/api/health` returns
   `{"status":"ok", ...}`.
2. Open `http://127.0.0.1:8080/` in a browser on the host (or
   through an SSH tunnel). While the database has zero accounts,
   that page is a **first-time setup form**: name, email, password.
   (When the server runs outside a container, it opens this page in
   your default browser on first boot automatically; the URL is in
   the boot log either way.)
3. **Submit it now.** The account it creates is the server's
   administrator, and you land signed in to the dashboard. Once any
   account exists the form is gone for good and `/` is a normal
   login page.

The container binds only to `127.0.0.1`, so nothing is reachable
from outside the host until the reverse proxy in step 6 is up.
That's intentional: claim admin before exposure, not after.

## 6. Put TLS in front

Pick whichever proxy you already run. Both examples forward
`snippets.yourcompany.com` to the container.

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

Once TLS resolves, confirm `https://snippets.yourcompany.com/api/health`
answers, then point desktop clients at that URL
(Settings -> Server).

### Cross-origin web clients (CORS)

CORS is off by default and only matters if a separate web frontend
on a different origin needs to call `/api/*`. The standard topology
(desktop client + admin dashboard) never triggers it. To enable:

```toml
cors_allowed_origins = [
    "https://app.example.com",
    "http://localhost:5173",   # dev only
]
```

Each entry needs the scheme and (if non-default) the port. All
methods and headers are allowed on listed origins; credentials are
allowed. Restart to apply. Typo'd origins are dropped with a WARN
log rather than failing the boot. There is no wildcard on purpose;
list every origin, or have a reverse proxy serve the API
same-origin via path-rewrite.

## 7. Add SSO (optional)

Password sign-in always works; SSO is additive. Configure one
provider, both, or neither. After editing the config, apply it
with:

```
docker compose restart snipdesk-server
```

When at least one provider is configured, sign-in buttons appear
automatically in both places that need them: the desktop client's
Server tab and the dashboard login page. There is no client
configuration; clients ask the server what's enabled
(`GET /api/auth/methods`) and render exactly that.

### Google Workspace

Add to `snipdesk-server.toml`:

```toml
[oidc.google]
client_id = "<from Google Cloud Console>"
client_secret = "<from Google Cloud Console>"
redirect_uri = "https://snippets.yourcompany.com/api/auth/oidc/google/callback"
# Workspace lock: reject any token whose hd claim doesn't match.
# Comment out for "any Google account allowed" mode.
required_hd = "yourcompany.com"
# Softer fallback: allow emails whose domain matches one of these.
allowed_email_domains = ["yourcompany.com"]
```

To get the `client_id` / `client_secret`:

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
   config exactly. For local testing you can add
   `http://127.0.0.1:8080/api/auth/oidc/google/callback` as a
   second URI.
4. Copy the `client_id` and `client_secret` into the config and
   restart.

`required_hd` is the lockdown decision: Google stamps an `hd`
claim on tokens issued to Workspace members, and the server rejects
any token whose `hd` doesn't match. Set it and only your Workspace
can sign in; leave it unset and any Google account that passes the
consent screen can sign up.

### Keycloak (or any compliant OIDC IdP)

Works with Keycloak, Authentik, Authelia, or anything whose
discovery document lives at
`<issuer_url>/.well-known/openid-configuration`. Add to the config:

```toml
[oidc.keycloak]
client_id = "snipdesk"
client_secret = "<from the realm client's Credentials tab>"
# The realm URL, without the .well-known suffix.
issuer_url = "https://kc.yourcompany.com/realms/main"
redirect_uri = "https://snippets.yourcompany.com/api/auth/oidc/keycloak/callback"
# Optional: only realm members holding this role may sign in.
# required_realm_role = "snipdesk-user"
# Optional: this realm role grants admin in SnipDesk. Re-checked on
# every sign-in, so revoking it in Keycloak demotes on next sign-in.
# admin_role = "snipdesk-admin"
# Button label. Falls back to "Sign in with SSO" when unset.
display_name = "Sign in with Acme SSO"
```

Keycloak-side setup:

1. In the admin console, pick the realm your users live in.
2. **Clients -> Create client.** Type **OpenID Connect**, client ID
   `snipdesk` (or anything; it goes into `client_id`). Turn
   **Client authentication on** (the server uses the
   confidential-client flow). Keep **Standard flow** enabled;
   untick Direct access grants and Implicit flow.
3. On the client's **Settings**: add the `redirect_uri` from your
   config under Valid Redirect URIs (exactly). For local testing,
   `http://127.0.0.1:8080/api/auth/oidc/keycloak/callback` can be a
   second entry.
4. On the client's **Credentials** tab: copy the client secret into
   the config. Treat it like a password.
5. If you want role gating, create the realm role(s) under Realm
   roles and assign to the groups or users who should have access
   (`required_realm_role`) or hold admin (`admin_role`).

### How the dashboard fits in

The dashboard login page shows the same provider buttons under its
password form. This matters for accounts created via SSO: they have
no password, and without dashboard SSO they couldn't reach
`/dashboard` at all. Admin gating is unchanged; a non-admin who
signs into the dashboard via SSO sees the "members can't access the
dashboard" page.

One detail that keeps IdP setup simple: the desktop and dashboard
flows share the same IdP-side callback URL
(`/api/auth/oidc/<provider>/callback`), so each provider needs
exactly one registered redirect URI. Which experience the user gets
is determined by where the flow started, not where it lands.

---

Setup ends here. Everything below is reference for the running
system.

## Operations

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

The mounted TOML can omit `[brand]` and `[oidc].allowed_deep_link_schemes`
entirely; the image's env supplies them.

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
than `tombstone_retention_days` (default 90; configurable in the
TOML). If you need to hold deleted data longer (legal hold, audit),
bump the value and restart. Set to `0` to disable purging entirely.
The default is right unless users routinely sync from devices that
stay offline longer than the window.

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

## Security posture

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
the *Potential upgrade path: end-to-end encryption* section of
[Server architecture](/server-design#potential-upgrade-path-end-to-end-encryption).
The v1 schema is forward-compatible with that upgrade, but the
upgrade itself is not part of v1.0.

## Troubleshooting

**"no master encryption key configured"** at startup: the server
couldn't find a key via any source. Either `SNIPDESK_MASTER_KEY` is
missing from `.env` (or the env var is otherwise unset),
`master_key_file` points at something unreadable, or `master_key`
is missing from the config. See step 3.

**"jwt_secret is required but missing"** at startup: the config
file has no `jwt_secret`. Generate one (`gen-jwt-secret`, step 3)
and add it to `snipdesk-server.toml`.

**Container exits with `read config /etc/snipdesk/config.toml`**:
the config volume isn't mounted, or the host path in the compose
file doesn't point at your `snipdesk-server.toml`. Compare against
the compose file in step 5.

**Server starts but immediately exits with a SQLite error**: usually
`data_dir` is unwritable. Confirm `./data` exists on the host (step
2) and the volume line in the compose file maps it to
`/var/lib/snipdesk`.

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
config doesn't EXACTLY match the redirect URI registered with the
IdP (Google Cloud Console / Keycloak Valid Redirect URIs). Fix
whichever side is wrong; they must be byte-identical.

**Desktop shows no SSO button after configuring a provider**: the
server wasn't restarted after the config edit, or the provider
block failed to parse (check `docker compose logs` for a TOML
error at boot). The client renders buttons strictly from
`GET /api/auth/methods`; hit that URL directly to see what the
server thinks is enabled.

**Snippets aren't syncing on a client**: check the client's local
`high_water_mark` sync state. The server-side `/api/snippets` and
`/api/library` log lines (`tracing::info!` at debug level) show
`since=N returned=M` - if `returned=0` even when there should be
data, the client's cursor is past the server's data.
