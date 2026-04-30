# Roadmap

Post-1.0 work, ordered by impact-per-effort. None of these block the offline
v1.0.0 ship.

---

## Phase B — Window-title parser (~1 day)

**Goal:** When an agent triggers SnipDesk while focused on a WHMCS ticket,
pre-fill `{ticket_id}`, `{customer_name}` etc. by parsing the foreground
browser window title — no extension, no API credentials.

**Scope**

- New module `src-tauri/src/foreground_title.rs` exposing `current_title() -> Option<String>`.
  - Windows: `GetForegroundWindow` + `GetWindowTextW`.
  - macOS: `NSWorkspace.frontmostApplication` via the `cocoa` crate.
  - Linux (X11): `xdotool getactivewindow getwindowname` shell-out, or
    direct `XGetWMName` via the `x11` crate. Wayland is a no-op for now.
- New module `crates/snipdesk-core/src/title_parser.rs` with regex bank
  per browser (Chrome, Edge, Firefox put the URL/title in slightly
  different formats). Returns `HashMap<String, String>` of variable
  candidates the user can override.
- Wire into the existing variable-prompt modal: pre-populate fields,
  highlight ones still requiring input.

**Why it's the first move:** zero install, zero permissions, instant win
for every agent on day one. The existing variable-substitution pipeline
already takes a `HashMap<String, String>` so integration is two lines.

**Risk:** browser title formats change occasionally. The parser bank is
data — keep regexes in a `serde_json` config file the user can edit
without recompiling.

---

## Phase C — WHMCS Admin API client (~1 week)

**Goal:** When the title parser identifies a ticket / client / invoice ID,
upgrade the pre-fill from "whatever's in the title" to authoritative
values pulled from WHMCS.

**Scope**

- Settings panel gains a "WHMCS" tab: base URL, API identifier, API
  secret, optional access key. Secrets via the `keyring` crate, never
  `settings.json`, never logs.
- New module `crates/snipdesk-core/src/whmcs.rs`:
  - `fetch_ticket_context(ticket_id) -> Result<HashMap<String, String>>`
  - `fetch_client_context(client_id) -> ...`
  - `fetch_invoice_context(invoice_id) -> ...`
  - Each maps to the WHMCS endpoints documented in
    `docs/browser-integration.md` (`GetTicket`, `GetClientsDetails`,
    `GetInvoice`, `GetClientsProducts`).
- Variable-name canon already drafted in
  `docs/browser-integration.md`. Implement that mapping verbatim so
  snippets stay portable to Kayako / Zendesk later.
- HTTP via `ureq` (already a workspace dep, blocking, no Tokio).
- Logging: endpoint + status code only. Never request/response bodies
  (customer PII).
- Cache: 60-second TTL keyed by ID. Avoids hammering WHMCS when the
  agent retriggers the launcher repeatedly.

**Why second:** higher value than Phase B, but Phase B is a strict subset
of the user-visible flow. Build B first so the pipeline is proven
end-to-end before introducing the credential-storage and HTTP plumbing.

**Risk:** WHMCS API per-agent credential management. Best practice is to
issue a credential per agent (one leaving doesn't invalidate the rest)
scoped to the read-only roles their role needs. Document this in the
settings UI's helper text.

---

## Code signing (~1 day setup, ongoing cost)

**Why:** Unsigned MSIs trigger SmartScreen warnings on every fresh
machine. Defender occasionally false-positives on hotkey-simulating
binaries.

**Two paths**

1. **Standard Authenticode certificate.** Reduces SmartScreen warnings
   over time as the binary builds reputation with each install. ~$200–400/yr.
2. **EV (Extended Validation) Authenticode.** SmartScreen reputation is
   granted immediately on first run. ~$300–700/yr, requires hardware
   token or cloud HSM.

**Constraint as of mid-2023:** all code-signing certificates (standard
and EV) require the private key to live on a hardware token or HSM.
Cannot ship a `.pfx` to GitHub Secrets and `signtool` against it.

**CI implementation options**

- Cloud signing service (SignPath, SSL.com eSigner, DigiCert KeyLocker):
  CI hits a signing API with credentials in secrets. ~$20–30/mo addon.
- Self-hosted signing runner: GH Actions runs on a machine with the USB
  token plugged in. Cheaper, less convenient.

**Sequencing:** ship v1.0.0 unsigned to a small internal pilot first.
Procurement of an EV cert takes 1–4 weeks (vetting), so kick that off
in parallel with Phase B development.

---

## Teams sync hardening (~2 weeks)

Currently `snipdesk-teams` is pull-only: clients fetch a JSON document
from a configured URL. Next milestones:

- **Two-way sync.** Agents publishing edits back to the team library.
  Conflict resolution (last-write-wins is the obvious starting point;
  per-snippet locks the obvious upgrade if collisions become common).
- **SSO-gated dashboards.** Today the URL is a bearer-style secret.
  Move to OAuth (Microsoft Entra / Google Workspace) so agents
  authenticate as themselves and the server can revoke per-user.
- **Server-side admin UI.** Currently the JSON document is hand-curated.
  A small Rails / Django / Next.js admin to manage it.

This is materially more work than Phase B/C combined. Don't start until
there's at least one paying team and concrete feedback on what they
actually want sync-wise.

---

## Smaller follow-ups

The kind of items you knock out in an afternoon when the bigger work
needs a break.

- **Bundle icons in the repo.** `src-tauri/icons/icon.ico` and friends
  currently regenerate on each fresh checkout via `npx @tauri-apps/cli icon`.
  Commit a generated set so a clean clone builds without that step.
- **Wayland paste fallback.** `enigo` doesn't drive Wayland. Detect
  `WAYLAND_DISPLAY` at runtime and force `paste_mode = "clipboard"` so
  the agent at least gets a copy-only flow instead of silent failure.
- **Side-by-side install identifiers.** `tauri.conf.json` uses the same
  `productName` and bundle identifier for free + Teams. If we want both
  installed on one machine, those need to differ (`SnipDesk` vs
  `SnipDesk Teams`, `com.shockbyte.snipdesk` vs
  `com.shockbyte.snipdesk-teams`). Deferred until Teams ships externally.
- **First-run onboarding.** `Settings.onboarding_completed` exists but
  the flow that flips it is currently minimal. A 3-screen welcome
  (hotkey, sample snippet, optional WHMCS setup) would help adoption.
- **Crash uplink for Teams flavor.** Local crash logs already work.
  An opt-in "send anonymized crash report" toggle would dramatically
  shorten our debug loop on rare bugs.
- **Localization.** Strings are inline in `src/main.js` and
  `src/index.html`. Lift them to a JSON dictionary, ship `en.json`
  first, accept community translations.

---

## Review cadence

Update this file when scope changes meaningfully or items ship. Every
shipped item moves to the README's "Recent changes" if we add one;
every new follow-up logged here gets a one-line entry in the relevant
section above.
