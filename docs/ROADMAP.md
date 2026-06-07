# Roadmap

What's done, what's next, ordered by impact-per-effort. The big v1
work (offline launcher + Teams server-sync + shared library + SSO +
admin dashboard) is shipped; this is the list of things that come
after.

---

## Shipped in v1.0

- **Offline launcher** with hotkey, search, variables, folders, tags,
  usage tracking, auto-paste, import/export, local SQLite, auto-update.
- **Teams edition** layered on top: server-backed sync of personal
  snippets (AES-256-GCM at rest), shared team library, persistent
  login with rolling JWT refresh, trash + restore, configurable
  tombstone purge.
- **Self-hosted server** (`snipdesk-server`): Axum + SQLite, OIDC
  (Google Workspace) + password auth, htmx admin dashboard with
  folder-aware library curation + drag-and-drop, per-user detail
  pages, server-wide stats, CLI commands + in-process console for
  ops.
- **Cross-cutting**: disabled-account propagation, real-time role
  changes, role-gated admin endpoints, secure cookies, body size
  limits, health endpoint with 503 on DB down, sync version unique
  constraints, JWT algorithm pinning + iss/aud, OIDC state-store
  bounds.

---

## Phase B - Window-title parser (~1 day)

**Goal:** When an agent triggers SnipDesk while focused on a WHMCS
ticket, pre-fill `{ticket_id}`, `{customer_name}` etc. by parsing the
foreground browser window title - no extension, no API credentials.

**Scope**

- New module `src-tauri/src/foreground_title.rs` exposing
  `current_title() -> Option<String>`.
  - Windows: `GetForegroundWindow` + `GetWindowTextW`.
  - macOS: `NSWorkspace.frontmostApplication` via the `cocoa` crate.
  - Linux (X11): `xdotool getactivewindow getwindowname` shell-out,
    or direct `XGetWMName` via the `x11` crate. Wayland is a no-op
    for now.
- New module `crates/snipdesk-core/src/title_parser.rs` with a regex
  bank per browser. Returns `HashMap<String, String>` of variable
  candidates the user can override.
- Wire into the existing variable-prompt modal: pre-populate fields,
  highlight ones still requiring input.

**Why now:** zero install, zero permissions, instant win for every
agent on day one. The existing variable-substitution pipeline already
takes a `HashMap<String, String>` so integration is two lines.

---

## Phase C - WHMCS Admin API client (~1 week)

**Goal:** When the title parser identifies a ticket / client /
invoice ID, upgrade the pre-fill from "whatever's in the title" to
authoritative values pulled from WHMCS.

**Scope**

- Settings panel gains a "WHMCS" tab: base URL, API identifier, API
  secret, optional access key. Secrets via the `keyring` crate,
  never `settings.json`, never logs.
- New module `crates/snipdesk-core/src/whmcs.rs` with
  `fetch_ticket_context`, `fetch_client_context`,
  `fetch_invoice_context` mapping to the WHMCS endpoints documented
  in `docs/browser-integration.md`.
- Variable-name canon already drafted in
  `docs/browser-integration.md`; implement that mapping verbatim so
  snippets stay portable to other ticketing tools later.
- 60-second TTL cache keyed by ID.

**Risk:** per-agent credential management. Issue credentials per
agent (revoke on departure) scoped to the read-only roles their work
needs. Document in the settings UI's helper text.

---

## Code signing (~1 day setup, ongoing cost)

**Why:** Unsigned MSIs trigger SmartScreen warnings on every fresh
machine. Defender occasionally false-positives on hotkey-simulating
binaries.

**Paths**

1. **Standard Authenticode** - ~$200-400/yr. Reputation builds with
   each install.
2. **EV Authenticode** - ~$300-700/yr, hardware token. SmartScreen
   reputation granted immediately.

**Constraint:** all code-signing certificates require the private key
on a hardware token or HSM. Either use a cloud signing service
(SignPath, SSL.com eSigner, DigiCert KeyLocker) with CI secrets, or a
self-hosted runner with the token plugged in.

**Sequencing:** ship v1.0.0 unsigned to internal users first.
Procurement of an EV cert takes 1-4 weeks (vetting); kick off in
parallel with the polish items below.

---

## Conflict-loser preservation (v1.1, ~half day)

Today's sync is last-write-wins on concurrent edits to the same
snippet across devices. The loser is silently dropped. The design
doc maps the v1.1 upgrade: when a `PUT /api/snippets/:id` returns
409 (stale `expected_version`), the client preserves the local copy
as a new snippet titled `<original> (conflict YYYY-MM-DD)`, encrypts
+ uploads it. No data loss; user picks which version they want.

Wire shape compatible with current API - no protocol change needed.

---

## End-to-end encryption (v2)

The server currently holds the master key, so an operator with shell
access can decrypt personal snippets. The design doc spells out the
v2 upgrade path:

1. Client generates a `vault_key` on first signup.
2. Server stores the wrapped key (not the plain key) per user.
3. New snippets are encrypted with `vault_key` before upload; server
   never sees plaintext.
4. Existing rows are server-decrypted once during migration, sent to
   the now-logged-in client, re-encrypted with `vault_key`,
   re-uploaded; server discards the decryptable copy.

This is a v2 lift, not v1.x work. The current v1 schema is
forward-compatible so the migration doesn't break the wire protocol.

---

## Server polish (~2-3 days, mostly low-priority hardening)

From the v1.0 audit, items deferred because they're scale-or-policy
calls rather than active bugs:

- **Login-error enumeration**: today's distinct `no_account` /
  `wrong_password` / `account_disabled` codes are deliberate
  internal-tool ergonomics; collapse to one opaque code if SnipDesk
  ever ships server-side to a wider audience.
- **CORS**: opt-in `cors_allowed_origins` config + `tower_http::cors`
  layer for the day a separate web frontend lands.
- **Audit log**: structured `actor / target / action` entries for
  every admin mutation. Today the writes log via `tracing::info!`
  with ad-hoc fields; a real audit format would let operators answer
  "who promoted whom and when" without grepping.
- **Cached OIDC discovery**: the openidconnect crate hits Google's
  metadata endpoint on every request. Fine at v1 scale; cache for
  an hour if traffic grows.
- **Pre-aggregated `user_activity` table** (created in 0001, never
  referenced). Either wire it up or drop in a new migration.
- **Body limits on a per-route basis**: 2 MiB global cap is fine but
  `/api/admin/users` doesn't need it; `/api/snippets/*` is the
  natural target.

---

## Smaller follow-ups

The kind of items you knock out in an afternoon when the bigger work
needs a break.

- **Bundle icons in the repo.** `src-tauri/icons/icon.ico` and
  friends currently regenerate on each fresh checkout via
  `npx @tauri-apps/cli icon`. Commit a generated set so a clean clone
  builds without that step.
- **Wayland paste fallback.** `enigo` doesn't drive Wayland. Detect
  `WAYLAND_DISPLAY` at runtime and force `paste_mode = "clipboard"`
  so the agent at least gets a copy-only flow instead of silent
  failure.
- **First-run onboarding.** `Settings.onboarding_completed` exists
  but the flow that flips it is minimal. A 3-screen welcome (hotkey,
  sample snippet, optional Teams sign-in) would help adoption.
- **Crash uplink for Teams flavor.** Local crash logs already work.
  An opt-in "send anonymized crash report" toggle would dramatically
  shorten the debug loop on rare bugs.
- **Localization.** Strings are inline in `src/main.js` and
  `src/index.html`. Lift to a JSON dictionary, ship `en.json` first,
  accept community translations.
- **Mobile clients.** Explicitly non-goal in v1. If they ever land,
  the same JSON sync API serves them - no server-side work needed.
- **WebSocket / SSE push sync.** v1 uses 5-minute polling. Real-time
  push would shrink the cross-device sync window from minutes to
  seconds; not worth the complexity until a customer actually
  notices the lag.

---

## Review cadence

Update this file when scope changes meaningfully or items ship. The
"Shipped in v1.0" section above is the canonical record of what's
done; new follow-ups go in the smaller-follow-ups section with a
one-line entry.
