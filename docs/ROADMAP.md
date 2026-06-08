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
- **Paste telemetry (server + client)**: `POST /api/usage/report`
  with batched deltas; migration 0005 adds `users.chars_pasted /
  snippets_pasted`, per-user `wpm / hourly_wage / currency`
  overrides (via `PATCH /api/me`), `personal_snippets.usage_count`
  + `last_used`, and a `library_usage(user_id, snippet_id, ...)`
  junction table. Client tracks deltas in a `pending_telemetry`
  table and flushes via a snapshot/commit pattern so bumps during
  the network round-trip survive.
- **Dashboard money/time saved estimate**: computed per-user from
  real chars_pasted using each user's own wpm/wage/currency (server
  defaults as fallback). Live FX module (opt-in `[fx]` config,
  daily refresh from open.er-api.com, static AUD table as fallback)
  overlays a static rate table; currency dropdown on the stats
  page reweights the displayed value client-side, default picked
  from `navigator.language`.
- **Dashboard polish**: stat-card picker flipped to default-off
  with `+ Add card` menu (default set: Users, Admins, Hours, Money,
  Adoption); library page uses a responsive multi-column grid;
  per-snippet usage pill on library cards; per-user detail page
  shows pastes / hours / money / top library; HX-Trigger
  `libraryChanged` makes sidebar + cards refresh immediately on
  every mutation; currency symbols served as JS unicode escapes
  to dodge any layer that might re-encode the response.
- **Server polish from v1.0 audit**: dropped the unused
  `user_activity` table (migration 0006); per-route body limits
  (32 KiB default, 256 KiB for telemetry, 2 MiB for snippet +
  library content); opt-in CORS via `cors_allowed_origins` config;
  1-hour cache for the OIDC discovery document so each sign-in
  doesn't refetch Google's metadata; structured admin audit log
  (migration 0007, new `audit` module, `/dashboard/audit` tab)
  recording every user + library mutation with actor/target/details.

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

## Phase D - First-run onboarding (~half day)

**Goal:** A new user opens the app and lands on a guided modal that
sets them up in under a minute. Replaces today's silent first-launch
where `Settings.onboarding_completed` is set without the user
seeing anything.

**Scope (6 panels):**

1. **Welcome** - one sentence + app icon.
2. **Sign in** - prominent "Sign in with Google" button; email /
   password collapsed behind an "Other sign-in options" disclosure.
   Hidden entirely on SSO-only white-label builds (`window.__BRAND.ssoOnly`).
3. **Try the hotkey** - one-shot keypress listener confirms the
   user can fire their launcher hotkey.
4. **Typing speed test** - fixed ~25-word phrase; timer runs from
   first keystroke; `wpm = (words * 60) / seconds`. "Use this number"
   posts to `server_update_profile({ wpm })`.
5. **Wage + currency** - hourly wage + currency dropdown (locale
   default ported from the stats-page map). Skip retains server
   defaults. Save posts to `server_update_profile`.
6. **Done** - flips `settings.onboarding_completed = true` via
   `update_settings`.

**Reuse**: existing `<section class="modal hidden">` pattern,
`closeAllModals()`, the `onboarding_completed` settings flag,
`server_update_profile` IPC. Settings UI gains a "Replay
onboarding" button at the bottom of General that clears the flag
and re-opens the modal.

**No backend changes.** Roughly 150 lines HTML + 250 lines JS + 80
lines CSS.

---

## Phase E - White-label platform (~1.5 days)

**Goal:** Per-customer builds with custom name, icon, baked-in
server URL, and SSO-only sign-in flow, while the main github repo
stays brand-neutral. No customer config tracked in the tree.

**Hard invariant:** customer name / icons / URLs live entirely
outside this repo. Build picks them up via `$WHITELABEL_CONFIG`
pointing at a toml file. A stock `npm run tauri build` with no env
var continues to produce a "SnipDesk" branded binary.

**Scope:**

- `scripts/whitelabel.mjs` (new): prebuild step that reads
  `$WHITELABEL_CONFIG`, generates `src-tauri/src/whitelabel.rs`
  + `src/whitelabel.js` constants, patches `tauri.conf.json`
  (productName, identifier, icons, updater endpoint) from a
  checked-in `tauri.conf.json.template`, copies icons from the
  customer's `source_dir`. Idempotent; runs on every build.
- `scripts/whitelabel.example.toml` (new): documented sample
  showing the shape (brand, server.baked_url, server.sso_only,
  icons.source_dir, updater.endpoint).
- `src-tauri/tauri.conf.json.template` (new): the canonical
  pre-patch tauri config; the build script restores from it
  before patching so customer builds don't drift.
- One-time refactor: ~15-25 client-visible `"SnipDesk"` literals
  (window title, tray menu, About panel, settings copy,
  notification source) lifted to `whitelabel::BRAND_NAME` or
  `window.__BRAND.name`.
- `src/index.html` + `src/main.js`: `data-sso-only` wrapper on the
  server-section hides email/password when set; `set-server-url`
  pre-filled + readonly when a baked URL is configured; the boot
  flow auto-uses the baked URL without prompting.
- Server-side brand (just the dashboard nav header) via a new
  `[brand].dashboard_name` config field; default `"Admin"` so the
  server binary stays brand-neutral.

**Build flow:**

```sh
# Stock - "SnipDesk" everywhere
npm run tauri build

# Customer
WHITELABEL_CONFIG=/path/to/acme/whitelabel.toml npm run tauri build
```

**Out of scope (call out in docs):**

- Code-signing certs are per-publisher; SmartScreen still shows
  the cert owner, not the brand. Customer needs their own EV cert
  for full SmartScreen branding.
- Microsoft Store / Mac App Store listings are per-publisher;
  this is side-loaded MSI/DMG only.
- Auto-update server is baked per-build via the toml; customers
  needing their own release stream supply their own endpoint.

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

## Server polish

The v1.0-audit list is shipped (see "Shipped in v1.0" above). One
item from that audit stayed deliberately deferred:

- **Login-error enumeration**: today's distinct `no_account` /
  `wrong_password` / `account_disabled` codes are deliberate
  internal-tool ergonomics. Collapse to one opaque code only if
  SnipDesk ever ships server-side to a wider audience.

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
