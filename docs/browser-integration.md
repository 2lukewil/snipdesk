# Browser / WHMCS Integration — Design Notes

**Goal:** When a support agent picks a snippet that contains `{customer_name}`, `{invoice_id}`, `{cancellation_date}`, etc., SnipDesk should pre-fill those values from whatever ticket / customer page the agent is currently looking at in their browser. The agent can still override before pasting.

Five realistic paths, what each can and can't do, and a recommendation at the end.

---

## Path comparison

| Path | What it reads | OS support | Works on SaaS | Extension install? | Ongoing maintenance |
| --- | --- | --- | --- | --- | --- |
| **1. Browser extension + native messaging** | Whatever the extension can see in the DOM of any tab it has host permissions for | Win / Mac / Linux | Yes (any site) | Yes, per-agent | Medium — extension must be kept in sync with WHMCS markup changes |
| **2. WHMCS API, triggered by active-tab URL sniff** | Whatever the WHMCS admin API returns for a given client/ticket/invoice ID | Win / Mac / Linux | WHMCS only | No | Low — WHMCS API is stable |
| **3. OS UI Automation (Windows UIA / macOS AX)** | Visible text in any foreground window, including browser | Win / Mac | Yes, if text is on screen | No | High — accessibility trees change per browser version |
| **4. Browser window title parsing** | Active tab URL + title only | Win / Mac / Linux | Limited | No | Low |
| **5. Clipboard trigger (e.g. "copy ticket ID, then hotkey")** | Whatever the agent copied last | All | Any | No | Very low |

### 1. Browser extension + native messaging (most power)

A Chrome/Firefox/Edge extension runs on WHMCS admin pages, scrapes the DOM, and sends a structured context packet (customer name, invoice ID, service, etc.) to SnipDesk via Chrome's [native messaging](https://developer.chrome.com/docs/apps/nativeMessaging) API.

**Flow**

```
Agent on WHMCS ticket  ──(extension sends ctx)──►  SnipDesk native host
                                                    ▼
Agent hits Ctrl+Shift+Space                 SnipDesk pre-fills variables
```

**What you'd build**

- A small browser extension (MV3) with `host_permissions` scoped to your WHMCS domain. Content scripts read specific DOM selectors and forward `{customer_name, customer_id, invoice_id, service_type, cancellation_date}` on page change or on request.
- A [native messaging host manifest](https://developer.chrome.com/docs/apps/nativeMessaging#native-messaging-host) pointing at the SnipDesk binary (or a small sidecar) with an API mode flag. Chrome spawns SnipDesk with stdin/stdout bound to the extension.
- An extra Rust module (`native_messaging.rs`) that handles the stdin message loop when the binary is launched by Chrome. Parses length-prefixed JSON, caches latest context in the running SnipDesk process (via IPC — e.g. a Unix socket on macOS/Linux, named pipe on Windows) or in a short-lived file.

**Pros:** Works on any WHMCS version regardless of API access. Can also work on Kayako, Zendesk, HelpScout with per-platform content scripts.
**Cons:** Agents must install the extension. Every WHMCS theme/template upgrade can break DOM selectors. You own and ship a browser extension (Chrome Web Store review, policy updates, etc.).

---

### 2. WHMCS Admin API + active-tab URL sniffing (recommended)

Most WHMCS pages encode the relevant ID in the URL:

- `/admin/clientssummary.php?userid=1234`
- `/admin/supporttickets.php?action=view&id=5678`
- `/admin/invoices.php?action=edit&id=9012`

If SnipDesk can read the active tab URL, it knows *which* record to look up, and then uses the [WHMCS Admin API](https://developers.whmcs.com/api/) (`GetClientsDetails`, `GetTicket`, `GetInvoice`, `GetClientsProducts`) to pull authoritative data.

**How to read the active tab URL without a browser extension**

| Method | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Window title parsing | Yes | Yes | Yes |
| Native automation (UIA / AppleScript / AT-SPI) | Yes | Yes | X11 only |

Most browsers already put the page URL or title in the window title. A small native shim in SnipDesk polls the foreground window title every ~500 ms when idle.

**Flow**

```
Agent focuses WHMCS ticket #5678 in Chrome
     │
     ▼
SnipDesk foreground-title watcher sees "Ticket #5678 - WHMCS"
     │  (parses id=5678)
     ▼
SnipDesk calls https://billing.shockbyte.com/includes/api.php
  action=GetTicket  ticketid=5678
     │
     ▼
Variables cached: {ticket_id, customer_name, service_type, ...}
     │
Agent hits hotkey, picks "Refund follow-up"
     ▼
Variable prompt pre-filled; agent tweaks, presses Enter, paste.
```

**Pros:**
- No browser extension. No per-agent install beyond SnipDesk.
- WHMCS API is stable and versioned — far lower maintenance.
- Works with 2FA/SSO agent sessions; API is keyed by an admin API credential, not the session.
- Credentials stored in SnipDesk settings (encrypted via OS keychain — [keyring](https://crates.io/crates/keyring) crate).

**Cons:**
- Needs a WHMCS Admin API identifier/secret. Those should be scoped per agent (so one leaving doesn't invalidate the rest) and stored in the OS keychain, never in the settings JSON.
- Only works for WHMCS records. For a generic "active tab" lookup (e.g. Zendesk), you'd fall back to extension or UIA.
- The title-parse needs a small amount of per-browser heuristics (Chrome/Edge/Firefox title formats differ slightly).

---

### 3. OS UI Automation (UIA on Windows, AX on macOS)

UIA can read the address bar value of Chrome/Edge/Firefox without any extension. Works, but fragile — browsers occasionally change their UIA tree structure and your detection breaks overnight.

Better as a *fallback* when URL isn't in the title, not as the primary integration.

---

### 4. Window title only

The laziest option. Many WHMCS themes put ticket number / customer name right in the page title (`"Support Ticket #5678 — John Smith"`). SnipDesk parses the browser window title with regex and fills what it can.

Zero install, zero permissions. But data quality depends on the theme putting enough info in the title.

Good cheap first cut — ship this on day one, then layer the WHMCS API on top.

---

### 5. Clipboard trigger

Agent copies a ticket ID, presses Ctrl+Shift+Space, SnipDesk sees the clipboard was just updated with a numeric ID and pre-fills. Dumb but zero-integration. Could be a nice ergonomic fallback.

---

## Recommended stack

Ship in this order — each step is useful on its own, none blocks the next.

### Phase A — already in v0.1

Manual variable prompts. The agent types the values. Works today, ships today.

### Phase B — window-title parser (low effort, high ceiling)

Add `foreground_title.rs` to the Rust side. On the system-hotkey "open" event, read the foreground window's title (via `GetForegroundWindow` + `GetWindowTextW` on Windows; `NSWorkspace.frontmostApplication` on macOS). Run it through a small regex library per browser, surface any extracted fields (`ticket_id`, `client_name`) to the frontend as pre-fill candidates.

**Cost:** a day of work. Half the value of full API integration with 1/10 the work.

### Phase C — WHMCS API integration (main event)

1. Settings panel gets a new "WHMCS" section: base URL, API identifier, API secret, (optional) accesskey. Secrets stored via the `keyring` crate, never in `settings.json`.
2. `whmcs.rs` module exposes `fetch_ticket_context(ticket_id)`, `fetch_client_context(client_id)`, `fetch_invoice_context(invoice_id)`. Each returns `HashMap<String, String>` mapping your standard variable names (`customer_name`, `service_type`, `cancellation_date`, `invoice_due_date`, `invoice_amount`, `ticket_subject`, …) to values.
3. When SnipDesk opens the variable-prompt modal, it first calls `whmcs::resolve_context(last_known_url_or_title)` to pre-populate. Fields the agent has to type themselves are highlighted; auto-filled ones are shown dimmed but editable.

**Cost:** a week-ish. Needs a list of canonical variable names and a mapping from each name to which WHMCS endpoint provides it.

### Phase D — browser extension (only if Phase C isn't enough)

Build the extension only if you need context from non-WHMCS tools (Zendesk, internal dashboards, game-panel URLs). Keep it as thin as possible — it just forwards a JSON blob to the native host. All business logic stays in SnipDesk.

**Cost:** extension scaffold (~2-3 days), plus native-messaging plumbing (~2 days), plus Chrome Web Store review and internal distribution (~1 week calendar). Start only after Phase C is in production and you have a concrete second use case.

---

## Security / data-handling notes

- WHMCS API credentials: OS keychain only. Never settings.json, never logs.
- API calls log only endpoint and status code, not bodies (customer PII).
- The app should only ever fetch from the WHMCS host configured in settings — never from arbitrary URLs.
- Before a paste, show the agent a preview of the filled-in body. Prevents "oh no, that was the wrong customer" moments.
- TLS certificate pinning for the WHMCS host is optional overkill, but do enforce HTTPS.
- If the browser is in an incognito window, the extension (Phase D) should refuse to read context unless the user opts in. Private-window traffic is typically not meant to be mined.

---

## Variable naming convention

To make snippets portable across providers (WHMCS today, Kayako tomorrow), standardize variable names in snippet bodies:

| Variable | Source |
| --- | --- |
| `{customer_name}` | `GetClientsDetails.fullname` |
| `{customer_email}` | `GetClientsDetails.email` |
| `{customer_id}` | `GetClientsDetails.id` |
| `{service_type}` | `GetClientsProducts.name` |
| `{service_domain}` | `GetClientsProducts.domain` |
| `{invoice_id}` | From URL or ticket relation |
| `{invoice_due_date}` | `GetInvoice.duedate` |
| `{invoice_amount}` | `GetInvoice.total` |
| `{cancellation_date}` | `GetClientsProducts.nextduedate` (or cancel request date) |
| `{ticket_id}` | From URL |
| `{ticket_subject}` | `GetTicket.subject` |
| `{agent_name}` | SnipDesk settings (local, not fetched) |

Snippets in the library then work regardless of which integration is live — if no provider resolves a variable, the agent still gets a prompt.

---

## Recommendation

Go Phase B → Phase C, skip the browser extension for now. Window-title parsing plus a WHMCS admin-API client covers ~95% of support-agent use cases with no agent-side extension install, no DOM-scraping maintenance, and no new attack surface in the browser. Revisit Phase D only when a second tool without a usable API forces our hand.
