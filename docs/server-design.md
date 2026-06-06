# SnipDesk Teams - Server Design

> **Status:** Draft for review. Nothing in this document has been implemented
> yet. The current Teams build talks to a static JSON URL (`shared_url.rs`)
> and is read-only; this design replaces that with a real backend.

## Goals

A self-hostable backend for the SnipDesk Teams edition that adds:

1. **Per-user accounts** with OIDC (Google Workspace primary) and a
   username/password fallback for orgs without SSO.
2. **Personal snippets encrypted at rest** and synced across the user's
   devices. The server holds the encryption key; database dumps reveal
   nothing, but operators with shell access can technically decrypt. The
   admin dashboard never exposes personal snippet bodies via any path. See
   the *Encryption* section for the full trust model and the *Future:
   end-to-end encryption* section for the upgrade path.
3. A **shared team library** of canned snippets, curated by admins, visible
   to all signed-in members.
4. A **manager dashboard** for user management, library curation, and
   visibility into team usage - without ever exposing personal snippet
   bodies.
5. A **single Docker container** deployment with no external dependencies
   beyond a config file and a TLS cert.

### Non-goals (deferred)

- Multi-tenant hosting (one server = one organization).
- Real-time push (WebSocket sync). Polling is fine for snippet counts in
  the hundreds.
- Per-snippet sharing controls beyond "personal" vs "shared library."
- Audit log (best-effort logging exists, but no tamper-evident chain).
- Mobile clients.

## Architecture overview

```
┌──────────────────┐                  ┌────────────────────────────┐
│ SnipDesk Teams   │   HTTPS + JWT    │   snipdesk-server (Rust)   │
│ desktop client   │ ───────────────► │  ┌──────────────────────┐  │
│  (Tauri)         │                  │  │  Axum HTTP API       │  │
│                  │                  │  │  /api/auth/*         │  │
│ - Talks plain    │ ◄─────────────── │  │  /api/snippets/*     │  │
│   JSON to API    │  JSON +          │  │  /api/library/*      │  │
│ - Server-side    │  metadata        │  │  /api/admin/*        │  │
│   crypto         │                  │  └──────────────────────┘  │
└──────────────────┘                  │  ┌──────────────────────┐  │
                                      │  │  htmx dashboard      │  │
┌──────────────────┐                  │  │  (embedded assets)   │  │
│ Browser          │   HTTPS + JWT    │  │  /                   │  │
│ (admin)          │ ───────────────► │  └──────────────────────┘  │
│                  │                  │  ┌──────────────────────┐  │
│                  │   HTML partials  │  │  Crypto layer        │  │
│                  │ ◄─────────────── │  │  AES-GCM, master     │  │
└──────────────────┘                  │  │  key from env/config │  │
                                      │  └──────────────────────┘  │
                                      │  ┌──────────────────────┐  │
                                      │  │  SQLite (default)    │  │
                                      │  │  Postgres optional   │  │
                                      │  └──────────────────────┘  │
                                      └────────────────────────────┘
```

Single binary, embedded dashboard assets, SQLite by default. No separate
process, no separate frontend deployment.

## Server stack

- **Language:** Rust (matches the rest of the workspace; the workspace gets
  a new crate `crates/snipdesk-server`).
- **HTTP:** [Axum](https://github.com/tokio-rs/axum) (modern, ecosystem
  alignment with `tokio`, `tower`).
- **Storage:** SQLite via `sqlx` (async, compile-time-checked queries).
  Postgres support via the same `sqlx` driver is a one-line config swap.
- **Passwords:** `argon2` crate for hashing (Argon2id, default params).
- **Sessions:** Stateless JWTs (`jsonwebtoken` crate). 24-hour TTL, refresh
  on each authenticated request.
- **OIDC:** `openidconnect` crate. Google Workspace is the primary IdP;
  the design generalizes to any OIDC provider.
- **Templates:** `askama` (compile-time-checked, similar feel to Jinja).
- **htmx:** served as a vendored static file; partial HTML responses
  from the same handlers as the JSON API where appropriate.
- **TLS:** terminated by the reverse proxy your backend team chooses
  (Caddy, nginx, Cloudflare). The server speaks plain HTTP behind it.
  Config flag enables built-in TLS for simpler deployments.

## Database schema

```sql
-- Accounts
CREATE TABLE users (
  id              TEXT PRIMARY KEY,           -- UUID
  email           TEXT NOT NULL UNIQUE,
  display_name    TEXT NOT NULL,
  role            TEXT NOT NULL DEFAULT 'member',  -- 'member' | 'admin'
  is_disabled     INTEGER NOT NULL DEFAULT 0,
  created_at      INTEGER NOT NULL,
  last_seen_at    INTEGER,
  -- Auth - exactly one of these is populated per user
  password_hash   TEXT,                       -- Argon2id (local auth)
  oidc_subject    TEXT UNIQUE                 -- OIDC `sub` claim (SSO)
);

-- Personal snippets: user-provided content is encrypted at rest by the
-- server using its master key. Client sends plaintext JSON over TLS; the
-- server encrypts before insert and decrypts before returning to an
-- authorized owner. A DB dump reveals ciphertext + key_version only.
CREATE TABLE personal_snippets (
  id                  TEXT PRIMARY KEY,        -- client-generated UUID
  owner_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- AES-256-GCM ciphertext + nonce. Plaintext is a JSON object:
  -- { title, body, tags: [...], folder_path: "..." | null }
  payload_ciphertext  BLOB NOT NULL,
  payload_nonce       BLOB NOT NULL,
  key_version         INTEGER NOT NULL,        -- which master key generation
  -- Server-managed metadata (plaintext)
  version             INTEGER NOT NULL,        -- monotonic per-snippet, for sync
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  is_deleted          INTEGER NOT NULL DEFAULT 0  -- tombstones for sync
);
CREATE INDEX idx_personal_owner_updated ON personal_snippets(owner_id, updated_at);

-- Shared library: plaintext, admin-managed, visible to all signed-in members
CREATE TABLE library_snippets (
  id              TEXT PRIMARY KEY,
  title           TEXT NOT NULL,
  body            TEXT NOT NULL,
  tags            TEXT NOT NULL DEFAULT '',    -- ",tag1,tag2," like local
  folder_path     TEXT,
  created_by      TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  version         INTEGER NOT NULL
);

-- For dashboard "last seen" and per-user counts (no body access)
CREATE TABLE user_activity (
  user_id         TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  snippet_count   INTEGER NOT NULL DEFAULT 0,
  last_sync_at    INTEGER
);
```

## Encryption (server-side at rest)

### Why server-side, not end-to-end

This is an internal company tool deployed inside a trust boundary that
already includes the team operating the server. Engineering cryptographic
protection *from* operators who could also push a malicious client binary,
or capture credentials at the OIDC step, is theatre rather than security.
A simpler model that's honestly described is more valuable than a complex
one whose claims don't hold up to scrutiny.

The pragmatic posture for v1:

- **Database dumps are safe.** Stolen backup files, lost laptops with the
  DB cloned, accidental S3 misconfigurations - none of these expose snippet
  content.
- **Server operators with shell access can decrypt** by reading the master
  key from the server's config. We don't advertise otherwise.
- **The admin dashboard never exposes personal snippet bodies.** Admins
  see usage metrics, not content.
- **API access is strictly per-user.** A signed-in user can only read
  their own personal snippets via the API. Cross-user access is impossible
  without admin shell access.

If this trust model ever changes - e.g. SnipDesk Teams becomes a hosted
SaaS where customers don't trust the operators - the *Future: end-to-end
encryption* section below outlines the upgrade path. The schema and API
have been designed so that E2E can be added later without breaking the
v1 protocol.

### Master key management

The server holds a 256-bit master key used for AES-GCM encryption of all
personal snippet payloads. Sources, in priority order:

1. `SNIPDESK_MASTER_KEY` environment variable (base64-encoded). Preferred
   for container deployments - keeps the secret out of disk-resident
   config.
2. `master_key_file = "/path/to/file"` in `config.toml`. The file must be
   readable only by the server's user (mode `0600`).
3. `master_key = "..."` in `config.toml` directly. Discouraged but
   supported for development.

If no key is configured at startup, the server **refuses to start**. There
is no auto-generated default - that's a footgun (operators forget to set
it, then can't decrypt their data after a config wipe).

A one-time bootstrap helper command generates a fresh key:

```
snipdesk-server gen-key   # prints base64; pipe to your secret store
```

### Per-snippet encryption

Each snippet's user-provided fields are serialized as a JSON object:

```json
{ "title": "...", "body": "...", "tags": ["..."], "folder_path": "..." }
```

...then encrypted as a single blob using **AES-256-GCM** with a fresh
96-bit nonce. The authentication tag is stored inline (the `aes-gcm` crate
handles this). Associated data (AD) is `snippet_id || owner_id ||
version` so server-side swapping of ciphertext between snippets or users
is detected on decrypt.

Encrypting the payload as one blob rather than per-field keeps the schema
flat and makes future schema additions (new optional fields) trivial -
they just become new keys in the JSON, no migration of column structure.

### Key rotation

`key_version` on each row identifies which master key encrypted it.
Multiple master keys can be active simultaneously: the latest is used for
writes; older versions stay in the config to decrypt existing rows. A
background re-encryption job (not in v1) walks old rows and re-encrypts
under the latest key, then the old key can be removed.

For v1, key rotation is a documented manual procedure (stop server, run
re-encrypt CLI subcommand against the DB, start with new key). Automated
zero-downtime rotation is v1.1 work.

### What the client sees

The desktop client talks plain JSON over TLS. It does no cryptography
itself for the snippet payloads - the server is responsible. Local
snippets are mirrored in the client's SQLite cache (unencrypted, same as
the Lite build today; the OS file permissions are the protection).

### Search

Server-side: full-text search over personal snippets requires
on-the-fly decryption (a hot path could maintain an in-memory index, but
that's v1.1). For v1, the client downloads the user's full snippet
collection on sync and searches locally - exactly how the launcher already
works in Lite. Snippet counts are small (typically <1k per user), so this
is fast and the privacy posture is consistent.

### What's in plaintext server-side and what's encrypted

- **Plaintext:** user account info (email, display name, role, last-seen),
  snippet IDs, owner IDs, timestamps, sync versions, tombstone flags.
- **Encrypted:** snippet title, body, tags, folder path.
- **Shared library:** plaintext. Library snippets are explicitly shared
  content (canned replies everyone uses); encrypting them buys nothing
  because every authenticated member needs to read them anyway.

### Future: end-to-end encryption

If the trust model ever changes - a SaaS offering, regulated customer
deployments, or simply user demand - the schema upgrade path is:

1. Add `user_vault` table (server-stored wrapped keys, never the plain
   key).
2. On vault setup, client generates `vault_key`, encrypts payloads with
   it from then on, server's master key becomes irrelevant for new rows.
3. Migrate existing rows by server-decrypting once, sending plaintext over
   TLS to the now-logged-in client, client re-encrypts with `vault_key`,
   uploads, server discards its decryptable copy.

This is a v2 feature with a clear roadmap, not a v1 commitment.

## Authentication

### OIDC flow (Google Workspace primary)

Standard OAuth 2.0 authorization code flow with PKCE:

1. Client opens the server's `/api/auth/oidc/start` URL in the system
   browser (Tauri's `shell::open`).
2. Server redirects to the IdP. Scope: `openid email profile`.
3. User authenticates with the IdP and consents.
4. IdP redirects back to `/api/auth/oidc/callback`.
5. Server validates the ID token, matches `oidc_subject` to an existing
   user or creates a new one (if first-run org policy allows), issues a
   JWT.
6. Server hands the JWT back to the desktop client via a custom URL
   scheme (`snipdesk://auth?token=...`) registered by Tauri.

For desktop clients without the custom-scheme handler ready, fallback is
to show the JWT on a one-page form and have the client paste it in.

### Locking down to a Workspace domain

The server enforces "Workspace-only" sign-in entirely server-side. Two
config knobs control it:

```toml
[oidc.google]
client_id     = "..."
client_secret = "..."
redirect_uri  = "..."
required_hd           = "yourcompany.com"           # strict: reject if hd ≠ this
allowed_email_domains = ["yourcompany.com"]         # softer: email pattern match
```

- **`required_hd`** is the rigorous check. Google's OIDC ID tokens include
  an `hd` (hosted domain) claim that Google sets *itself* based on actual
  Workspace membership. Personal `@gmail.com` accounts and accounts from
  other workspaces lack the claim or carry a different value. If
  `required_hd` is set, the server rejects any token whose `hd` doesn't
  match. Can't be spoofed by changing your display name; it's a claim in
  the signed token.
- **`allowed_email_domains`** is the softer fallback for orgs that want to
  let in non-Workspace email accounts whose addresses happen to be under
  their domain (e.g. contractors with custom-domain Gmail).

Either or both can be set. `required_hd` set alone is the right answer
when you want strict Workspace-only access.

#### Migration paths between Google Cloud project types

Personal Google Cloud projects can only host "External" OAuth apps:
anyone with a Google account can grant consent, and the server's
`required_hd` / `allowed_email_domains` does the policing. Workspace-owned
projects can host "Internal" apps that are scoped at the OAuth layer too -
non-Workspace users can't even reach the consent screen.

The transition between them is **config-only, no code changes**:

1. **Tighten an existing personal-GCP setup:** keep the same client_id /
   client_secret; add `required_hd = "yourcompany.com"` to the server
   config; restart. The server now rejects anyone outside the Workspace.
   Zero infrastructure change.
2. **Move to a Workspace-owned project for a cleaner consent screen:**
   create a new OAuth client in a GCP project owned by your Workspace,
   mark the app as "Internal" user type, copy the new client_id /
   client_secret into the server config, restart. Already-issued JWTs
   remain valid until their 24h TTL - users only see the new flow on
   their next sign-in.

Either way, no server code touches Workspace specifics: the difference is
which OAuth client the server is configured with and whether `required_hd`
is set.

### Username/password fallback

Standard signup/login with Argon2id-hashed passwords. The login form is
served by the server's dashboard handler - both the desktop client and the
browser dashboard reach it the same way.

### First admin / bootstrap

On a fresh database, the **first successful login** is automatically
granted `role = 'admin'`. This avoids a chicken-and-egg config step and
matches how most self-hosted tools handle the bootstrap.

## API surface

All endpoints under `/api`. JSON request/response unless noted. JWT in the
`Authorization: Bearer ...` header.

### Auth
- `POST /api/auth/signup` (password mode) - `{ email, password, display_name }` → `{ token, user }`
- `POST /api/auth/login` (password mode) - `{ email, password }` → `{ token, user }`
- `GET  /api/auth/oidc/start` → 302 to IdP
- `GET  /api/auth/oidc/callback` → 302 back to client via custom URL scheme
- `POST /api/auth/logout` - invalidates server-side refresh token (no-op for stateless JWTs in v1)
- `GET  /api/me` → `{ user, has_vault }`

### Personal snippets (server-encrypted at rest, plaintext JSON over TLS)
- `GET  /api/snippets?since=VERSION` - incremental sync; returns snippets where `version > since`. Server decrypts before returning.
- `POST /api/snippets` - create. Body: client-generated UUID + `{ title, body, tags, folder_path }`. Server encrypts before insert.
- `PUT  /api/snippets/:id` - update. Body: same shape + `expected_version` for optimistic concurrency.
- `DELETE /api/snippets/:id` - soft delete (sets `is_deleted = 1`).

### Shared library (plaintext, all members can read)
- `GET  /api/library?since=VERSION` - incremental sync of library snippets
- `POST /api/library` - create (admin only)
- `PUT  /api/library/:id` - update (admin only)
- `DELETE /api/library/:id` - delete (admin only)

### Admin
- `GET  /api/admin/users` - list users + activity (no snippet content)
- `PUT  /api/admin/users/:id` - disable/enable, change role
- `DELETE /api/admin/users/:id` - soft-delete account (cascades to vault + snippets)

## Two-way sync algorithm

Snippets carry a server-assigned monotonic `version`. The client tracks
the highest version it has seen.

### Client → server (push)

On each sync tick (default 60s), client sends pending local creates,
updates, and deletes:

- **Create:** `POST /api/snippets` with client-generated UUID and ciphertext.
- **Update:** `PUT /api/snippets/:id` with `expected_version`. If the server's
  current version differs, return `409 Conflict` with the server copy.
- **Delete:** `DELETE /api/snippets/:id`.

### Server → client (pull)

`GET /api/snippets?since=LAST_KNOWN_VERSION` returns all snippets (including
tombstones) modified since that version. Client decrypts, merges into the
local SQLite mirror, advances its `last_known_version`.

### Conflict resolution

When a `PUT` returns `409 Conflict`, the client:

1. Decrypts the server's copy and the local copy.
2. Compares `updated_at`. The newer one **wins** and is what the user sees
   going forward.
3. **The loser is not discarded.** It's preserved as a *new* snippet with
   the original title + ` (conflict 2026-06-05 14:32)` suffix, encrypted
   and uploaded. So both edits survive; the user can inspect and merge
   manually.

This is "last-write-wins with preserved loser" - it never silently loses
data, which is the entire point of having a sync system at all.

## Client changes (Teams build)

The existing `snipdesk-teams` crate (currently `shared_url.rs`, pull-only
JSON) is replaced. New responsibilities for the Teams build:

1. **Settings: replace the team-library URL field** with a server URL +
   login UI (email + password for fallback, "Sign in with Google" button
   for OIDC).
2. **First-login flow:** sign in, done. No vault passphrase, no recovery
   code - the server holds the encryption key. Total onboarding is
   typically two clicks (OIDC) or two fields (password fallback).
3. **Existing local snippets migration:** on first login, prompt to
   upload the user's existing local snippets to the server. Each is sent
   as plaintext JSON over TLS; the server encrypts before storing. The
   local copies become the read cache; they're no longer authoritative.
4. **Background sync thread** (replaces the existing team-library polling
   thread): pulls library + personal snippets, pushes local changes.
5. **Offline handling:** writes queue locally if the server is unreachable
   and drain on reconnect. Reads are served from the local SQLite mirror,
   so the launcher works fully offline.
6. **JWT handling:** the auth token is stored in the OS keychain via the
   `keyring` crate, scoped to the server URL. Sign-out clears it.

## Dashboard (htmx)

Routes (server-rendered HTML, htmx for interactivity):

- `/` - login form, or redirect to `/dashboard/users` if signed in
- `/dashboard/users` - table of all users with `last_seen_at`,
  `snippet_count`, role pill, enabled/disabled status, plus inline
  actions (promote/demote, disable/enable, delete, create new). All
  mutations are htmx PUT/DELETE/POST that re-render the affected row
  in place, no full-page reload.
- `/dashboard/library` - list of shared snippets as cards, with an
  inline create form. Delete via htmx; edit (inline) is deferred to
  phase 8 polish, with delete + recreate as the workaround for now.

Auth is cookie-based: a successful POST to `/dashboard/login` issues
the same HS256 JWT the JSON API uses, delivered via an `HttpOnly`,
`SameSite=Lax` cookie named `snipdesk_dashboard`. The
`DashboardSession` extractor reads the cookie, `DashboardAdmin` further
gates on `role=admin` - non-admins see a "members can't access the
dashboard" page rather than a bare 403.

Self-protection guards live in `handlers::admin::update_user` (server
side, defence in depth): admins can't disable or demote themselves,
and the last remaining admin can't be demoted by anyone.

The dashboard is bundled into the server binary via `include_str!` for
templates + htmx + CSS. No separate frontend deployment, no runtime
file reads, no external CDN dependency.

`/settings` (OIDC client config, allowed-domain list) lands in
phase 7 alongside the OIDC flow itself.

## Deployment (Docker)

A `Dockerfile` at the repo root builds a multi-stage image:

```
FROM rust:1.88 as builder
WORKDIR /src
COPY . .
RUN cargo build --release --bin snipdesk-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/snipdesk-server /usr/local/bin/
WORKDIR /var/lib/snipdesk
EXPOSE 8080
ENTRYPOINT ["snipdesk-server"]
CMD ["--config", "/etc/snipdesk/config.toml"]
```

Config example (`config.toml`):

```toml
bind_addr = "0.0.0.0:8080"
data_dir  = "/var/lib/snipdesk"     # SQLite lives here
jwt_secret = "..."                  # 256-bit random; rotate to invalidate all sessions

[oidc.google]
client_id     = "..."
client_secret = "..."
redirect_uri  = "https://snipdesk.example.com/api/auth/oidc/callback"
allowed_email_domains = ["example.com"]   # self-signup only for these

[tls]   # optional; omit if behind a reverse proxy
cert = "/etc/snipdesk/cert.pem"
key  = "/etc/snipdesk/key.pem"
```

`docker-compose.yml` is published alongside the image for the simple case
(server + Caddy reverse proxy with automatic Let's Encrypt). A bare-binary
release artifact is also attached to GitHub releases for ops teams that
don't want Docker.

## Build phases

Each phase is a separate committable milestone. We can pause for review
between any two.

1. **Server skeleton.** `crates/snipdesk-server`, Axum hello-world,
   SQLite + sqlx, config file parser with master-key loading (env var
   primary), `gen-key` CLI subcommand, Dockerfile, GitHub Actions release
   workflow that publishes the Docker image on tag push.
2. **Auth: password mode.** Signup, login, JWT, `/api/me`. Argon2id.
   First-admin auto-promotion. Tested with `curl` + integration tests.
3. **Personal snippet sync API.** CRUD + incremental sync. Server-side
   AES-GCM encryption layer in front of the storage path. Round-trip
   tested with a CLI tool.
4. **Client integration: login + sync.** The Teams desktop client gets
   the new settings UI, login flow, migration prompt for existing local
   snippets, and background sync. Throw away
   `snipdesk-teams::shared_url`.
5. **Shared library.** `/api/library` endpoints (GET for any signed-in
   member, POST/PUT/DELETE for admins). Client pulls library snippets
   on the same sync tick as personal snippets, with a separate
   high-water-mark, into the existing `team_snippets` table. When the
   user is signed in, the legacy `team_library_url` JSON poll pauses so
   the two paths don't fight over the same table. Admin CRUD over the
   dashboard lands in the next phase; for now writes happen via curl /
   admin tooling against `/api/library`.
6. **Dashboard.** htmx-driven admin UI mounted on the same Axum
   listener as the JSON API. Cookie-based session reusing the same JWT
   the desktop client uses (no separate auth). Pages: `/` (login),
   `/dashboard/users` (list, create, role toggle, disable/enable,
   delete), `/dashboard/library` (list + create + delete). Templates
   are hand-rolled HTML with `{{KEY}}` substitution; htmx vendored at
   `/static/htmx.min.js`. Inline library edit and per-user detail view
   defer to phase 8 polish; the admin can delete + recreate for now.
7. **OIDC.** Google Workspace flow end-to-end. Tauri custom-URL handler
   for the JWT handoff.
8. **Polish + docs.** Deployment guide, security review of crypto code
   (ideally by someone outside the project), public release notes.

## Local smoke test

After phase 4 lands, you can exercise the full flow on one machine -
client + server + sign-in + sync - without any deployment ceremony.

**1. Spin up the server.**

```bash
cd crates/snipdesk-server
# Generate two one-off secrets:
cargo run -p snipdesk-server -- gen-key         # → base64 master key
cargo run -p snipdesk-server -- gen-jwt-secret  # → base64 JWT secret

# Copy example config, fill in the secrets:
cp snipdesk-server.example.toml snipdesk-server.toml
# Edit snipdesk-server.toml (NOT the .example.toml - that's a committed
# template; secrets pasted there would land in git history). The file
# layout matters: master_key goes UNDER `[crypto]`, not at top level.
#
#   bind_addr = "0.0.0.0:8080"
#   data_dir = "./data"
#   jwt_secret = "<base64 from gen-jwt-secret>"
#
#   [crypto]
#   master_key = "<base64 from gen-key>"

cargo run -p snipdesk-server -- --config snipdesk-server.toml
# → "snipdesk-server listening on 127.0.0.1:8080"
```

The server's data lives at `./data/snipdesk.db` (under the crate dir
when running from there). Curl-check it's alive:

```bash
curl http://127.0.0.1:8080/api/health
# → {"status":"ok","version":"0.1.0","db":true}
```

**2. Build + launch the Teams desktop client.**

```bash
npm run tauri:build:teams   # or `npm run tauri:dev` for a hot-reload session
.\target\release\snipdesk.exe   # produced by tauri:build
```

**3. In the app, open Settings → Team Library tab.**

The Server section is at the top. Fill in:

- **Server URL:** `http://127.0.0.1:8080` (NOT `http://0.0.0.0:8080` -
  `0.0.0.0` is "all interfaces" for binding only; connecting to it on
  Windows fails with `os error 10049`.)
- **Email / Password:** any (the first signup is auto-promoted to admin)
- Click **Create account** → enter a display name → if you have local
  snippets, the migration prompt offers to upload them.

You should see "Signed in as &lt;display name&gt; - admin", and your
snippets should appear in the server's DB:

```bash
sqlite3 crates/snipdesk-server/data/snipdesk.db \
  "SELECT id, length(payload_ciphertext), is_deleted FROM personal_snippets;"
```

Note `payload_ciphertext` is opaque - no plaintext readable here.

**4. Simulate a second device.**

Close the app. Move or wipe its data dir (`%APPDATA%\com.snipdesk.teams\`
on Windows; the dir name follows the `identifier` in tauri.teams.conf.json).
Relaunch. Open Settings → Team Library, sign in with the same
credentials at the same server URL. Watch your snippets sync back -
content intact, including folder paths and tags. Edit a snippet on
"device 1," wait ~60s for the background tick (or click **Sync now**),
edit returns on "device 2" after its next tick.

**5. Tear-down.** `Ctrl+C` the server (or type `stop` at the in-process
console - Minecraft-style - for graceful shutdown that drains in-flight
requests); delete `crates/snipdesk-server/data/` if you want a fresh
start next time.

## Interactive console

When stdin is a TTY, the server boots with an interactive console
attached to the same process. Type `help` for the command list; the
short version:

- `users list / promote / demote / disable / enable <email>`
- `users delete <email> --yes`  (interactive confirm is disabled in
  console mode because it would race with the console's own stdin)
- `stop` (or `quit` / `exit`) - graceful shutdown

The console reads from `std::io::stdin()` and dispatches against the
same SQLite pool the HTTP layer is using, so commands you type are
immediately reflected by the API (no separate process, no second
shell). Log output interleaves with your typing - same trade-off
Minecraft makes; the alternative is pulling in a line-editor crate.

**Force-on / off:** `snipdesk-server run --console` forces it on,
`--no-console` forces it off. Useful in MSYS / Git Bash / mintty,
which pipe stdio rather than expose Win32 console handles and so
report `is_terminal() == false` even when the operator is sitting
right there.

When stdin isn't a TTY (systemd, `docker run` without `-it`, CI), the
console is suppressed by default. The server runs headless until
`Ctrl+C` / `SIGTERM`. Logs switch to JSON in that mode so log shippers
can parse them without regex.

`users reset-password` deliberately isn't available inside the
console - its prompt would race with the console's stdin reader. Run
it from a separate shell:

```
snipdesk-server -c snipdesk-server.toml users reset-password alice@example.com
```

## Open questions for the backend team

- **Reverse proxy / TLS:** is there a preferred convention in your
  infrastructure? (Caddy / nginx / Cloudflare / something internal?)
- **OIDC client:** is there an existing Google Workspace OIDC client we
  should reuse, or create a new one for SnipDesk?
- **Master-key storage:** preferred form for the `SNIPDESK_MASTER_KEY`
  secret - env var injected by your container orchestrator (e.g. K8s
  Secret, Docker secret), a mounted file, or something else?
- **Logging/observability:** any preferred stack to integrate with?
  Structured JSON to stdout is the default; works with Loki, Vector,
  Datadog, etc.
- **Backup/restore:** filesystem-level backup of the data directory
  (including the encrypted DB file) is the simplest answer. Should the
  server also expose a maintenance endpoint for online dumps?
- **Capacity expectations:** how many users on the initial deployment?
  SQLite is comfortable up to ~1k active users on one box; Postgres
  becomes worth the swap above that. The schema is identical either way.
- **Disaster recovery:** if the master key is lost (e.g. secret rotation
  mistake without preserving the old key), personal snippets become
  unrecoverable. Confirm your team has a key-backup convention you want
  us to document around.

## Security posture summary

If your CTO or security reviewer asks "what protects user data?", the
honest answer for v1:

- **In transit:** TLS, terminated either at the reverse proxy or by the
  server's built-in option.
- **At rest (personal snippets):** AES-256-GCM with a server-held master
  key sourced from an env var or mode-`0600` config file (never embedded
  in the image). A full DB dump reveals nothing without the key.
- **At rest (shared library):** plaintext. Only accessible to
  authenticated members. Shared canned replies aren't secret content.
- **In memory (server):** plaintext briefly during encrypt/decrypt around
  each API call. The server is the trusted middleware here.
- **In memory (client):** plaintext, same as Lite - the local SQLite
  mirror is a regular file, protected by OS user permissions.
- **API authorization:** every personal-snippet endpoint enforces
  `snippet.owner_id == authenticated_user.id`. A signed-in user
  cannot access another user's snippets via any documented API path.
- **Dashboard:** never displays personal snippet bodies. Admin views are
  counts, timestamps, and account metadata only.
- **Account compromise (OIDC session theft):** an attacker can read that
  user's personal snippets via the API. Mitigation: short JWT TTL (24h),
  ability for admins to disable users from the dashboard.
- **Server compromise (shell access):** an attacker with the master key
  and DB can decrypt all personal snippets. This is the explicit
  v1 limit - operators of the server are inside the trust boundary.

If the trust model needs to change (external SaaS, untrusted operators),
see *Future: end-to-end encryption* in the Encryption section for the
upgrade path. The v1 schema is forward-compatible.

---

*Reviewed by:* (sign-off list)
*Next:* once approved, implementation begins at phase 1.
