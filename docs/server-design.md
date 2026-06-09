# Server architecture

Reference for `snipdesk-server`: the self-hosted backend that powers
the SnipDesk Teams edition. Covers the wire protocol, schema, sync
algorithm, encryption posture, and authentication model.

For installation and operations, see
[Deploying snipdesk-server](/deploy). For a quick local dev loop,
see [Build from source](/build).

## What the server provides

- **Per-user accounts** with OIDC (Google Workspace primary) and a
  username/password fallback for organisations without SSO.
- **Personal snippets encrypted at rest** and synced across each
  user's devices. The server holds the encryption key; database
  dumps reveal nothing useful, but operators with shell access can
  decrypt. The admin dashboard never exposes personal snippet bodies.
- **A shared team library** of canned snippets, curated by admins,
  visible to every signed-in member.
- **An admin dashboard** for user management, library curation,
  audit log, and per-user activity, with no exposure of personal
  snippet bodies.
- **Single Docker container** deployment with no external
  dependencies beyond a config file and a TLS cert.

### Out of scope

- Multi-tenant hosting (one server runs one organisation's data).
- Real-time push (WebSocket sync). The current polling cadence is
  adequate for snippet counts in the hundreds.
- Per-snippet sharing controls beyond "personal" vs "shared library."
- Tamper-evident audit chain. The audit log is append-only from
  the application path but a database operator with write access
  can still mutate rows.
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
                                      │  │  SQLite              │  │
                                      │  └──────────────────────┘  │
                                      └────────────────────────────┘
```

Single binary, embedded dashboard assets, SQLite. No separate
process, no separate frontend deployment.

## Server stack

- **Language:** Rust. The workspace crate is `crates/snipdesk-server`.
- **HTTP:** [Axum](https://github.com/tokio-rs/axum) on Tokio.
- **Storage:** SQLite via `sqlx` (async, compile-time-checked
  queries). The schema is portable to Postgres via the same driver,
  but the in-tree migrations target SQLite.
- **Passwords:** `argon2` crate (Argon2id, default params).
- **Sessions:** Stateless JWTs (`jsonwebtoken` crate). 30-day TTL,
  refreshed on each authenticated request, so a daily user stays
  signed in indefinitely.
- **OIDC:** `openidconnect` crate. Google Workspace is the primary
  IdP; the implementation accepts any compliant OIDC provider.
- **Templates:** hand-rolled HTML with `{{KEY}}` substitution. htmx
  is vendored and served from `/static/htmx.min.js`.
- **TLS:** terminated at a reverse proxy in front of the server.
  The server speaks plain HTTP behind it. A config flag enables
  built-in TLS for the simplest deployments.

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
  -- Auth: exactly one of these is populated per user.
  password_hash   TEXT,                       -- Argon2id (local auth)
  oidc_subject    TEXT UNIQUE                 -- OIDC `sub` claim (SSO)
);

-- Personal snippets: user-provided content is encrypted at rest by
-- the server using its master key. The client sends plaintext JSON
-- over TLS; the server encrypts before insert and decrypts before
-- returning to an authorised owner. A DB dump reveals ciphertext +
-- key_version only.
CREATE TABLE personal_snippets (
  id                  TEXT PRIMARY KEY,        -- client-generated UUID
  owner_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- AES-256-GCM ciphertext + nonce. Plaintext is a JSON object:
  -- { title, body, tags: [...], folder_path: "..." | null }
  payload_ciphertext  BLOB NOT NULL,
  payload_nonce       BLOB NOT NULL,
  key_version         INTEGER NOT NULL,        -- which master key encrypted this row
  -- Server-managed metadata (plaintext)
  version             INTEGER NOT NULL,        -- monotonic per-snippet, for sync
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  is_deleted          INTEGER NOT NULL DEFAULT 0  -- tombstones for sync
);
CREATE INDEX idx_personal_owner_updated ON personal_snippets(owner_id, updated_at);

-- Shared library: plaintext, admin-managed, readable by every
-- signed-in member.
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

-- Append-only admin actions for the dashboard's audit log.
CREATE TABLE audit_log (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  at            INTEGER NOT NULL,
  actor_id      TEXT REFERENCES users(id) ON DELETE SET NULL,
  actor_email   TEXT NOT NULL,                 -- denormalised, survives user deletion
  action        TEXT NOT NULL,                 -- 'user.promote', 'library.create', ...
  target        TEXT,
  details_json  TEXT
);
```

Live migrations live at `crates/snipdesk-server/migrations/`. The
server applies them at boot and tolerates comment-only edits to
already-applied migrations via a checksum-repair path.

## Encryption (server-side at rest)

### Trust model

This is an internal tool deployed inside a trust boundary that
already includes whoever operates the server. Cryptographic
protection *from* operators who could also push a malicious client
binary or capture credentials at the OIDC step is theatre rather
than security. The simpler model that's honestly described is more
valuable than a complex one whose claims don't hold up.

The pragmatic posture for v1.0:

- **Database dumps are safe.** Stolen backups, lost laptops with
  the DB cloned, accidental S3 misconfigurations: none of these
  expose snippet content.
- **Server operators with shell access can decrypt** by reading the
  master key from the server's config.
- **The admin dashboard never exposes personal snippet bodies.**
  Admin views are counts, timestamps, and account metadata only.
- **API access is strictly per-user.** A signed-in user can only
  read their own personal snippets via the API. Cross-user access
  via the documented API is impossible.

End-to-end encryption is the v2 upgrade path (sketched at the end
of this section). The schema is forward-compatible with it.

### Per-snippet encryption

Each snippet's user-provided fields are serialised as a JSON object:

```json
{ "title": "...", "body": "...", "tags": ["..."], "folder_path": "..." }
```

The blob is then encrypted with **AES-256-GCM** using a fresh 96-bit
nonce. The authentication tag is stored inline (handled by the
`aes-gcm` crate). Associated data (AD) is `snippet_id || owner_id ||
version`, so server-side swapping of ciphertext between snippets or
users is detected on decrypt.

Encrypting the payload as one blob (rather than per-field) keeps
the schema flat and makes future field additions trivial: new keys
in the JSON, no column-structure migration.

### What the client sees

The desktop client talks plain JSON over TLS. It does no cryptography
for snippet payloads; the server is responsible. Local snippets are
mirrored in the client's SQLite cache (unencrypted, same as the Lite
build; OS file permissions are the only protection).

### Search

Full-text search over personal snippets requires decryption. The
client downloads the full snippet collection on sync and searches
locally, the same way the Lite launcher does. Snippet counts are
small (typically under 1,000 per user) so this is fast and the
privacy posture is consistent: search never round-trips through the
server.

### What's plaintext server-side, what's encrypted

- **Plaintext:** user account info (email, display name, role,
  last-seen), snippet IDs, owner IDs, timestamps, sync versions,
  tombstone flags.
- **Encrypted:** snippet title, body, tags, folder path.
- **Shared library:** plaintext at rest. Library snippets are
  explicitly shared content (canned replies every signed-in member
  needs to read); encrypting them buys nothing operationally.

### Future: end-to-end encryption

The v1 schema is forward-compatible with an end-to-end variant.
The upgrade path:

1. Add a `user_vault` table holding server-stored wrapped keys
   (never the plaintext vault key).
2. On vault setup, the client generates a `vault_key` and encrypts
   payloads with it from then on. The server's master key becomes
   irrelevant for new rows.
3. Migrate existing rows by server-decrypting once, sending plaintext
   over TLS to the now-signed-in client, then having the client
   re-encrypt with `vault_key` and upload. The server discards its
   decryptable copy.

This is a v2 commitment, not a v1.0 feature.

## Authentication

### OIDC (Google Workspace primary)

Standard OAuth 2.0 authorisation code flow with PKCE:

1. The client opens `/api/auth/oidc/start` in the system browser
   (via Tauri's `shell::open`).
2. The server redirects to the IdP with scope `openid email profile`.
3. The user authenticates with the IdP.
4. The IdP redirects back to `/api/auth/oidc/callback`.
5. The server validates the ID token, matches `oidc_subject` to an
   existing user (or creates a new one if org policy allows), and
   issues a JWT.
6. The server hands the JWT back to the desktop client via a custom
   URL scheme (`snipdesk://auth?token=...`) registered by Tauri.

If the OS doesn't claim the custom scheme (corporate-locked Windows,
antivirus interference), the browser landing page also displays the
token in a paste-able form. The desktop client has a fallback field
under Settings -> Team Library to accept it.

### Workspace lockdown

The OIDC handler accepts two server-side knobs that gate sign-in
to a specific Google Workspace. Configuration details for both live
in [Deploying snipdesk-server](/deploy#5-set-up-google-oidc-optional-recommended).

The mechanism:

- **`required_hd`** is the rigorous check. Google sets an `hd`
  (hosted domain) claim on tokens issued to Workspace members.
  Personal `@gmail.com` accounts lack the claim, and accounts from
  other workspaces carry a different value. When `required_hd` is
  set, the server rejects any token whose `hd` doesn't match.
- **`allowed_email_domains`** is the softer fallback for orgs that
  want to let in non-Workspace email accounts whose addresses
  happen to be under their domain (e.g. contractors with custom-domain
  Gmail).

Either or both can be set. Neither set means any Google account
that passes the OAuth consent screen can sign up.

### Username/password fallback

Standard signup/login with Argon2id-hashed passwords. The login form
is served by the dashboard's `/` handler; the desktop client posts to
the same endpoints.

### First admin

On a fresh database, the first successful signup is automatically
granted `role = 'admin'`. This avoids a chicken-and-egg config step.
Operators who want a specific person to be the admin should sign
themselves up before sharing the server URL with the rest of the
team.

## API surface

All endpoints under `/api`. JSON request/response. JWT in the
`Authorization: Bearer ...` header.

### Auth
- `POST /api/auth/signup` (password) - `{ email, password, display_name }` -> `{ token, user }`
- `POST /api/auth/login` (password) - `{ email, password }` -> `{ token, user }`
- `GET  /api/auth/oidc/start` -> 302 to IdP
- `GET  /api/auth/oidc/callback` -> 302 back to client via custom URL scheme
- `POST /api/auth/logout` - clears server-side state (no-op for stateless JWTs in v1)
- `GET  /api/me` -> `{ user }`
- `PATCH /api/me` - update profile (WPM, hourly wage, currency for dashboard estimates)

### Personal snippets (server-encrypted, plaintext JSON over TLS)
- `GET  /api/snippets?since=VERSION` - incremental sync; returns snippets with `version > since`. Server decrypts before returning.
- `POST /api/snippets` - create. Body: client-generated UUID + `{ title, body, tags, folder_path }`. Server encrypts before insert.
- `PUT  /api/snippets/:id` - update. Body: same shape + `expected_version` for optimistic concurrency.
- `DELETE /api/snippets/:id` - soft delete (sets `is_deleted = 1`).
- `GET  /api/snippets/trash` - list tombstones still within the retention window.
- `POST /api/snippets/trash/:id/restore` - undelete.

### Shared library (plaintext, every signed-in member reads)
- `GET  /api/library?since=VERSION` - incremental sync
- `POST /api/library` - create (admin only)
- `PUT  /api/library/:id` - update (admin only)
- `DELETE /api/library/:id` - delete (admin only)

### Admin
- `GET  /api/admin/users` - list users + activity (no snippet content)
- `PUT  /api/admin/users/:id` - disable/enable, change role
- `DELETE /api/admin/users/:id` - soft-delete account (cascades to snippets)

### Health
- `GET  /api/health` - liveness probe. 200 with `{ "status": "ok", "db": true }` when alive; 503 when the DB ping fails.

## Sync algorithm

Snippets carry a server-assigned monotonic `version`. The client
tracks the highest version it has seen.

### Client -> server (push)

On each sync tick (default 60s), the client sends pending local
creates, updates, and deletes:

- **Create:** `POST /api/snippets` with a client-generated UUID and
  plaintext payload.
- **Update:** `PUT /api/snippets/:id` with `expected_version`. If
  the server's current version differs, return `409 Conflict` with
  the server's copy.
- **Delete:** `DELETE /api/snippets/:id`.

### Server -> client (pull)

`GET /api/snippets?since=LAST_KNOWN_VERSION` returns all snippets
(including tombstones) modified since that version. The client
decrypts, merges into its local SQLite mirror, and advances its
`last_known_version`.

### Conflict resolution

When a `PUT` returns `409 Conflict`, the client compares `updated_at`
timestamps. The newer one wins and is what the user sees going
forward.

The loser is not preserved in v1.0: the older copy is overwritten.
This is "last-write-wins per snippet." A future release may preserve
the loser as a `(conflict YYYY-MM-DD)` copy so a user can manually
merge; for now, simultaneous edits of the same snippet on two
offline devices result in the second-to-sync winning.

In practice, conflicts are extremely rare because most users have
one active device at a time.

## Client (Teams build)

The Teams desktop client replaces the read-only `shared_url`
JSON-pull from the Lite build with:

- **Sign-in UI** in Settings -> Team Library: server URL plus
  email/password fields and a "Sign in with Google" button. The
  brand bundle may pre-fill the URL and hide the field; see
  [Whitelabel brand bundles](/whitelabel).
- **First-login flow:** sign in, optionally upload existing local
  snippets to the server. No vault passphrase, no recovery code:
  the server holds the encryption key.
- **Background sync thread:** pulls library + personal snippets,
  pushes local changes, on a 60-second tick.
- **Offline handling:** writes queue locally if the server is
  unreachable and drain on reconnect. Reads are served from the
  local SQLite mirror, so the launcher works fully offline.
- **JWT storage:** the auth token lives in the OS keychain via the
  `keyring` crate, scoped to the server URL. Sign-out clears it.

The Teams build is feature-gated at compile time; the Lite build's
binary contains no networking code beyond the auto-updater.

## Dashboard (htmx)

Server-rendered HTML with htmx for interactivity. Routes:

- `/` - login form, or redirect to `/dashboard/users` if signed in.
- `/dashboard/users` - table of all users with `last_seen_at`,
  `snippet_count`, role pill, enabled/disabled status, and per-row
  actions (promote/demote, disable/enable, delete). Mutations are
  htmx `PUT`/`DELETE`/`POST` that re-render the affected row in
  place, no full-page reload.
- `/dashboard/library` - shared snippets with folder tree, inline
  edit, and drag-and-drop reordering.
- `/dashboard/audit` - paginated view of the `audit_log` table
  (50 entries per page, newest first).
- `/dashboard/stats` - server-wide and per-user time-and-money-saved
  estimates derived from paste telemetry.

Auth is cookie-based: a successful POST to `/dashboard/login` issues
the same HS256 JWT the JSON API uses, delivered via an `HttpOnly`,
`SameSite=Lax` cookie named `snipdesk_dashboard`. The
`DashboardSession` extractor reads the cookie; `DashboardAdmin`
further gates on `role=admin`. Non-admins see a "members can't
access the dashboard" page rather than a bare 403.

Self-protection guards live server-side in
`handlers::admin::update_user`: admins cannot disable or demote
themselves, and the last remaining admin cannot be demoted by anyone.

All dashboard assets (templates, htmx, CSS) are bundled into the
server binary via `include_str!`. No separate frontend deployment,
no runtime file reads, no CDN dependency.

## Audit log

Every admin mutation lands in `audit_log` with the actor's id +
email (denormalised so rows survive `user.delete`), the action, the
target, and a small JSON details blob. The table is append-only from
the application path; pruning for retention is an out-of-band SQLite
operation. See [Deploying snipdesk-server](/deploy#audit-log) for
the retention command.

## Tombstones

Deletes are soft. A `is_deleted = 1` row stays in `personal_snippets`
for `tombstone_retention_days` (default 90) so devices that have
been offline can pick up the deletion on their next sync. An hourly
background sweep removes expired tombstones. Setting the value to
`0` disables purging entirely.

## Security posture

What protects user data:

- **In transit:** TLS, terminated at the reverse proxy or by the
  server's built-in option.
- **At rest, personal snippets:** AES-256-GCM with a server-held
  master key. DB dumps reveal nothing without the key.
- **At rest, library snippets:** plaintext, intentionally. Library
  content is shared and meant to be visible to every authenticated
  member.
- **API authorisation:** every personal-snippet endpoint enforces
  `owner_id == authenticated_user.id`. Cross-user access via the
  documented API is impossible.
- **Admin dashboard:** never exposes personal snippet bodies. Admin
  views are counts, timestamps, account metadata, and the audit log.
- **OIDC token compromise:** an attacker with a stolen JWT can read
  the victim's snippets via the API. Mitigations: 30-day TTL with
  rolling refresh, plus admins can disable a user from the dashboard
  to invalidate active sessions on the next request.
- **Server compromise (shell access):** an attacker with the master
  key plus the DB can decrypt all personal snippets. This is the
  explicit v1.0 trust-boundary limit: the operator is inside the
  boundary.

For deployments where the trust model needs to be tighter (external
SaaS, regulated customer environments), the *Future: end-to-end
encryption* path under the Encryption section above describes the
upgrade.
