# SnipDesk Teams — Server Design

> **Status:** Draft for review. Nothing in this document has been implemented
> yet. The current Teams build talks to a static JSON URL (`shared_url.rs`)
> and is read-only; this design replaces that with a real backend.

## Goals

A self-hostable backend for the SnipDesk Teams edition that adds:

1. **Per-user accounts** with OIDC (Google Workspace primary) and a
   username/password fallback for orgs without SSO.
2. **End-to-end encrypted personal snippets** synced across the user's
   devices. The server stores ciphertext only and cannot decrypt.
3. A **shared team library** of canned snippets, curated by admins, visible
   to all signed-in members.
4. A **manager dashboard** for user management, library curation, and
   visibility into team usage — without ever exposing personal snippet
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
│ - vault_key in   │ ◄─────────────── │  │  /api/snippets/*     │  │
│   OS keychain    │  ciphertext +    │  │  /api/library/*      │  │
│ - encrypt before │  metadata only   │  │  /api/admin/*        │  │
│   upload         │                  │  └──────────────────────┘  │
└──────────────────┘                  │  ┌──────────────────────┐  │
                                      │  │  htmx dashboard      │  │
┌──────────────────┐                  │  │  (embedded assets)   │  │
│ Browser          │   HTTPS + JWT    │  │  /                   │  │
│ (admin)          │ ───────────────► │  └──────────────────────┘  │
│                  │                  │  ┌──────────────────────┐  │
│                  │   HTML partials  │  │  SQLite (default)    │  │
│                  │ ◄─────────────── │  │  Postgres optional   │  │
└──────────────────┘                  │  └──────────────────────┘  │
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
  -- Auth — exactly one of these is populated per user
  password_hash   TEXT,                       -- Argon2id (local auth)
  oidc_subject    TEXT UNIQUE                 -- OIDC `sub` claim (SSO)
);

-- E2E vault: server stores only the WRAPPED key (never the plain key)
CREATE TABLE user_vault (
  user_id                   TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  -- AES-GCM(vault_key, key=Argon2id(vault_passphrase, salt=vault_salt))
  wrapped_vault_key         BLOB NOT NULL,
  vault_salt                BLOB NOT NULL,
  -- AES-GCM(vault_key, key=Argon2id(recovery_code, salt=recovery_salt))
  recovery_wrapped_vault_key BLOB NOT NULL,
  recovery_salt             BLOB NOT NULL,
  kdf_params                TEXT NOT NULL,    -- JSON: argon2 m, t, p
  created_at                INTEGER NOT NULL,
  rotated_at                INTEGER
);

-- Personal snippets: ciphertext only. Server cannot read any body field.
CREATE TABLE personal_snippets (
  id                  TEXT PRIMARY KEY,        -- client-generated UUID
  owner_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- All of these are AES-GCM ciphertext with the user's vault_key.
  -- Server treats them as opaque bytes.
  title_ciphertext    BLOB NOT NULL,
  title_nonce         BLOB NOT NULL,
  body_ciphertext     BLOB NOT NULL,
  body_nonce          BLOB NOT NULL,
  tags_ciphertext     BLOB NOT NULL,           -- JSON array, then encrypted
  tags_nonce          BLOB NOT NULL,
  folder_ciphertext   BLOB,                    -- nullable: NULL = unfiled
  folder_nonce        BLOB,
  -- Server-managed
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

## Encryption design (E2E)

### Vault model

Every user has a 256-bit `vault_key` that encrypts all their personal
snippet content (title, body, tags, folder path). **The server never sees
the plain `vault_key`.** Two wrapped copies of `vault_key` are stored
server-side:

1. **Passphrase-wrapped:** `wrapped_vault_key = AES-GCM(vault_key, K_p)`
   where `K_p = Argon2id(vault_passphrase, salt=vault_salt)`.
2. **Recovery-wrapped:** `recovery_wrapped_vault_key = AES-GCM(vault_key, K_r)`
   where `K_r = Argon2id(recovery_code, salt=recovery_salt)`.

The `vault_passphrase` is set by the user on first login, **separate from
SSO/password.** It is required once per device (then cached in the OS
keychain via the `keyring` crate). The `recovery_code` is a 24-character
base32 string shown once at vault creation with strong instructions to save
it; it never appears in the server logs.

This separation is what lets E2E coexist with OIDC: SSO authenticates the
user to the server; the vault passphrase decrypts their data on the client.
The server can authenticate but cannot decrypt.

### Per-snippet encryption

Each field uses **AES-256-GCM** with a fresh 96-bit nonce. The authentication
tag is stored alongside the ciphertext (the `aes-gcm` crate handles this
inline). Associated data (AD) for each field is
`snippet_id || field_name || version` so a server-side swap of ciphertext
between fields or snippets is detected on decrypt.

### Client key handling

- New device login: client downloads `wrapped_vault_key` + `vault_salt` +
  `kdf_params`, prompts the user for the vault passphrase, runs Argon2id +
  AES-GCM-decrypt locally, caches the resulting `vault_key` in the OS
  keychain (Windows Credential Manager, macOS Keychain, libsecret on Linux).
- The cached key has the same scope as the OIDC/auth session: signing out
  wipes both the JWT and the cached vault key.

### Vault passphrase recovery

If the user forgets their passphrase, they enter the recovery code instead.
Client downloads `recovery_wrapped_vault_key` + `recovery_salt`, decrypts
locally, then prompts the user to set a new passphrase, which re-wraps
`vault_key` under the new passphrase + a fresh salt. The new
`wrapped_vault_key` is uploaded to the server. The recovery code itself
remains unchanged so the user doesn't have to write down a new one.

### What about searching encrypted snippets?

Search is client-side: the client downloads the user's full ciphertext
collection (typically tens of KB to a few MB for hundreds of snippets),
decrypts in memory, and searches locally. This is also how the launcher's
search field already works today against the local SQLite mirror — the
existing code path doesn't change, only its data source does.

### What metadata leaks to the server?

By design:

- The fact that user X owns N snippets and when they were last modified.
- Snippet IDs (random UUIDs, no information content).
- The user's email, display name, role, last-seen time.

What does **not** leak: snippet titles, bodies, tags, folder names, and
the contents of any field a user enters into a snippet.

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

### Username/password fallback

Standard signup/login with Argon2id-hashed passwords. The login form is
served by the server's dashboard handler — both the desktop client and the
browser dashboard reach it the same way.

### First admin / bootstrap

On a fresh database, the **first successful login** is automatically
granted `role = 'admin'`. This avoids a chicken-and-egg config step and
matches how most self-hosted tools handle the bootstrap.

## API surface

All endpoints under `/api`. JSON request/response unless noted. JWT in the
`Authorization: Bearer ...` header.

### Auth
- `POST /api/auth/signup` (password mode) — `{ email, password, display_name }` → `{ token, user }`
- `POST /api/auth/login` (password mode) — `{ email, password }` → `{ token, user }`
- `GET  /api/auth/oidc/start` → 302 to IdP
- `GET  /api/auth/oidc/callback` → 302 back to client via custom URL scheme
- `POST /api/auth/logout` — invalidates server-side refresh token (no-op for stateless JWTs in v1)
- `GET  /api/me` → `{ user, has_vault }`

### Vault
- `POST /api/vault/init` — first-time vault setup. Body:
  `{ wrapped_vault_key, vault_salt, recovery_wrapped_vault_key, recovery_salt, kdf_params }`
- `GET  /api/vault` — return current wrapped key + salt for this device to decrypt
- `PUT  /api/vault/passphrase` — re-wrap under a new passphrase (after recovery flow or rotation)

### Personal snippets (E2E ciphertext)
- `GET  /api/snippets?since=VERSION` — incremental sync; returns snippets where `version > since`
- `POST /api/snippets` — create. Body: client-generated UUID + all ciphertext fields
- `PUT  /api/snippets/:id` — update. Body: ciphertext fields + `expected_version` for optimistic concurrency
- `DELETE /api/snippets/:id` — soft delete (sets `is_deleted = 1`)

### Shared library (plaintext, all members can read)
- `GET  /api/library?since=VERSION` — incremental sync of library snippets
- `POST /api/library` — create (admin only)
- `PUT  /api/library/:id` — update (admin only)
- `DELETE /api/library/:id` — delete (admin only)

### Admin
- `GET  /api/admin/users` — list users + activity (no snippet content)
- `PUT  /api/admin/users/:id` — disable/enable, change role
- `DELETE /api/admin/users/:id` — soft-delete account (cascades to vault + snippets)

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

This is "last-write-wins with preserved loser" — it never silently loses
data, which is the entire point of having a sync system at all.

## Client changes (Teams build)

The existing `snipdesk-teams` crate (currently `shared_url.rs`, pull-only
JSON) is replaced. New responsibilities for the Teams build:

1. **Settings: replace the team-library URL field** with a server URL +
   login UI (email + password for fallback, "Sign in with Google" button
   for OIDC).
2. **First-login flow:** after authentication, prompt to either set up
   the vault (new account) or unlock it (existing account from another
   device). Show the recovery code once, with copy-to-clipboard + "I've
   saved this somewhere safe" confirmation.
3. **Existing local snippets migration:** on first login, prompt to upload
   the user's existing local snippets to the server. Each is encrypted
   with the freshly-created `vault_key` and POSTed. The local copies
   become the read cache; they're no longer authoritative.
4. **Background sync thread** (replaces the existing team-library polling
   thread): pulls library + personal snippets, pushes local changes.
5. **Offline handling:** writes queue locally if the server is unreachable
   and drain on reconnect. Reads are served from the local SQLite mirror,
   so the launcher works fully offline (encryption keys stay in the OS
   keychain).
6. **Vault key handling:** decrypt-on-load when the app starts; clear on
   logout. Never written to disk except in the OS keychain.

## Dashboard (htmx)

Routes (server-rendered HTML, htmx for interactivity):

- `/` — login or redirect to `/users` if signed in
- `/users` — table of all users with `last_seen_at`, `snippet_count`, role,
  enabled/disabled toggle
- `/users/:id` — user detail (no snippet content), disable/promote actions
- `/library` — list of shared snippets, create/edit/delete (admin only)
- `/settings` — server settings (OIDC client ID/secret, allowed-domain
  list for self-signup gating)

The dashboard is bundled into the server binary via `include_dir!` and
served from the same Axum instance. No separate frontend deployment.

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
   SQLite + sqlx, config file parser, Dockerfile, GitHub Actions release
   workflow that publishes the Docker image on tag push.
2. **Auth: password mode.** Signup, login, JWT, `/api/me`. Argon2id.
   Tested with `curl` + integration tests in the crate.
3. **Vault.** `POST /api/vault/init`, `GET /api/vault`,
   `PUT /api/vault/passphrase`. Client-side AES-GCM + Argon2id helpers
   added to `snipdesk-core` (shared by client and server-side test
   harness).
4. **Personal snippet sync API.** CRUD + incremental sync. Round-trip
   tested with a CLI tool.
5. **Client integration: login + sync.** The Teams desktop client gets
   the new settings UI, login flow, vault setup, migration prompt for
   existing local snippets, and background sync. Throw away
   `snipdesk-teams::shared_url`.
6. **Shared library.** `/api/library` endpoints, client-side rendering
   under the existing "Team Library" sidebar pseudo-folder. Admins manage
   via the dashboard (next phase).
7. **Dashboard.** htmx + askama. Users list, library curation, server
   settings.
8. **OIDC.** Google Workspace flow end-to-end. Tauri custom-URL handler
   for the JWT handoff.
9. **Polish + docs.** Deployment guide, recovery-code UX review,
   security review of crypto code (ideally by someone outside the project),
   public release notes.

## Open questions for the backend team

- Is there a preferred reverse proxy / TLS termination convention in your
  infrastructure? (Caddy / nginx / Cloudflare / something internal?)
- Is there an existing Google Workspace OIDC client we should reuse, or
  create a new one for SnipDesk?
- Any preferred logging/observability stack to integrate with (structured
  JSON to stdout is the default and works with most things)?
- Backup/restore expectations: should the server expose a maintenance
  endpoint to dump-and-restore the SQLite DB, or is filesystem-level backup
  (the data directory) sufficient?
- Capacity expectations for v1: how many users on the initial deployment?
  (SQLite is comfortable to ~1k active users on one box; Postgres becomes
  worth the swap above that.)

## Security posture summary

If your CTO or security reviewer asks "what protects user data?", the
honest answer:

- **In transit:** TLS (either built-in or terminated at the reverse proxy).
- **At rest (personal snippets):** AES-256-GCM with a per-user key the
  server has never seen. A full DB dump reveals nothing.
- **At rest (shared library):** plaintext, but only accessible to
  authenticated members. Shared canned replies aren't a secret.
- **In memory (server):** never. The server handles only ciphertext for
  personal snippets.
- **In memory (client):** the `vault_key` is in process memory while the
  app is running, and in the OS keychain when it isn't. Standard for any
  E2E desktop app.
- **Account compromise:** if an attacker steals a user's OIDC session,
  they can read snippet metadata but cannot decrypt content without also
  obtaining the vault passphrase. The vault is the second factor.
- **Server compromise:** an attacker with shell access to the server can
  see ciphertext and metadata only. They cannot decrypt personal snippets
  even with the JWT secret and DB access combined.
- **Lost passphrase:** recovered via the recovery code shown once at
  setup. Lost both: data is unrecoverable (this is the correct outcome
  for E2E).

---

*Reviewed by:* (sign-off list)
*Next:* once approved, implementation begins at phase 1.
