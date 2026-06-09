# Browser / WHMCS integration: design notes

**Goal:** when a support agent picks a snippet containing
`{customer_name}`, `{invoice_id}`, `{cancellation_date}`, etc.,
SnipDesk pre-fills the values from whatever ticket or customer
page the agent is currently looking at in their browser. The agent
can still override before pasting.

These are design notes for a feature that is not in v1.0. They
exist so a future implementer doesn't have to redo the same
analysis. The recommendation at the bottom is the path most likely
to ship; the alternatives are documented to explain why they were
ruled out.

## Path comparison

| Path | What it reads | OS support | Works on SaaS | Extension install? | Maintenance |
| --- | --- | --- | --- | --- | --- |
| **1. Browser extension + native messaging** | DOM of any tab the extension has host permissions for | Win / Mac / Linux | Yes (any site) | Yes, per agent | Medium: extension must track WHMCS markup changes |
| **2. WHMCS Admin API + active-tab URL sniff** | What the WHMCS admin API returns for a given client/ticket/invoice ID | Win / Mac / Linux | WHMCS only | No | Low: WHMCS API is stable |
| **3. OS UI Automation (Windows UIA, macOS AX)** | Visible text in any foreground window, including the browser | Win / Mac | Yes, if text is on screen | No | High: accessibility trees shift per browser version |
| **4. Browser window title parsing** | Active tab URL + title only | Win / Mac / Linux | Limited | No | Low |
| **5. Clipboard trigger ("copy ticket ID, then hotkey")** | Whatever the agent copied last | All | Any | No | Very low |

## Why each path is or isn't a fit

### 1. Browser extension + native messaging

A Chrome/Firefox/Edge extension runs on WHMCS admin pages, scrapes
the DOM, and sends a structured context packet (customer name,
invoice ID, service, etc.) to SnipDesk via Chrome's
[native messaging](https://developer.chrome.com/docs/apps/nativeMessaging)
API.

- **Pros:** works on any WHMCS version regardless of API access.
  Can extend to Kayako, Zendesk, HelpScout, etc. with per-platform
  content scripts.
- **Cons:** agents must install the extension. Every WHMCS theme
  upgrade can break DOM selectors. Owning a browser extension
  means Chrome Web Store reviews, MV3 policy churn, and ongoing
  per-platform maintenance.

### 2. WHMCS Admin API + active-tab URL sniff (recommended)

Most WHMCS pages encode the relevant ID in the URL:

- `/admin/clientssummary.php?userid=1234`
- `/admin/supporttickets.php?action=view&id=5678`
- `/admin/invoices.php?action=edit&id=9012`

If SnipDesk can read the active tab URL, it knows which record to
look up, then uses the
[WHMCS Admin API](https://developers.whmcs.com/api/) (`GetClientsDetails`,
`GetTicket`, `GetInvoice`, `GetClientsProducts`) to pull authoritative
data.

Reading the active-tab URL without a browser extension:

| Method | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Window title parsing | Yes | Yes | Yes |
| Native automation (UIA / AppleScript / AT-SPI) | Yes | Yes | X11 only |

Most browsers already put the page URL or title in the window title.
A small native shim in SnipDesk polls the foreground window title
every ~500ms when idle.

**Flow**

```
Agent focuses WHMCS ticket #5678 in Chrome
     |
     v
SnipDesk foreground-title watcher sees "Ticket #5678 - WHMCS"
     |  (parses id=5678)
     v
SnipDesk calls https://billing.example.com/includes/api.php
  action=GetTicket  ticketid=5678
     |
     v
Variables cached: {ticket_id, customer_name, service_type, ...}
     |
Agent hits hotkey, picks "Refund follow-up"
     v
Variable prompt pre-filled; agent tweaks, presses Enter, paste.
```

- **Pros:** no browser extension, no per-agent install beyond
  SnipDesk. WHMCS API is stable and versioned. Works with 2FA/SSO
  agent sessions because the API is keyed by an admin credential,
  not the browser session. Credentials live in the OS keychain via
  the `keyring` crate, never in `settings.json`.
- **Cons:** needs a WHMCS Admin API identifier/secret, scoped per
  agent (so departures don't invalidate the rest). Only works for
  WHMCS records; generic "active tab" support would still need
  extension or UIA. Per-browser heuristics needed for title formats.

### 3. OS UI Automation

UIA can read the address bar value of Chrome/Edge/Firefox without
an extension. Fragile in practice: browsers occasionally change
their UIA tree structure and detection breaks overnight. Better as
a fallback when the URL isn't in the window title, not as a primary
integration.

### 4. Window title only

The cheapest option. Many WHMCS themes already put ticket number
and customer name in the page title
(`"Support Ticket #5678 - John Smith"`). SnipDesk parses the
browser window title with a regex and fills what it can.

Zero install, zero permissions. Data quality depends on whether
the theme puts enough info in the title. Good first cut on its own;
also the foundation that path 2 builds on.

### 5. Clipboard trigger

The agent copies a ticket ID, presses the hotkey, SnipDesk sees a
fresh numeric clipboard and pre-fills. Dumb but zero-integration.
Useful as an ergonomic fallback.

## Recommended path

Window-title parsing first, then WHMCS API on top of it.

### Step 1: window-title parser

Add `foreground_title.rs` to the Rust side. On the hotkey "open"
event, read the foreground window's title (`GetForegroundWindow` +
`GetWindowTextW` on Windows; `NSWorkspace.frontmostApplication` on
macOS). Run it through a small regex library per browser, surface
any extracted fields (`ticket_id`, `client_name`) to the frontend
as pre-fill candidates.

Roughly a day of work. Half the value of full API integration with
a tenth of the work.

### Step 2: WHMCS API integration

1. Settings panel gets a new "WHMCS" section: base URL, API
   identifier, API secret, optional accesskey. Secrets via the
   `keyring` crate, never in `settings.json`.
2. `whmcs.rs` module exposes `fetch_ticket_context(ticket_id)`,
   `fetch_client_context(client_id)`, `fetch_invoice_context(invoice_id)`.
   Each returns a `HashMap<String, String>` mapping canonical
   variable names (`customer_name`, `service_type`,
   `cancellation_date`, `invoice_due_date`, `invoice_amount`,
   `ticket_subject`, ...) to values.
3. When SnipDesk opens the variable-prompt modal, it first calls
   `whmcs::resolve_context(last_known_url_or_title)` to pre-populate.
   Fields the agent has to type are highlighted; auto-filled ones
   show dimmed but stay editable.

Roughly a week. Needs a canonical variable-name list and a mapping
from each name to the WHMCS endpoint that provides it.

### When (or whether) to build the browser extension

Only if context is needed from non-WHMCS tools (Zendesk, internal
dashboards, game-panel URLs). Keep the extension as thin as
possible: forward a JSON blob to a native host, leave all business
logic in SnipDesk. Costs: extension scaffold (2-3 days), native
messaging plumbing (2 days), Chrome Web Store review and
distribution (about a week of calendar time).

## Variable naming convention

To keep snippets portable across providers (WHMCS today, Kayako
tomorrow), variable names in snippet bodies should standardise on
canonical names. When a provider doesn't supply a variable, the
agent gets a prompt to fill it manually:

| Variable | Source |
| --- | --- |
| `{customer_name}` | `GetClientsDetails.fullname` |
| `{customer_email}` | `GetClientsDetails.email` |
| `{customer_id}` | `GetClientsDetails.id` |
| `{service_type}` | `GetClientsProducts.name` |
| `{service_domain}` | `GetClientsProducts.domain` |
| `{invoice_id}` | URL or ticket relation |
| `{invoice_due_date}` | `GetInvoice.duedate` |
| `{invoice_amount}` | `GetInvoice.total` |
| `{cancellation_date}` | `GetClientsProducts.nextduedate` (or cancel-request date) |
| `{ticket_id}` | URL |
| `{ticket_subject}` | `GetTicket.subject` |
| `{agent_name}` | SnipDesk settings (local, not fetched) |

## Security / data handling

- WHMCS API credentials live in the OS keychain. Never in
  `settings.json`, never in logs.
- API calls log only endpoint and status code, never bodies
  (customer PII).
- SnipDesk only ever calls the WHMCS host configured in settings,
  never arbitrary URLs.
- Before a paste, show the agent a preview of the filled body to
  catch wrong-customer mistakes.
- TLS certificate pinning for the WHMCS host is optional overkill;
  enforce HTTPS regardless.
- If the active browser tab is incognito, an extension (if shipped)
  should refuse to read context unless the user explicitly opts in.
